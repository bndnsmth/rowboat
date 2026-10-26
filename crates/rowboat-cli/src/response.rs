//! Versioned agent response assembled from observed stream events, never SQL replay.
use rowboat_core::{Cell, QueryEvent, QueryResult};
use serde_json::{Value, json};

#[derive(Default)]
pub struct ResultSet {
    pub data: QueryResult,
    pub rows_seen: u64,
    pub complete: bool,
    dropped_rows: bool,
}

#[derive(Default)]
pub struct Response {
    pub results: Vec<ResultSet>,
    retained_bytes: usize,
    exhausted: bool,
}

const RESPONSE_BUDGET: usize = 64 * 1024 * 1024;

impl Response {
    /// Bound the whole batch, including cell/vector and column storage. Core
    /// already has a global row-byte budget; this also accounts for schemas.
    pub fn observe(&mut self, event: &QueryEvent) -> bool {
        let index = match event {
            QueryEvent::Schema { result_index, .. }
            | QueryEvent::Rows { result_index, .. }
            | QueryEvent::ResultEnd { result_index, .. } => *result_index,
            _ => return true,
        };
        let bytes = match event {
            QueryEvent::Schema { columns, .. } => columns
                .iter()
                .map(|column| {
                    std::mem::size_of_val(column)
                        + column.name.len()
                        + column.data_type.len()
                        + column.default_value.as_ref().map_or(0, String::len)
                })
                .sum(),
            QueryEvent::Rows { rows, .. } => rows
                .iter()
                .map(|row| {
                    std::mem::size_of_val(row)
                        + row
                            .iter()
                            .map(|cell| {
                                std::mem::size_of_val(cell)
                                    + match cell {
                                        Cell::Null => 0,
                                        Cell::Text(text) => text.len(),
                                        Cell::Binary(bytes) => bytes.len(),
                                    }
                            })
                            .sum::<usize>()
                })
                .sum(),
            _ => 0,
        };
        if bytes > 0 && (self.exhausted || bytes > RESPONSE_BUDGET - self.retained_bytes) {
            self.exhausted = true;
            if matches!(event, QueryEvent::Rows { .. })
                && let Some(result) = self.results.get_mut(index)
            {
                result.dropped_rows = true;
            }
            return false;
        }
        // If the schema itself did not fit, do not manufacture a column-less
        // result from its later completion during cancellation cleanup.
        if self.exhausted && index >= self.results.len() {
            return false;
        }
        self.retained_bytes += bytes;
        self.results.resize_with(index + 1, ResultSet::default);
        let result = &mut self.results[index];
        match event {
            QueryEvent::Schema { columns, .. } => result.data.columns = columns.clone(),
            QueryEvent::Rows { rows, .. } => {
                result.rows_seen += rows.len() as u64;
                result.data.rows.extend(rows.iter().cloned());
            }
            QueryEvent::ResultEnd {
                rows_seen,
                rows_affected,
                elapsed_ms,
                truncated,
                ..
            } => {
                result.rows_seen = *rows_seen;
                result.data.rows_affected = *rows_affected;
                result.data.elapsed_ms = *elapsed_ms;
                result.data.truncated = *truncated || result.dropped_rows;
                result.complete = true;
            }
            _ => {}
        }
        true
    }

    pub fn envelope(&self, elapsed_ms: u64, failure: Option<&super::execution::Failure>) -> Value {
        json!({
            "version": 1,
            "ok": failure.is_none(),
            "status": failure.map_or("complete", |f| f.status),
            "results": self.results.iter().enumerate().map(|(index, result)| {
                json!({
                    "index": index,
                    "columns": result.data.columns,
                    "rows": result.data.rows.iter().map(|row| row.iter().map(cell).collect::<Vec<_>>()).collect::<Vec<_>>(),
                    // Until ResultEnd, these values are unknown. In particular,
                    // cancellation is not evidence of zero writes or no truncation.
                    "rows_seen": result.complete.then_some(result.rows_seen),
                    "rows_returned": result.data.rows.len(),
                    "rows_affected": result.complete.then_some(result.data.rows_affected),
                    "truncated": result.complete.then_some(result.data.truncated),
                    "complete": result.complete,
                    "elapsed_ms": result.complete.then_some(result.data.elapsed_ms),
                })
            }).collect::<Vec<_>>(),
            "elapsed_ms": elapsed_ms,
            "error": failure.map(|f| json!({"code": f.code, "message": f.message})),
        })
    }
}

fn cell(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Text(value) => Value::String(value.clone()),
        Cell::Binary(bytes) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            use std::fmt::Write;
            for byte in bytes {
                write!(hex, "{byte:02x}").expect("write to string");
            }
            json!({"$binary": hex})
        }
    }
}
