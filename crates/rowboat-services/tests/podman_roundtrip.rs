//! Explicitly opt-in disposable databases; never reads saved/user connections.
use anyhow::{Context, Result, ensure};
use rowboat_core::{Cell, Column, ConnectionProfile, Database, Engine, QueryResult, TableRef};
use rowboat_services::{ClientRuntime, backup, export_sql, restore};
use std::{process::Command, time::Duration};

struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "--force", &self.0])
            .output();
    }
}

async fn roundtrip(engine: Engine) -> Result<()> {
    let name = format!("rowboat-services-test-{}", uuid::Uuid::new_v4());
    let password = "rowboat-test:'quoted\\password";
    // The MySQL image's initialization script interpolates its initial password
    // into SQL. Set the punctuation-rich password through SQL after startup.
    let initial_password = match engine {
        Engine::Postgres => password,
        Engine::MySql => "rowboat-bootstrap",
        Engine::Sqlite => anyhow::bail!("SQLite has no Podman fixture"),
    };
    let (image, internal, user) = match engine {
        Engine::Postgres => ("docker.io/library/postgres:18", "5432", "postgres"),
        Engine::MySql => ("docker.io/library/mysql:8.4", "3306", "root"),
        Engine::Sqlite => anyhow::bail!("SQLite has no Podman fixture"),
    };
    let mut command = Command::new("podman");
    command.args([
        "run",
        "--detach",
        "--rm",
        "--pull=never",
        "--name",
        &name,
        "--publish",
        &format!("127.0.0.1::{internal}"),
    ]);
    match engine {
        Engine::Sqlite => anyhow::bail!("SQLite has no Podman fixture"),
        Engine::Postgres => {
            command.env("POSTGRES_PASSWORD", password).args([
                "--env",
                "POSTGRES_PASSWORD",
                "--env",
                "POSTGRES_DB=rowboat_services",
            ]);
        }
        Engine::MySql => {
            command.env("MYSQL_ROOT_PASSWORD", initial_password).args([
                "--env",
                "MYSQL_ROOT_PASSWORD",
                "--env",
                "MYSQL_DATABASE=rowboat_services",
                "--env",
                "MYSQL_ROOT_HOST=%",
            ]);
        }
    }
    let output = command
        .arg(image)
        .output()
        .context("Run disposable Podman database")?;
    ensure!(
        output.status.success(),
        "Podman database failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _container = Container(name.clone());
    let output = Command::new("podman")
        .args(["port", &name, &format!("{internal}/tcp")])
        .output()?;
    ensure!(output.status.success(), "Read disposable database's port");
    let address = String::from_utf8(output.stdout)?;
    let port = address
        .trim()
        .rsplit(':')
        .next()
        .context("Missing port")?
        .parse()?;
    let profile = ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port,
        database: "rowboat_services".into(),
        username: user.into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let database = loop {
        match Database::connect(&profile, initial_password).await {
            Ok(database) => break database,
            Err(error) => {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "Disposable database did not become ready: {error:#}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    if engine == Engine::MySql {
        let literal = password.replace('\\', "\\\\").replace('\'', "''");
        database
            .query(
                &format!("ALTER USER CURRENT_USER() IDENTIFIED BY '{literal}'"),
                100,
            )
            .await?;
    }
    let binary_type = match engine {
        Engine::Postgres => "BYTEA",
        Engine::MySql => "BLOB",
        Engine::Sqlite => anyhow::bail!("SQLite has no Podman fixture"),
    };
    database.query(&format!("CREATE TABLE service_roundtrip (id INTEGER PRIMARY KEY, note TEXT, payload {binary_type})"), 100).await?;
    let result = QueryResult {
        columns: ["id", "note", "payload"]
            .into_iter()
            .map(|name| Column {
                name: name.into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: name == "id",
                default_value: None,
            })
            .collect(),
        rows: vec![
            vec![
                Cell::Text("1".into()),
                Cell::Text("quote'\\\n日本語; DROP TABLE innocent; --".into()),
                Cell::Binary(vec![0, 1, 255]),
            ],
            vec![
                Cell::Text("2".into()),
                Cell::Text("NULL".into()),
                Cell::Null,
            ],
            vec![Cell::Text("3".into()), Cell::Null, Cell::Binary(vec![])],
        ],
        ..Default::default()
    };
    let table = TableRef {
        schema: String::new(),
        name: "service_roundtrip".into(),
    };
    database
        .query(&export_sql(engine, &table, &result)?, 100)
        .await?;
    let before = database
        .query(
            "SELECT id, note, payload FROM service_roundtrip ORDER BY id",
            100,
        )
        .await?;
    assert_eq!(
        before.last().context("Missing initial query result")?.rows,
        result.rows
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("backup.sql");
    let runtime = ClientRuntime::Podman {
        image: image.into(),
    };
    backup(&profile, password, &path, &runtime).await?;
    ensure!(std::fs::metadata(&path)?.len() > 0, "Empty backup");
    assert!(backup(&profile, password, &path, &runtime).await.is_err());
    database.query("DROP TABLE service_roundtrip", 100).await?;
    restore(&profile, password, &path, &runtime).await?;
    let after = database
        .query(
            "SELECT id, note, payload FROM service_roundtrip ORDER BY id",
            100,
        )
        .await?;
    assert_eq!(
        after.last().context("Missing restored query result")?.rows,
        result.rows
    );
    Ok(())
}

#[tokio::test]
#[ignore = "starts a disposable local Podman PostgreSQL 18 container; image must already exist"]
async fn postgres_sql_export_backup_restore() -> Result<()> {
    roundtrip(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "starts a disposable local Podman MySQL 8.4 container; image must already exist"]
async fn mysql_sql_export_backup_restore() -> Result<()> {
    roundtrip(Engine::MySql).await
}
