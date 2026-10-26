//! Real native clients in Podman over authenticated SSH and strict TLS/mTLS.
//! Uses only labeled fixtures from scripts/test-ssh-databases.sh and unique DBs.
use anyhow::{Context, Result, ensure};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, SshConfig};
use rowboat_services::{ClientRuntime, Tunnel, TunnelOptions, backup, restore};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/rowboat-ssh")
}
fn runtime(engine: Engine) -> ClientRuntime {
    ClientRuntime::Podman {
        image: match engine {
            Engine::Postgres => "docker.io/library/postgres:18",
            Engine::MySql => "docker.io/library/mysql:8.4",
            Engine::Sqlite => panic!("SQLite has no SSH fixture"),
        }
        .into(),
    }
}
fn options() -> TunnelOptions {
    TunnelOptions {
        known_hosts_file: Some(fixture().join("client/known_hosts")),
    }
}
fn profile(engine: Engine, database: &str) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: match engine {
            Engine::Postgres => "postgres.rowboat.test",
            Engine::MySql => "mysql.rowboat.test",
            Engine::Sqlite => panic!("SQLite has no SSH fixture"),
        }
        .into(),
        port: match engine {
            Engine::Postgres => 5432,
            Engine::MySql => 3306,
            Engine::Sqlite => panic!("SQLite has no SSH fixture"),
        },
        database: database.into(),
        username: "rowboat_cert".into(),
        ssl_mode: "verify-full".into(),
        ssl_ca: Some(fixture().join("server/ca.crt").display().to_string()),
        ssl_cert: Some(fixture().join("client/client.crt").display().to_string()),
        ssl_key: Some(fixture().join("client/client.key").display().to_string()),
        ssh: Some(SshConfig {
            host: "127.0.0.1".into(),
            port: 12222,
            username: "root".into(),
            identity_file: Some(fixture().join("client/identity").display().to_string()),
        }),
        ..Default::default()
    }
}

