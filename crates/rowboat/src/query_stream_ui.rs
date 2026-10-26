//! Bounded GUI bridge. Only this operation's receiver can update its captured SQL tab.
use crate::{
    app::{Event, Workbench},
    workspace_model::Route,
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::ActiveTheme;
use rowboat_core::{Database, QueryEvent, QueryResult, QueryStreamOptions};
use std::time::Instant;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Complete,
    Cancelled,
    Failed,
}
#[derive(Default)]
pub struct ResultProgress {
    pub timing: Option<crate::query_timing_ui::QueryTiming>,
    rows_read: std::collections::HashMap<usize, u64>,
    retention_limits: Option<(Option<usize>, Option<usize>)>,
    ended: usize,
    outcome: Option<Outcome>,
    elapsed_ms: Option<u64>,
}
impl ResultProgress {
    /// Presentation only: terminal delivery proves cleanup is the remaining work.
    pub fn loading_label(&self, results: &[QueryResult]) -> &'static str {
        if self.outcome.is_some() {
            "Finishing query…"
        } else if results.get(self.ended).is_some_and(|r| !r.rows.is_empty()) {
            "Fetching rows…"
        } else {
            "Waiting for results…"
        }
    }

    pub fn result_pending(&self, index: usize) -> bool {
        index >= self.ended && self.outcome.is_none()
    }

    pub fn byte_limit(&self) -> Option<usize> {
        self.retention_limits.and_then(|(_, bytes)| bytes)
    }
    fn observe(&mut self, event: &QueryEvent) {
        match event {
            QueryEvent::ResultEnd {
                result_index,
                rows_seen,
                ..
            } => {
                self.ended = result_index + 1;
                self.rows_read.insert(*result_index, *rows_seen);
            }
            QueryEvent::Complete { .. } => self.outcome = Some(Outcome::Complete),
            QueryEvent::Error { cancelled, .. } => {
                self.outcome = Some(if *cancelled {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed
                })
            }
            _ => {}
        }
    }
    fn finish(&mut self, elapsed_ms: u64, failed: bool) {
        self.elapsed_ms = Some(elapsed_ms);
        if failed && self.outcome != Some(Outcome::Cancelled) {
            self.outcome = Some(Outcome::Failed);
        }
    }
}
pub fn result_row_count(
    progress: Option<&ResultProgress>,
    index: usize,
    result: &QueryResult,
) -> String {
    let retained = result.rows.len();
    if let Some(read) = progress.and_then(|p| p.rows_read.get(&index))
        && (result.truncated || *read != retained as u64)
    {
        return format!("{retained} retained of {read} rows read");
    }
    if progress.is_some_and(|p| index >= p.ended) {
        return format!("{retained} rows retained so far");
    }
    if result.truncated {
        return format!("{retained} retained rows");
    }
    format!("{retained} rows")
}
pub fn result_presentation(
    progress: Option<&ResultProgress>,
    index: usize,
    result: &QueryResult,
    running: bool,
) -> (String, String) {
    let incomplete = progress.is_some_and(|p| index >= p.ended);
    let timing = if incomplete {
        progress
            .and_then(|p| p.elapsed_ms)
            .map(|ms| format!("{ms} ms batch elapsed"))
            .unwrap_or_else(|| {
                if progress.is_some_and(|p| p.outcome.is_some()) {
                    "waiting for cleanup"
                } else {
                    "running"
                }
                .into()
            })
    } else {
        format!("{} ms SQL/fetch", result.elapsed_ms)
    };
    let mut notices = vec![];
    if incomplete {
        notices.push(match progress.and_then(|p| p.outcome) {
            Some(Outcome::Cancelled) => "Incomplete · query cancelled",
            Some(Outcome::Failed) => "Incomplete · query failed",
            _ => "Streaming · incomplete result",
        });
    } else {
        match progress.and_then(|p| p.outcome) {
            Some(Outcome::Cancelled) => notices.push("Result complete · query cancelled"),
            Some(Outcome::Failed) => notices.push("Result complete · query failed"),
            _ if running => notices.push("Result complete · query still running"),
            _ => {}
        }
    }
    // Only core ResultEnd reports truncation. Identify the byte limit only when
    // the emitted result demonstrably did not reach this run's row bound.
    let retention_notice = if result.truncated {
        match progress
            .filter(|p| p.rows_read.contains_key(&index))
            .and_then(|p| p.retention_limits)
        {
            Some((rows, Some(bytes))) if rows.is_none_or(|limit| result.rows.len() < limit) => {
                let bound = if bytes % (1024 * 1024) == 0 {
                    format!("{} MiB", bytes / (1024 * 1024))
                } else {
                    format!("{bytes} bytes")
                };
                format!(
                    "Partial display · {bound} per-query retention limit reached · Adjust in Preferences / Transfer → Stream all"
                )
            }
            Some((Some(rows), Some(bytes))) => format!(
                "Partial display · retention limits: {rows} rows per result / {} MiB per query · Adjust in Preferences / Transfer → Stream all",
                bytes / (1024 * 1024)
            ),
            _ => "Partial display · row/byte retention limit reached".into(),
        }
    } else {
        String::new()
    };
    if result.truncated {
        notices.push(&retention_notice);
    }
    (timing, notices.join(" · "))
}

