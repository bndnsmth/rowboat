//! A database handle represents one connection-affine workbench session.
#[path = "adapters/mod.rs"]
mod adapters;
use adapters::dialect::{self, parameter};
use adapters::structure;
use adapters::{Conn, Options};
use adapters::{bound_results, execute, raw_results, raw_stream, single_statement_metadata};
use adapters::{
    close_broken_connection, drain_connection, probe_transaction, restore_readonly_default,
    server_in_transaction,
};
#[path = "completion.rs"]
mod completion;
pub use completion::CompletionCatalog;
#[path = "diagram_metadata.rs"]
mod diagram_metadata;
pub use diagram_metadata::{DiagramCancellation, DiagramMetadata, DiagramReadError};
#[path = "administrative.rs"]
mod administrative;
#[path = "namespace.rs"]
mod namespace;
use crate::{
    importer::{ImportCommand, ImportCompletion, RowImport, RowImportOptions},
    model::*,
    readonly::{self, Action, LexMode},
    sql::{qualified_table, quote_ident},
    stream::{Completion, EventSink, QueryEvent, QueryStream, QueryStreamOptions, StreamControl},
};
pub use administrative::{
    AdministrativeMonitor, AdministrativeOperation, AdministrativeOutcome, AdministrativeStatus,
    QuerySession,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
pub use namespace::{DatabaseCatalog, NamespaceCatalog};
#[path = "query_edit.rs"]
mod query_edit;
pub use query_edit::{QueryEditCapture, QueryEditColumn, QueryEditPlan, QueryEditSource};
use std::{
    collections::HashSet,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const MAX_ROWS: usize = 100_000;
const MAX_RESULTS: usize = 256;
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}
struct Inner {
    engine: Engine,
    read_only: bool,
    generation: AtomicU64,
    lex_mode: LexMode,
    mysql_sql_mode: String,
    session: Arc<Mutex<Session>>,
    options: Options,
    backend_id: u64,
    running: AtomicBool,
    cancel_requested: AtomicBool,
    // A delayed cancellation connection must never kill the following operation.
    cancel_gate: Mutex<()>,
    active_stream: std::sync::Mutex<Option<Weak<StreamControl>>>,
    // Sampling owns cancellation before running/backend cancellation is safe:
    // preflight may be probing a caller-owned (even failed) transaction.
    sampling_control: std::sync::Mutex<Option<Weak<StreamControl>>>,
}
struct Session {
    conn: Conn,
}
// Detached import workers retain the session through cleanup. An aborted worker
// must retire its physical connection rather than lend an owned transaction out.
// Disposable query streams use this same guard against worker abort/panic.
struct ImportLease<'a> {
    session: tokio::sync::MutexGuard<'a, Session>,
    armed: bool,
}
impl std::ops::Deref for ImportLease<'_> {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}
impl std::ops::DerefMut for ImportLease<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}
impl Drop for ImportLease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.conn = Conn::Closed;
        }
    }
}
// If a worker is aborted by runtime shutdown or panic, never release an
// uncertain physical connection to the next caller.
struct SamplingLease {
    session: tokio::sync::OwnedMutexGuard<Session>,
    armed: bool,
    inner: Arc<Inner>,
}
impl Drop for SamplingLease {
    fn drop(&mut self) {
        if self.armed {
            self.session.conn = Conn::Closed;
        }
        *self
            .inner
            .sampling_control
            .lock()
            .expect("sampling control mutex") = None;
    }
}

