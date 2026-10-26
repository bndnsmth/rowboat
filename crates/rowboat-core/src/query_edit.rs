//! Execution-bound, conservative single-table result editing.
use super::*;
use adapters::source::{self, Target, inspect_target};
#[path = "query_edit_parser.rs"]
mod parser;

#[derive(Debug, Clone)]
pub struct QueryEditColumn {
    pub result_index: usize,
    pub result_name: String,
    pub base_name: String,
    pub primary_key: bool,
    pub editable: bool,
}

pub struct QueryEditCapture {
    completion: tokio::sync::oneshot::Receiver<QueryEditSource>,
}
pub(super) struct EditCaptureRequest {
    pub completion: tokio::sync::oneshot::Sender<QueryEditSource>,
    pub required: bool,
}
impl QueryEditCapture {
    /// Drain the QueryStream first: capture completion follows its terminal event.
    pub async fn finish(self) -> QueryEditSource {
        self.completion.await.unwrap_or_else(|_| {
            QueryEditSource::unavailable("Query stopped before edit provenance was captured")
        })
    }
}

#[derive(Debug, Clone)]
struct Captured {
    owner: Weak<Inner>,
    engine: Engine,
    target: Target,
    projections: Option<Vec<parser::Projection>>,
    execution: Option<Arc<QueryResult>>,
    native: source::ExecutionEvidence,
}
#[derive(Debug, Clone)]
pub struct QueryEditSource {
    captured: std::result::Result<Captured, String>,
}
impl QueryEditSource {
    pub(super) fn bind_native(&mut self, native: source::ExecutionEvidence) {
        if let Ok(captured) = &mut self.captured {
            captured.native = native;
        }
    }
    pub(super) fn unavailable(reason: &str) -> Self {
        Self {
            captured: Err(reason.into()),
        }
    }
    pub fn read_only_reason(&self) -> Option<&str> {
        self.captured.as_ref().err().map(String::as_str)
    }
    pub(super) fn bind_execution(
        &mut self,
        snapshot: std::result::Result<Arc<QueryResult>, String>,
    ) {
        if let Ok(captured) = &mut self.captured {
            match snapshot {
                Ok(snapshot) => captured.execution = Some(snapshot),
                Err(reason) => *self = Self::unavailable(&reason),
            }
        }
    }
    pub(super) async fn revalidate_after_execution(&mut self, conn: &mut Conn) {
        if let Ok(captured) = &self.captured {
            let result = inspect_target(conn, captured.engine, &captured.target.table).await;
            if !result
                .as_ref()
                .is_ok_and(|target| same_target(target, &captured.target))
            {
                *self = Self::unavailable(
                    "Source definition changed or could not be verified after execution",
                );
            }
        }
    }
    /// Bind only the immutable result belonging to this capture's execution.
    pub fn plan(&self, result: &QueryResult) -> Result<QueryEditPlan> {
        let captured = self
            .captured
            .as_ref()
            .map_err(|reason| anyhow!(reason.clone()))?;
        let execution = captured
            .execution
            .as_ref()
            .context("Query execution snapshot is not complete")?;
        ensure!(
            crate::query_edit_snapshot::exact_result(execution, result),
            "Result does not match the exact captured execution; bind before sorting or editing"
        );
        let target = &captured.target;
        let projection: Vec<_> = captured.projections.clone().unwrap_or_else(|| {
            target
                .columns
                .iter()
                .filter(|c| c.star_visible)
                .map(|c| parser::Projection {
                    column: c.column.name.clone(),
                    label: c.column.name.clone(),
                })
                .collect()
        });
        ensure!(
            projection.len() == result.columns.len(),
            "Result projection no longer matches the captured source"
        );
        let mut names = HashSet::new();
        let mut labels = HashSet::new();
        let mut mapping = Vec::new();
        let mut columns = Vec::new();
        for (index, (projected, returned)) in projection.iter().zip(&result.columns).enumerate() {
            let base = target
                .columns
                .iter()
                .find(|c| equal_name(captured.engine, &c.column.name, &projected.column))
                .context("Projection is not a direct source column")?;
            ensure!(
                equal_name(captured.engine, &returned.name, &projected.label),
                "Returned column label does not match the captured projection"
            );
            ensure!(
                names.insert(base.column.name.clone()),
                "Repeated source columns are ambiguous for editing"
            );
            let label = if captured.engine == Engine::MySql {
                returned.name.to_ascii_lowercase()
            } else {
                returned.name.clone()
            };
            ensure!(
                labels.insert(label),
                "Duplicate result labels are ambiguous for editing"
            );
            mapping.push(QueryEditColumn {
                result_index: index,
                result_name: returned.name.clone(),
                base_name: base.column.name.clone(),
                primary_key: base.column.primary_key,
                editable: base.editable,
            });
            columns.push(base.column.clone());
        }
        let keys: Vec<_> = target
            .columns
            .iter()
            .filter(|c| c.column.primary_key)
            .collect();
        captured.native.validate(target, result, &mapping)?;
        ensure!(
            !keys.is_empty(),
            "Source table has no primary key; labels are not proof of row identity"
        );
        ensure!(
            keys.iter().all(|k| names.contains(&k.column.name)),
            "Result must contain the complete primary key, including every composite-key column"
        );
        let key_indices: Vec<_> = columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.primary_key.then_some(i))
            .collect();
        let mut seen = HashSet::new();
        for row in &result.rows {
            ensure!(row.len() == columns.len(), "Result has an incomplete row");
            ensure!(
                key_indices.iter().all(|i| row[*i] != Cell::Null),
                "Result primary keys must be non-NULL"
            );
            let key: Vec<_> = key_indices.iter().map(|i| &row[*i]).collect();
            ensure!(
                seen.insert(serde_json::to_string(&key)?),
                "Result contains duplicate primary keys"
            );
        }
        let mut canonical = execution.as_ref().clone();
        canonical.columns = columns;
        let mut captured = captured.clone();
        // The canonical snapshot is now authoritative; a plan does not retain
        // an additional alias-labelled copy of all captured rows.
        captured.execution = None;
        Ok(QueryEditPlan {
            captured,
            columns: mapping,
            result: Arc::new(canonical),
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryEditPlan {
    captured: Captured,
    columns: Vec<QueryEditColumn>,
    result: Arc<QueryResult>,
}
impl QueryEditPlan {
    pub fn table(&self) -> &TableRef {
        &self.captured.target.table
    }
    pub fn columns(&self) -> &[QueryEditColumn] {
        &self.columns
    }
    pub fn result(&self) -> &QueryResult {
        &self.result
    }
    pub fn preview_changes(&self, changes: &[RowChange]) -> Result<String> {
        self.validate_changes(changes)?;
        if let Some(preview) =
            self.captured
                .native
                .preview(&self.captured.target, &self.result, changes)?
        {
            return Ok(preview);
        }
        crate::sql::preview_changes(self.captured.engine, self.table(), changes)
    }
    fn validate_changes(&self, changes: &[RowChange]) -> Result<()> {
        let mut seen_rows = HashSet::new();
        for change in changes {
            let RowChange::Update { original, values } = change else {
                bail!("Query results support updates of captured rows only");
            };
            ensure!(
                original.len() == self.columns.len(),
                "Original must contain every projected source value"
            );
            let mut row = Vec::new();
            for col in &self.columns {
                let mut cells = original.iter().filter(|(name, _)| name == &col.base_name);
                row.push(
                    cells
                        .next()
                        .context("Original source column missing")?
                        .1
                        .clone(),
                );
                ensure!(cells.next().is_none(), "Duplicate original source column");
            }
            let index = self
                .result
                .rows
                .iter()
                .position(|r| r == &row)
                .context("Original values do not match the captured result snapshot")?;
            ensure!(
                seen_rows.insert(index),
                "Merge edits to the same captured row before applying"
            );
            ensure!(!values.is_empty(), "Update has no changed values");
            let mut names = HashSet::new();
            for (name, _) in values {
                let column = self
                    .columns
                    .iter()
                    .find(|c| &c.base_name == name)
                    .context("Only projected source columns may be edited")?;
                ensure!(
                    column.editable,
                    "Generated/computed or always-generated identity columns are read-only"
                );
                ensure!(names.insert(name), "Duplicate edited column");
            }
        }
        Ok(())
    }
}

fn equal_name(engine: Engine, a: &str, b: &str) -> bool {
    if engine != Engine::Postgres {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}
fn same_target(a: &Target, b: &Target) -> bool {
    a.table == b.table && a.identity == b.identity && a.signature == b.signature
}

// Reject known-uneditable projections before an explicit refresh executes.
// Runtime result/native evidence is still checked by QueryEditSource::plan.
fn validate_edit_projection(
    engine: Engine,
    target: &Target,
    projections: Option<&[parser::Projection]>,
) -> Result<()> {
    let mut names = HashSet::new();
    let mut labels = HashSet::new();
    if let Some(projections) = projections {
        for projection in projections {
            let base = target
                .columns
                .iter()
                .find(|c| equal_name(engine, &c.column.name, &projection.column))
                .context("Projection is not a direct source column")?;
            ensure!(
                names.insert(base.column.name.as_str()),
                "Repeated source columns are ambiguous for editing"
            );
            let label = if engine == Engine::MySql {
                projection.label.to_ascii_lowercase()
            } else {
                projection.label.clone()
            };
            ensure!(
                labels.insert(label),
                "Duplicate result labels are ambiguous for editing"
            );
        }
    } else {
        names.extend(
            target
                .columns
                .iter()
                .filter(|c| c.star_visible)
                .map(|c| c.column.name.as_str()),
        );
    }
    let keys: Vec<_> = target
        .columns
        .iter()
        .filter(|c| c.column.primary_key)
        .collect();
    ensure!(
        !keys.is_empty(),
        "Source table has no primary key; labels are not proof of row identity"
    );
    ensure!(
        keys.iter().all(|c| names.contains(c.column.name.as_str())),
        "Result must contain the complete primary key, including every composite-key column"
    );
    ensure!(
        engine != Engine::Sqlite || keys.iter().all(|c| !c.column.nullable),
        "SQLite editing requires non-nullable primary keys"
    );
    Ok(())
}

impl Database {
    pub async fn query_stream_with_edit_source(
        &self,
        sql: &str,
        options: QueryStreamOptions,
    ) -> Result<(QueryStream, QueryEditCapture)> {
        let (sender, completion) = tokio::sync::oneshot::channel();
        let stream = self
            .start_query_stream(
                sql,
                options,
                Some(EditCaptureRequest {
                    completion: sender,
                    required: false,
                }),
            )
            .await?;
        Ok((stream, QueryEditCapture { completion }))
    }
    /// Explicitly refresh results for editing. Unlike best-effort capture,
    /// unavailable source proof rejects the operation without executing the SQL.
    /// This is a new execution, never a promotion of previously displayed rows.
    /// Drain the stream, then finish capture and bind its exact unmodified result.
    /// Preflight rejection is reported as a terminal stream error. Successful
    /// execution still requires `QueryEditSource::plan`: runtime native evidence,
    /// exact result binding and bounded snapshot retention can reject editing.
    pub async fn query_stream_for_editing(
        &self,
        sql: &str,
        options: QueryStreamOptions,
    ) -> Result<(QueryStream, QueryEditCapture)> {
        let (sender, completion) = tokio::sync::oneshot::channel();
        let stream = self
            .start_query_stream(
                sql,
                options,
                Some(EditCaptureRequest {
                    completion: sender,
                    required: true,
                }),
            )
            .await?;
        Ok((stream, QueryEditCapture { completion }))
    }
    pub(super) async fn capture_query_edit_source(
        &self,
        conn: &mut Conn,
        sql: &str,
        required: bool,
    ) -> (QueryEditSource, Option<String>) {
        let mut owned_transaction = false;
        let capture: Result<(Captured, String)> = async {
            ensure!(!self.inner.read_only, "This connection is read-only");
            // A negative grammar result under every supported lexical mode
            // needs no server probes. Never assume the cached mode is current.
            let preliminary = parser::parse(self.engine(), sql, self.inner.lex_mode);
            if preliminary.is_err() {
                let possible_mysql_mode = self.engine() == Engine::MySql
                    && [false, true].into_iter().any(|mysql_ansi_quotes| {
                        [false, true].into_iter().any(|mysql_no_backslash_escapes| {
                            parser::parse(
                                self.engine(),
                                sql,
                                LexMode {
                                    mysql_ansi_quotes,
                                    mysql_no_backslash_escapes,
                                },
                            )
                            .is_ok()
                        })
                    });
                if !possible_mysql_mode {
                    preliminary?;
                }
            }
            let (mode, database) = source::context(conn).await?;
            // Existing failed/caller-owned transactions are not modified for
            // best-effort metadata. No metadata failure can abort such a txn.
            ensure!(
                !server_in_transaction(conn).await?,
                "Query-result editing is unavailable inside an existing transaction"
            );
            let selected = parser::parse(self.engine(), sql, mode)?;
            let requested = TableRef {
                schema: selected.schema.or(database).unwrap_or_default(),
                name: selected.table,
            };
            // Resolve only the canonical name before locking; authoritative
            // identity/columns are captured fresh under the relation/MDL lock.
            let resolved = source::resolve(conn, self.engine(), &requested).await?;
            execute(conn, "START TRANSACTION").await?;
            owned_transaction = true;
            let qualified = qualified_table(self.engine(), &resolved);
            source::lock(conn, self.engine(), &resolved, false).await?;
            let target = inspect_target(conn, self.engine(), &resolved).await?;
            ensure!(
                target.table == resolved,
                "Source name changed while acquiring its metadata lock"
            );
            if required {
                validate_edit_projection(self.engine(), &target, selected.projections.as_deref())?;
            }
            // Pin resolution to the object just locked, including against a new
            // earlier-search_path shadow created by another session. This is
            // the sole execution, not a replay, with only FROM qualified.
            let mut execution_sql = sql.to_owned();
            execution_sql.replace_range(selected.table_range, &qualified);
            Ok((
                Captured {
                    owner: Arc::downgrade(&self.inner),
                    engine: self.engine(),
                    target,
                    projections: selected.projections,
                    execution: None,
                    native: Default::default(),
                },
                execution_sql,
            ))
        }
        .await;
        match capture {
            Ok((captured, sql)) => (
                QueryEditSource {
                    captured: Ok(captured),
                },
                Some(sql),
            ),
            Err(error) => {
                if owned_transaction && end_owned_transaction(conn, false).await.is_err() {
                    close_broken_connection(conn).await;
                }
                (
                    QueryEditSource::unavailable(&format!("Query result is read-only: {error:#}")),
                    None,
                )
            }
        }
    }

    /// Explicitly apply reviewed updates; never re-execute the source SELECT.
    pub async fn apply_query_changes(
        &self,
        plan: &QueryEditPlan,
        changes: &[RowChange],
    ) -> Result<u64> {
        ensure!(!self.inner.read_only, "This connection is read-only");
        ensure!(
            plan.captured
                .owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.inner)),
            "Query edit plan belongs to another physical session; rerun explicitly"
        );
        plan.validate_changes(changes)?;
        if changes.is_empty() {
            return Ok(0);
        }
        let mut lease = EditLease {
            session: self.inner.session.lock().await,
            armed: true,
        };
        // Do not allow backend cancellation to interrupt a caller-owned
        // transaction probe before we have rejected that transaction intact.
        let preflight_gate = self.inner.cancel_gate.lock().await;
        let preflight: Result<()> = async {
            ensure!(
                !adapters::manual_commit(&mut lease.session.conn).await?,
                "Query edits require MySQL autocommit=1"
            );
            ensure!(
                !server_in_transaction(&mut lease.session.conn).await?,
                "Query edits require no existing transaction"
            );
            Ok(())
        }
        .await;
        if let Err(error) = preflight {
            // Completed guard rejections must preserve the caller's transaction.
            if !adapters::driver_error(&error) {
                lease.armed = false;
            }
            return Err(error);
        }
        drop(preflight_gate);
        let _running = self.running().await;
        if source::requires_native_originals(&lease.session.conn) {
            let result = source::apply_native(
                &mut lease.session.conn,
                plan.table(),
                changes,
                Some((&plan.captured.target, &plan.result, &plan.captured.native)),
                adapters::EditCancellation {
                    requested: &self.inner.cancel_requested,
                    gate: &self.inner.cancel_gate,
                },
            )
            .await;
            lease.armed = !adapters::catalog::operation_reusable(&lease.session.conn);
            return result;
        }
        execute(&mut lease.session.conn, "START TRANSACTION").await?;
        let operation: Result<u64> = async {
            source::lock(&mut lease.session.conn, self.engine(), plan.table(), true).await?;
            let actual =
                inspect_target(&mut lease.session.conn, self.engine(), plan.table()).await?;
            ensure!(
                same_target(&actual, &plan.captured.target),
                "Query source table identity or definition changed; rerun the SELECT explicitly"
            );
            let columns: Vec<_> = actual.columns.iter().map(|c| c.column.clone()).collect();
            let statements = change_statements(self.engine(), plan.table(), &columns, changes)?;
            let mut affected = 0;
            for (sql, params) in statements {
                ensure!(
                    !self.inner.cancel_requested.load(Ordering::Acquire),
                    "Query edits cancelled"
                );
                let result = bound_results(&mut lease.session.conn, &sql, &params, 0).await?;
                ensure!(
                    result.rows_affected == 1,
                    "Conflict: original query row changed or disappeared; no query edits applied"
                );
                affected += result.rows_affected;
            }
            Ok(affected)
        }
        .await;
        let _gate = self.inner.cancel_gate.lock().await;
        let operation = operation.and_then(|count| {
            ensure!(
                !self.inner.cancel_requested.load(Ordering::Acquire),
                "Query edits cancelled"
            );
            Ok(count)
        });
        match operation {
            Ok(count) => {
                end_owned_transaction(&mut lease.session.conn, true)
                    .await
                    .context("Finalizing query edits; commit outcome unknown on connection loss")?;
                lease.armed = false;
                Ok(count)
            }
            Err(error) => {
                end_owned_transaction(&mut lease.session.conn, false)
                    .await
                    .context("Query edit rollback failed; session retired")?;
                lease.armed = false;
                Err(error)
            }
        }
    }
}
pub(super) struct EditLease<'a> {
    pub(super) session: tokio::sync::MutexGuard<'a, Session>,
    pub(super) armed: bool,
}
impl Drop for EditLease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.conn = Conn::Closed;
        }
    }
}
