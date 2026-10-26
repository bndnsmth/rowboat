//! Exclusive, cancellation-safe execution of trusted reviewed administrative SQL.
//! The detached worker owns the physical-session mutex from preflight through
//! cleanup and terminal observation. Dropping a producer never releases a live
//! transaction to ordinary Database methods. This is not an arbitrary-SQL editor.
use super::*;
use serde::Serialize;
use tokio::sync::{MutexGuard, mpsc, oneshot, watch};

/// Read-only catalog-query surface shared by Database and its exclusive lease.
/// Implementations must not acquire a second physical-session lock recursively.
pub trait QuerySession: Send + Sync {
    fn sqlite_schema_identity(
        &self,
        _schema: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Cell>>> + Send {
        async { bail!("SQLite schema identity is unsupported by this session") }
    }
    fn completion_catalog(
        &self,
        _extra_namespaces: &[String],
    ) -> impl std::future::Future<Output = Result<CompletionCatalog>> + Send {
        async { bail!("Typed completion catalog is unsupported by this session") }
    }
    fn completion_column_names(
        &self,
        _table: &TableRef,
    ) -> impl std::future::Future<Output = Result<Vec<String>>> + Send {
        async { bail!("Typed completion columns are unsupported by this session") }
    }
    fn engine(&self) -> Engine;
    fn query(
        &self,
        sql: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<QueryResult>>> + Send;
}
impl QuerySession for Database {
    async fn sqlite_schema_identity(&self, schema: &str) -> Result<Vec<Cell>> {
        let mut session = self.inner.session.lock().await;
        adapters::administrative::sqlite_identity(&mut session.conn, schema).await
    }
    async fn completion_catalog(&self, extra: &[String]) -> Result<CompletionCatalog> {
        Database::completion_catalog(self, extra).await
    }
    async fn completion_column_names(&self, table: &TableRef) -> Result<Vec<String>> {
        Database::completion_column_names(self, table).await
    }
    fn engine(&self) -> Engine {
        Database::engine(self)
    }
    async fn query(&self, sql: &str, limit: usize) -> Result<Vec<QueryResult>> {
        Database::query(self, sql, limit).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AdministrativeStatus {
    Committed,
    CompletedNonAtomic,
    RolledBack,
    FailedNonAtomic,
    Rejected,
    OutcomeUnknown,
}
#[derive(Debug, Clone, Serialize)]
pub struct AdministrativeOutcome {
    pub status: AdministrativeStatus,
    /// Successfully acknowledged execute calls, excluding inspection queries.
    pub completed_steps: usize,
    pub failed_step: Option<usize>,
    pub rolled_back: bool,
    pub reusable: bool,
    pub error: Option<String>,
    /// Runs after COMMIT/ROLLBACK, but before exclusive session ownership ends.
    pub observation: Option<Vec<QueryResult>>,
    pub observation_error: Option<String>,
}
enum Command {
    SqliteIdentity {
        schema: String,
        reply: oneshot::Sender<Result<Vec<Cell>, String>>,
    },
    Catalog {
        extra: Vec<String>,
        reply: oneshot::Sender<Result<CompletionCatalog, String>>,
    },
    Columns {
        table: TableRef,
        reply: oneshot::Sender<Result<Vec<String>, String>>,
    },
    Query {
        sql: String,
        limit: usize,
        step: bool,
        reply: oneshot::Sender<std::result::Result<Vec<QueryResult>, String>>,
    },
    Finish,
}
#[derive(Clone)]
pub struct AdministrativeMonitor {
    completion: watch::Receiver<Option<AdministrativeOutcome>>,
}
impl AdministrativeMonitor {
    pub async fn wait(mut self) -> Result<AdministrativeOutcome> {
        loop {
            if let Some(done) = self.completion.borrow().clone() {
                return Ok(done);
            }
            self.completion.changed().await.context(
                "Administrative worker stopped unexpectedly; its physical session was quarantined",
            )?;
        }
    }
}
pub struct AdministrativeOperation {
    engine: Engine,
    tx: Option<mpsc::Sender<Command>>,
    control: Arc<StreamControl>,
    monitor: AdministrativeMonitor,
}
struct RequestGuard {
    control: Arc<StreamControl>,
    armed: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.control.stop("Administrative request future dropped");
        }
    }
}
impl AdministrativeOperation {
    pub fn monitor(&self) -> AdministrativeMonitor {
        self.monitor.clone()
    }
    pub fn cancel(&self) {
        self.control.stop("Administrative operation cancelled");
    }
    async fn request(&self, sql: &str, limit: usize, step: bool) -> Result<Vec<QueryResult>> {
        let mut guard = RequestGuard {
            control: self.control.clone(),
            armed: true,
        };
        self.control.check()?;
        let (reply, response) = oneshot::channel();
        let command = Command::Query {
            sql: sql.into(),
            limit: limit.min(MAX_ROWS),
            step,
            reply,
        };
        let tx = self
            .tx
            .as_ref()
            .context("Administrative operation closed")?;
        tokio::select! {biased; _=self.control.cancelled()=>{self.control.check()?;unreachable!()}, sent=tx.send(command)=>sent.context("Administrative worker has finished")?,}
        let result = response
            .await
            .context("Administrative worker stopped before answering request")?;
        guard.armed = false;
        result.map_err(|message| anyhow!(message))
    }
    /// Only trusted single administrative statements, never transaction controls.
    /// PostgreSQL and SQLite use native preparation to reject hidden batches.
    /// MySQL routines may contain body semicolons; callers must validate their
    /// intended-object grammar before supplying generated CREATE text.
    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        self.request(sql, 0, true).await?;
        Ok(())
    }
    pub async fn finish(mut self) -> Result<AdministrativeOutcome> {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Finish).await;
        }
        self.monitor.clone().wait().await
    }
    pub async fn close(mut self) -> Result<AdministrativeOutcome> {
        self.control
            .stop("Administrative operation closed without Finish");
        self.tx.take();
        self.monitor.clone().wait().await
    }
}
impl QuerySession for AdministrativeOperation {
    async fn sqlite_schema_identity(&self, schema: &str) -> Result<Vec<Cell>> {
        let mut guard = RequestGuard {
            control: self.control.clone(),
            armed: true,
        };
        self.control.check()?;
        let (reply, response) = oneshot::channel();
        self.tx
            .as_ref()
            .context("Administrative operation closed")?
            .send(Command::SqliteIdentity {
                schema: schema.into(),
                reply,
            })
            .await
            .context("Administrative worker finished")?;
        let result = response
            .await
            .context("Administrative identity worker stopped")?;
        guard.armed = false;
        result.map_err(anyhow::Error::msg)
    }
    async fn completion_catalog(&self, extra: &[String]) -> Result<CompletionCatalog> {
        let mut guard = RequestGuard {
            control: self.control.clone(),
            armed: true,
        };
        self.control.check()?;
        let (reply, response) = oneshot::channel();
        self.tx
            .as_ref()
            .context("Administrative operation closed")?
            .send(Command::Catalog {
                extra: extra.to_vec(),
                reply,
            })
            .await
            .context("Administrative worker finished")?;
        let result = response.await.context("Administrative worker stopped")?;
        guard.armed = false;
        result.map_err(anyhow::Error::msg)
    }
    async fn completion_column_names(&self, table: &TableRef) -> Result<Vec<String>> {
        let mut guard = RequestGuard {
            control: self.control.clone(),
            armed: true,
        };
        self.control.check()?;
        let (reply, response) = oneshot::channel();
        self.tx
            .as_ref()
            .context("Administrative operation closed")?
            .send(Command::Columns {
                table: table.clone(),
                reply,
            })
            .await
            .context("Administrative worker finished")?;
        let result = response.await.context("Administrative worker stopped")?;
        guard.armed = false;
        result.map_err(anyhow::Error::msg)
    }
    fn engine(&self) -> Engine {
        self.engine
    }
    async fn query(&self, sql: &str, limit: usize) -> Result<Vec<QueryResult>> {
        self.request(sql, limit, false).await
    }
}
impl Drop for AdministrativeOperation {
    fn drop(&mut self) {
        self.control
            .stop("Administrative operation handle dropped before Finish");
    }
}

