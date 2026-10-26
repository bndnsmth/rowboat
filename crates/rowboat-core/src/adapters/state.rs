//! Native connection state, cancellation transport and recovery operations.
use super::super::*;
use sqlx::{Connection, Executor, mysql::MySqlConnection, postgres::PgConnection};
pub(crate) fn transaction_end(conn: &Conn, commit: bool) -> &'static str {
    match (conn, commit) {
        (Conn::My(_), true) => "COMMIT AND NO CHAIN NO RELEASE",
        (Conn::My(_), false) => "ROLLBACK AND NO CHAIN NO RELEASE",
        (_, true) => "COMMIT",
        (_, false) => "ROLLBACK",
    }
}
pub(crate) async fn select_database(conn: &mut Conn, name: &str) -> Result<()> {
    execute(conn, &format!("USE {}", quote_ident(Engine::MySql, name))).await?;
    clear_cache(conn).await
}
pub(crate) fn driver_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<sqlx::Error>().is_some()
}
pub(crate) fn unknown_outcome(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<sqlx::Error>(),Some(error)if !matches!(error,sqlx::Error::Database(_)))
}
pub(crate) async fn require_transactional_table(conn: &mut Conn, table: &TableRef) -> Result<()> {
    if matches!(conn, Conn::My(_)) {
        let rows=bound_results(conn,"SELECT ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND TABLE_NAME=?",&[Cell::Text(table.schema.clone()),Cell::Text(table.name.clone())],1).await?;
        ensure!(
            rows.rows
                .first()
                .is_some_and(|r| text(&r[0]).eq_ignore_ascii_case("InnoDB")),
            "Atomic editing requires a MySQL InnoDB base table"
        );
    }
    Ok(())
}
pub(crate) async fn manual_commit(conn: &mut Conn) -> Result<bool> {
    match conn {
        Conn::My(c) => Ok(sqlx::query_scalar::<_, i64>("SELECT @@session.autocommit")
            .fetch_one(c)
            .await?
            == 0),
        Conn::Pg(_) | Conn::Sqlite(_) => Ok(false),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
}
pub(crate) async fn clear_cache(conn: &mut Conn) -> Result<()> {
    match conn {
        Conn::Pg(c) => c.clear_cached_statements().await?,
        Conn::My(c) => c.clear_cached_statements().await?,
        Conn::Sqlite(_) => {}
        Conn::Closed => bail!("Connection closed; reconnect required"),
    };
    Ok(())
}
pub(crate) async fn restore_lexical_mode(conn: &mut Conn, mysql_mode: &str) -> Result<()> {
    match conn {
        Conn::Pg(c) => {
            c.execute("SET standard_conforming_strings=on; SET bytea_output='hex'")
                .await?;
        }
        Conn::My(c) => {
            sqlx::query("SET SESSION sql_mode=?")
                .bind(mysql_mode)
                .execute(c)
                .await?;
        }
        Conn::Sqlite(_) => {}
        Conn::Closed => bail!("Connection closed; reconnect required"),
    };
    Ok(())
}
pub(crate) async fn cancel(options: &Options, id: u64, running: &AtomicBool) -> Result<()> {
    cancel_query_session(options, id, running, false).await
}
/// Only disposable query workers may terminate their physical MySQL session.
/// The caller holds both its session lease and the cancellation gate.
pub(crate) async fn cancel_query_session(
    options: &Options,
    id: u64,
    running: &AtomicBool,
    retire: bool,
) -> Result<()> {
    match options {
        Options::Sqlite(control) => control.interrupt(),
        Options::Pg(o) => {
            let mut c =
                tokio::time::timeout(Duration::from_secs(15), PgConnection::connect_with(o))
                    .await
                    .context("Cancellation connection timed out")?
                    .context("Opening cancellation connection")?;
            if !running.load(Ordering::Acquire) {
                return Ok(());
            }
            let cancelled: bool = tokio::time::timeout(
                Duration::from_secs(15),
                sqlx::query_scalar("SELECT pg_cancel_backend($1)")
                    .bind(id as i32)
                    .fetch_one(&mut c),
            )
            .await
            .context("Cancellation timed out")?
            .context("Cancelling PostgreSQL query")?;
            ensure!(cancelled, "PostgreSQL backend did not accept cancellation");
        }
        Options::My(o) => {
            let mut c =
                tokio::time::timeout(Duration::from_secs(15), MySqlConnection::connect_with(o))
                    .await
                    .context("Cancellation connection timed out")?
                    .context("Opening cancellation connection")?;
            if !running.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::time::timeout(
                Duration::from_secs(15),
                sqlx::query(&format!(
                    "KILL {} {id}",
                    if retire { "CONNECTION" } else { "QUERY" }
                ))
                .execute(&mut c),
            )
            .await
            .context("Cancellation timed out")?
            .context("Cancelling MySQL query")?;
        }
    }
    Ok(())
}
pub(crate) async fn drain_connection(conn: &mut Conn) -> Result<()> {
    if let Conn::Sqlite(c) = conn {
        c.in_transaction().await?;
        return Ok(());
    }
    for _ in 0..4 {
        let result = match conn {
            Conn::Pg(c) => c.ping().await,
            Conn::My(c) => c.ping().await,
            Conn::Sqlite(_) => unreachable!(),
            Conn::Closed => bail!("Connection closed; reconnect required"),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(sqlx::Error::Database(_)) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("Could not drain database protocol after query interruption")
}
pub(crate) async fn close_broken_connection(conn: &mut Conn) {
    let connection = std::mem::replace(conn, Conn::Closed);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        match connection {
            Conn::Pg(c) => {
                let _ = c.close_hard().await;
            }
            Conn::My(c) => {
                let _ = c.close_hard().await;
            }
            Conn::Sqlite(_) | Conn::Closed => {}
        }
    })
    .await;
}
pub(crate) async fn restore_readonly_default(conn: &mut Conn) -> Result<()> {
    let setting = match conn {
        Conn::Pg(_) => {
            "SET default_transaction_read_only = on; SET standard_conforming_strings = on"
        }
        Conn::My(_) => "SET SESSION TRANSACTION READ ONLY",
        Conn::Sqlite(_) => return Ok(()),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    };
    execute(conn, setting)
        .await
        .context("Restoring server read-only session default")
}
pub(crate) async fn server_in_transaction(conn: &mut Conn) -> Result<bool> {
    if let Conn::Sqlite(c) = conn {
        return c.in_transaction().await;
    }
    let name = format!("rowboat_probe_{}", uuid::Uuid::new_v4().simple());
    match probe_transaction(conn, &name).await {
        Ok(true) => {
            execute(conn, &format!("RELEASE SAVEPOINT {name}")).await?;
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(error) if matches!(conn, Conn::Pg(_)) && database_error_code(&error, "25P02") => {
            Ok(true)
        }
        Err(error) => Err(error),
    }
}
pub(crate) fn database_error_code(error: &anyhow::Error, expected: &str) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .and_then(sqlx::Error::as_database_error)
        .is_some_and(|e| {
            e.code().is_some_and(|code| code == expected)
                || e.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                    .is_some_and(|e| e.number().to_string() == expected)
        })
}
pub(crate) async fn probe_transaction(conn: &mut Conn, name: &str) -> Result<bool> {
    if let Conn::Sqlite(c) = conn {
        let active = c.in_transaction().await?;
        if active {
            c.execute(&format!("SAVEPOINT {name}")).await?;
        }
        return Ok(active);
    }
    let savepoint = format!("SAVEPOINT {name}");
    match execute(conn, &savepoint).await {
        Ok(()) => {}
        Err(error) if matches!(conn, Conn::Pg(_)) && database_error_code(&error, "25P01") => {
            return Ok(false);
        }
        Err(error) => return Err(error.context("Checking active transaction")),
    }
    if matches!(conn, Conn::My(_)) {
        match execute(conn, &format!("RELEASE SAVEPOINT {name}")).await {
            Ok(()) => execute(conn, &savepoint).await?,
            Err(error) if database_error_code(&error, "1305") => return Ok(false),
            Err(error) => return Err(error.context("Checking active MySQL transaction")),
        }
    }
    Ok(true)
}