pub struct StatementPresentation {
    pub complete: bool,
    pub icon: &'static str,
    pub message: String,
    pub timing: String,
    pub notice: String,
}

/// ResultEnd proves this statement finished, not that its effects committed.
/// A later batch failure/cancellation must not erase that per-result evidence.
pub fn statement_presentation(
    progress: Option<&ResultProgress>,
    index: usize,
    result: &QueryResult,
    running: bool,
    metadata_missing: bool,
) -> StatementPresentation {
    let complete = progress.is_none_or(|p| index < p.ended);
    let (timing, notice) = result_presentation(progress, index, result, running);
    StatementPresentation {
        complete,
        icon: if complete {
            "✓"
        } else if progress.is_some_and(|p| p.outcome.is_some()) {
            "!"
        } else {
            "…"
        },
        message: if complete {
            format!(
                "Statement complete · {} rows affected{}",
                result.rows_affected,
                if metadata_missing {
                    " · empty-result column metadata unavailable"
                } else {
                    ""
                }
            )
        } else {
            "Statement incomplete".into()
        },
        timing,
        notice,
    }
}

pub struct RunningQuery {
    route: Route,
    tab: usize,
    connection: String,
    sql: String,
    receiver: mpsc::Receiver<Delivery>,
    cancel: watch::Sender<bool>,
    order: Order,
    error: Option<String>,
    metadata_missing: bool,
    retention_rejected: bool,
}
enum Delivery {
    Frame(QueryEvent),
    Finished {
        error_position: Option<rowboat_core::execution::ErrorPosition>,
        timing: Option<Box<crate::query_timing_ui::QueryTiming>>,
        elapsed_ms: u64,
        error: Option<String>,
        transaction: Result<bool, String>,
        edit_source: Option<Box<rowboat_core::QueryEditSource>>,
    },
}
#[derive(Default)]
struct Order {
    active: Option<usize>,
    ended: usize,
    terminal: bool,
}
impl Order {
    fn accept(&mut self, event: &QueryEvent) -> Result<(), String> {
        if self.terminal {
            return Err("Query event arrived after terminal completion".into());
        }
        match event {
            QueryEvent::Schema { result_index, .. } => {
                if self.active.is_some() || *result_index != self.ended {
                    return Err("Query schema arrived out of order".into());
                }
                self.active = Some(*result_index);
            }
            QueryEvent::Rows { result_index, .. } => {
                if self.active != Some(*result_index) {
                    return Err("Query rows belong to another result".into());
                }
            }
            QueryEvent::ResultEnd { result_index, .. } => {
                if self.active != Some(*result_index) {
                    return Err("Query result ended out of order".into());
                }
                self.active = None;
                self.ended += 1;
            }
            QueryEvent::Complete { result_count, .. } => {
                if self.active.is_some() || *result_count != self.ended {
                    return Err("Incomplete query results at completion".into());
                }
                self.terminal = true;
            }
            QueryEvent::Error { .. } => self.terminal = true,
        }
        Ok(())
    }
}
async fn pump(
    db: Database,
    sql: String,
    options: QueryStreamOptions,
    tx: mpsc::Sender<Delivery>,
    mut cancel: watch::Receiver<bool>,
    mut timing: crate::query_timing_ui::QueryTiming,
    for_editing: bool,
) {
    let mut error_position = None;
    let result = async {
        let (mut stream, capture) = if for_editing {
            let (stream, capture) = db.query_stream_for_editing(&sql, options).await?;
            (stream, Some(capture))
        } else {
            (db.query_stream(&sql, options).await?, None)
        };
        timing.launch_ms = timing.elapsed();
        let mut cancelled = false;
        let outcome = loop {
            let event = tokio::select! {
                biased;
                _=cancel.changed(),if !cancelled=>{cancelled=true;stream.cancel();continue;},
                event=stream.recv()=>event,
            };
            let Some(event) = event else {
                break Err(anyhow::anyhow!(
                    "Query stream ended without terminal completion"
                ));
            };
            let terminal = matches!(
                event,
                QueryEvent::Complete { .. } | QueryEvent::Error { .. }
            );
            match &event {
                QueryEvent::Schema { .. } if timing.first_schema_ms.is_none() => {
                    timing.first_schema_ms = Some(timing.elapsed())
                }
                QueryEvent::ResultEnd { elapsed_ms, .. } => {
                    timing.result_fetch_ms = timing.result_fetch_ms.saturating_add(*elapsed_ms)
                }
                _ => {}
            }
            if terminal {
                timing.terminal_ms = Some(timing.elapsed());
            }
            let error = if let QueryEvent::Error { message, .. } = &event {
                Some(message.clone())
            } else {
                None
            };
            // Reserve before moving a batch: cancellation remains responsive under UI backpressure.
            let permit = loop {
                tokio::select! {
                    biased;
                    _=cancel.changed(),if !cancelled=>{cancelled=true;stream.cancel();},
                    permit=tx.reserve()=>break permit,
                }
            };
            match permit {
                Ok(permit) => permit.send(Delivery::Frame(event)),
                Err(_) => {
                    let _ = stream.close().await;
                    return Err(anyhow::anyhow!("GUI receiver closed"));
                }
            }
            if terminal {
                break error.map_or(Ok(()), |e| Err(anyhow::anyhow!(e)));
            }
        };
        timing.core = stream.timings();
        error_position = stream.error_position();
        let cleanup_started = Instant::now();
        let cleanup = stream.close().await;
        timing.cleanup_capture_ms = cleanup_started.elapsed().as_millis() as u64;
        outcome?;
        cleanup?;
        let source = match capture {
            Some(capture) => Some(capture.finish().await),
            None => None,
        };
        timing.cleanup_capture_ms = cleanup_started.elapsed().as_millis() as u64;
        Ok::<_, anyhow::Error>(source)
    }
    .await;
    let state_started = Instant::now();
    let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
    timing.state_check_ms = state_started.elapsed().as_millis() as u64;
    let elapsed_ms = timing.elapsed();
    timing.worker_ms = elapsed_ms;
    let (edit_source, error) = match result {
        Ok(source) => (source, None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let _ = tx
        .send(Delivery::Finished {
            error_position,
            timing: Some(Box::new(timing)),
            elapsed_ms,
            error,
            transaction,
            edit_source: edit_source.map(Box::new),
        })
        .await;
}
impl Workbench {
    pub fn query_stream_options(&self, for_editing: bool) -> anyhow::Result<QueryStreamOptions> {
        let bytes = self.available_results_bytes(Some(self.tabs[self.active_tab].id))? / 2;
        anyhow::ensure!(
            bytes > 0,
            "Shared result memory budget exhausted; close results or pins before running SQL"
        );
        Ok(QueryStreamOptions {
            row_limit: Some(self.state.preferences.query_row_limit),
            byte_limit: Some(if for_editing {
                bytes.min(64 * 1024 * 1024)
            } else {
                bytes
            }),
            ..Default::default()
        })
    }
    pub fn render_statement_result(
        &self,
        result: &QueryResult,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab = &self.tabs[self.active_tab];
        let presentation = statement_presentation(
            tab.query_progress.as_ref(),
            tab.result_index,
            result,
            self.query_running_for(tab.id),
            tab.metadata_incomplete.contains(&tab.result_index),
        );
        let theme = cx.omarchy();
        crate::view::column()
            .id("statement-result")
            .debug_selector(|| "statement-result".into())
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(30.))
                    .text_color(if presentation.complete {
                        theme.success
                    } else {
                        theme.warning
                    })
                    .child(presentation.icon),
            )
            .child(presentation.message)
            .child(presentation.timing)
            .when(!presentation.notice.is_empty(), |element| {
                element.child(div().text_color(theme.warning).child(presentation.notice))
            })
            .into_any_element()
    }
    pub fn query_running_for(&self, tab: usize) -> bool {
        self.query_stream
            .as_ref()
            .is_some_and(|q| q.tab == tab && q.route == self.tx.route)
    }
    pub fn begin_query_stream(
        &mut self,
        db: Database,
        tab: usize,
        sql: String,
        execution: String,
        for_editing: bool,
        options: QueryStreamOptions,
    ) {
        self.begin_query_edit_capture(tab, for_editing);
        // Local sorting describes the previous display, not the next SQL result.
        // Reset at launch, before any replacement rows or headers can arrive.
        if self.tabs[self.active_tab].id == tab {
            self.query_sort = None;
        }
        let (tx, receiver) = mpsc::channel(2);
        let (cancel, cancel_rx) = watch::channel(false);
        self.query_stream = Some(RunningQuery {
            route: self.tx.route,
            tab,
            connection: self.profile.as_ref().unwrap().id.clone(),
            sql,
            receiver,
            cancel,
            order: Order::default(),
            error: None,
            metadata_missing: false,
            retention_rejected: false,
        });
        if let Some(progress) = self
            .tabs
            .iter_mut()
            .find(|t| t.id == tab)
            .and_then(|t| t.query_progress.as_mut())
        {
            progress.retention_limits = Some((options.row_limit, options.byte_limit));
        }
        self.runtime.spawn(pump(
            db,
            execution,
            options,
            tx,
            cancel_rx,
            crate::query_timing_ui::QueryTiming::new(),
            for_editing,
        ));
    }
    pub fn cancel_query_stream(&mut self) -> bool {
        if let Some(query) = &self.query_stream {
            let _ = query.cancel.send(true);
            return true;
        }
        false
    }
    pub fn poll_query_stream(&mut self, cx: &mut Context<Self>) {
        let Some(mut query) = self.query_stream.take() else {
            return;
        };
        if query.route != self.tx.route {
            let _ = query.cancel.send(true);
            return;
        }
        for _ in 0..8 {
            let delivery = match query.receiver.try_recv() {
                Ok(delivery) => delivery,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => Delivery::Finished {
                    error_position: None,
                    timing: None,
                    edit_source: None,
                    elapsed_ms: 0,
                    error: Some("Query worker stopped without reporting cleanup".into()),
                    transaction: Err("Query transaction state is unknown".into()),
                },
            };
            match delivery {
                Delivery::Frame(event) => {
                    if let Err(error) = query.order.accept(&event) {
                        query.error = Some(error);
                        let _ = query.cancel.send(true);
                        continue;
                    }
                    if query.retention_rejected {
                        continue;
                    }
                    if !self.result_frame_fits(query.tab, &event) {
                        query.retention_rejected = true;
                        let error = "Shared result memory budget exhausted; query cancelled and retained results are incomplete. Close results or pins before running again.".to_owned();
                        query.error = Some(error.clone());
                        let _ = query.cancel.send(true);
                        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == query.tab) {
                            tab.error = Some(error.clone());
                            if let Some(result) = tab.results.last_mut() {
                                result.truncated = true;
                            }
                            if let Some(progress) = &mut tab.query_progress {
                                progress.outcome = Some(Outcome::Failed);
                            }
                        }
                        self.fail(error);
                        cx.notify();
                        continue;
                    }
                    let Some(tab) = self.tabs.iter_mut().find(|t| {
                        t.id == query.tab
                            && t.connection_id.as_deref() == Some(query.connection.as_str())
                    }) else {
                        let _ = query.cancel.send(true);
                        continue;
                    };
                    if let Some(progress) = &mut tab.query_progress {
                        progress.observe(&event);
                    }
                    match event {
                        QueryEvent::Schema {
                            columns,
                            metadata_complete,
                            result_index,
                        } => {
                            query.metadata_missing |= !metadata_complete;
                            if !metadata_complete {
                                tab.metadata_incomplete.push(result_index);
                            }
                            tab.results.push(QueryResult {
                                columns,
                                ..Default::default()
                            });
                        }
                        QueryEvent::Rows { result_index, rows } => {
                            tab.results[result_index].rows.extend(rows);
                        }
                        QueryEvent::ResultEnd {
                            result_index,
                            rows_affected,
                            elapsed_ms,
                            truncated,
                            ..
                        } => {
                            let result = &mut tab.results[result_index];
                            result.rows_affected = rows_affected;
                            result.elapsed_ms = elapsed_ms;
                            result.truncated = truncated;
                        }
                        QueryEvent::Error { message, .. } => {
                            tab.error = Some(message.clone());
                            query.error = Some(message);
                        }
                        QueryEvent::Complete { .. } => {}
                    }
                    self.selected = None;
                    self.status = format!(
                        "Streaming · SQL tab {} · {} retained rows · partial / read-only{}",
                        query.tab,
                        tab.results.iter().map(|r| r.rows.len()).sum::<usize>(),
                        if query.metadata_missing {
                            " · some empty-result metadata unavailable"
                        } else {
                            ""
                        }
                    );
                    self.activity_revision = self.activity_revision.wrapping_add(1);
                    cx.notify();
                }
                Delivery::Finished {
                    error_position,
                    timing,
                    elapsed_ms,
                    error,
                    transaction,
                    edit_source,
                } => {
                    self.receive_execution_position(query.tab, error_position);
                    let error = query.error.or(error).or_else(|| {
                        (!query.order.terminal)
                            .then(|| "Query ended without successful terminal completion".into())
                    });
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == query.tab)
                        && let Some(progress) = &mut tab.query_progress
                    {
                        progress.finish(elapsed_ms, error.is_some());
                        progress.timing = timing.map(|t| *t);
                    }
                    self.finish_query_edit_capture(
                        query.tab,
                        edit_source.map(|source| *source),
                        error.as_deref(),
                    );
                    let result = match error {
                        Some(error) => Err(error),
                        None => Ok(self
                            .tabs
                            .iter_mut()
                            .find(|t| t.id == query.tab)
                            .map(|t| std::mem::take(&mut t.results))
                            .unwrap_or_default()),
                    };
                    let _ = self.tx.send(Event::TransactionState(transaction));
                    let _ = self.tx.send(Event::Query {
                        tab: query.tab,
                        sql: query.sql,
                        connection: query.connection,
                        elapsed_ms,
                        result,
                    });
                    return;
                }
            }
        }
        self.query_stream = Some(query);
    }
}
#[cfg(test)]
mod tests {
    use super::{Order, QueryEvent};
    #[test]
    fn retained_counts_use_completed_rows_read_without_claiming_an_incomplete_total() {
        use super::{ResultProgress, result_row_count};
        let result = rowboat_core::QueryResult {
            rows: vec![vec![]; 7165],
            truncated: true,
            ..Default::default()
        };
        let mut progress = ResultProgress::default();
        assert_eq!(
            result_row_count(Some(&progress), 0, &result),
            "7165 rows retained so far"
        );
        progress.observe(&QueryEvent::ResultEnd {
            result_index: 0,
            rows_seen: 24489,
            rows_emitted: 7165,
            rows_affected: 0,
            elapsed_ms: 12000,
            truncated: true,
        });
        assert_eq!(
            result_row_count(Some(&progress), 0, &result),
            "7165 retained of 24489 rows read"
        );
        assert_eq!(
            result_row_count(Some(&progress), 1, &result),
            "7165 rows retained so far"
        );
        progress.observe(&QueryEvent::Error {
            message: "cancelled later statement".into(),
            cancelled: true,
        });
        assert_eq!(
            result_row_count(Some(&progress), 1, &result),
            "7165 rows retained so far"
        );
        assert_eq!(result_row_count(None, 0, &result), "7165 retained rows");
        progress.retention_limits = Some((Some(100000), Some(64 * 1024 * 1024)));
        let (_, notice) = super::result_presentation(Some(&progress), 0, &result, false);
        assert!(
            notice.contains("64 MiB per-query retention limit"),
            "{notice}"
        );
        progress.retention_limits = Some((Some(7165), Some(64 * 1024 * 1024)));
        let (_, notice) = super::result_presentation(Some(&progress), 0, &result, false);
        assert!(
            notice.contains("7165 rows per result / 64 MiB per query"),
            "{notice}"
        );
    }
    #[test]
    fn partial_cancel_and_error_footers_do_not_claim_limits_or_zero_elapsed() {
        use super::{ResultProgress, result_presentation};
        use rowboat_core::{Cell, QueryResult};
        let result = QueryResult {
            rows: vec![vec![Cell::Text("row".into())]; 1647],
            ..Default::default()
        };
        for cancelled in [true, false] {
            let mut progress = ResultProgress::default();
            progress.observe(&QueryEvent::Schema {
                result_index: 0,
                columns: vec![],
                metadata_complete: true,
            });
            assert_eq!(
                result_presentation(Some(&progress), 0, &result, true),
                ("running".into(), "Streaming · incomplete result".into())
            );
            progress.observe(&QueryEvent::Error {
                message: if cancelled {
                    "Query cancelled"
                } else {
                    "Server error"
                }
                .into(),
                cancelled,
            });
            progress.finish(16470, true);
            let (timing, notice) = result_presentation(Some(&progress), 0, &result, false);
            assert_eq!(timing, "16470 ms batch elapsed");
            assert_eq!(
                notice,
                if cancelled {
                    "Incomplete · query cancelled"
                } else {
                    "Incomplete · query failed"
                }
            );
            assert!(!notice.contains("limit"));
        }
    }
    #[test]
    fn completed_results_keep_real_retention_limits_when_a_later_result_is_cancelled() {
        use super::{ResultProgress, result_presentation};
        use rowboat_core::QueryResult;
        let mut progress = ResultProgress::default();
        progress.observe(&QueryEvent::ResultEnd {
            result_index: 0,
            rows_affected: 0,
            rows_seen: 20,
            rows_emitted: 3,
            elapsed_ms: 7,
            truncated: true,
        });
        progress.observe(&QueryEvent::Schema {
            result_index: 1,
            columns: vec![],
            metadata_complete: true,
        });
        progress.observe(&QueryEvent::Error {
            message: "Query cancelled".into(),
            cancelled: true,
        });
        progress.finish(100, true);
        let limited = QueryResult {
            elapsed_ms: 7,
            truncated: true,
            ..Default::default()
        };
        let (timing, notice) = result_presentation(Some(&progress), 0, &limited, false);
        assert_eq!(timing, "7 ms SQL/fetch");
        assert_eq!(
            notice,
            "Result complete · query cancelled · Partial display · row/byte retention limit reached"
        );
        let (_, notice) = result_presentation(Some(&progress), 1, &QueryResult::default(), false);
        assert_eq!(notice, "Incomplete · query cancelled");
        let (_, notice) = result_presentation(None, 0, &limited, false);
        assert_eq!(notice, "Partial display · row/byte retention limit reached");
    }
    #[test]
    fn stream_order_rejects_cross_result_rows_duplicate_terminal_and_partial_success() {
        let mut order = Order::default();
        assert!(
            order
                .accept(&QueryEvent::Rows {
                    result_index: 0,
                    rows: vec![]
                })
                .is_err()
        );
        order
            .accept(&QueryEvent::Schema {
                result_index: 0,
                columns: vec![],
                metadata_complete: true,
            })
            .unwrap();
        assert!(
            order
                .accept(&QueryEvent::Rows {
                    result_index: 1,
                    rows: vec![]
                })
                .is_err()
        );
        assert!(
            order
                .accept(&QueryEvent::Complete {
                    result_count: 1,
                    elapsed_ms: 0
                })
                .is_err()
        );
        order
            .accept(&QueryEvent::ResultEnd {
                result_index: 0,
                rows_affected: 0,
                rows_seen: 0,
                rows_emitted: 0,
                elapsed_ms: 0,
                truncated: false,
            })
            .unwrap();
        order
            .accept(&QueryEvent::Complete {
                result_count: 1,
                elapsed_ms: 1,
            })
            .unwrap();
        assert!(
            order
                .accept(&QueryEvent::Complete {
                    result_count: 1,
                    elapsed_ms: 1
                })
                .is_err()
        );
    }
    async fn exercise_bridge(engine: rowboat_core::Engine) -> anyhow::Result<()> {
        use super::{Delivery, pump};
        use rowboat_core::{Cell, ConnectionProfile, Database, Engine, QueryStreamOptions};
        let db = Database::connect(
            &ConnectionProfile {
                engine,
                host: "127.0.0.1".into(),
                port: if engine == Engine::Postgres {
                    15435
                } else {
                    13309
                },
                database: "rowboat".into(),
                username: if engine == Engine::Postgres {
                    "postgres"
                } else {
                    "root"
                }
                .into(),
                ssl_mode: "disable".into(),
                ..Default::default()
            },
            "rowboat-test",
        )
        .await?;
        let table = format!("gui_bridge_{}", uuid::Uuid::new_v4().simple());
        db.query(
            &format!("CREATE TEMPORARY TABLE {table}(id INTEGER); INSERT INTO {table} VALUES(0)"),
            0,
        )
        .await?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let (_cancel, cancel) = tokio::sync::watch::channel(false);
        let job = tokio::spawn(pump(
            db.clone(),
            format!("UPDATE {table} SET id=id+1; SELECT id FROM {table}"),
            QueryStreamOptions {
                batch_rows: 1,
                ..Default::default()
            },
            tx,
            cancel,
            crate::query_timing_ui::QueryTiming::new(),
            false,
        ));
        let mut terminal = 0;
        let mut finished = 0;
        while let Some(delivery) =
            tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await?
        {
            match delivery {
                Delivery::Frame(QueryEvent::Complete { .. }) => terminal += 1,
                Delivery::Finished {
                    error, transaction, ..
                } => {
                    anyhow::ensure!(error.is_none(), "{error:?}");
                    anyhow::ensure!(!transaction.map_err(anyhow::Error::msg)?);
                    finished += 1;
                }
                _ => {}
            }
        }
        job.await?;
        anyhow::ensure!(
            terminal == 1 && finished == 1,
            "Exactly one completion and cleanup acknowledgement required"
        );
        let results = db.query(&format!("SELECT id FROM {table}"), 1).await?;
        anyhow::ensure!(
            results[0].rows[0][0] == Cell::Text("1".into()),
            "Query was replayed"
        );
        let sql = if engine == Engine::Postgres {
            "SELECT i FROM generate_series(1,10000) i"
        } else {
            "SELECT a.n+b.n*10+c.n*100 AS n FROM (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) b CROSS JOIN (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) c"
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        let job = tokio::spawn(pump(
            db.clone(),
            sql.into(),
            QueryStreamOptions {
                batch_rows: 1,
                row_limit: Some(10000),
                ..Default::default()
            },
            tx,
            cancel_rx,
            crate::query_timing_ui::QueryTiming::new(),
            false,
        ));
        // The producer fills a two-slot bridge while the GUI intentionally pauses.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        anyhow::ensure!(rx.len() <= 2);
        cancel.send(true)?;
        let mut failed = false;
        while let Some(delivery) =
            tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv()).await?
        {
            if let Delivery::Finished {
                error, transaction, ..
            } = delivery
            {
                failed = error.is_some();
                anyhow::ensure!(!transaction.map_err(anyhow::Error::msg)?);
            }
        }
        job.await?;
        anyhow::ensure!(
            failed,
            "Cancelled backpressured stream must not report success"
        );
        db.query("SELECT 1", 1).await?;
        Ok(())
    }
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL fixture on 15435"]
    async fn gui_stream_postgres_bounded_bridge_once_only_and_backpressure_cancel()
    -> anyhow::Result<()> {
        exercise_bridge(rowboat_core::Engine::Postgres).await
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL fixture on 13309"]
    async fn gui_stream_mysql_bounded_bridge_once_only_and_backpressure_cancel()
    -> anyhow::Result<()> {
        exercise_bridge(rowboat_core::Engine::MySql).await
    }
}