/// Panic/runtime task-abort fallback. Dropping the socket closes the backend and
/// rolls back owned transactions rather than handing one to a queued waiter.
struct Lease<'a> {
    session: MutexGuard<'a, Session>,
    armed: bool,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.conn = Conn::Closed;
        }
    }
}

fn validate_sql(engine: Engine, mode: LexMode, sql: &str, step: bool) -> Result<()> {
    let head = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if step {
        ensure!(
            matches!(
                head.as_str(),
                "CREATE" | "DROP" | "ALTER" | "COMMENT" | "GRANT" | "REVOKE" | "REFRESH"
            ),
            "Exclusive administrative steps do not accept transaction controls or unclassified SQL"
        );
    } else {
        ensure!(
            matches!(head.as_str(), "SELECT" | "SHOW"),
            "Administrative inspection/observation requires SELECT or SHOW"
        );
        let statements = readonly::statements(engine, sql, mode)?;
        ensure!(
            statements.len() == 1 && statements[0].action == Action::Read,
            "Administrative inspection requires one read statement"
        );
    }
    Ok(())
}
impl Database {
    /// Acquire a whole-operation physical-session lease. Preconditions and native
    /// BEGIN run under the same lock used by query/commit/rollback/import. An
    /// existing caller transaction is rejected, never adopted or rolled back.
    /// The optional trusted SELECT/SHOW observation runs before the lock releases.
    pub async fn administrative_operation(
        &self,
        observation: Option<String>,
    ) -> Result<AdministrativeOperation> {
        ensure!(!self.inner.read_only, "This connection is read-only");
        if let Some(sql) = &observation {
            validate_sql(self.engine(), self.inner.lex_mode, sql, false)?;
        }
        let (tx, rx) = mpsc::channel(1);
        let (done, completion) = watch::channel(None);
        let (ready, started) = oneshot::channel();
        let control = StreamControl::new();
        let handle = AdministrativeOperation {
            engine: self.engine(),
            tx: Some(tx),
            control: control.clone(),
            monitor: AdministrativeMonitor { completion },
        };
        let db = self.clone();
        tokio::spawn(async move {
            db.administrative_worker(control, rx, ready, done, observation)
                .await;
        });
        started
            .await
            .context("Administrative worker stopped during preflight")?
            .map_err(|message| anyhow!(message))?;
        Ok(handle)
    }
    async fn administrative_worker(
        &self,
        control: Arc<StreamControl>,
        mut rx: mpsc::Receiver<Command>,
        ready: oneshot::Sender<std::result::Result<(), String>>,
        done: watch::Sender<Option<AdministrativeOutcome>>,
        observation_sql: Option<String>,
    ) {
        let mut ready = Some(ready);
        let session = tokio::select! {biased;
            _=control.cancelled()=>{let message=control.reason().unwrap_or_else(||"Administrative operation cancelled".into());let _=ready.take().unwrap().send(Err(message.clone()));done.send_replace(Some(AdministrativeOutcome{status:AdministrativeStatus::Rejected,completed_steps:0,failed_step:None,rolled_back:false,reusable:true,error:Some(message),observation:None,observation_error:None}));return;},
            session=self.inner.session.lock()=>session,
        };
        let mut lease = Lease {
            session,
            armed: true,
        };
        let running = self.running().await;
        *self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex") = Some(Arc::downgrade(&control));
        let (mut accepted, mut owned_transaction, mut commit_started, mut committed) =
            (false, false, false, false);
        let mut completed_steps = 0;
        let mut failed_step = None;
        let result = {
            let execution = async {
                control.check()?;
                ensure!(
                    !server_in_transaction(&mut lease.session.conn).await?,
                    "Administrative operation requires no existing transaction, including failed transactions"
                );
                ensure!(
                    !adapters::manual_commit(&mut lease.session.conn).await?,
                    "Non-atomic administration requires MySQL autocommit=1"
                );
                control.check()?;
                accepted = true;
                if adapters::administrative::atomic(self.engine()) {
                    owned_transaction = true;
                    adapters::administrative::begin(&mut lease.session.conn)
                        .await
                        .context("Beginning owned administrative transaction")?;
                }
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
                loop {
                    control.check()?;
                    let command = tokio::select! {biased;
                        _=control.cancelled()=>{control.check()?;unreachable!()},
                        received=tokio::time::timeout(Duration::from_secs(30),rx.recv())=>received.context("Administrative producer idle timeout")?.context("Administrative producer ended without Finish")?,
                    };
                    match command {
                        Command::SqliteIdentity { schema, reply } => {
                            let result = adapters::administrative::sqlite_identity(
                                &mut lease.session.conn,
                                &schema,
                            )
                            .await
                            .map_err(|e| format!("{e:#}"));
                            let failed = result.as_ref().err().cloned();
                            ensure!(
                                reply.send(result).is_ok(),
                                "Administrative identity consumer dropped"
                            );
                            if let Some(error) = failed {
                                bail!(error);
                            }
                        }
                        Command::Catalog { extra, reply } => {
                            let result =
                                completion::catalog(&mut lease.session.conn, self.engine(), &extra)
                                    .await
                                    .map_err(|e| format!("{e:#}"));
                            let failed = result.as_ref().err().cloned();
                            ensure!(
                                reply.send(result).is_ok(),
                                "Administrative metadata consumer dropped"
                            );
                            if let Some(error) = failed {
                                bail!(error);
                            }
                        }
                        Command::Columns { table, reply } => {
                            let result = completion::column_names(
                                &mut lease.session.conn,
                                self.engine(),
                                &table,
                            )
                            .await
                            .map_err(|e| format!("{e:#}"));
                            let failed = result.as_ref().err().cloned();
                            ensure!(
                                reply.send(result).is_ok(),
                                "Administrative metadata consumer dropped"
                            );
                            if let Some(error) = failed {
                                bail!(error);
                            }
                        }
                        Command::Query {
                            sql,
                            limit,
                            step,
                            reply,
                        } => {
                            if step {
                                failed_step = Some(completed_steps);
                            }
                            let query_result: Result<Vec<QueryResult>> = async {
                                validate_sql(self.engine(), self.inner.lex_mode, &sql, step)?;
                                // Native preparation rejects extra top-level commands,
                                // including COMMIT hidden after a quoted routine body.
                                adapters::administrative::validate_statement(
                                    &mut lease.session.conn,
                                    &sql,
                                )
                                .await?;
                                raw_results(&mut lease.session.conn, &sql, limit).await
                            }
                            .await;
                            match query_result {
                                Ok(rows) => {
                                    if step {
                                        completed_steps += 1;
                                        failed_step = None;
                                    }
                                    if reply.send(Ok(rows)).is_err() {
                                        bail!("Administrative query consumer dropped");
                                    }
                                }
                                Err(error) => {
                                    let message = format!("{error:#}");
                                    let _ = reply.send(Err(message.clone()));
                                    bail!(message);
                                }
                            }
                        }
                        Command::Finish => {
                            if owned_transaction {
                                // COMMIT in an aborted PG transaction can return a
                                // successful ROLLBACK tag. Prove health first; every
                                // prior query error already ends this worker loop.
                                adapters::administrative::validate_commit(&mut lease.session.conn)
                                    .await
                                    .context(
                                        "Validating administrative transaction before commit",
                                    )?;
                            }
                            {
                                let _gate = self.inner.cancel_gate.lock().await;
                                control.begin_commit()?;
                                commit_started = true;
                            }
                            if owned_transaction {
                                tokio::time::timeout(
                                    Duration::from_secs(30),
                                    execute(&mut lease.session.conn, "COMMIT"),
                                )
                                .await
                                .context("Administrative COMMIT timed out; outcome unknown")?
                                .context("Administrative COMMIT failed; outcome may be unknown")?;
                                owned_transaction = false;
                                committed = true;
                            }
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
            };
            tokio::pin!(execution);
            tokio::select! {biased;
                _=control.cancelled()=>{
                    {let _gate=self.inner.cancel_gate.lock().await;let _=self.cancel_backend().await;}
                    match tokio::time::timeout(Duration::from_secs(10),&mut execution).await{Ok(result)=>result,Err(_)=>Err(anyhow!("Administrative cancellation cleanup timed out"))}
                },
                result=&mut execution=>result,
            }
        };
        // No delayed Database::cancel can hit rollback/observation or the next
        // session user: finish outstanding cancellation before disabling it.
        {
            let _gate = self.inner.cancel_gate.lock().await;
            self.inner.running.store(false, Ordering::Release);
        }
        let mut reusable = !matches!(lease.session.conn, Conn::Closed);
        let mut rolled_back = false;
        let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
            if accepted || control.is_cancelled() {
                drain_connection(&mut lease.session.conn).await?;
            }
            if owned_transaction {
                adapters::administrative::rollback(&mut lease.session.conn).await?;
                rolled_back = true;
            }
            if accepted && self.engine() == Engine::MySql {
                execute(&mut lease.session.conn, "ROLLBACK").await?; // never claims to undo implicit DDL commits
                ensure!(
                    !adapters::manual_commit(&mut lease.session.conn).await?,
                    "Administrative SQL changed autocommit; retire session"
                );
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;
        let cleanup_error = match cleanup {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!(
                "Administrative cleanup failed; session retired: {error:#}"
            )),
            Err(error) => Some(format!(
                "Administrative cleanup timed out; session retired: {error}"
            )),
        };
        if cleanup_error.is_some() {
            reusable = false;
            close_broken_connection(&mut lease.session.conn).await;
        }
        if commit_started {
            rolled_back = false;
        } // a later ROLLBACK cannot prove COMMIT did not happen
        let status = if committed {
            AdministrativeStatus::Committed
        } else if (commit_started && adapters::administrative::atomic(self.engine())) || !reusable {
            AdministrativeStatus::OutcomeUnknown
        } else if !accepted {
            AdministrativeStatus::Rejected
        } else if adapters::administrative::atomic(self.engine()) {
            AdministrativeStatus::RolledBack
        } else if result.is_ok() {
            AdministrativeStatus::CompletedNonAtomic
        } else {
            AdministrativeStatus::FailedNonAtomic
        };
        let (observation, observation_error) = if accepted
            && reusable
            && let Some(sql) = observation_sql
        {
            match tokio::time::timeout(
                Duration::from_secs(10),
                raw_results(&mut lease.session.conn, &sql, 1),
            )
            .await
            {
                Ok(Ok(rows)) => (Some(rows), None),
                error => {
                    reusable = false;
                    close_broken_connection(&mut lease.session.conn).await;
                    (
                        None,
                        Some(format!("Terminal observation unavailable: {error:?}")),
                    )
                }
            }
        } else {
            (None, None)
        };
        let error = result.err().map(|e| format!("{e:#}")).or(cleanup_error);
        if let Some(ready) = ready.take() {
            let _ = ready.send(Err(error
                .clone()
                .unwrap_or_else(|| "Administrative preflight stopped".into())));
        }
        *self
            .inner
            .active_stream
            .lock()
            .expect("stream control mutex") = None;
        lease.armed = false;
        drop(running);
        drop(lease);
        drop(rx);
        done.send_replace(Some(AdministrativeOutcome {
            status,
            completed_steps,
            failed_step,
            rolled_back,
            reusable,
            error,
            observation,
            observation_error,
        }));
    }
}
