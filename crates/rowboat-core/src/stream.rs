//! Bounded progressive query delivery. A stream owns one cancellation request;
//! dropping its receiver never aborts the cleanup task or cancels a queued peer.
use crate::{Cell, Column};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

pub const MAX_STREAM_RESULTS: usize = 256;
pub const MAX_STREAM_BATCH_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_STREAM_CELL_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;

/// Cancellation policy for this operation, not other users of the connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueryCancellation {
    /// Interrupt the current statement and drain for subsequent session reuse.
    /// MySQL may continue statements already submitted in the same raw batch.
    #[default]
    PreserveSession,
    /// Stop a disposable query session, including its pending raw SQL batch.
    /// MySQL uses KILL CONNECTION. Cancellation (including receiver loss or
    /// delivery failure) retires the session; queued operations must reconnect.
    RetireSession,
}

#[derive(Debug, Clone)]
pub struct QueryStreamOptions {
    pub cancellation: QueryCancellation,
    pub row_limit: Option<usize>,
    pub byte_limit: Option<usize>,
    pub batch_rows: usize,
    pub batch_bytes: usize,
    pub channel_capacity: usize,
    pub max_cell_bytes: usize,
    pub stall_timeout_ms: u64,
}
impl Default for QueryStreamOptions {
    fn default() -> Self {
        Self {
            cancellation: QueryCancellation::PreserveSession,
            row_limit: Some(1000),
            byte_limit: Some(64 * 1024 * 1024),
            batch_rows: 256,
            batch_bytes: 1024 * 1024,
            channel_capacity: 2,
            max_cell_bytes: 1024 * 1024,
            stall_timeout_ms: 30_000,
        }
    }
}
impl QueryStreamOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.batch_rows),
            "batch_rows must be between 1 and 4096"
        );
        ensure!(
            (1..=MAX_STREAM_BATCH_BYTES).contains(&self.batch_bytes),
            "batch_bytes must be between 1 and {MAX_STREAM_BATCH_BYTES}"
        );
        ensure!(
            (1..=8).contains(&self.channel_capacity),
            "channel_capacity must be between 1 and 8"
        );
        ensure!(
            (1..=MAX_STREAM_CELL_BYTES).contains(&self.max_cell_bytes)
                && self.max_cell_bytes <= self.batch_bytes,
            "max_cell_bytes must be between 1 and {MAX_STREAM_CELL_BYTES} and fit batch_bytes"
        );
        ensure!(
            (100..=300_000).contains(&self.stall_timeout_ms),
            "stall_timeout_ms must be between 100 and 300000"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QueryEvent {
    Schema {
        result_index: usize,
        columns: Vec<Column>,
        metadata_complete: bool,
    },
    Rows {
        result_index: usize,
        rows: Vec<Vec<Cell>>,
    },
    ResultEnd {
        result_index: usize,
        rows_affected: u64,
        rows_seen: u64,
        rows_emitted: u64,
        elapsed_ms: u64,
        truncated: bool,
    },
    Complete {
        result_count: usize,
        elapsed_ms: u64,
    },
    Error {
        message: String,
        cancelled: bool,
    },
}

pub(crate) struct Completion {
    pub event: QueryEvent,
    pub reusable: bool,
    pub error_position: Option<crate::execution::ErrorPosition>,
}
#[derive(Clone)]
struct Stop {
    message: String,
    cancelled: bool,
}
pub(crate) struct StreamControl {
    stop: watch::Sender<Option<Stop>>,
    committing: std::sync::Mutex<bool>,
    cancellation: QueryCancellation,
}
impl StreamControl {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_cancellation(QueryCancellation::PreserveSession)
    }
    pub(crate) fn with_cancellation(cancellation: QueryCancellation) -> Arc<Self> {
        Arc::new(Self {
            stop: watch::channel(None).0,
            committing: std::sync::Mutex::new(false),
            cancellation,
        })
    }
    pub(crate) fn retires_session(&self) -> bool {
        self.cancellation == QueryCancellation::RetireSession
    }
    pub(crate) fn stop(&self, reason: &str) {
        self.interrupt(reason, true);
    }
    pub(crate) fn fail(&self, reason: &str) {
        self.interrupt(reason, false);
    }
    fn interrupt(&self, reason: &str, cancelled: bool) {
        let committing = self.committing.lock().expect("commit state mutex");
        if *committing {
            return;
        }
        self.stop.send_if_modified(|state| {
            if state.is_some() {
                false
            } else {
                *state = Some(Stop {
                    message: reason.into(),
                    cancelled,
                });
                true
            }
        });
    }
    pub(crate) fn begin_commit(&self) -> Result<()> {
        let mut committing = self.committing.lock().expect("commit state mutex");
        self.check()?;
        *committing = true;
        Ok(())
    }
    pub(crate) fn is_committing(&self) -> bool {
        *self.committing.lock().expect("commit state mutex")
    }
    pub(crate) fn reason(&self) -> Option<String> {
        self.stop.borrow().as_ref().map(|s| s.message.clone())
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.stop.borrow().as_ref().is_some_and(|s| s.cancelled)
    }
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(reason) = self.reason() {
            Err(anyhow!(reason))
        } else {
            Ok(())
        }
    }
    pub(crate) async fn cancelled(&self) {
        let mut rx = self.stop.subscribe();
        let _ = rx.wait_for(|state| state.is_some()).await;
    }
}

