//! Real TLS handshakes through rowboat-core, including server-side encryption proof.
//! Prerequisite: bash scripts/test-tls-databases.sh up (or mise run db:tls:up).
//! Run: mise exec -- cargo test -p rowboat-tests --test tls -- --ignored --nocapture
//! Requires cached postgres:18/mysql:8.4, rootless Podman and host OpenSSL.
//! Cleanup: bash scripts/test-tls-databases.sh down (or mise run db:tls:down).
//! The tests use existing fixtures; the integration task owns their lifecycle.

use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn certificate(relative: &str) -> String {
    root()
        .join(".local/rowboat-tls")
        .join(relative)
        .to_string_lossy()
        .into()
}

fn profile(engine: Engine, mode: &str, host: &str) -> ConnectionProfile {
    ConnectionProfile {
        name: "Rowboat disposable TLS database".into(),
        engine,
        host: host.into(),
        port: if engine == Engine::Postgres {
            15433
        } else {
            13307
        },
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "rowboat"
        }
        .into(),
        ssl_mode: mode.into(),
        ssl_ca: Some(certificate("server/ca.crt")),
        ..Default::default()
    }
}

async fn encrypted(profile: &ConnectionProfile, password: &str) -> Result<()> {
    let db = Database::connect(profile, password)
        .await
        .with_context(|| {
            format!(
                "{:?} {} host={} user={}",
                profile.engine, profile.ssl_mode, profile.host, profile.username
            )
        })?;
    let result = db.query("SELECT 42 AS answer", 10).await?;
    ensure!(
        result.len() == 1 && result[0].rows == vec![vec![Cell::Text("42".into())]],
        "Live query failed"
    );
    match profile.engine {
        Engine::Postgres => {
            let result = db.query(
                "SELECT CASE WHEN ssl THEN 'encrypted' ELSE 'plaintext' END, version, cipher, client_dn FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                10,
            ).await?;
            ensure!(
                result.len() == 1 && result[0].rows.len() == 1,
                "Missing PostgreSQL TLS session status"
            );
            let row = &result[0].rows[0];
            ensure!(
                row[0].display() == "encrypted",
                "PostgreSQL session is plaintext"
            );
            ensure!(
                row[1].display().starts_with("TLSv1."),
                "Missing TLS protocol"
            );
            ensure!(
                matches!(&row[2], Cell::Text(cipher) if !cipher.is_empty()),
                "Missing TLS cipher"
            );
            if profile.username == "rowboat_cert" {
                ensure!(
                    row[3].display().contains("CN=rowboat_cert"),
                    "Server did not authenticate the supplied client certificate"
                );
            }
        }
        Engine::MySql => {
            let result = db
                .query("SHOW SESSION STATUS LIKE 'Ssl_cipher'", 10)
                .await?;
            ensure!(
                result.len() == 1 && result[0].rows.len() == 1,
                "Missing MySQL TLS session status"
            );
            ensure!(
                matches!(&result[0].rows[0][1], Cell::Text(cipher) if !cipher.is_empty()),
                "MySQL session is plaintext"
            );
            if profile.username == "rowboat_cert" {
                let result = db.query("SELECT CURRENT_USER()", 10).await?;
                ensure!(
                    result.len() == 1
                        && result[0].rows.len() == 1
                        && result[0].rows[0][0].display() == "rowboat_cert@%",
                    "Wrong MySQL X509 account"
                );
            }
        }
        Engine::Sqlite => bail!("SQLite file connections do not use TLS"),
    }
    Ok(())
}

async fn rejected(profile: &ConnectionProfile, reason: &str, expected: &[&str]) -> Result<()> {
    match Database::connect(profile, "rowboat-test").await {
        Ok(_) => bail!("{:?} unexpectedly accepted {reason}", profile.engine),
        Err(error) => {
            let message = format!("{error:#}").to_lowercase();
            ensure!(
                expected.iter().any(|needle| message.contains(needle)),
                "{:?} {reason} failed for an unexpected reason: {message}",
                profile.engine
            );
            println!("{:?}: rejected {reason}", profile.engine);
        }
    }
    Ok(())
}

