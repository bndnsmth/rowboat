//! Exact execution binding recorded at the stream emission boundary. No hashing
//! or caller-supplied originals establish provenance. Overflow disables editing,
//! never the query/stream, and immediately releases the retained snapshot.
use crate::{Column, QueryEvent, QueryResult, QueryStreamOptions, stream::row_bytes};
use std::sync::Arc;

const MAX_ROWS: usize = 100_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;

pub(crate) struct ExecutionSnapshot {
    result: Option<QueryResult>,
    error: Option<&'static str>,
    ended: bool,
    bytes: usize,
    max_bytes: usize,
    max_rows: usize,
}
impl ExecutionSnapshot {
    pub(crate) fn new(options: &QueryStreamOptions) -> Self {
        Self {
            result: None,
            error: None,
            ended: false,
            bytes: 0,
            max_bytes: options.byte_limit.unwrap_or(MAX_BYTES).min(MAX_BYTES),
            max_rows: options.row_limit.unwrap_or(MAX_ROWS).min(MAX_ROWS),
        }
    }
    fn reject(&mut self, reason: &'static str) {
        self.error = Some(reason);
        self.result = None;
    }
    pub(crate) fn observe(&mut self, event: &QueryEvent) {
        if self.error.is_some() {
            return;
        }
        match event {
            QueryEvent::Schema {
                result_index: 0,
                columns,
                metadata_complete: true,
            } if self.result.is_none() && !self.ended => {
                let bytes = columns.iter().fold(0usize, |total, c| {
                    total
                        .saturating_add(std::mem::size_of::<Column>())
                        .saturating_add(c.name.len())
                        .saturating_add(c.data_type.len())
                        .saturating_add(c.default_value.as_ref().map_or(0, String::len))
                });
                if columns.len() > 4096 || bytes > MAX_SCHEMA_BYTES {
                    self.reject("Query edit execution schema exceeds its retention limit");
                    return;
                }
                self.result = Some(QueryResult {
                    columns: columns.clone(),
                    ..Default::default()
                });
            }
            QueryEvent::Rows {
                result_index: 0,
                rows,
            } if self.result.is_some() && !self.ended => {
                let result = self.result.as_mut().expect("checked snapshot");
                let bytes = rows.iter().fold(self.bytes, |total, row| {
                    total.saturating_add(row_bytes(row))
                });
                if result.rows.len().saturating_add(rows.len()) > self.max_rows
                    || bytes > self.max_bytes
                {
                    self.reject("Query result exceeds the bounded edit snapshot (at most 100000 rows / 64 MiB); rerun with a smaller LIMIT");
                    return;
                }
                if rows.iter().any(|row| row.len() != result.columns.len()) {
                    self.reject("Captured result has inconsistent row widths");
                    return;
                }
                self.bytes = bytes;
                result.rows.extend_from_slice(rows);
            }
            QueryEvent::ResultEnd {
                result_index: 0,
                rows_affected,
                rows_seen,
                rows_emitted,
                elapsed_ms,
                truncated,
            } if self.result.is_some() && !self.ended => {
                let result = self.result.as_mut().expect("checked snapshot");
                if *rows_emitted != result.rows.len() as u64 || rows_seen < rows_emitted {
                    self.reject("Captured result completion does not match emitted rows");
                    return;
                }
                result.rows_affected = *rows_affected;
                result.elapsed_ms = *elapsed_ms;
                result.truncated = *truncated;
                self.ended = true;
            }
            _ => self.reject("Editing requires one complete, unambiguous emitted result"),
        }
    }
    pub(crate) fn finish(self, terminal: &QueryEvent) -> Result<Arc<QueryResult>, String> {
        if let Some(error) = self.error {
            return Err(error.into());
        }
        if !self.ended
            || !matches!(
                terminal,
                QueryEvent::Complete {
                    result_count: 1,
                    ..
                }
            )
        {
            return Err("Query execution did not successfully complete one captured result".into());
        }
        self.result
            .map(Arc::new)
            .ok_or_else(|| "Missing execution snapshot".into())
    }
}