/// Wall-clock phases of one worker, including server/network wait and consumer
/// backpressure. Available after its terminal event; excludes connection setup.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct QueryTimings {
    pub queue_wait_ms: u64,
    pub source_capture_ms: u64,
    pub execution_ms: u64,
    pub cleanup_ms: u64,
    pub total_ms: u64,
}
pub(crate) type TimingSlot = Arc<std::sync::Mutex<Option<QueryTimings>>>;

pub struct QueryStream {
    rx: mpsc::Receiver<QueryEvent>,
    control: Arc<StreamControl>,
    completion: Option<oneshot::Receiver<Completion>>,
    timings: TimingSlot,
    error_position: Option<crate::execution::ErrorPosition>,
}
impl QueryStream {
    pub(crate) fn new(
        rx: mpsc::Receiver<QueryEvent>,
        control: Arc<StreamControl>,
        completion: oneshot::Receiver<Completion>,
    ) -> Self {
        Self {
            rx,
            control,
            completion: Some(completion),
            timings: Arc::new(std::sync::Mutex::new(None)),
            error_position: None,
        }
    }
    pub fn timings(&self) -> Option<QueryTimings> {
        *self.timings.lock().expect("query timing mutex")
    }
    /// Available after the terminal error. Offset addresses this stream's exact
    /// submitted SQL; absent for cancellations, internal SQL and unknown offsets.
    pub fn error_position(&self) -> Option<crate::execution::ErrorPosition> {
        self.error_position
    }
    pub(crate) fn timing_slot(&self) -> TimingSlot {
        self.timings.clone()
    }
    pub async fn recv(&mut self) -> Option<QueryEvent> {
        if let Some(event) = self.rx.recv().await {
            return Some(event);
        }
        // recv itself must be cancellation-safe for timeout/select callers.
        // Keep the terminal receiver owned by self until its await completes.
        let outcome = self.completion.as_mut()?.await;
        self.completion.take();
        Some(match outcome {
            Ok(done) => {
                self.error_position = done.error_position;
                done.event
            }
            Err(_) => QueryEvent::Error {
                message: "Query worker stopped before reporting completion".into(),
                cancelled: false,
            },
        })
    }
    pub fn cancel(&self) {
        self.control.stop("Query cancelled");
    }
    pub async fn close(mut self) -> Result<()> {
        self.control.stop("Query stream closed");
        self.rx.close();
        if let Some(done) = self.completion.take() {
            let done = done
                .await
                .map_err(|_| anyhow!("Query worker stopped before cleanup completed"))?;
            ensure!(
                done.reusable,
                "Query cleanup could not preserve the connection; reconnect required"
            );
        }
        Ok(())
    }
}
impl Drop for QueryStream {
    fn drop(&mut self) {
        self.control.stop("Query stream receiver dropped");
    }
}