struct Running<'a>(&'a AtomicBool);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Database {
    pub async fn connect(profile: &ConnectionProfile, password: &str) -> Result<Self> {
        ensure!(
            profile.ssh.is_none(),
            "SSH must be established by the tunnel service; pass its returned transport profile before connecting"
        );
        let (options, mut conn) = adapters::connect(profile, password).await?;
        let (backend_id, lex_mode, mysql_sql_mode) =
            adapters::initialize(&mut conn, profile).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                engine: profile.engine,
                read_only: profile.read_only,
                generation: AtomicU64::new(0),
                lex_mode,
                mysql_sql_mode,
                session: Arc::new(Mutex::new(Session { conn })),
                options,
                backend_id,
                running: AtomicBool::new(false),
                cancel_requested: AtomicBool::new(false),
                cancel_gate: Mutex::new(()),
                active_stream: std::sync::Mutex::new(None),
                sampling_control: std::sync::Mutex::new(None),
            }),
        })
    }

    pub fn engine(&self) -> Engine {
        self.inner.engine
    }
    async fn running(&self) -> Running<'_> {
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
        self.metadata_running().await
    }
    async fn metadata_running(&self) -> Running<'_> {
        let _gate = self.inner.cancel_gate.lock().await;
        self.inner.cancel_requested.store(false, Ordering::Release);
        self.inner.running.store(true, Ordering::Release);
        Running(&self.inner.running)
    }

    pub async fn query(&self, sql: &str, limit: usize) -> Result<Vec<QueryResult>> {
        let options = QueryStreamOptions {
            row_limit: Some(limit.min(MAX_ROWS)),
            byte_limit: Some(MAX_BYTES),
            batch_bytes: crate::stream::MAX_STREAM_BATCH_BYTES,
            max_cell_bytes: crate::stream::MAX_STREAM_CELL_BYTES,
            ..Default::default()
        };
        let mut stream = self.query_stream(sql, options).await?;
        let mut results: Vec<QueryResult> = Vec::new();
        while let Some(event) = stream.recv().await {
            match event {
                QueryEvent::Schema {
                    result_index,
                    columns,
                    ..
                } => {
                    ensure!(result_index == results.len(), "Unexpected result index");
                    results.push(QueryResult {
                        columns,
                        ..Default::default()
                    });
                }
                QueryEvent::Rows { result_index, rows } => results
                    .get_mut(result_index)
                    .context("Rows received before schema")?
                    .rows
                    .extend(rows),
                QueryEvent::ResultEnd {
                    result_index,
                    rows_affected,
                    elapsed_ms,
                    truncated,
                    ..
                } => {
                    let result = results
                        .get_mut(result_index)
                        .context("Result ended before schema")?;
                    result.rows_affected = rows_affected;
                    result.elapsed_ms = elapsed_ms;
                    result.truncated = truncated;
                }
                QueryEvent::Complete { .. } => return Ok(results),
                QueryEvent::Error { message, .. } => bail!(message),
            }
        }
        bail!("Query stream ended without successful completion")
    }

    /// Execute one bounded SELECT in an owned server READ ONLY transaction.
    /// Busy/transaction-active sessions are rejected, never queued. Dropping this
    /// future cancels the detached worker; it retains the session through rollback
    /// or retirement. Bounds are errors, not successful partial results. READ ONLY
    /// is a server guarantee, not a sandbox for external effects of functions.
    pub async fn sample_readonly(&self, sql: &str) -> Result<Vec<QueryResult>> {
        ensure!(
            self.engine() != Engine::Sqlite,
            "SQLite sampling provider is unsupported"
        );
        let control = StreamControl::new();
        let lease = {
            // Register atomically with lease acquisition relative to cancel().
            // No await/backend operation while this short-lived lock is held.
            let mut sampling = self
                .inner
                .sampling_control
                .lock()
                .expect("sampling control mutex");
            let session = self
                .inner
                .session
                .clone()
                .try_lock_owned()
                .map_err(|_| anyhow!("Sampling rejected: session is busy"))?;
            ensure!(sql.len() <= 64 * 1024, "Sample SQL exceeds 64 KiB");
            *sampling = Some(Arc::downgrade(&control));
            SamplingLease {
                session,
                armed: true,
                inner: self.inner.clone(),
            }
        };
        let (done, completion) = tokio::sync::oneshot::channel();
        let db = self.clone();
        let sql = sql.to_owned();
        tokio::spawn(async move { db.sampling_worker(lease, sql, control, done).await });
        completion
            .await
            .context("Sampling worker stopped; session retired")?
    }

    async fn sampling_worker(
        &self,
        mut lease: SamplingLease,
        sql: String,
        control: Arc<StreamControl>,
        mut done: tokio::sync::oneshot::Sender<Result<Vec<QueryResult>>>,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // Complete the state probe before responding to explicit cancellation or
        // a dropped consumer: cancelling SAVEPOINT must not abort a caller's TX.
        // No user SQL or transaction-mode changes occur during this phase.
        let preflight = tokio::time::timeout_at(deadline, async {
            ensure!(
                !adapters::manual_commit(&mut lease.session.conn).await?,
                "Sampling requires MySQL autocommit=1"
            );
            ensure!(
                !server_in_transaction(&mut lease.session.conn).await?,
                "Sampling rejected: an existing server transaction is active"
            );
            let (settings, mode) = adapters::sampling::settings(&mut lease.session.conn).await?;
            let sql = readonly::sample_statement(self.engine(), &sql, mode)?;
            Ok::<_, anyhow::Error>((settings, sql))
        })
        .await;
        let (settings, sql) = match preflight {
            Ok(Ok(value)) => value,
            error => {
                let error = match error {
                    Ok(Err(error)) => error,
                    Err(_) => anyhow!("Sampling preflight exceeded five seconds; session retired"),
                    Ok(Ok(_)) => unreachable!(),
                };
                // A timed-out probe might leave protocol/control state unknown.
                // A completed rejection can retain a healthy or failed caller TX.
                if tokio::time::Instant::now() < deadline
                    && matches!(
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            drain_connection(&mut lease.session.conn)
                        )
                        .await,
                        Ok(Ok(()))
                    )
                {
                    lease.armed = false;
                } else {
                    close_broken_connection(&mut lease.session.conn).await;
                }
                let _ = done.send(Err(error));
                return;
            }
        };
        if done.is_closed() || control.is_cancelled() {
            lease.armed = false;
            let _ = done.send(Err(anyhow!("Sampling cancelled during preflight")));
            return;
        }
        let running = self.running().await;
        *self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex") = Some(Arc::downgrade(&control));
        let mut owned = false;
        let mut outcome = {
            let execution = async {
                control.check()?;
                owned = true; // BEGIN may reach the server before its reply arrives.
                execute(&mut lease.session.conn, "START TRANSACTION READ ONLY").await?;
                control.check()?;
                adapters::sampling::configure(&mut lease.session.conn).await?;
                control.check()?;
                let before_timeout = adapters::sampling::timeouts(&mut lease.session.conn).await?;
                let result = adapters::sampling::result(&mut lease.session.conn, &sql).await?;
                // MySQL SLEEP can return a successful row (1) when the server's
                // execution timer interrupts it. Do not mistake that for a
                // complete sample merely because no native error was emitted.
                ensure!(
                    before_timeout == adapters::sampling::timeouts(&mut lease.session.conn).await?,
                    "Sampling server execution timeout; sample discarded"
                );
                Ok(result)
            };
            tokio::pin!(execution);
            tokio::select! {
                biased;
                _ = done.closed() => Err(anyhow!("Sampling cancelled: consumer dropped")),
                _ = control.cancelled() => Err(anyhow!("Sampling cancelled")),
                _ = tokio::time::sleep_until(deadline) => Err(anyhow!("Sampling execution exceeded five seconds")),
                result = &mut execution => result,
            }
        };
        if control.is_cancelled() {
            outcome = Err(anyhow!("Sampling cancelled; sample discarded"));
        }
        // Cancel once, then drain the same physical connection. Never replay the
        // sample. Hold the cancellation gate until no delayed cancel can hit the
        // rollback or the following session user.
        let mut cancellation_confirmed = true;
        {
            let _gate = self.inner.cancel_gate.lock().await;
            if outcome.is_err() && owned {
                cancellation_confirmed = matches!(
                    tokio::time::timeout(Duration::from_secs(2), self.cancel_backend()).await,
                    Ok(Ok(()))
                );
            }
            self.inner.running.store(false, Ordering::Release);
        }
        let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
            ensure!(cancellation_confirmed, "Sampling cancellation was not acknowledged; retire session to prevent a delayed cancel reaching another operation");
            drain_connection(&mut lease.session.conn).await?;
            if owned {
                end_owned_transaction(&mut lease.session.conn,false).await?;
            }
            ensure!(!server_in_transaction(&mut lease.session.conn).await?, "Sampling transaction remained active after rollback");
            adapters::sampling::restore(&mut lease.session.conn,settings).await?;
            Ok::<(), anyhow::Error>(())
        }).await;
        if !matches!(cleanup, Ok(Ok(()))) {
            close_broken_connection(&mut lease.session.conn).await;
            outcome = Err(anyhow!(
                "Sampling cleanup could not be confirmed; session retired; sample discarded"
            ));
        }
        *self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex") = None;
        lease.armed = false;
        // Cancellation remains token-only through cleanup. Serialize completion
        // with cancel() so a cancellation already acknowledged cannot publish a
        // fresh sample after a delayed rollback/restoration.
        {
            let mut sampling = self
                .inner
                .sampling_control
                .lock()
                .expect("sampling control mutex");
            if control.is_cancelled() && outcome.is_ok() {
                outcome = Err(anyhow!("Sampling cancelled; sample discarded"));
            }
            *sampling = None;
        }
        drop(running);
        drop(lease);
        let _ = done.send(outcome);
    }

    pub async fn query_stream(
        &self,
        sql: &str,
        options: QueryStreamOptions,
    ) -> Result<QueryStream> {
        self.start_query_stream(sql, options, None).await
    }

    async fn start_query_stream(
        &self,
        sql: &str,
        options: QueryStreamOptions,
        edit_capture: Option<query_edit::EditCaptureRequest>,
    ) -> Result<QueryStream> {
        options.validate()?;
        let (tx, rx) = tokio::sync::mpsc::channel(options.channel_capacity);
        let (done, completion) = tokio::sync::oneshot::channel();
        let control = StreamControl::with_cancellation(options.cancellation);
        let stream = QueryStream::new(rx, control.clone(), completion);
        let timings = stream.timing_slot();
        let enqueued = Instant::now();
        let db = self.clone();
        let sql = sql.to_owned();
        tokio::spawn(async move {
            db.stream_worker(
                sql,
                EventSink::new(options, control, tx),
                done,
                edit_capture,
                timings,
                enqueued,
            )
            .await;
        });
        Ok(stream)
    }

    async fn stream_worker(
        &self,
        sql: String,
        mut sink: EventSink,
        done: tokio::sync::oneshot::Sender<Completion>,
        edit_capture: Option<query_edit::EditCaptureRequest>,
        timing_slot: crate::stream::TimingSlot,
        started: Instant,
    ) {
        let mut timings = crate::stream::QueryTimings::default();
        let control = sink.control.clone();
        let session = tokio::select! {
            biased;
            _=control.cancelled()=>{timings.queue_wait_ms=started.elapsed().as_millis() as u64;timings.total_ms=timings.queue_wait_ms;*timing_slot.lock().expect("query timing mutex")=Some(timings);let _=done.send(Completion{event:QueryEvent::Error{message:control.reason().unwrap_or_else(||"Query cancelled".into()),cancelled:true},reusable:true,error_position:None});return;},
            session=self.inner.session.lock()=>session,
        };
        let mut session = ImportLease {
            session,
            armed: control.retires_session(),
        };
        // Never send cancellation to a previously retired backend ID: it is no
        // longer this handle's session, and could eventually be reassigned.
        if matches!(session.conn, Conn::Closed) {
            let _ = done.send(Completion {
                error_position: None,
                event: QueryEvent::Error {
                    message: "Connection closed; reconnect required".into(),
                    cancelled: control.is_cancelled(),
                },
                reusable: false,
            });
            return;
        }
        let running = {
            // Publish this operation's cancellation policy atomically with its
            // running flag. A concurrent Database::cancel must not fall back to
            // KILL QUERY in the gap before a disposable stream is registered.
            let _gate = self.inner.cancel_gate.lock().await;
            self.inner.generation.fetch_add(1, Ordering::AcqRel);
            self.inner.cancel_requested.store(false, Ordering::Release);
            *self
                .inner
                .active_stream
                .lock()
                .expect("stream control mutex") = Some(Arc::downgrade(&control));
            self.inner.running.store(true, Ordering::Release);
            Running(&self.inner.running)
        };
        let retire_on_stop = control.retires_session();
        let mut retirement_error = None;
        timings.queue_wait_ms = started.elapsed().as_millis() as u64;
        let capture_started = Instant::now();
        let mut owned_readonly = false;
        let required_capture = edit_capture
            .as_ref()
            .is_some_and(|request| request.required);
        let (mut edit_source, edit_sql) = if edit_capture.is_some() {
            let captured = {
                let capture =
                    self.capture_query_edit_source(&mut session.conn, &sql, required_capture);
                tokio::pin!(capture);
                tokio::select! {
                    biased;
                    _=control.cancelled()=>{
                        if retire_on_stop {
                            let _gate = self.inner.cancel_gate.lock().await;
                            retirement_error = self.cancel_query_session().await.err();
                            None
                        } else {
                        {let _gate=self.inner.cancel_gate.lock().await;let _=self.cancel_backend().await;}
                        tokio::time::timeout(Duration::from_secs(10), &mut capture).await.ok()
                        }
                    },
                    result=&mut capture=>Some(result),
                }
            };
            match captured {
                Some((source, execution_sql)) => (Some(source), execution_sql),
                None => {
                    close_broken_connection(&mut session.conn).await;
                    (
                        Some(QueryEditSource::unavailable(if retire_on_stop {
                            "Source capture cancelled; session retired"
                        } else {
                            "Source capture cancelled; connection retired after cleanup timeout"
                        })),
                        None,
                    )
                }
            }
        } else {
            (None, None)
        };
        let capture_rejection = required_capture
            .then(|| match (&edit_source, &edit_sql) {
                (Some(source), Some(_)) => source.read_only_reason().map(str::to_owned),
                _ => Some(
                    edit_source
                        .as_ref()
                        .and_then(QueryEditSource::read_only_reason)
                        .unwrap_or("Source proof unavailable; SQL was not executed")
                        .to_owned(),
                ),
            })
            .flatten();
        let capture_rejected = capture_rejection.is_some();
        let mut edit_transaction = edit_sql.is_some();
        timings.source_capture_ms = capture_started.elapsed().as_millis() as u64;
        if edit_source
            .as_ref()
            .is_some_and(|source| source.read_only_reason().is_none())
        {
            sink.enable_edit_snapshot();
        }
        let execution_started = Instant::now();
        let mut outcome = if let Some(reason) = capture_rejection {
            Err(anyhow!(reason))
        } else {
            let execution = self.execute_stream_query(
                &mut session.conn,
                edit_sql.as_deref().unwrap_or(&sql),
                &mut sink,
                &mut owned_readonly,
            );
            tokio::pin!(execution);
            tokio::select! {
                biased;
                _=control.cancelled()=>{
                    if retire_on_stop {
                        Err(anyhow!("Query session cancellation requested"))
                    } else {
                    {let _gate=self.inner.cancel_gate.lock().await;let _=self.cancel_backend().await;}
                    match tokio::time::timeout(Duration::from_secs(10),&mut execution).await {
                        Ok(result)=>result,
                        Err(_)=>Err(anyhow!("Query did not finish within cancellation cleanup timeout")),
                    }
                    }
                },
                result=&mut execution=>result,
            }
        };
        // Rewritten editing SQL and read-only split execution have different
        // source coordinates. Do not invent a mapping for those paths.
        let error_position = if edit_sql.is_none()
            && (!self.inner.read_only || self.engine() == Engine::Sqlite)
            && control.reason().is_none()
        {
            outcome
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<crate::execution::LocatedError>())
                .map(|e| e.position)
        } else {
            None
        };
        timings.execution_ms = execution_started.elapsed().as_millis() as u64;
        if let Some(source) = edit_source.as_mut() {
            source.bind_native(adapters::source::take_execution_evidence(&mut session.conn));
        }
        let cleanup_started = Instant::now();
        let cancelled =
            control.is_cancelled() || self.inner.cancel_requested.load(Ordering::Acquire);
        let mut reusable = !matches!(session.conn, Conn::Closed);
        // Capture already rolls back only its own transaction on rejection.
        // Do not retire or cancel/drain an undispatched guard rejection: those
        // operations could interrupt or abort the caller's existing transaction.
        let retired = retire_on_stop
            && (cancelled
                || (!capture_rejected && (outcome.is_err() || control.reason().is_some())));
        if retired {
            // Do not resume/drain a raw MySQL batch with KILL QUERY: standalone
            // SLEEP can return success and let its following INSERT execute.
            // Keep ownership until termination completes, then permanently close
            // the session before any queued operation can acquire it.
            let _gate = self.inner.cancel_gate.lock().await;
            if !matches!(session.conn, Conn::Closed) {
                retirement_error = self.cancel_query_session().await.err();
                close_broken_connection(&mut session.conn).await;
            }
            self.inner.running.store(false, Ordering::Release);
            reusable = false;
            edit_transaction = false;
        } else if !capture_rejected && (outcome.is_err() || cancelled || owned_readonly) {
            // Execution can stop at a batch/row bound or with its receiver gone.
            // Cancel independently, then let SQLx consume pending protocol replies
            // before this mutex can be handed to another command.
            {
                let _gate = self.inner.cancel_gate.lock().await;
                let _ = self.cancel_backend().await;
            }
            let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
                drain_connection(&mut session.conn).await?;
                if owned_readonly || edit_transaction {
                    end_owned_transaction(&mut session.conn, false).await?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
            if !matches!(cleanup, Ok(Ok(()))) {
                reusable = false;
                close_broken_connection(&mut session.conn).await;
            }
            edit_transaction = false;
        }
        if edit_transaction {
            // Retain the source relation/metadata lock through execution and
            // verification. Only this successful supported SELECT owns the txn.
            let _gate = self.inner.cancel_gate.lock().await;
            if let Some(source) = edit_source.as_mut() {
                source.revalidate_after_execution(&mut session.conn).await;
            }
            if let Err(error) = end_owned_transaction(&mut session.conn, true).await {
                outcome = Err(error.context("Finalizing query source capture"));
                reusable = false;
                close_broken_connection(&mut session.conn).await;
            }
        }
        let retirement_message = if let Some(error) = retirement_error {
            format!("session retired; backend cancellation could not be confirmed: {error:#}")
        } else if retired {
            "session retired after query cancellation".to_owned()
        } else {
            "connection closed after unsuccessful cleanup".to_owned()
        };
        let event = if let Some(reason) = control.reason() {
            QueryEvent::Error {
                message: if reusable {
                    reason
                } else {
                    format!("{reason}; {retirement_message}")
                },
                cancelled,
            }
        } else {
            match outcome {
                Ok(()) if !cancelled => QueryEvent::Complete {
                    result_count: sink.result_count,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                },
                Ok(()) => QueryEvent::Error {
                    message: "Query cancelled".into(),
                    cancelled: true,
                },
                Err(error) => QueryEvent::Error {
                    message: if reusable {
                        format!("{error:#}")
                    } else {
                        format!("{error:#}; {retirement_message}")
                    },
                    cancelled,
                },
            }
        };
        if let Some(source) = edit_source.as_mut()
            && !matches!(
                event,
                QueryEvent::Complete {
                    result_count: 1,
                    ..
                }
            )
        {
            *source =
                QueryEditSource::unavailable("Editing requires one successfully completed result");
        }
        if let Some(source) = edit_source.as_mut() {
            source.bind_execution(sink.finish_edit_snapshot(&event));
        }
        *self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex") = None;
        drop(running);
        session.armed = false;
        drop(session);
        timings.cleanup_ms = cleanup_started.elapsed().as_millis() as u64;
        timings.total_ms = started.elapsed().as_millis() as u64;
        *timing_slot.lock().expect("query timing mutex") = Some(timings);
        // Terminal status bypasses the bounded data channel, so a stopped reader
        // cannot deadlock cleanup or publication of completion.
        drop(sink);
        let error_position = if matches!(
            event,
            QueryEvent::Error {
                cancelled: false,
                ..
            }
        ) && control.reason().is_none()
        {
            error_position
        } else {
            None
        };
        let _ = done.send(Completion {
            event,
            reusable,
            error_position,
        });
        if let Some(capture) = edit_capture {
            let _ =
                capture.completion.send(edit_source.unwrap_or_else(|| {
                    QueryEditSource::unavailable("Query edit source unavailable")
                }));
        }
    }

    async fn execute_stream_query(
        &self,
        conn: &mut Conn,
        sql: &str,
        sink: &mut EventSink,
        owned_readonly: &mut bool,
    ) -> Result<()> {
        sink.control.check()?;
        if self.inner.read_only && !adapters::native_readonly_authorization(conn) {
            self.readonly_stream(conn, sql, sink, owned_readonly).await
        } else {
            raw_stream(conn, sql, sink, None)
                .await
                .context("Executing SQL")
        }
    }

    /// Actual server transaction state, including PostgreSQL's failed transaction
    /// state. Does not commit, rollback, or depend on parsing previous editor SQL.
    pub async fn in_transaction(&self) -> Result<bool> {
        let mut session = self.inner.session.lock().await;
        let _running = self.running().await;
        server_in_transaction(&mut session.conn).await
    }

    async fn readonly_stream(
        &self,
        conn: &mut Conn,
        sql: &str,
        sink: &mut EventSink,
        owned_readonly: &mut bool,
    ) -> Result<()> {
        // Validate the ENTIRE batch first, so a later forbidden control cannot
        // leave an earlier statement or explicit transaction partially executed.
        let statements = readonly::statements(self.engine(), sql, self.inner.lex_mode)?;
        ensure!(
            statements.len() <= MAX_RESULTS,
            "More than {MAX_RESULTS} statements; split the batch"
        );
        for statement in statements {
            {
                let _gate = self.inner.cancel_gate.lock().await;
                ensure!(
                    !self.inner.cancel_requested.load(Ordering::Acquire),
                    "Query cancelled"
                );
            }
            sink.control.check()?;
            match statement.action {
                Action::Begin => {
                    ensure!(
                        !server_in_transaction(conn).await?,
                        "A transaction is already active"
                    );
                    start_readonly(conn).await?;
                    sink.end(0).await?;
                }
                Action::Commit { chain } | Action::Rollback { chain } => {
                    let command = if matches!(statement.action, Action::Commit { .. }) {
                        "COMMIT"
                    } else {
                        "ROLLBACK"
                    };
                    raw_stream(conn, command, sink, None).await?;
                    restore_readonly_default(conn).await?;
                    if chain {
                        start_readonly(conn).await?;
                    }
                }
                Action::Savepoint => {
                    ensure!(
                        server_in_transaction(conn).await?,
                        "Savepoints require an explicit transaction"
                    );
                    // Policy emits a canonical control plus one quoted name;
                    // original comments/tokens are never forwarded here. MySQL
                    // does not support SAVEPOINT in the prepared protocol.
                    raw_stream(conn, &statement.sql, sink, None).await?;
                }
                Action::Read => {
                    let owned = !server_in_transaction(conn).await?;
                    if owned {
                        *owned_readonly = true;
                        start_readonly(conn).await?;
                    }
                    // A callable routine might change session parsing settings.
                    // Match the lexical policy's captured dialect before every
                    // separately executed statement, including within a batch.
                    let result: Result<()> = async {
                        adapters::restore_lexical_mode(conn, &self.inner.mysql_sql_mode).await?;
                        // The server must agree this is exactly one statement.
                        // Parse/prepare rejects hidden batches WITHOUT executing
                        // their leading SELECT, COMMIT, or any other command.
                        let metadata = single_statement_metadata(conn, &statement.sql).await?;
                        raw_stream(conn, &statement.sql, sink, Some(metadata))
                            .await
                            .context("Executing read-only SQL")
                    }
                    .await;
                    // On error/cancel, the worker cancels/drains before rollback.
                    // Keep the ownership flag live until that cleanup completes.
                    if result.is_ok() && owned && sink.control.reason().is_none() {
                        execute(conn, "ROLLBACK")
                            .await
                            .context("Closing read-only operation transaction")?;
                        *owned_readonly = false;
                    }
                    ensure!(
                        !self.inner.cancel_requested.load(Ordering::Acquire),
                        "Query cancelled"
                    );
                    result?;
                }
            }
        }
        Ok(())
    }

    pub async fn begin(&self) -> Result<()> {
        let mut s = self.inner.session.lock().await;
        let _running = self.running().await;
        ensure!(
            !server_in_transaction(&mut s.conn).await?,
            "A transaction is already active"
        );
        if self.inner.read_only {
            start_readonly(&mut s.conn).await?;
        } else {
            execute(&mut s.conn, "START TRANSACTION")
                .await
                .context("Beginning transaction")?;
        }
        Ok(())
    }
    pub async fn commit(&self) -> Result<()> {
        self.finish_transaction("COMMIT").await
    }
    pub async fn rollback(&self) -> Result<()> {
        self.finish_transaction("ROLLBACK").await
    }
    async fn finish_transaction(&self, command: &str) -> Result<()> {
        let mut s = self.inner.session.lock().await;
        let _running = self.running().await;
        execute(&mut s.conn, command)
            .await
            .with_context(|| format!("{command} transaction"))?;
        if self.inner.read_only {
            restore_readonly_default(&mut s.conn).await?;
        }
        Ok(())
    }

    /// Does not acquire the busy session mutex. Uses the same login on a separate connection.
    pub async fn cancel(&self) -> Result<()> {
        let _gate = self.inner.cancel_gate.lock().await;
        {
            let sampling = self
                .inner
                .sampling_control
                .lock()
                .expect("sampling control mutex");
            if let Some(control) = sampling.as_ref().and_then(Weak::upgrade) {
                // The detached sampling worker alone decides when backend
                // cancellation is safe. In preflight it remembers this request
                // without interrupting a probe in an existing caller transaction.
                control.stop("Sampling cancelled");
                return Ok(());
            }
        }
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(());
        }
        let control = self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex")
            .as_ref()
            .and_then(Weak::upgrade);
        // Import/admin workers check cancellation and enter COMMIT under this same gate.
        // Once entered, await the real commit outcome instead of cancelling it
        // and falsely reporting a rollback after a successful commit.
        if control
            .as_ref()
            .is_some_and(|control| control.is_committing())
        {
            return Ok(());
        }
        self.inner.cancel_requested.store(true, Ordering::Release);
        if let Some(control) = control {
            control.stop("Query cancelled");
            if control.retires_session() {
                // The operation-owning worker performs termination while it
                // still owns the lease; never precede it with KILL QUERY.
                return Ok(());
            }
        }
        self.cancel_backend().await
    }

    async fn cancel_backend(&self) -> Result<()> {
        adapters::cancel(
            &self.inner.options,
            self.inner.backend_id,
            &self.inner.running,
        )
        .await
    }

    async fn cancel_query_session(&self) -> Result<()> {
        adapters::cancel_query_session(
            &self.inner.options,
            self.inner.backend_id,
            &self.inner.running,
            true,
        )
        .await
    }

    pub async fn catalog(&self) -> Result<Vec<TableInfo>> {
        let mut s = self.inner.session.lock().await;
        let _running = self.running().await;
        adapters::catalog::catalog(&mut s.conn, self.engine()).await
    }

    pub async fn structure(&self, table: &TableRef) -> Result<Structure> {
        let mut s = self.inner.session.lock().await;
        let _running = self.running().await;
        structure(&mut s.conn, self.engine(), table)
            .await
            .with_context(|| format!("Inspecting {}", qualified_table(self.engine(), table)))
    }

    pub async fn relationships(&self, table: &TableRef) -> Result<Vec<ForeignKeyRelationship>> {
        let mut session = self.inner.session.lock().await;
        let _running = self.running().await;
        adapters::catalog::relationships(&mut session.conn, self.engine(), table).await
    }

    pub async fn browse(&self, table: &TableRef, request: &BrowseRequest) -> Result<QueryResult> {
        self.browse_filtered(table, request, None).await
    }

    pub async fn browse_filtered(
        &self,
        table: &TableRef,
        request: &BrowseRequest,
        expression: Option<&FilterExpr>,
    ) -> Result<QueryResult> {
        crate::filters::validate_filters(&request.filters, expression)?;
        let mut lease = query_edit::EditLease {
            session: self.inner.session.lock().await,
            armed: true,
        };
        let _running = self.running().await;
        let result = adapters::catalog::browse(
            &mut lease.session.conn,
            self.engine(),
            table,
            request,
            expression,
        )
        .await;
        lease.armed = !adapters::catalog::operation_reusable(&lease.session.conn);
        result
    }

    pub async fn import_rows(
        &self,
        table: &TableRef,
        options: RowImportOptions,
    ) -> Result<RowImport> {
        ensure!(!self.inner.read_only, "This connection is read-only");
        options.validate()?;
        let (tx, rx) = tokio::sync::mpsc::channel(options.channel_capacity);
        let (done, completion) = tokio::sync::watch::channel(None);
        let (ready, started) = tokio::sync::oneshot::channel();
        let control = StreamControl::new();
        let handle = RowImport::new(tx, control.clone(), completion, options.clone());
        let db = self.clone();
        let table = table.clone();
        tokio::spawn(async move {
            db.import_worker(table, options, control, rx, ready, done)
                .await;
        });
        started
            .await
            .context("Import worker stopped during preflight")?
            .map_err(|message| anyhow!(message))?;
        Ok(handle)
    }

    async fn import_worker(
        &self,
        table: TableRef,
        options: RowImportOptions,
        control: Arc<StreamControl>,
        mut rx: tokio::sync::mpsc::Receiver<ImportCommand>,
        ready: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
        done: tokio::sync::watch::Sender<Option<ImportCompletion>>,
    ) {
        let mut ready = Some(ready);
        let session = tokio::select! {
            biased;
            _=control.cancelled()=>{
                let message=control.reason().unwrap_or_else(||"Import cancelled".into());
                let _=ready.take().expect("ready sender").send(Err(message.clone()));
                done.send_replace(Some(ImportCompletion{result:Err(message),reusable:true}));return;
            },
            session=self.inner.session.lock()=>session,
        };
        let mut lease = ImportLease {
            session,
            armed: true,
        };
        let session = &mut lease.session;
        let running = self.running().await;
        {
            let _gate = self.inner.cancel_gate.lock().await;
            *self
                .inner
                .active_stream
                .lock()
                .expect("stream control mutex") = Some(Arc::downgrade(&control));
            // Remember a cancellation between marking the session running and
            // installing its producer token. Registration shares cancel's gate.
            if self.inner.cancel_requested.load(Ordering::Acquire) {
                control.stop("Import cancelled during startup");
            }
        }
        let mut owned = false;
        let mut commit_started = false;
        let outcome = {
            let execution = async {
                control.check()?;
                ensure!(
                    !server_in_transaction(&mut session.conn).await?,
                    "Atomic import requires no existing transaction (including failed transactions)"
                );
                ensure!(
                    !adapters::manual_commit(&mut session.conn).await?,
                    "Atomic import requires MySQL autocommit=1 before starting its owned transaction"
                );
                let target =
                    adapters::import::inspect(&mut session.conn, self.engine(), &table).await?;
                owned = true;
                adapters::import::begin(&mut session.conn)
                    .await
                    .context("Beginning exclusive atomic import")?;
                let columns =
                    adapters::import::lock_and_columns(&mut session.conn, self.engine(), &target)
                        .await?;
                control.check()?;
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
                let mut inserted = 0u64;
                loop {
                    let command = tokio::select! {
                        biased;
                        _=control.cancelled()=>{control.check()?;unreachable!("stop signal has a reason")},
                        command=tokio::time::timeout(Duration::from_millis(options.idle_timeout_ms),rx.recv())=>{
                            match command {
                                Ok(Some(command))=>command,
                                Ok(None)=>bail!("Import producer ended without explicit Finish; all rows rolled back"),
                                Err(_)=>bail!("Import producer idle timeout before Finish; all rows rolled back"),
                            }
                        },
                    };
                    match command {
                        ImportCommand::Rows(rows) => {
                            options.validate_batch(&rows)?;
                            for values in rows {
                                control.check()?;
                                let affected = adapters::import::insert(
                                    &mut session.conn,
                                    self.engine(),
                                    &target,
                                    &columns,
                                    values,
                                )
                                .await
                                .with_context(|| format!("Importing row {}", inserted + 1))?;
                                ensure!(
                                    affected == 1,
                                    "Import row {} affected {} rows, expected one",
                                    inserted + 1,
                                    affected
                                );
                                inserted = inserted
                                    .checked_add(1)
                                    .context("Import row count overflow")?;
                            }
                        }
                        ImportCommand::Finish => {
                            {
                                let _gate = self.inner.cancel_gate.lock().await;
                                control.begin_commit()?;
                                commit_started = true;
                            }
                            // Do not race COMMIT against cancellation. A successful
                            // response is always reported as committed, even if a
                            // late producer/Database cancellation requested rollback.
                            tokio::time::timeout(Duration::from_secs(30),execute(&mut session.conn,"COMMIT")).await
                                .context("Import COMMIT timed out; outcome is unknown, do not retry blindly")?
                                .context("Import COMMIT failed; outcome may be unknown if the connection was lost")?;
                            owned = false;
                            return Ok::<u64, anyhow::Error>(inserted);
                        }
                    }
                }
            };
            tokio::pin!(execution);
            tokio::select! {
                biased;
                _=control.cancelled()=>{
                    {let _gate=self.inner.cancel_gate.lock().await;let _=self.cancel_backend().await;}
                    match tokio::time::timeout(Duration::from_secs(10),&mut execution).await {
                        Ok(result)=>result,
                        Err(_)=>Err(anyhow!("Import cancellation cleanup timed out")),
                    }
                },
                result=&mut execution=>result,
            }
        };
        let mut reusable = !matches!(session.conn, Conn::Closed);
        if let Err(error) = &outcome
            && !commit_started
        {
            control.fail(&format!("{error:#}"));
        }
        if owned || outcome.is_err() {
            {
                let _gate = self.inner.cancel_gate.lock().await;
                if owned || control.is_cancelled() {
                    let _ = self.cancel_backend().await;
                }
                // Cleanup belongs to this worker. Repeated cancellation must
                // not interrupt rollback or race the next session user.
                self.inner.running.store(false, Ordering::Release);
            }
            let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
                drain_connection(&mut session.conn).await?;
                if owned {
                    adapters::import::rollback(&mut session.conn)
                        .await
                        .context("Rolling back atomic import")?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
            if !matches!(cleanup, Ok(Ok(()))) {
                reusable = false;
                close_broken_connection(&mut session.conn).await;
            }
        }
        let result=outcome.map_err(|error|if reusable{format!("{error:#}")}else{format!("{error:#}; connection closed after failed cleanup; transaction outcome may be unknown")});
        if let Some(ready) = ready.take() {
            let _ = ready.send(Err(result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "Import stopped during preflight".into())));
        }
        *self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex") = None;
        drop(running);
        lease.armed = false;
        drop(lease);
        drop(rx);
        done.send_replace(Some(ImportCompletion { result, reusable }));
    }

    pub async fn apply_changes(&self, table: &TableRef, changes: &[RowChange]) -> Result<u64> {
        ensure!(!self.inner.read_only, "This connection is read-only");
        if changes.is_empty() {
            return Ok(0);
        }
        let mut s = self.inner.session.lock().await;
        let _running = self.running().await;
        if adapters::source::requires_native_originals(&s.conn) {
            let mut lease = query_edit::EditLease {
                session: s,
                armed: true,
            };
            let result = adapters::source::apply_native(
                &mut lease.session.conn,
                table,
                changes,
                None,
                adapters::EditCancellation {
                    requested: &self.inner.cancel_requested,
                    gate: &self.inner.cancel_gate,
                },
            )
            .await;
            lease.armed = !adapters::catalog::operation_reusable(&lease.session.conn);
            return result;
        }
        let structure = structure(&mut s.conn, self.engine(), table).await?;
        let statements = change_statements(self.engine(), table, &structure.columns, changes)?;
        adapters::require_transactional_table(&mut s.conn, table).await?;
        let savepoint = format!("rowboat_edit_{}", uuid::Uuid::new_v4().simple());
        // Consult the server, not SQL-text bookkeeping: arbitrary SQL may have
        // entered a transaction or caused a MySQL implicit commit.
        let mut outer = probe_transaction(&mut s.conn, &savepoint).await?;
        if !outer {
            let manual_commit = adapters::manual_commit(&mut s.conn).await?;
            execute(&mut s.conn, "START TRANSACTION")
                .await
                .context("Starting atomic edit batch")?;
            if manual_commit {
                execute(&mut s.conn, &format!("SAVEPOINT {savepoint}")).await?;
                outer = true;
            }
        }
        let operation: Result<u64> = async {
            let mut affected = 0;
            for (index, (sql, params)) in statements.iter().enumerate() {
                let r = bound_results(&mut s.conn, sql, params, 0).await.with_context(|| format!("Applying row change {}", index+1))?;
                {
                    let _gate = self.inner.cancel_gate.lock().await;
                    ensure!(!self.inner.cancel_requested.load(Ordering::Acquire), "Edit batch cancelled");
                }
                ensure!(r.rows_affected == 1, "Conflict in row change {}: expected exactly one matching row, got {}. Reload the row before retrying.", index+1, r.rows_affected);
                affected += r.rows_affected;
            }
            Ok(affected)
        }.await;
        match operation {
            Ok(count) => {
                let end = if outer {
                    format!("RELEASE SAVEPOINT {savepoint}")
                } else {
                    "COMMIT".into()
                };
                if let Err(error) = execute(&mut s.conn, &end).await {
                    let rollback = if outer {
                        format!("ROLLBACK TO SAVEPOINT {savepoint}")
                    } else {
                        "ROLLBACK".into()
                    };
                    let _ = execute(&mut s.conn, &rollback).await;
                    return Err(error.context("Finalizing edit batch; commit outcome may be unknown if the connection was lost"));
                }
                Ok(count)
            }
            Err(error) => {
                // Wait for KILL/cancel to finish before issuing the cleanup rollback.
                let _gate = self.inner.cancel_gate.lock().await;
                let rollback = if outer {
                    format!("ROLLBACK TO SAVEPOINT {savepoint}")
                } else {
                    "ROLLBACK".into()
                };
                execute(&mut s.conn, &rollback).await.context(
                    "Edit failed and rollback also failed; session transaction state is uncertain",
                )?;
                if outer {
                    execute(&mut s.conn, &format!("RELEASE SAVEPOINT {savepoint}")).await?;
                }
                Err(error)
            }
        }
    }
}

