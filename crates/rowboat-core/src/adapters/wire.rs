//! Wire protocol execution and decoding. Shared limits/delivery stay in EventSink.
use super::super::*;
use crate::lex::{dash_comment, line_comment_end};
fn command_words(engine: Engine, sql: &str) -> String {
    let mut s = sql.trim_start();
    loop {
        if (s.starts_with("--") && dash_comment(engine, s.as_bytes().get(2)))
            || (engine == Engine::MySql && s.starts_with('#'))
        {
            s = s
                .bytes()
                .position(|b| line_comment_end(engine, b))
                .map_or("", |end| &s[end..])
                .trim_start();
        } else if s.starts_with("/*") {
            s = s.split_once("*/").map_or("", |(_, s)| s).trim_start();
        } else {
            break;
        }
    }
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}
use futures_util::TryStreamExt;
use sqlx::{
    Column as _, Either, Executor, Row, TypeInfo, ValueRef, mysql::MySqlRow, postgres::PgRow,
};
fn columns<R: Row>(row: &R) -> Vec<Column> {
    row.columns()
        .iter()
        .map(|c| Column {
            name: c.name().into(),
            data_type: c.type_info().name().into(),
            nullable: true,
            primary_key: false,
            default_value: None,
        })
        .collect()
}

pub(crate) async fn execute(conn: &mut Conn, sql: &str) -> Result<()> {
    match conn {
        Conn::Pg(c) => {
            c.execute(sql).await?;
        }
        Conn::My(c) => {
            c.execute(sql).await?;
        }
        Conn::Sqlite(c) => c.execute(sql).await?,
        Conn::Closed => bail!("Connection closed; reconnect required"),
    };
    Ok(())
}
pub(crate) async fn metadata(conn: &mut Conn, sql: &str) -> Result<Vec<Column>> {
    macro_rules! prepare {
        ($c:expr) => {{
            let statement = $c
                .prepare(sql)
                .await
                .context("Read-only SQL must be one server-parsed statement")?;
            Ok(sqlx::Statement::columns(&statement)
                .iter()
                .map(|c| Column {
                    name: c.name().into(),
                    data_type: c.type_info().name().into(),
                    nullable: true,
                    primary_key: false,
                    default_value: None,
                })
                .collect())
        }};
    }
    match conn {
        Conn::Pg(c) => prepare!(c),
        Conn::My(c) => prepare!(c),
        Conn::Sqlite(_) => bail!("SQLite metadata is emitted by native prepare"),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
}
pub(crate) async fn stream(
    conn: &mut Conn,
    sql: &str,
    sink: &mut EventSink,
    metadata: Option<Vec<Column>>,
) -> Result<()> {
    if let Conn::Sqlite(c) = conn {
        return c.stream(sql, sink).await;
    }
    sink.dispatch();
    if let Some(columns) = metadata {
        sink.start(columns, true).await?;
    }
    macro_rules! pump {($connection:expr,$decode:expr,$wire:expr)=>{{
        let mut stream=sqlx::raw_sql(sql).fetch_many(&mut *$connection);
        loop {
            let deadline=sink.deadline();
            let next=tokio::select! {next=stream.try_next()=>next,_=tokio::time::sleep_until(deadline),if sink.pending()&&sink.control.reason().is_none()=>{if let Err(error)=sink.flush().await {if sink.control.reason().is_none(){sink.control.fail(&format!("{error:#}"));}}continue;},};
            let item=match next {Ok(Some(item))=>item,Ok(None)=>break,Err(error)=>{if sink.control.reason().is_none(){sink.flush().await?;}return Err(crate::execution::wire_error(error, sql));}};
            // Drain this very execution after cancellation; never replay SQL.
            if sink.control.reason().is_some(){continue;}
            let delivery:Result<()>=async {match item {
                Either::Left(done)=>sink.end(done.rows_affected()).await?,
                Either::Right(row)=>{
                    if !sink.active(){sink.start(columns(&row),true).await?;}
                    let mut wire_bytes=0usize;
                    for index in 0..row.len(){let size=$wire(&row,index)?;ensure!(size<=sink.options.max_cell_bytes,"Result cell {} exceeds max_cell_bytes {} (wire size {size})",index+1,sink.options.max_cell_bytes);wire_bytes=wire_bytes.saturating_add(size);}
                    ensure!(wire_bytes<=crate::stream::MAX_STREAM_BATCH_BYTES,"Incoming row exceeds absolute {} byte wire-row limit",crate::stream::MAX_STREAM_BATCH_BYTES);
                    sink.row((0..row.len()).map(|i|$decode(&row,i)).collect::<Result<Vec<_>>>()?).await?;
                }
            }Ok(())}.await;
            if let Err(error)=delivery {if sink.control.reason().is_none(){sink.control.fail(&format!("{error:#}"));}}
        }
    }};}
    match conn {
        Conn::Pg(c) => pump!(c, super::postgres::cell, |row: &PgRow,
                                                        i|
         -> Result<usize> {
            let raw = row.try_get_raw(i)?;
            Ok(if raw.is_null() {
                0
            } else {
                raw.as_bytes().map_err(|e| anyhow!(e.to_string()))?.len()
            })
        }),
        Conn::My(c) => pump!(
            c,
            |row, i| super::mysql::cell(row, i, true),
            |row: &MySqlRow, i| -> Result<usize> {
                Ok(if row.try_get_raw(i)?.is_null() {
                    0
                } else {
                    row.try_get_unchecked::<&[u8], _>(i)?.len()
                })
            }
        ),
        Conn::Sqlite(_) => unreachable!(),
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
    sink.control.check()
}
pub(crate) async fn raw(conn: &mut Conn, sql: &str, limit: usize) -> Result<Vec<QueryResult>> {
    let start = Instant::now();
    let mut results = Vec::new();
    let mut current = QueryResult::default();
    let mut bytes = 0;
    macro_rules! collect {
        ($c:expr,$decode:expr) => {{
            let mut stream = sqlx::raw_sql(sql).fetch_many(&mut *$c);
            while let Some(item) = stream.try_next().await? {
                match item {
                    Either::Left(done) => {
                        ensure!(
                            results.len() < MAX_RESULTS,
                            "More than {MAX_RESULTS} result sets; split the batch"
                        );
                        current.rows_affected = done.rows_affected();
                        current.elapsed_ms = start.elapsed().as_millis() as u64;
                        results.push(std::mem::take(&mut current));
                    }
                    Either::Right(row) => {
                        if current.columns.is_empty() {
                            current.columns = columns(&row);
                        }
                        let values = (0..row.len())
                            .map(|i| $decode(&row, i))
                            .collect::<Result<Vec<_>>>()?;
                        retain(&mut current, values, limit, &mut bytes);
                    }
                }
            }
        }};
    }
    match conn {
        Conn::Pg(c) => collect!(c, super::postgres::cell),
        Conn::My(c) => collect!(c, |r, i| super::mysql::cell(r, i, true)),
        Conn::Sqlite(c) => return c.query(sql, &[], limit).await,
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
    // Preserve headings for empty network results without executing twice.
    let engine = if matches!(conn, Conn::Pg(_)) {
        Engine::Postgres
    } else {
        Engine::MySql
    };
    let command = command_words(engine, sql);
    if results.len() == 1
        && results[0].columns.is_empty()
        && results[0].rows_affected == 0
        && (command.starts_with("SELECT ")
            || command.starts_with("WITH ")
            || command.starts_with("VALUES "))
    {
        macro_rules! describe {
            ($c:expr) => {
                if let Ok(d) = $c.describe(sql).await {
                    results[0].columns = d
                        .columns()
                        .iter()
                        .enumerate()
                        .map(|(i, c)| Column {
                            name: c.name().into(),
                            data_type: c.type_info().name().into(),
                            nullable: d.nullable(i).unwrap_or(true),
                            primary_key: false,
                            default_value: None,
                        })
                        .collect();
                }
            };
        }
        match conn {
            Conn::Pg(c) => describe!(c),
            Conn::My(c) => describe!(c),
            Conn::Sqlite(_) => unreachable!(),
            Conn::Closed => bail!("Connection closed; reconnect required"),
        }
    }
    Ok(results)
}
pub(crate) async fn bound(
    conn: &mut Conn,
    sql: &str,
    parameters: &[Cell],
    limit: usize,
) -> Result<QueryResult> {
    let start = Instant::now();
    let mut result = QueryResult::default();
    let mut bytes = 0;
    macro_rules! collect {
        ($c:expr,$decode:expr) => {{
            let mut query = sqlx::query(sql);
            for value in parameters {
                query = match value {
                    Cell::Null => query.bind(Option::<String>::None),
                    Cell::Text(s) => query.bind(s),
                    Cell::Binary(b) => query.bind(b),
                };
            }
            #[allow(deprecated)]
            let mut stream = query.fetch_many(&mut *$c);
            while let Some(item) = stream.try_next().await? {
                match item {
                    Either::Left(done) => result.rows_affected += done.rows_affected(),
                    Either::Right(row) => {
                        if result.columns.is_empty() {
                            result.columns = columns(&row);
                        }
                        let values = (0..row.len())
                            .map(|i| $decode(&row, i))
                            .collect::<Result<Vec<_>>>()?;
                        retain(&mut result, values, limit, &mut bytes);
                    }
                }
            }
        }};
    }
    match conn {
        Conn::Pg(c) => collect!(c, super::postgres::cell),
        Conn::My(c) => collect!(c, |r, i| super::mysql::cell(r, i, false)),
        Conn::Sqlite(c) => {
            let mut sets = c.query(sql, parameters, limit).await?;
            ensure!(sets.len() == 1, "Expected one bound SQLite result");
            return Ok(sets.remove(0));
        }
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
    result.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(result)
}