async fn exercise(engine: Engine) -> Result<()> {
    // Encryption-only accepts a name absent from the certificate, without a CA.
    let mut required = profile(engine, "require", "127.0.0.1");
    required.ssl_ca = None;
    encrypted(&required, "rowboat-test").await?;
    encrypted(&profile(engine, "verify-ca", "localhost"), "rowboat-test").await?;
    let verified = profile(engine, "verify-full", "localhost");
    encrypted(&verified, "rowboat-test").await?;
    println!(
        "{engine:?}: require (IP), verify-ca and verify-full (localhost) encrypted queries passed"
    );

    rejected(
        &profile(engine, "verify-full", "127.0.0.1"),
        "wrong hostname",
        &[
            "notvalidforname",
            "not valid for name",
            "name mismatch",
            "hostname mismatch",
            "ip address mismatch",
        ],
    )
    .await?;
    for mode in ["verify-ca", "verify-full"] {
        let mut untrusted = profile(engine, mode, "localhost");
        untrusted.ssl_ca = Some(certificate("client/untrusted-ca.crt"));
        rejected(
            &untrusted,
            &format!("untrusted server CA ({mode})"),
            &[
                "unknownissuer",
                "unknown issuer",
                "certificate verify failed",
            ],
        )
        .await?;
    }
    rejected(
        &profile(engine, "disable", "127.0.0.1"),
        "plaintext connection",
        &[
            "pg_hba.conf rejects",
            "insecure transport",
            "require_secure_transport",
        ],
    )
    .await?;

    let mut client = verified.clone();
    client.username = "rowboat_cert".into();
    rejected(
        &client,
        "missing required client certificate",
        &["valid client certificate", "access denied"],
    )
    .await?;
    client.ssl_cert = Some(certificate("client/client.crt"));
    client.ssl_key = Some(certificate("client/client.key"));
    // PostgreSQL cert auth proves possession of the key with no password at all.
    let password = if engine == Engine::Postgres {
        ""
    } else {
        "rowboat-test"
    };
    encrypted(&client, password).await?;
    println!("{engine:?}: required client certificate authenticated");
    client.ssl_cert = Some(certificate("client/untrusted-client.crt"));
    client.ssl_key = Some(certificate("client/untrusted-client.key"));
    rejected(
        &client,
        "untrusted client certificate",
        &["unknownca", "unknown ca", "certificate", "access denied"],
    )
    .await?;
    // Same endpoint and credentials still work after every negative handshake.
    encrypted(&verified, "rowboat-test").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "First run bash scripts/test-tls-databases.sh up (rootless Podman, cached postgres:18/mysql:8.4, OpenSSL)"]
async fn postgres_tls_handshakes() -> Result<()> {
    exercise(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "First run bash scripts/test-tls-databases.sh up (rootless Podman, cached postgres:18/mysql:8.4, OpenSSL)"]
async fn mysql_tls_handshakes() -> Result<()> {
    exercise(Engine::MySql).await
}

// SQLx 0.8.6's NoHostnameTlsVerifier only suppresses NotValidForName, whereas
// current rustls can return NotValidForNameContext. Keep the intended CA-only
// semantics explicit. Rowboat uses SQLx's native TLS backend to avoid this mismatch.
#[tokio::test]
#[ignore = "First run bash scripts/test-tls-databases.sh up; regression: verify-ca must accept a trusted certificate with a different hostname"]
async fn postgres_verify_ca_ignores_hostname() -> Result<()> {
    encrypted(
        &profile(Engine::Postgres, "verify-ca", "127.0.0.1"),
        "rowboat-test",
    )
    .await
}

#[tokio::test]
#[ignore = "First run bash scripts/test-tls-databases.sh up; regression: verify-ca must accept a trusted certificate with a different hostname"]
async fn mysql_verify_ca_ignores_hostname() -> Result<()> {
    encrypted(
        &profile(Engine::MySql, "verify-ca", "127.0.0.1"),
        "rowboat-test",
    )
    .await
}