pub(crate) fn exact_result(a: &QueryResult, b: &QueryResult) -> bool {
    a.rows_affected == b.rows_affected && a.elapsed_ms == b.elapsed_ms && a.truncated == b.truncated
        && a.rows == b.rows // Cell equality includes variant, lengths and exact bytes.
        && a.columns.len() == b.columns.len()
        && a.columns.iter().zip(&b.columns).all(|(a,b)| {
            a.name == b.name && a.data_type == b.data_type && a.nullable == b.nullable
                && a.primary_key == b.primary_key && a.default_value == b.default_value
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cell;
    fn schema() -> QueryEvent {
        QueryEvent::Schema {
            result_index: 0,
            columns: vec![Column {
                name: "id".into(),
                data_type: "TEXT".into(),
                nullable: false,
                primary_key: false,
                default_value: None,
            }],
            metadata_complete: true,
        }
    }
    fn end(rows: u64) -> QueryEvent {
        QueryEvent::ResultEnd {
            result_index: 0,
            rows_affected: rows,
            rows_seen: rows,
            rows_emitted: rows,
            elapsed_ms: 7,
            truncated: false,
        }
    }
    fn complete() -> QueryEvent {
        QueryEvent::Complete {
            result_count: 1,
            elapsed_ms: 10,
        }
    }
    #[test]
    fn exact_cells_columns_order_and_completion_are_bound() {
        let mut snapshot = ExecutionSnapshot::new(&Default::default());
        snapshot.observe(&schema());
        snapshot.observe(&QueryEvent::Rows {
            result_index: 0,
            rows: vec![
                vec![Cell::Null],
                vec![Cell::Text(String::new())],
                vec![Cell::Text("a\0b".into())],
                vec![Cell::Binary(b"a\0b".to_vec())],
            ],
        });
        snapshot.observe(&end(4));
        let actual = snapshot.finish(&complete()).unwrap();
        assert!(exact_result(&actual, &actual.clone()));
        let mut changed = actual.as_ref().clone();
        changed.rows.swap(0, 1);
        assert!(!exact_result(&actual, &changed));
        changed = actual.as_ref().clone();
        changed.rows[2][0] = Cell::Binary(b"a\0b".to_vec());
        assert!(!exact_result(&actual, &changed));
        changed = actual.as_ref().clone();
        changed.rows[2][0] = Cell::Text("a\0b\0".into());
        assert!(!exact_result(&actual, &changed));
        changed = actual.as_ref().clone();
        changed.columns[0].default_value = Some(String::new());
        assert!(!exact_result(&actual, &changed));
        changed = actual.as_ref().clone();
        changed.truncated = true;
        assert!(!exact_result(&actual, &changed));
        changed = actual.as_ref().clone();
        changed.rows_affected = 0;
        assert!(!exact_result(&actual, &changed));
        changed = actual.as_ref().clone();
        changed.elapsed_ms = 0;
        assert!(!exact_result(&actual, &changed));
    }
    #[test]
    fn overflow_releases_snapshot_and_incomplete_or_failed_execution_is_rejected() {
        let options = QueryStreamOptions {
            row_limit: Some(1),
            ..Default::default()
        };
        let mut snapshot = ExecutionSnapshot::new(&options);
        snapshot.observe(&schema());
        snapshot.observe(&QueryEvent::Rows {
            result_index: 0,
            rows: vec![vec![Cell::Null], vec![Cell::Null]],
        });
        assert!(snapshot.result.is_none());
        assert!(snapshot.finish(&complete()).is_err());
        let options = QueryStreamOptions {
            byte_limit: Some(1),
            ..Default::default()
        };
        let mut snapshot = ExecutionSnapshot::new(&options);
        snapshot.observe(&schema());
        snapshot.observe(&QueryEvent::Rows {
            result_index: 0,
            rows: vec![vec![Cell::Null]],
        });
        assert!(snapshot.result.is_none());
        assert!(snapshot.finish(&complete()).is_err());
        let mut snapshot = ExecutionSnapshot::new(&Default::default());
        snapshot.observe(&schema());
        assert!(snapshot.finish(&complete()).is_err());
        let mut snapshot = ExecutionSnapshot::new(&Default::default());
        snapshot.observe(&schema());
        snapshot.observe(&end(0));
        assert!(
            snapshot
                .finish(&QueryEvent::Error {
                    message: "failed".into(),
                    cancelled: false
                })
                .is_err()
        );
    }
}
