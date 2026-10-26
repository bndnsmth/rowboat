//! Internal semantic driver registration. Database owns scheduling and leases;
//! adapters own physical connections, native state, decoding and engine evidence.
pub(super) mod mysql;
pub(super) mod postgres;
pub(super) mod sampling;
pub(super) mod source;
pub(super) mod sqlite;
mod state;
pub(super) use state::{
    driver_error, manual_commit, require_transactional_table, restore_lexical_mode,
    select_database, transaction_end, unknown_outcome,
};
pub(super) mod administrative;
pub(super) mod catalog;
pub(super) mod completion;
pub(super) mod dialect;
pub(super) mod import;
mod structure;
mod wire;
use crate::{ConnectionProfile, Engine, readonly::LexMode};
use anyhow::{Result, bail};
pub(super) use state::{
    cancel, cancel_query_session, close_broken_connection, drain_connection, probe_transaction,
    restore_readonly_default, server_in_transaction,
};
use std::sync::Arc;
pub(super) use structure::structure;
/// Shared cancellation policy supplied by the facade to a native edit batch.
pub(super) struct EditCancellation<'a> {
    pub requested: &'a std::sync::atomic::AtomicBool,
    pub gate: &'a tokio::sync::Mutex<()>,
}
impl EditCancellation<'_> {
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !self.requested.load(std::sync::atomic::Ordering::Acquire),
            "Edit batch cancelled"
        );
        Ok(())
    }
}
pub(super) use wire::{
    bound as bound_results, execute, metadata as single_statement_metadata, raw as raw_results,
    stream as raw_stream,
};

pub(super) enum Options {
    Pg(sqlx::postgres::PgConnectOptions),
    My(sqlx::mysql::MySqlConnectOptions),
    Sqlite(Arc<sqlite::Control>),
}
pub(super) enum Conn {
    Pg(sqlx::PgConnection),
    My(sqlx::MySqlConnection),
    Sqlite(Box<sqlite::SqliteAdapter>),
    Closed,
}
pub(super) fn native_readonly_authorization(conn: &Conn) -> bool {
    matches!(conn, Conn::Sqlite(_))
}
pub(super) async fn connect(
    profile: &ConnectionProfile,
    password: &str,
) -> Result<(Options, Conn)> {
    match profile.engine {
        Engine::Postgres => postgres::connect(profile, password).await,
        Engine::MySql => mysql::connect(profile, password).await,
        Engine::Sqlite => {
            let adapter = sqlite::SqliteAdapter::connect(profile).await?;
            Ok((
                Options::Sqlite(adapter.control.clone()),
                Conn::Sqlite(Box::new(adapter)),
            ))
        }
    }
}
pub(super) async fn initialize(
    conn: &mut Conn,
    profile: &ConnectionProfile,
) -> Result<(u64, LexMode, String)> {
    match conn {
        Conn::Pg(c) => postgres::initialize(c, profile).await,
        Conn::My(c) => mysql::initialize(c, profile).await,
        Conn::Sqlite(_) => Ok((0, LexMode::default(), String::new())),
        Conn::Closed => bail!("Connection closed during initialization"),
    }
}