pub(crate) struct EventSink {
    pub options: QueryStreamOptions,
    pub control: Arc<StreamControl>,
    tx: mpsc::Sender<QueryEvent>,
    pub result_count: usize,
    active: bool,
    rows_seen: u64,
    rows_emitted: u64,
    total_bytes: usize,
    truncated: bool,
    batch: Vec<Vec<Cell>>,
    batch_size: usize,
    deadline: Option<Instant>,
    result_started: Instant,
    edit_snapshot: Option<crate::query_edit_snapshot::ExecutionSnapshot>,
}
impl EventSink {
    pub fn new(
        options: QueryStreamOptions,
        control: Arc<StreamControl>,
        tx: mpsc::Sender<QueryEvent>,
    ) -> Self {
        Self {
            options,
            control,
            tx,
            result_count: 0,
            active: false,
            rows_seen: 0,
            rows_emitted: 0,
            total_bytes: 0,
            truncated: false,
            batch: Vec::new(),
            batch_size: 0,
            deadline: None,
            result_started: Instant::now(),
            edit_snapshot: None,
        }
    }
    pub fn active(&self) -> bool {
        self.active
    }
    pub(crate) fn emitted_rows(&self) -> u64 {
        self.rows_emitted
    }
    pub fn dispatch(&mut self) {
        if !self.active {
            self.result_started = Instant::now();
        }
    }
    pub fn pending(&self) -> bool {
        !self.batch.is_empty()
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400))
    }
    pub(crate) fn enable_edit_snapshot(&mut self) {
        self.edit_snapshot = Some(crate::query_edit_snapshot::ExecutionSnapshot::new(
            &self.options,
        ));
    }
    pub(crate) fn finish_edit_snapshot(
        &mut self,
        terminal: &QueryEvent,
    ) -> Result<Arc<crate::QueryResult>, String> {
        self.edit_snapshot
            .take()
            .ok_or_else(|| "No execution snapshot was recorded".to_owned())?
            .finish(terminal)
    }
    async fn send(&mut self, event: QueryEvent) -> Result<()> {
        self.control.check()?;
        if let Some(snapshot) = &mut self.edit_snapshot {
            snapshot.observe(&event);
        }
        tokio::select! {
            biased;
            _=self.control.cancelled()=>self.control.check(),
            result=tokio::time::timeout(Duration::from_millis(self.options.stall_timeout_ms),self.tx.send(event))=>{
                match result {
                    Ok(Ok(()))=>Ok(()),
                    Ok(Err(_))=>{self.control.stop("Query stream receiver dropped");self.control.check()},
                    Err(_)=>{self.control.stop("Query stream consumer stalled");self.control.check()},
                }
            }
        }
    }
    pub async fn start(&mut self, columns: Vec<Column>, metadata_complete: bool) -> Result<()> {
        ensure!(!self.active, "Internal duplicate result schema");
        ensure!(
            self.result_count < MAX_STREAM_RESULTS,
            "More than {MAX_STREAM_RESULTS} result sets; split the batch"
        );
        ensure!(columns.len() <= 4096, "Result schema exceeds 4096 columns");
        let bytes: usize = columns
            .iter()
            .map(|c| {
                std::mem::size_of::<Column>()
                    + c.name.len()
                    + c.data_type.len()
                    + c.default_value.as_ref().map_or(0, String::len)
            })
            .sum();
        ensure!(
            bytes <= MAX_SCHEMA_BYTES,
            "Result schema exceeds {MAX_SCHEMA_BYTES} bytes"
        );
        self.active = true;
        self.rows_seen = 0;
        self.rows_emitted = 0;
        self.truncated = false;
        self.send(QueryEvent::Schema {
            result_index: self.result_count,
            columns,
            metadata_complete,
        })
        .await
    }
    pub async fn row(&mut self, row: Vec<Cell>) -> Result<()> {
        self.rows_seen = self.rows_seen.saturating_add(1);
        let size = row_bytes(&row);
        ensure!(
            size <= self.options.batch_bytes,
            "A result row ({size} bytes) exceeds batch_bytes {}; increase the bounded chunk limit or select smaller values",
            self.options.batch_bytes
        );
        if self
            .options
            .row_limit
            .is_some_and(|limit| self.rows_emitted >= limit as u64)
            || self
                .options
                .byte_limit
                .is_some_and(|limit| self.total_bytes.saturating_add(size) > limit)
        {
            self.truncated = true;
            return Ok(());
        }
        if !self.batch.is_empty() && self.batch_size.saturating_add(size) > self.options.batch_bytes
        {
            self.flush().await?;
        }
        if self.batch.is_empty() {
            self.deadline = Some(Instant::now() + Duration::from_millis(50));
        }
        self.rows_emitted = self.rows_emitted.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.batch_size += size;
        self.batch.push(row);
        if self.batch.len() >= self.options.batch_rows
            || self.batch_size >= self.options.batch_bytes
        {
            self.flush().await?;
        }
        Ok(())
    }
    pub async fn flush(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.batch);
        self.batch_size = 0;
        self.deadline = None;
        self.send(QueryEvent::Rows {
            result_index: self.result_count,
            rows,
        })
        .await
    }
    pub async fn end(&mut self, rows_affected: u64) -> Result<()> {
        if !self.active {
            self.start(Vec::new(), false).await?;
        }
        self.flush().await?;
        self.send(QueryEvent::ResultEnd {
            result_index: self.result_count,
            rows_affected,
            rows_seen: self.rows_seen,
            rows_emitted: self.rows_emitted,
            elapsed_ms: self.result_started.elapsed().as_millis() as u64,
            truncated: self.truncated,
        })
        .await?;
        self.result_count += 1;
        self.active = false;
        self.result_started = Instant::now();
        Ok(())
    }
}

