//! Explicit task-owned local Rowboat servers; no profile discovery/lifecycle.
use anyhow::{Result, ensure};
use rowboat_core::{
    ConnectionProfile, Database, Engine, QueryEvent, QueryStreamOptions, sql::quote_ident,
};
use std::time::Duration;
use tokio::time::timeout;

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        database: "rowboat".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 Podman fixtures"]
async fn capture_transactions_cleanup_on_error_and_dropped_stream_and_reject_temp_shadow()
-> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = Database::connect(&profile(engine), "rowboat-test").await?;
        let name = format!(
            "pass12_core_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let q = quote_ident(engine, &name);
        db.query(
            &format!(
                "CREATE {} {q}",
                if engine == Engine::Postgres {
                    "SCHEMA"
                } else {
                    "DATABASE"
                }
            ),
            0,
        )
        .await?;
        let result: Result<()> = async {
            if engine == Engine::Postgres {
                db.query(&format!("SET search_path TO {q}"), 0).await?;
            } else {
                db.select_database(&name).await?;
            }
            db.query(
                "CREATE TABLE records (id INT PRIMARY KEY, payload VARCHAR(100))",
                0,
            )
            .await?;
            db.query(
                "INSERT INTO records VALUES (1,'one'),(2,'two'),(3,'three')",
                0,
            )
            .await?;
            let (mut stream, capture) = db
                .query_stream_with_edit_source(
                    "SELECT * FROM records WHERE missing_column=1",
                    Default::default(),
                )
                .await?;
            let mut failed = false;
            while let Some(event) = stream.recv().await {
                failed |= matches!(event, QueryEvent::Error { .. });
            }
            ensure!(failed && capture.finish().await.read_only_reason().is_some());
            ensure!(!db.in_transaction().await?);
            let (mut stream, capture) = db
                .query_stream_with_edit_source(
                    "SELECT * FROM records",
                    QueryStreamOptions {
                        batch_rows: 1,
                        channel_capacity: 1,
                        ..Default::default()
                    },
                )
                .await?;
            ensure!(matches!(
                stream.recv().await,
                Some(QueryEvent::Schema { .. })
            ));
            drop(stream);
            ensure!(
                timeout(Duration::from_secs(10), capture.finish())
                    .await?
                    .read_only_reason()
                    .is_some()
            );
            ensure!(!db.in_transaction().await?);
            db.query(
                "CREATE TEMPORARY TABLE records (id INT PRIMARY KEY, payload VARCHAR(100))",
                0,
            )
            .await?;
            db.query("INSERT INTO records VALUES (9,'shadow')", 0)
                .await?;
            let (mut stream, capture) = db
                .query_stream_with_edit_source("SELECT * FROM records", Default::default())
                .await?;
            let mut rows = Vec::new();
            let mut complete = false;
            while let Some(event) = stream.recv().await {
                match event {
                    QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
                    QueryEvent::Complete { .. } => complete = true,
                    QueryEvent::Error { message, .. } => anyhow::bail!(message),
                    _ => {}
                }
            }
            ensure!(complete && rows.len() == 1 && rows[0][1].display() == "shadow");
            ensure!(capture.finish().await.read_only_reason().is_some());
            ensure!(!db.in_transaction().await?);
            Ok(())
        }
        .await;
        // Separate owner connection avoids temporary-name/default-context effects.
        let admin = Database::connect(&profile(engine), "rowboat-test").await?;
        let cleaned = admin
            .query(
                &match engine {
                    Engine::Sqlite => unreachable!("network fixture test"),
                    Engine::Postgres => format!("DROP SCHEMA {q} CASCADE"),
                    Engine::MySql => format!("DROP DATABASE {q}"),
                },
                0,
            )
            .await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit MySQL13309 Podman fixture"]
async fn unavailable_stable_identity_permission_never_blocks_the_original_query() -> Result<()> {
    let engine = Engine::MySql;
    let admin = Database::connect(&profile(engine), "rowboat-test").await?;
    let name = format!(
        "pass12_core_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let q = quote_ident(engine, &name);
    admin.query(&format!("CREATE DATABASE {q}"), 0).await?;
    admin
        .query(
            &format!("CREATE USER '{name}'@'%' IDENTIFIED BY 'rowboat-test'"),
            0,
        )
        .await?;
    let result:Result<()>=async {
        admin.query(&format!("CREATE TABLE {q}.records (id INT PRIMARY KEY,payload VARCHAR(100)); INSERT INTO {q}.records VALUES(1,'visible')"),0).await?;
        admin.query(&format!("GRANT SELECT,UPDATE ON {q}.* TO '{name}'@'%'"),0).await?;
        let mut limited=profile(engine);limited.username=name.clone();limited.database=name.clone();
        let db=Database::connect(&limited,"rowboat-test").await?;
        let (mut stream,capture)=db.query_stream_with_edit_source("SELECT * FROM records",Default::default()).await?;
        let mut complete=false;let mut count=0;
        while let Some(event)=stream.recv().await {
            match event {
                QueryEvent::Rows{rows,..}=>count+=rows.len(),
                QueryEvent::Complete{..}=>complete=true,
                QueryEvent::Error{message,..}=>anyhow::bail!(message),
                _=>{}
            }
        }
        ensure!(complete&&count==1);
        let source=capture.finish().await;
        ensure!(source.read_only_reason().is_some());
        ensure!(!db.in_transaction().await?);
        ensure!(db.query("SELECT 42",1).await?[0].rows[0][0].display()=="42");
        Ok(())
    }.await;
    let dropped_user = admin.query(&format!("DROP USER '{name}'@'%'"), 0).await;
    let cleaned = admin.query(&format!("DROP DATABASE {q}"), 0).await;
    result?;
    dropped_user?;
    cleaned?;
    Ok(())
}