/// End only an operation-owned transaction, never inherit MySQL's session
/// completion_type (CHAIN or RELEASE), and leave that user setting untouched.
async fn end_owned_transaction(conn: &mut Conn, commit: bool) -> Result<()> {
    let command = adapters::transaction_end(conn, commit);
    execute(conn, command).await
}

/// Native prepared protocols accept a single server-parsed statement. Do not
/// fall back to raw execution when preparation fails: this is an independent
/// barrier against tokenizer discrepancies in read-only editor input.
#[cfg(test)]
async fn validate_single_statement(conn: &mut Conn, sql: &str) -> Result<()> {
    single_statement_metadata(conn, sql).await.map(|_| ())
}

async fn start_readonly(conn: &mut Conn) -> Result<()> {
    restore_readonly_default(conn).await?;
    execute(conn, "START TRANSACTION READ ONLY")
        .await
        .context("Starting server read-only transaction")
}

// Metadata fields have known textual semantics. MySQL marks several
// information_schema strings BINARY despite returning UTF-8 text.
fn text(cell: &Cell) -> String {
    match cell {
        Cell::Text(s) => s.clone(),
        Cell::Null => String::new(),
        Cell::Binary(b) => String::from_utf8(b.clone()).unwrap_or_else(|_| hex::encode(b)),
    }
}
fn optional_text(cell: &Cell) -> Option<String> {
    if *cell == Cell::Null {
        None
    } else {
        Some(text(cell))
    }
}
fn truth(cell: &Cell) -> bool {
    matches!(
        text(cell).to_ascii_lowercase().as_str(),
        "t" | "true" | "1" | "yes"
    )
}