fn admin(engine: Engine, sql: &str) -> Result<()> {
    let name = match engine {
        Engine::Postgres => "rowboat-test-postgres-ssh",
        Engine::MySql => "rowboat-test-mysql-ssh",
        Engine::Sqlite => anyhow::bail!("SQLite has no SSH fixture"),
    };
    let label = Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"io.rowboat.fixture\"}}",
            name,
        ])
        .output()?;
    ensure!(
        label.status.success() && label.stdout == b"ssh\n",
        "Refusing an unrelated database fixture"
    );
    let mut command = Command::new("podman");
    command.args(["exec", "-i"]);
    match engine {
        Engine::Sqlite => anyhow::bail!("SQLite has no SSH fixture"),
        Engine::Postgres => {
            command.args([
                name,
                "psql",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                "postgres",
                "-d",
                "rowboat",
            ]);
        }
        Engine::MySql => {
            command.env("MYSQL_PWD", "rowboat-test").args([
                "--env",
                "MYSQL_PWD",
                name,
                "mysql",
                "-u",
                "root",
            ]);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("Missing fixture SQL stdin")?
        .write_all(sql.as_bytes())?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "Task-owned fixture SQL failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct OwnedDatabase {
    engine: Engine,
    name: Option<String>,
}
impl OwnedDatabase {
    fn create(engine: Engine) -> Result<Self> {
        let name = format!("rowboat_ssh_backup_{}", uuid::Uuid::new_v4().simple());
        let mut database = Self {
            engine,
            name: Some(name.clone()),
        };
        let sql = match engine {
            Engine::Sqlite => anyhow::bail!("SQLite has no SSH fixture"),
            Engine::Postgres => format!("CREATE DATABASE \"{name}\" OWNER rowboat_cert;"),
            Engine::MySql => {
                format!("CREATE DATABASE `{name}`; GRANT ALL ON `{name}`.* TO 'rowboat_cert'@'%';")
            }
        };
        if let Err(error) = admin(engine, &sql) {
            let _ = database.cleanup();
            return Err(error);
        }
        Ok(database)
    }
    fn name(&self) -> &str {
        self.name.as_deref().expect("active database")
    }
    fn cleanup(&mut self) -> Result<()> {
        if let Some(name) = &self.name {
            let sql = match self.engine {
                Engine::Sqlite => anyhow::bail!("SQLite has no SSH fixture"),
                Engine::Postgres => format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE);"),
                Engine::MySql => format!(
                    "DROP DATABASE IF EXISTS `{name}`; REVOKE ALL PRIVILEGES ON `{name}`.* FROM 'rowboat_cert'@'%';"
                ),
            };
            admin(self.engine, &sql)?;
            self.name = None;
        }
        Ok(())
    }
}
impl Drop for OwnedDatabase {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn clients(id: &str) -> Result<Vec<String>> {
    let output = Command::new("podman")
        .args([
            "ps",
            "--all",
            "--filter",
            &format!("label=io.rowboat.connection-id={id}"),
            "--format",
            "{{.Names}}",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "Could not inspect task-owned native clients"
    );
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

async fn roundtrip(engine: Engine) -> Result<()> {
    let mut owned = OwnedDatabase::create(engine)?;
    let original = profile(engine, owned.name());
    let (guard, forwarded) = Tunnel::open_with_options(&original, &options()).await?;
    let database = Database::connect(&forwarded, "rowboat-test").await?;
    database.query("CREATE TABLE roundtrip (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO roundtrip VALUES (1, '日本語 quoted'' text'), (2, NULL), (3, 'NULL');", 20).await?;
    let before = database
        .query("SELECT id, note FROM roundtrip ORDER BY id", 20)
        .await?
        .remove(0)
        .rows;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("backup.sql");
    let client = runtime(engine);
    // Backup accepts read-only profiles; restore must reject them before tools.
    let mut read_only = forwarded.clone();
    read_only.read_only = true;
    backup(&read_only, "rowboat-test", &path, &client).await?;
    ensure!(
        restore(&read_only, "rowboat-test", &path, &client)
            .await
            .is_err(),
        "Read-only restore accepted"
    );
    ensure!(
        clients(&forwarded.id)?.is_empty(),
        "Successful client container leaked"
    );

    let marker = directory.path().join("marker.sql");
    std::fs::write(
        &marker,
        "CREATE TABLE rowboat_tls_rejection_marker (id INTEGER);",
    )?;
    let mut wrong_name = forwarded.clone();
    wrong_name.host = "wrong.rowboat.test".into();
    let mut wrong_ca = forwarded.clone();
    wrong_ca.ssl_ca = Some(
        fixture()
            .join("client/untrusted-ca.crt")
            .display()
            .to_string(),
    );
    let mut missing_cert = forwarded.clone();
    missing_cert.ssl_cert = None;
    missing_cert.ssl_key = None;
    for rejected in [wrong_name, wrong_ca, missing_cert] {
        let rejected_path = directory.path().join("rejected.sql");
        ensure!(
            backup(&rejected, "rowboat-test", &rejected_path, &client)
                .await
                .is_err(),
            "Unverified backup connected"
        );
        ensure!(
            !rejected_path.exists(),
            "Failed backup published a destination"
        );
        ensure!(
            restore(&rejected, "rowboat-test", &marker, &client)
                .await
                .is_err(),
            "Unverified restore connected"
        );
        ensure!(
            clients(&forwarded.id)?.is_empty(),
            "Failed client container leaked"
        );
    }
    let marker_count = database.query("SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'rowboat_tls_rejection_marker'", 10).await?;
    ensure!(
        marker_count[0].rows[0][0] == Cell::Text("0".into()),
        "Rejected restore executed SQL"
    );
    database.query("DROP TABLE roundtrip", 10).await?;
    restore(&forwarded, "rowboat-test", &path, &client).await?;
    let after = database
        .query("SELECT id, note FROM roundtrip ORDER BY id", 20)
        .await?
        .remove(0)
        .rows;
    ensure!(
        after == before,
        "SSH native-client restore changed Unicode/NULL/text-NULL data"
    );
    ensure!(
        clients(&forwarded.id)?.is_empty(),
        "Restore client container leaked"
    );
    drop(database);
    guard.close().await?;
    owned.cleanup()?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; unique fixture database only"]
async fn postgres_podman_ssh_backup_restore_mtls_and_rejections() -> Result<()> {
    roundtrip(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; unique fixture database only"]
async fn mysql_podman_ssh_backup_restore_mtls_and_rejections() -> Result<()> {
    roundtrip(Engine::MySql).await
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; verifies real cancelled/timeout client cleanup"]
async fn postgres_ssh_client_cancellation_and_timeout_release_relay_and_container() -> Result<()> {
    let mut owned = OwnedDatabase::create(Engine::Postgres)?;
    let (guard, forwarded) =
        Tunnel::open_with_options(&profile(Engine::Postgres, owned.name()), &options()).await?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("slow.sql");
    std::fs::write(&path, "SELECT pg_sleep(30);")?;
    for mode in ["abort", "timeout", "kill"] {
        let profile = forwarded.clone();
        let source = path.clone();
        let task = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(3),
                restore(
                    &profile,
                    "rowboat-test",
                    &source,
                    &runtime(Engine::Postgres),
                ),
            )
            .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let name = loop {
            if let Some(name) = clients(&forwarded.id)?.first() {
                let state = Command::new("podman")
                    .args(["inspect", "--format", "{{.State.Running}}", name])
                    .output()?;
                if state.status.success() && state.stdout == b"true\n" {
                    break name.clone();
                }
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Native client never started"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        // Only the task-owned client's nonsensitive environment is inspected;
        // its password is in a private mounted passfile, never in the env.
        let output = Command::new("podman")
            .args(["inspect", "--format", "{{json .Config.Env}}", &name])
            .output()?;
        let environment: Vec<String> = serde_json::from_slice(&output.stdout)?;
        let port: u16 = environment
            .iter()
            .find_map(|s| s.strip_prefix("PGPORT="))
            .context("Relay port missing")?
            .parse()?;
        match mode {
            "abort" => {
                task.abort();
                ensure!(
                    task.await.unwrap_err().is_cancelled(),
                    "Restore task was not cancelled"
                );
            }
            "timeout" => ensure!(task.await?.is_err(), "Restore did not time out"),
            "kill" => {
                let status = Command::new("podman")
                    .args(["kill", &name])
                    .stdout(Stdio::null())
                    .status()?;
                ensure!(status.success(), "Could not kill operation-owned client");
                ensure!(
                    task.await?
                        .context("Killed client did not return before timeout")?
                        .is_err(),
                    "Killed client reported success"
                );
            }
            _ => unreachable!(),
        }
        ensure!(
            clients(&forwarded.id)?.is_empty(),
            "Cancelled/timeout client container leaked"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("Cancelled/timeout relay listener leaked")?;
        let database = Database::connect(&forwarded, "rowboat-test").await?;
        database.query("SELECT 1", 10).await?; // caller-owned Tunnel is still usable
        drop(database);
    }
    guard.close().await?;
    owned.cleanup()?;
    Ok(())
}
