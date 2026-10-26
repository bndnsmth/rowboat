use super::{Conn, Options};
use crate::Cell;
use crate::{ConnectionProfile, ConnectionTransport, readonly::LexMode};
use anyhow::{Context, Result, bail, ensure};
use sqlx::{
    ConnectOptions, Connection, Executor,
    mysql::{MySqlConnectOptions, MySqlConnection, MySqlSslMode},
};
use sqlx::{Row, TypeInfo, ValueRef, mysql::MySqlRow};
use std::time::Duration;

pub(crate) fn cell(row: &MySqlRow, i: usize, raw_text: bool) -> Result<Cell> {
    let raw = row.try_get_raw(i)?;
    if raw.is_null() {
        return Ok(Cell::Null);
    }
    let ty = raw.type_info().name().to_owned();
    if matches!(
        ty.as_str(),
        "BINARY"
            | "VARBINARY"
            | "TINYBLOB"
            | "BLOB"
            | "MEDIUMBLOB"
            | "LONGBLOB"
            | "BIT"
            | "GEOMETRY"
    ) {
        return Ok(Cell::Binary(row.try_get_unchecked::<Vec<u8>, _>(i)?));
    }
    if raw_text {
        let bytes = row.try_get_unchecked::<Vec<u8>, _>(i)?;
        return Ok(match String::from_utf8(bytes) {
            Ok(s) => Cell::Text(s),
            Err(e) => Cell::Binary(e.into_bytes()),
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
    value!(i64);
    value!(u64);
    value!(f64);
    value!(sqlx::types::BigDecimal);
    value!(chrono::NaiveDate);
    value!(chrono::NaiveDateTime);
    value!(sqlx::mysql::types::MySqlTime);
    value!(serde_json::Value);
    Ok(Cell::Binary(row.try_get_unchecked::<Vec<u8>, _>(i)?))
}

pub(super) async fn connect(
    profile: &ConnectionProfile,
    password: &str,
) -> Result<(Options, Conn)> {
    let ssl = match profile.ssl_mode.as_str() {
        "disable" => MySqlSslMode::Disabled,
        "prefer" | "" => MySqlSslMode::Preferred,
        "require" => MySqlSslMode::Required,
        "verify-ca" => MySqlSslMode::VerifyCa,
        "verify-full" => MySqlSslMode::VerifyIdentity,
        other => bail!("Unsupported MySQL TLS mode: {other}"),
    };
    let mut options = MySqlConnectOptions::new()
        .host(&profile.host)
        .port(profile.port)
        .database(&profile.database)
        .username(&profile.username)
        .password(password)
        .charset("utf8mb4")
        .ssl_mode(ssl)
        .disable_statement_logging();
    if let Some(p) = &profile.ssl_ca {
        options = options.ssl_ca(p);
    }
    if let Some(p) = &profile.ssl_cert {
        options = options.ssl_client_cert(p);
    }
    if let Some(p) = &profile.ssl_key {
        options = options.ssl_client_key(p);
    }
    if let Some(ConnectionTransport::UnixSocket { path }) = &profile.transport {
        ensure!(path.is_absolute(), "Tunnel socket path must be absolute");
        options = options.socket(path);
    }
    let conn = tokio::time::timeout(
        Duration::from_secs(15),
        MySqlConnection::connect_with(&options),
    )
    .await
    .context("MySQL connection timed out")?
    .context("Connecting to MySQL")?;
    Ok((Options::My(options), Conn::My(conn)))
}
pub(super) async fn initialize(
    c: &mut MySqlConnection,
    profile: &ConnectionProfile,
) -> Result<(u64, LexMode, String)> {
    let mode: String = sqlx::query_scalar("SELECT @@session.sql_mode")
        .fetch_one(&mut *c)
        .await?;
    let lex = LexMode {
        mysql_no_backslash_escapes: mode
            .split(',')
            .any(|m| m.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")),
        mysql_ansi_quotes: mode
            .split(',')
            .any(|m| m.eq_ignore_ascii_case("ANSI_QUOTES")),
    };
    if profile.read_only {
        c.execute("SET SESSION TRANSACTION READ ONLY")
            .await
            .context("Enabling MySQL server read-only session")?;
    }
    let id = sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()")
        .fetch_one(c)
        .await?;
    Ok((id, lex, mode))
}
pub(crate) async fn column_names(
    c: &mut MySqlConnection,
    table: &crate::TableRef,
) -> Result<Vec<String>> {
    let names:Vec<String>=sqlx::query_scalar("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND TABLE_NAME=? ORDER BY ORDINAL_POSITION LIMIT 2001").bind(&table.schema).bind(&table.name).persistent(false).fetch_all(c).await?;
    ensure!(names.len() <= 2000, "Completion columns exceed 2000");
    Ok(names)
}
