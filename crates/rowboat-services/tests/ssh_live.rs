//! Only the explicit disposable containers in scripts/test-ssh-databases.sh.
use anyhow::{Context, Result, ensure};
use rowboat_core::{ConnectionProfile, ConnectionTransport, Database, Engine, SshConfig};
use rowboat_services::{Tunnel, TunnelOptions};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/rowboat-ssh")
}
fn profile(engine: Engine) -> ConnectionProfile {
    let (host, port, username) = match engine {
        Engine::Postgres => ("postgres.rowboat.test", 5432, "postgres"),
        Engine::MySql => ("mysql.rowboat.test", 3306, "rowboat"),
        Engine::Sqlite => panic!("SQLite has no SSH fixture"),
    };
    ConnectionProfile {
        engine,
        host: host.into(),
        port,
        username: username.into(),
        database: "rowboat".into(),
        ssl_mode: "verify-full".into(),
        ssl_ca: Some(fixture().join("server/ca.crt").display().to_string()),
        read_only: true,
        ssh: Some(SshConfig {
            host: "127.0.0.1".into(),
            port: 12222,
            username: "root".into(),
            identity_file: Some(fixture().join("client/identity").display().to_string()),
        }),
        ..Default::default()
    }
}
fn options() -> TunnelOptions {
    TunnelOptions {
        known_hosts_file: Some(fixture().join("client/known_hosts")),
    }
}
fn socket(profile: &ConnectionProfile) -> &Path {
    match profile.transport.as_ref().expect("SSH transport missing") {
        ConnectionTransport::UnixSocket { path } => path,
    }
}