pub(crate) fn row_bytes(row: &[Cell]) -> usize {
    std::mem::size_of::<Vec<Cell>>()
        + row
            .iter()
            .map(|c| {
                std::mem::size_of::<Cell>()
                    + match c {
                        Cell::Null => 0,
                        Cell::Text(s) => s.len(),
                        Cell::Binary(b) => b.len(),
                    }
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn options_have_explicit_unlimited_and_absolute_bounds() {
        let default = QueryStreamOptions::default();
        assert_eq!(default.row_limit, Some(1000));
        assert!(default.byte_limit.is_some());
        assert!(
            QueryStreamOptions {
                row_limit: None,
                byte_limit: None,
                ..default.clone()
            }
            .validate()
            .is_ok()
        );
        assert!(
            QueryStreamOptions {
                row_limit: Some(0),
                ..default.clone()
            }
            .validate()
            .is_ok()
        );
        assert!(
            QueryStreamOptions {
                channel_capacity: 9,
                ..default.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            QueryStreamOptions {
                max_cell_bytes: MAX_STREAM_CELL_BYTES + 1,
                ..default
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn ndjson_event_tags_are_stable() {
        assert_eq!(
            serde_json::to_value(QueryEvent::Complete {
                result_count: 2,
                elapsed_ms: 3
            })
            .unwrap()["event"],
            "complete"
        );
        assert_eq!(
            serde_json::to_value(QueryEvent::ResultEnd {
                result_index: 0,
                rows_affected: 0,
                rows_seen: 0,
                rows_emitted: 0,
                elapsed_ms: 0,
                truncated: false
            })
            .unwrap()["event"],
            "result_end"
        );
    }
    #[tokio::test]
    async fn cancelled_recv_preserves_pending_terminal_status() {
        for error in [false, true] {
            let (tx, rx) = mpsc::channel(1);
            let (done, completion) = oneshot::channel();
            let mut stream = QueryStream::new(rx, StreamControl::new(), completion);
            drop(tx);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), stream.recv())
                    .await
                    .is_err()
            );
            let event = if error {
                QueryEvent::Error {
                    message: "late failure".into(),
                    cancelled: false,
                }
            } else {
                QueryEvent::Complete {
                    result_count: 1,
                    elapsed_ms: 1,
                }
            };
            assert!(
                done.send(Completion {
                    error_position: None,
                    event,
                    reusable: true
                })
                .is_ok()
            );
            let received = stream.recv().await;
            assert!(if error {
                matches!(received, Some(QueryEvent::Error { .. }))
            } else {
                matches!(received, Some(QueryEvent::Complete { .. }))
            });
            assert!(stream.recv().await.is_none());
        }
    }
}
