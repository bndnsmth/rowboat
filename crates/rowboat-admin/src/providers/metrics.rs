//! Native metric transport/shape; rate/reset arithmetic stays shared.
use super::{Operation, ServerKind, server};
use crate::objects::cell_text;
use anyhow::{Result, ensure};
use rowboat_core::{Engine, QueryResult};
use std::collections::BTreeMap;

pub(crate) fn sql(engine: Engine) -> Result<&'static str> {
    Ok(match server(engine, Operation::ServerMetrics)? {
        ServerKind::Postgres => {
            "SELECT numbackends, xact_commit, xact_rollback, blks_read, blks_hit, tup_returned, tup_fetched, tup_inserted, tup_updated, tup_deleted, conflicts, temp_files, temp_bytes, deadlocks, pg_database_size(datid) AS database_bytes, stats_reset::text FROM pg_stat_database WHERE datname=current_database()"
        }
        ServerKind::MySql => {
            "SHOW GLOBAL STATUS WHERE Variable_name IN ('Uptime','Uptime_since_flush_status','Questions','Queries','Connections','Threads_connected','Threads_running','Bytes_received','Bytes_sent','Slow_queries','Aborted_connects','Innodb_buffer_pool_read_requests','Innodb_buffer_pool_reads','Innodb_rows_read','Innodb_rows_inserted','Innodb_rows_updated','Innodb_rows_deleted')"
        }
    })
}

pub(crate) struct Values {
    pub counters: BTreeMap<String, u64>,
    pub reset: Option<String>,
    pub scope: &'static str,
}

pub(crate) fn decode(engine: Engine, raw: &[QueryResult]) -> Result<Values> {
    let provider = server(engine, Operation::ServerMetrics)?;
    let mut values = Values {
        counters: BTreeMap::new(),
        reset: None,
        scope: match provider {
            ServerKind::Postgres => "current database",
            ServerKind::MySql => "server global",
        },
    };
    for result in raw {
        ensure!(!result.truncated, "Metrics query was truncated");
        match provider {
            ServerKind::MySql => {
                for row in &result.rows {
                    let name =
                        cell_text(row, 0)?.ok_or_else(|| anyhow::anyhow!("Missing metric name"))?;
                    if let Some(value) = cell_text(row, 1)? {
                        values
                            .counters
                            .insert(name.to_ascii_lowercase(), value.parse::<u64>()?);
                    }
                }
            }
            ServerKind::Postgres => {
                for row in &result.rows {
                    for (i, column) in result.columns.iter().enumerate() {
                        let value = cell_text(row, i)?;
                        if column.name == "stats_reset" {
                            values.reset = value;
                        } else if let Some(value) = value {
                            values
                                .counters
                                .insert(column.name.clone(), value.parse::<u64>()?);
                        }
                    }
                }
            }
        }
    }
    Ok(values)
}