fn retain(result: &mut QueryResult, row: Vec<Cell>, limit: usize, bytes: &mut usize) {
    let size: usize = std::mem::size_of::<Vec<Cell>>()
        + row
            .iter()
            .map(|v| {
                std::mem::size_of::<Cell>()
                    + match v {
                        Cell::Null => 0,
                        Cell::Text(t) => t.len(),
                        Cell::Binary(b) => b.len(),
                    }
            })
            .sum::<usize>();
    if result.rows.len() < limit.min(MAX_ROWS) && bytes.saturating_add(size) <= MAX_BYTES {
        *bytes += size;
        result.rows.push(row);
    } else {
        result.truncated = true;
    }
}

fn column<'a>(columns: &'a [Column], name: &str) -> Result<&'a Column> {
    columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("Unknown column: {name}"))
}

fn browse_sql(
    engine: Engine,
    table: &TableRef,
    columns: &[Column],
    request: &BrowseRequest,
) -> Result<(String, Vec<Cell>, usize)> {
    browse_sql_filtered(engine, table, columns, request, None)
}

fn browse_sql_filtered(
    engine: Engine,
    table: &TableRef,
    columns: &[Column],
    request: &BrowseRequest,
    expression: Option<&FilterExpr>,
) -> Result<(String, Vec<Cell>, usize)> {
    let mut params = Vec::new();
    // Text projections give PostgreSQL's canonical display/input form even for
    // extension types, ranges, arrays, intervals, infinity dates and composites.
    let projection = columns
        .iter()
        .map(|c| dialect::browse_projection(engine, c))
        .collect::<Vec<_>>()
        .join(", ");
    let source = quote_ident(engine, "__rowboat_source");
    let mut sql = format!(
        "SELECT {projection} FROM {} AS {source}",
        qualified_table(engine, table)
    );
    let predicates = crate::filters::compile_filters(
        engine,
        columns,
        &request.filters,
        expression,
        &mut params,
    )?;
    if !predicates.is_empty() {
        sql.push_str(&format!(" WHERE {}", predicates.join(" AND ")));
    }
    if let Some((name, ascending)) = &request.sort {
        column(columns, name)?;
        sql.push_str(&format!(
            " ORDER BY {source}.{} {}",
            quote_ident(engine, name),
            if *ascending { "ASC" } else { "DESC" }
        ));
    } else {
        let keys = columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| format!("{source}.{}", quote_ident(engine, &c.name)))
            .collect::<Vec<_>>();
        if !keys.is_empty() {
            sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
    }
    let limit = if request.limit == 0 {
        100
    } else {
        (request.limit as usize).min(MAX_ROWS)
    };
    // These are integer fields, never strings. Fetch one extra row for a true truncation flag.
    sql.push_str(&format!(" LIMIT {} OFFSET {}", limit + 1, request.offset));
    Ok((sql, params, limit))
}