async fn tls_matrix(engine: Engine) -> Result<()> {
    let original = profile(engine);
    let (tunnel, forwarded) = Tunnel::open_with_options(&original, &options()).await?;
    ensure!(
        forwarded.host == original.host && forwarded.port == original.port,
        "Remote identity was changed"
    );
    ensure!(
        forwarded.ssh.is_none() && forwarded.transport.is_some(),
        "Invalid forwarded profile"
    );
    let encoded = serde_json::to_string(&forwarded)?;
    ensure!(
        !encoded.contains("transport")
            && !encoded.contains(".s.PGSQL")
            && !encoded.contains("database.sock"),
        "Ephemeral endpoint persisted"
    );
    let restored: ConnectionProfile = serde_json::from_str(&encoded)?;
    ensure!(
        restored.transport.is_none(),
        "Transport survived serialization"
    );
    let database = Database::connect(&forwarded, "rowboat-test").await?;
    ensure!(
        !database.query("SELECT 1", 10).await?.is_empty(),
        "Missing real result"
    );
    assert!(
        database
            .query("CREATE TABLE forbidden_ssh_test (id INT)", 10)
            .await
            .is_err()
    );
    let sleeper = database.clone();
    let query = match engine {
        Engine::Postgres => "SELECT pg_sleep(10)",
        Engine::MySql => "SELECT SLEEP(10)",
        Engine::Sqlite => anyhow::bail!("SQLite has no SSH fixture"),
    };
    let task = tokio::spawn(async move { sleeper.query(query, 10).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    database
        .cancel()
        .await
        .context("Cancellation must preserve SSH transport and TLS identity")?;
    ensure!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await??
            .is_err(),
        "Query did not cancel"
    );
    database.query("SELECT 1", 10).await?;
    drop(database);
    let endpoint = socket(&forwarded).to_owned();
    tunnel.close().await?;
    ensure!(
        !endpoint.exists(),
        "Explicit close left a forwarding socket"
    );

    // Wrong hostname still reaches the SAME server through its network alias.
    let mut wrong = original.clone();
    wrong.host = match engine {
        Engine::Postgres => "wrong-postgres.rowboat.test",
        Engine::MySql => "wrong-mysql.rowboat.test",
        Engine::Sqlite => anyhow::bail!("SQLite has no SSH fixture"),
    }
    .into();
    let (guard, wrong) = Tunnel::open_with_options(&wrong, &options()).await?;
    assert!(
        Database::connect(&wrong, "rowboat-test").await.is_err(),
        "Wrong remote TLS hostname accepted"
    );
    guard.close().await?;
    let mut untrusted = original.clone();
    untrusted.ssl_ca = Some(
        fixture()
            .join("client/untrusted-ca.crt")
            .display()
            .to_string(),
    );
    let (guard, untrusted) = Tunnel::open_with_options(&untrusted, &options()).await?;
    assert!(
        Database::connect(&untrusted, "rowboat-test").await.is_err(),
        "Untrusted remote CA accepted"
    );
    guard.close().await?;

    let mut certificate = original;
    certificate.username = "rowboat_cert".into();
    let (guard, missing) = Tunnel::open_with_options(&certificate, &options()).await?;
    assert!(
        Database::connect(&missing, "rowboat-test").await.is_err(),
        "Missing required client certificate accepted"
    );
    guard.close().await?;
    certificate.ssl_cert = Some(fixture().join("client/client.crt").display().to_string());
    certificate.ssl_key = Some(fixture().join("client/client.key").display().to_string());
    let (guard, certificate) = Tunnel::open_with_options(&certificate, &options()).await?;
    let database = Database::connect(&certificate, "rowboat-test").await?;
    database.query("SELECT 1", 10).await?;
    drop(database);
    guard.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; disposable Podman SSH/TLS only"]
async fn postgres_authenticated_ssh_tls_identity_mtls_cancel_readonly() -> Result<()> {
    tls_matrix(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; disposable Podman SSH/TLS only"]
async fn mysql_authenticated_ssh_tls_identity_mtls_cancel_readonly() -> Result<()> {
    tls_matrix(Engine::MySql).await
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; verifies immutable strict SSH trust"]
async fn ssh_rejects_unknown_changed_hosts_and_unauthorized_key() -> Result<()> {
    for name in ["empty_known_hosts", "wrong_known_hosts"] {
        let path = fixture().join("client").join(name);
        let before = std::fs::read(&path)?;
        let options = TunnelOptions {
            known_hosts_file: Some(path.clone()),
        };
        assert!(
            Tunnel::open_with_options(&profile(Engine::Postgres), &options)
                .await
                .is_err(),
            "Untrusted SSH host was accepted"
        );
        ensure!(
            std::fs::read(&path)? == before,
            "SSH modified the explicit trust file"
        );
    }
    let mut unauthorized = profile(Engine::Postgres);
    unauthorized.ssh.as_mut().unwrap().identity_file =
        Some(fixture().join("client/unauthorized").display().to_string());
    assert!(
        Tunnel::open_with_options(&unauthorized, &options())
            .await
            .is_err(),
        "Unauthorized identity authenticated"
    );
    let path = fixture().join("client/known_hosts");
    let before = std::fs::read(&path)?;
    let (guard, _) = Tunnel::open_with_options(&profile(Engine::Postgres), &options()).await?;
    guard.close().await?;
    ensure!(
        std::fs::read(&path)? == before,
        "SSH modified a trusted hosts file"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires scripts/test-ssh-databases.sh up; checks real process/socket Drop cleanup"]
async fn ssh_drop_terminates_master_and_removes_private_socket() -> Result<()> {
    let (guard, forwarded) =
        Tunnel::open_with_options(&profile(Engine::Postgres), &options()).await?;
    let endpoint = socket(&forwarded).to_owned();
    let directory = endpoint
        .parent()
        .context("Missing tunnel directory")?
        .to_owned();
    let output = tokio::process::Command::new("ssh")
        .args(["-F", "/dev/null", "-S"])
        .arg(directory.join("control"))
        .args(["-O", "check", "127.0.0.1"])
        .output()
        .await?;
    ensure!(output.status.success(), "SSH master was not ready");
    let stderr = String::from_utf8(output.stderr)?;
    let pid = stderr
        .split("pid=")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .context("SSH control reply did not contain PID")?
        .parse::<i32>()?;
    drop(guard);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ensure!(
        unsafe { libc::kill(pid, 0) } != 0,
        "SSH child remained after Drop"
    );
    ensure!(
        !directory.exists(),
        "Private SSH directory remained after Drop"
    );
    assert!(tokio::net::UnixStream::connect(endpoint).await.is_err());
    Ok(())
}
