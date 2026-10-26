use super::{Conn, Options};
use crate::Cell;
use crate::{ConnectionProfile, ConnectionTransport, readonly::LexMode};
use anyhow::{Context, Result, bail, ensure};
use sqlx::{
    ConnectOptions, Connection, Executor,
    postgres::{PgConnectOptions, PgConnection, PgSslMode},
};
use sqlx::{
    Row, TypeInfo, ValueRef,
    postgres::{PgRow, PgValueFormat},
};
use std::time::Duration;

pub(crate) fn cell(row: &PgRow, i: usize) -> Result<Cell> {
    let raw = row.try_get_raw(i)?;
    if raw.is_null() {
        return Ok(Cell::Null);
    }
    if raw.type_info().name() == "BYTEA" {
        return Ok(Cell::Binary(row.try_get_unchecked::<Vec<u8>, _>(i)?));
    }
    if raw.format() == PgValueFormat::Text {
        return Ok(match raw.as_str() {
            Ok(s) => Cell::Text(s.into()),
            Err(_) => Cell::Binary(
                raw.as_bytes()
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?
                    .to_vec(),
            ),
        });
    }
    macro_rules! value {
        ($t:ty) => {
            if let Ok(v) = row.try_get::<$t, _>(i) {
                return Ok(Cell::Text(v.to_string()));
            }
        };
    }
    value!(String);
    value!(bool);
    value!(i16);
    value!(i32);
    value!(i64);
    value!(f32);
    value!(f64);
    value!(sqlx::types::BigDecimal);
    value!(uuid::Uuid);
    value!(serde_json::Value);
    value!(chrono::NaiveDate);
    value!(chrono::NaiveTime);
    value!(chrono::NaiveDateTime);
    value!(chrono::DateTime<chrono::Utc>);
    Ok(Cell::Binary(
        raw.as_bytes()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .to_vec(),
    ))
}

pub(super) async fn connect(
    profile: &ConnectionProfile,
    password: &str,
) -> Result<(Options, Conn)> {
    let ssl = match profile.ssl_mode.as_str() {
        "disable" => PgSslMode::Disable,
        "prefer" | "" => PgSslMode::Prefer,
        "require" => PgSslMode::Require,
        "verify-ca" => PgSslMode::VerifyCa,
        "verify-full" => PgSslMode::VerifyFull,
        other => bail!("Unsupported PostgreSQL TLS mode: {other}"),
    };
    let mut options = PgConnectOptions::new()
        .host(&profile.host)
        .port(profile.port)
        .database(&profile.database)
        .username(&profile.username)
        .password(password)
        .ssl_mode(ssl)
        .application_name("Rowboat")
        .disable_statement_logging();
    if let Some(p) = &profile.ssl_ca {
        options = options.ssl_root_cert(p);
    }
    if let Some(p) = &profile.ssl_cert {
        options = options.ssl_client_cert(p);
    }
    if let Some(p) = &profile.ssl_key {
        options = options.ssl_client_key(p);
    }
    if let Some(ConnectionTransport::UnixSocket { path }) = &profile.transport {
        ensure!(path.is_absolute(), "Tunnel socket path must be absolute");
        ensure!(
            path.file_name().and_then(|s| s.to_str())
                == Some(&format!(".s.PGSQL.{}", profile.port)),
            "PostgreSQL tunnel socket name must match the configured port"
        );
        options = options.socket(
            path.parent()
                .context("Tunnel socket has no parent directory")?,
        );
    }
    let conn = tokio::time::timeout(
        Duration::from_secs(15),
        PgConnection::connect_with(&options),
    )
    .await
    .context("PostgreSQL connection timed out")?
    .context("Connecting to PostgreSQL")?;
    Ok((Options::Pg(options), Conn::Pg(conn)))
}
pub(super) async fn initialize(
    c: &mut PgConnection,
    profile: &ConnectionProfile,
) -> Result<(u64, LexMode, String)> {
    c.execute("SET bytea_output = 'hex'; SET standard_conforming_strings = on")
        .await?;
    if profile.read_only {
        c.execute("SET default_transaction_read_only = on")
            .await
            .context("Enabling PostgreSQL server read-only session")?;
    }
    let id = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
        .fetch_one(c)
        .await? as u64;
    Ok((id, LexMode::default(), String::new()))
}