type Statement = (String, Vec<Cell>);
fn change_statements(
    engine: Engine,
    table: &TableRef,
    columns: &[Column],
    changes: &[RowChange],
) -> Result<Vec<Statement>> {
    let table = qualified_table(engine, table);
    let check_values = |values: &[(String, Cell)]| -> Result<()> {
        let mut seen = HashSet::new();
        for (name, _) in values {
            column(columns, name)?;
            ensure!(seen.insert(name), "Duplicate column: {name}");
        }
        Ok(())
    };
    changes
        .iter()
        .map(|change| {
            let mut params = Vec::new();
            let predicate =
                |original: &[(String, Cell)], params: &mut Vec<Cell>| -> Result<String> {
                    check_values(original)?;
                    let keys = columns.iter().filter(|c| c.primary_key).collect::<Vec<_>>();
                    ensure!(
                        !keys.is_empty(),
                        "Updates and deletes require a primary key"
                    );
                    for key in keys {
                        ensure!(
                            original
                                .iter()
                                .any(|(name, value)| name == &key.name && *value != Cell::Null),
                            "Original row must contain non-NULL primary key column {}",
                            key.name
                        );
                    }
                    ensure!(!original.is_empty(), "Original row required");
                    let mut parts = Vec::new();
                    for (name, value) in original {
                        let col = column(columns, name)?;
                        let name = quote_ident(engine, name);
                        if *value == Cell::Null {
                            parts.push(format!("{name} IS NULL"));
                            continue;
                        }
                        if col.primary_key {
                            parts.push(format!(
                                "{name} = {}",
                                parameter(engine, col, value, params)
                            ));
                        }
                        // Compare canonical original text byte-for-byte, not a case-insensitive
                        // collation or JSON equality operator that may not exist.
                        params.push(value.clone());
                        parts.push(dialect::original_predicate(
                            engine,
                            &name,
                            value,
                            params.len(),
                        ));
                    }
                    Ok(parts.join(" AND "))
                };
            let sql = match change {
                RowChange::Insert { values } => {
                    check_values(values)?;
                    if values.is_empty() {
                        dialect::default_insert(engine, &table)
                    } else {
                        let names = values
                            .iter()
                            .map(|(n, _)| quote_ident(engine, n))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let placeholders = values
                            .iter()
                            .map(|(n, v)| {
                                parameter(
                                    engine,
                                    column(columns, n).expect("validated column"),
                                    v,
                                    &mut params,
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("INSERT INTO {table} ({names}) VALUES ({placeholders})")
                    }
                }
                RowChange::Update { original, values } => {
                    ensure!(!values.is_empty(), "Update has no changed values");
                    check_values(values)?;
                    let assignments = values
                        .iter()
                        .map(|(n, v)| {
                            format!(
                                "{} = {}",
                                quote_ident(engine, n),
                                parameter(
                                    engine,
                                    column(columns, n).expect("validated column"),
                                    v,
                                    &mut params
                                )
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "UPDATE {table} SET {assignments} WHERE {}",
                        predicate(original, &mut params)?
                    )
                }
                RowChange::Delete { original } => format!(
                    "DELETE FROM {table} WHERE {}",
                    predicate(original, &mut params)?
                ),
            };
            Ok((sql, params))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Requires explicit task-owned Podman fixtures on ports 15435/13309"]
    async fn native_single_statement_guard_never_executes_hidden_commit() -> Result<()> {
        for engine in [Engine::Postgres, Engine::MySql] {
            let profile = ConnectionProfile {
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
                    "rowboat"
                }
                .into(),
                ssl_mode: "disable".into(),
                read_only: true,
                ..Default::default()
            };
            let db = Database::connect(&profile, "rowboat-test").await?;
            db.begin().await?;
            {
                let mut session = db.inner.session.lock().await;
                assert!(
                    validate_single_statement(&mut session.conn, "SELECT 1; COMMIT; SELECT 2")
                        .await
                        .is_err()
                );
            }
            assert!(
                db.in_transaction().await?,
                "native preparation must not execute COMMIT"
            );
            db.rollback().await?;
            assert_eq!(
                db.query("SELECT 1", 10).await?[0].rows,
                vec![vec![Cell::Text("1".into())]]
            );
        }
        Ok(())
    }
    fn table() -> TableRef {
        TableRef {
            schema: "public".into(),
            name: "odd\"table".into(),
        }
    }
    fn cols() -> Vec<Column> {
        [
            ("id", "integer", true),
            ("tenant", "uuid", true),
            ("name", "text", false),
            ("payload", "json", false),
            ("bytes", "bytea", false),
        ]
        .into_iter()
        .map(|(n, t, p)| Column {
            name: n.into(),
            data_type: t.into(),
            nullable: !p,
            primary_key: p,
            default_value: None,
        })
        .collect()
    }
    #[test]
    fn database_is_clone_send_sync() {
        fn check<T: Clone + Send + Sync>() {}
        check::<Database>();
    }
    #[test]
    fn public_futures_are_send() {
        fn send<T: Send>(_: T) {}
        fn check(
            db: &Database,
            p: &ConnectionProfile,
            t: &TableRef,
            r: &BrowseRequest,
            c: &[RowChange],
        ) {
            send(Database::connect(p, ""));
            send(db.query("SELECT 1", 1));
            send(db.cancel());
            send(db.in_transaction());
            send(db.catalog());
            send(db.structure(t));
            send(db.browse(t, r));
            send(db.apply_changes(t, c));
            send(db.begin());
            send(db.commit());
            send(db.rollback());
        }
        let _ = check;
    }
    #[test]
    fn edits_require_complete_original_primary_key() {
        let incomplete = RowChange::Delete {
            original: vec![("id".into(), Cell::Text("1".into()))],
        };
        assert!(
            change_statements(Engine::Postgres, &table(), &cols(), &[incomplete])
                .unwrap_err()
                .to_string()
                .contains("tenant")
        );
        let mut no_pk = cols();
        for c in &mut no_pk {
            c.primary_key = false;
        }
        assert!(
            change_statements(
                Engine::MySql,
                &table(),
                &no_pk,
                &[RowChange::Delete { original: vec![] }]
            )
            .is_err()
        );
    }
    #[test]
    fn updates_bind_typed_values_and_compare_originals() {
        let original = vec![
            ("id".into(), Cell::Text("1".into())),
            (
                "tenant".into(),
                Cell::Text("d9355bb0-7885-42d2-a723-0f957a6ac1ab".into()),
            ),
            ("payload".into(), Cell::Text("{\"x\":1}".into())),
            ("name".into(), Cell::Null),
        ];
        let change = RowChange::Update {
            original,
            values: vec![("name".into(), Cell::Text("'; DROP TABLE x; --".into()))],
        };
        let result = change_statements(Engine::Postgres, &table(), &cols(), &[change]).unwrap();
        assert!(!result[0].0.contains("DROP"));
        assert!(result[0].0.contains("CAST($1::text AS text)"));
        assert!(
            result[0]
                .0
                .contains("\"payload\"::text COLLATE pg_catalog.\"C\" IS NOT DISTINCT FROM")
        );
        assert!(result[0].0.contains("\"name\" IS NULL"));
        assert_eq!(result[0].1.len(), 6);
    }
    #[test]
    fn browse_validates_and_escapes_literal_patterns() {
        let mut request = BrowseRequest {
            filters: vec![Filter {
                column: "name".into(),
                op: FilterOp::Contains,
                value: Cell::Text("a%_!b'".into()),
            }],
            limit: 2,
            ..Default::default()
        };
        let (sql, p, limit) = browse_sql(Engine::Postgres, &table(), &cols(), &request).unwrap();
        assert!(sql.contains("LIKE $1::text ESCAPE '!'"));
        assert!(sql.ends_with("LIMIT 3 OFFSET 0"));
        assert_eq!(p, vec![Cell::Text("%a!%!_!!b'%".into())]);
        assert_eq!(limit, 2);
        request.sort = Some(("injected; drop".into(), true));
        assert!(browse_sql(Engine::MySql, &table(), &cols(), &request).is_err());
    }
    #[test]
    fn bounded_rows_preserve_null_empty_and_binary() {
        let mut result = QueryResult::default();
        let mut bytes = 0;
        retain(
            &mut result,
            vec![Cell::Null, Cell::Text(String::new()), Cell::Binary(vec![])],
            1,
            &mut bytes,
        );
        retain(
            &mut result,
            vec![Cell::Text("overflow".into())],
            1,
            &mut bytes,
        );
        assert_eq!(result.rows.len(), 1);
        assert!(result.truncated);
        assert_eq!(result.rows[0].len(), 3);
    }
}
