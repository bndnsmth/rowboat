//! Administrative transaction policy and native identity hooks.
use super::{Conn, execute};
use crate::*;
use anyhow::Result;

pub(crate) fn atomic(engine: Engine) -> bool {
    matches!(engine, Engine::Postgres | Engine::Sqlite)
}

pub(crate) async fn begin(conn: &mut Conn) -> Result<()> {
    match conn {
        Conn::Sqlite(adapter) => adapter.execute("BEGIN IMMEDIATE").await,
        _ => execute(conn, "START TRANSACTION").await,
    }
}

pub(crate) async fn rollback(conn: &mut Conn) -> Result<()> {
    super::import::rollback(conn).await
}

pub(crate) async fn validate_commit(conn: &mut Conn) -> Result<()> {
    if let Conn::Sqlite(adapter) = conn {
        anyhow::ensure!(
            adapter.in_transaction().await?,
            "SQLite administrative transaction ended unexpectedly"
        );
        Ok(())
    } else {
        execute(conn, "SELECT 1").await
    }
}

pub(crate) async fn sqlite_identity(conn: &mut Conn, schema: &str) -> Result<Vec<Cell>> {
    match conn {
        Conn::Sqlite(adapter) => adapter.schema_identity(schema).await,
        _ => anyhow::bail!("SQLite schema identity requires a SQLite session"),
    }
}

pub(crate) async fn validate_statement(conn: &mut Conn, sql: &str) -> Result<()> {
    match conn {
        Conn::Pg(_) => {
            super::single_statement_metadata(conn, sql).await?;
            Ok(())
        }
        Conn::Sqlite(adapter) => adapter.validate_administrative_statement(sql).await,
        _ => Ok(()),
    }
}
