//! Engine setup and native prepared execution for bounded read-only samples.
use super::super::*;
use futures_util::TryStreamExt;
use sqlx::{Connection, Executor, Row, ValueRef, mysql::MySqlRow, postgres::PgRow};
pub(crate) enum Settings {
    Pg,
    My {
        sql_mode: String,
        read_only: i64,
        max_execution_time: u64,
    },
}
pub(crate) async fn settings(conn: &mut Conn) -> Result<(Settings, LexMode)> {
    match conn {
        Conn::Pg(_) => Ok((Settings::Pg, LexMode::default())),
        Conn::My(c) => {
            let(sql_mode,read_only,max_execution_time):(String,i64,u64)=sqlx::query_as("SELECT @@session.sql_mode, @@session.transaction_read_only, @@session.max_execution_time").fetch_one(c).await?;
            let mode = LexMode {
                mysql_no_backslash_escapes: sql_mode
                    .split(',')
                    .any(|m| m.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")),
                mysql_ansi_quotes: sql_mode
                    .split(',')
                    .any(|m| m.eq_ignore_ascii_case("ANSI_QUOTES")),
            };
            Ok((
                Settings::My {
                    sql_mode,
                    read_only,
                    max_execution_time,
                },
                mode,
            ))
        }
        Conn::Sqlite(_) => bail!("SQLite sampling provider is unsupported"),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
}
pub(crate) async fn configure(conn: &mut Conn) -> Result<()> {
    match conn {
        Conn::Pg(c) => {
            c.execute("SET LOCAL standard_conforming_strings=on; SET LOCAL bytea_output='hex'; SET LOCAL statement_timeout='5s'").await?;
            c.clear_cached_statements().await?;
        }
        Conn::My(c) => {
            c.execute("SET SESSION max_execution_time=5000").await?;
            c.clear_cached_statements().await?;
        }
        Conn::Sqlite(_) => bail!("SQLite sampling provider is unsupported"),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    };
    Ok(())
}
pub(crate) async fn restore(conn: &mut Conn, settings: Settings) -> Result<()> {
    if let Settings::My {
        sql_mode,
        read_only,
        max_execution_time,
    } = settings
    {
        let Conn::My(c) = conn else {
            bail!("Connection closed")
        };
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT @@session.autocommit")
                .fetch_one(&mut *c)
                .await?
                == 1,
            "Sampling changed autocommit; session must be retired"
        );
        sqlx::query(
            "SET SESSION sql_mode=?, SESSION transaction_read_only=?, SESSION max_execution_time=?",
        )
        .bind(&sql_mode)
        .bind(read_only)
        .bind(max_execution_time)
        .execute(&mut *c)
        .await?;
        let actual:(String,i64,u64)=sqlx::query_as("SELECT @@session.sql_mode, @@session.transaction_read_only, @@session.max_execution_time").fetch_one(c).await?;
        ensure!(
            actual == (sql_mode, read_only, max_execution_time),
            "Sampling session-mode restoration could not be confirmed"
        );
    }
    super::state::clear_cache(conn).await
}
pub(crate) async fn timeouts(conn: &mut Conn) -> Result<Option<String>> {
    if let Conn::My(c) = conn {
        let row = sqlx::query("SHOW SESSION STATUS LIKE 'Max_execution_time_exceeded'")
            .fetch_one(c)
            .await?;
        Ok(Some(row.try_get(1)?))
    } else {
        Ok(None)
    }
}
pub(crate) async fn result(conn: &mut Conn, sql: &str) -> Result<Vec<QueryResult>> {
    const BYTES: usize = 64 * 1024;
    let start = Instant::now();
    let metadata = single_statement_metadata(conn, sql).await?;
    ensure!(metadata.len() == 1, "Sampling requires exactly one column");
    let mut bytes = metadata[0].name.len() + metadata[0].data_type.len();
    ensure!(bytes <= BYTES, "Sampling output exceeds 64 KiB");
    let mut result = QueryResult {
        columns: metadata,
        ..Default::default()
    };
    macro_rules! collect {
        ($connection:expr,$decode:expr,$wire:expr) => {{
            let mut rows = sqlx::query(sql).fetch(&mut *$connection);
            while let Some(row) = rows.try_next().await? {
                ensure!(row.len() == 1, "Sampling requires exactly one column");
                ensure!(
                    result.rows.len() < 2,
                    "Sampling exceeded two rows; partial sample discarded"
                );
                ensure!(
                    $wire(&row)? <= BYTES,
                    "Sampling cell exceeds 64 KiB; sample discarded"
                );
                let cell = $decode(&row, 0)?;
                let size = match &cell {
                    Cell::Null => 0,
                    Cell::Text(s) => s.len(),
                    Cell::Binary(b) => b.len(),
                };
                ensure!(
                    size <= BYTES,
                    "Sampling cell exceeds 64 KiB; sample discarded"
                );
                bytes = bytes.saturating_add(size);
                ensure!(
                    bytes <= BYTES,
                    "Sampling output exceeds 64 KiB; sample discarded"
                );
                result.rows.push(vec![cell]);
            }
        }};
    }
    match conn {
        Conn::Pg(c) => collect!(c, super::postgres::cell, |row: &PgRow| -> Result<usize> {
            let raw = row.try_get_raw(0)?;
            Ok(if raw.is_null() {
                0
            } else {
                raw.as_bytes().map_err(|e| anyhow!(e.to_string()))?.len()
            })
        }),
        Conn::My(c) => collect!(
            c,
            |row, i| super::mysql::cell(row, i, false),
            |row: &MySqlRow| -> Result<usize> {
                Ok(if row.try_get_raw(0)?.is_null() {
                    0
                } else {
                    row.try_get_unchecked::<&[u8], _>(0)?.len()
                })
            }
        ),
        Conn::Sqlite(_) => bail!("SQLite sampling provider is unsupported"),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
    result.elapsed_ms = start.elapsed().as_millis() as u64;
    result.rows_affected = result.rows.len() as u64;
    Ok(vec![result])
}
