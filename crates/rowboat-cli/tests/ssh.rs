//! End-to-end saved-profile CLI transport with an explicit fixture trust file.
use anyhow::{Result, ensure};
use rowboat_core::{ConnectionProfile, Engine, SshConfig};
use rowboat_services::{AppState, Store};
use std::{path::Path, process::Command};

fn exercise(engine: Engine) -> Result<()> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/rowboat-ssh");
    let directory = tempfile::tempdir()?;
    let profile = ConnectionProfile {
        name: "CLI SSH fixture".into(),
        engine,
        host: if engine == Engine::Postgres {
            "postgres.rowboat.test"
        } else {
            "mysql.rowboat.test"
        }
        .into(),
        port: if engine == Engine::Postgres {
            5432
        } else {
            3306
        },
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "rowboat"
        }
        .into(),
        ssl_mode: "verify-full".into(),
        ssl_ca: Some(fixture.join("server/ca.crt").display().to_string()),
        read_only: true,
        ssh: Some(SshConfig {
            host: "127.0.0.1".into(),
            port: 12222,
            username: "root".into(),
            identity_file: Some(fixture.join("client/identity").display().to_string()),
        }),
        ..Default::default()
    };
    let id = profile.id.clone();
    let store = Store::open_at(directory.path())?;
    store.save(&AppState {
        connections: vec![profile],
        ..Default::default()
    })?;
    let before = std::fs::read(store.path().join("state.json"))?;
    let run = |trust: &str| {
        Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
            .env("ROWBOAT_DATA_DIR", directory.path())
            .env("ROWBOAT_PASSWORD", "rowboat-test")
            .env_remove("ROWBOAT_URL")
            .env_remove("ROWBOAT_SSH_KNOWN_HOSTS")
            .args(["--connection", &id, "--ssh-known-hosts"])
            .arg(fixture.join("client").join(trust))
            .args(["query", "--sql", "SELECT 42 AS answer", "--format", "json"])
            .output()
    };
    let valid = run("known_hosts")?;
    ensure!(
        valid.status.success(),
        "CLI SSH failed: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&valid.stdout)?;
    ensure!(result[0]["answer"] == "42");
    let invalid = run("wrong_known_hosts")?;
    ensure!(!invalid.status.success(), "Changed SSH host was accepted");
    ensure!(
        !String::from_utf8_lossy(&invalid.stderr).contains("rowboat-test"),
        "Error leaked a credential"
    );
    ensure!(
        std::fs::read(store.path().join("state.json"))? == before,
        "Ephemeral transport was persisted"
    );
    Ok(())
}

#[test]
#[ignore = "Requires mise run db:ssh:up"]
fn postgres_cli_ssh_preserves_verified_hostname() -> Result<()> {
    exercise(Engine::Postgres)
}

#[test]
#[ignore = "Requires mise run db:ssh:up"]
fn mysql_cli_ssh_preserves_verified_hostname() -> Result<()> {
    exercise(Engine::MySql)
}
