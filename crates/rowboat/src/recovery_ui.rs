//! Private staged batches are detached after restart; validation never executes mutations.
use crate::{
    app::{Event, Workbench},
    forms::RowTarget,
    view::{action, column, label, row},
};
use anyhow::{Result, ensure};
use gpui_kit::{prelude::*, *};
use rowboat_core::{
    BrowseRequest, Cell, ConnectionProfile, Database, Filter, FilterExpr, FilterOp, QueryResult,
    RowChange, Structure, TableRef,
};
use serde::{Deserialize, Serialize};

pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_CHANGES: usize = 200;
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryPhase {
    Staged,
    Stale,
    // Old checkpoints did not record intent; never infer that Apply had not started.
    #[default]
    ApplyStarted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryBatch {
    pub id: String,
    pub owner: String,
    pub table: TableRef,
    profile: String,
    columns: String,
    keys: Vec<String>,
    changes: String,
    #[serde(default)]
    pub phase: RecoveryPhase,
}
fn profile_binding(profile: &ConnectionProfile) -> Result<String> {
    let mut value = serde_json::to_value(profile)?;
    // Cosmetic changes do not change a destination. Transport/passwords are never serialized.
    for key in ["name", "group", "color"] {
        value.as_object_mut().unwrap().remove(key);
    }
    Ok(serde_json::to_string(&value)?)
}
impl RecoveryBatch {
    pub fn capture(
        profile: &ConnectionProfile,
        table: TableRef,
        structure: &Structure,
        changes: &[RowChange],
    ) -> Result<Self> {
        let batch = Self::capture_intent(profile, table, structure, changes)?;
        batch.check_keys()?;
        Ok(batch)
    }
    fn capture_intent(
        profile: &ConnectionProfile,
        table: TableRef,
        structure: &Structure,
        changes: &[RowChange],
    ) -> Result<Self> {
        ensure!(
            !changes.is_empty() && changes.len() <= MAX_CHANGES,
            "Recovery supports 1–{MAX_CHANGES} staged changes per batch"
        );
        let batch = Self {
            id: uuid::Uuid::new_v4().to_string(),
            owner: profile.id.clone(),
            table,
            profile: profile_binding(profile)?,
            columns: serde_json::to_string(&structure.columns)?,
            keys: structure
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect(),
            changes: serde_json::to_string(changes)?,
            phase: RecoveryPhase::Staged,
        };
        batch.validate()?;
        Ok(batch)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BATCH_BYTES,
            "Staged recovery exceeds the 1 MiB per-batch limit"
        );
        let changes: Vec<RowChange> = serde_json::from_str(&self.changes)?;
        ensure!(
            !changes.is_empty() && changes.len() <= MAX_CHANGES,
            "Invalid recovery batch size"
        );
        ensure!(
            !self.keys.is_empty(),
            "Recovery needs a complete primary key"
        );
        Ok(())
    }
    fn check_keys(&self) -> Result<()> {
        for change in serde_json::from_str::<Vec<RowChange>>(&self.changes)? {
            self.key_filter(&change)?;
        }
        Ok(())
    }
    fn can_revalidate(&self) -> Result<()> {
        ensure!(
            self.phase == RecoveryPhase::Staged,
            "This checkpoint is stale or Apply may have started. Reconcile with the database and explicitly discard this saved batch; it cannot be staged or replayed."
        );
        self.check_keys()
    }
    fn matches(
        &self,
        profile: &ConnectionProfile,
        table: &TableRef,
        structure: &Structure,
    ) -> Result<()> {
        ensure!(
            self.owner == profile.id && self.profile == profile_binding(profile)?,
            "Saved connection destination/settings changed; recovery is detached"
        );
        ensure!(
            &self.table == table,
            "Reopen the exact recovery table first"
        );
        ensure!(
            self.columns == serde_json::to_string(&structure.columns)?,
            "Table columns/primary key changed; recovery cannot be staged"
        );
        Ok(())
    }
    fn key_filter(&self, change: &RowChange) -> Result<FilterExpr> {
        let row = match change {
            RowChange::Insert { values } => values,
            RowChange::Update { original, .. } | RowChange::Delete { original } => original,
        };
        let filters = self.keys.iter().map(|key| {
            let values = row.iter().filter(|(name, _)| name == key).collect::<Vec<_>>();
            ensure!(values.len() == 1 && values[0].1 != Cell::Null, "Recovery requires every explicit, non-NULL primary-key value (including inserts)");
            Ok(FilterExpr::Leaf(Filter { column: key.clone(), op: FilterOp::Eq, value: values[0].1.clone() }))
        }).collect::<Result<Vec<_>>>()?;
        Ok(FilterExpr::And(filters))
    }
}
fn check_original(change: &RowChange, result: &QueryResult) -> Result<()> {
    ensure!(!result.truncated, "Recovery lookup was truncated");
    match change {
        RowChange::Insert { .. } => ensure!(
            result.rows.is_empty(),
            "Insert key already exists; the batch may already have been committed"
        ),
        RowChange::Update { original, .. } | RowChange::Delete { original } => {
            ensure!(
                result.rows.len() == 1,
                "Original key is missing or not unique; recovery remains detached"
            );
            let row = &result.rows[0];
            ensure!(
                row.len() == result.columns.len()
                    && original.len() == row.len()
                    && result.columns.iter().zip(row).all(|(c, v)| original
                        .iter()
                        .filter(|(n, _)| n == &c.name)
                        .count()
                        == 1
                        && original.iter().any(|(n, o)| n == &c.name && o == v)),
                "Original row changed; recovery remains detached"
            );
        }
    }
    Ok(())
}
async fn revalidate(
    db: &Database,
    profile: &ConnectionProfile,
    batch: &RecoveryBatch,
) -> Result<Vec<RowChange>> {
    batch.validate()?;
    batch.can_revalidate()?;
    ensure!(
        !db.in_transaction().await?,
        "End the active transaction before recovery validation"
    );
    let structure = db.structure(&batch.table).await?;
    batch.matches(profile, &batch.table, &structure)?;
    let changes: Vec<RowChange> = serde_json::from_str(&batch.changes)?;
    for change in &changes {
        let filter = batch.key_filter(change)?;
        let found = db
            .browse_filtered(
                &batch.table,
                &BrowseRequest {
                    limit: 2,
                    ..Default::default()
                },
                Some(&filter),
            )
            .await?;
        check_original(change, &found)?;
    }
    Ok(changes)
}
#[derive(Default)]
pub struct RecoveryState {
    pub detached: Vec<RecoveryBatch>,
    current: Option<RecoveryBatch>,
    pub error: Option<String>,
    pub open: bool,
    pending: Option<String>,
}
impl RecoveryState {
    fn record_capture(&mut self, captured: Result<RecoveryBatch>) {
        match captured {
            Ok(mut batch) => {
                if let Some(old) = &self.current {
                    batch.id = old.id.clone();
                }
                self.current = Some(batch);
                self.error = None;
            }
            Err(error) => {
                if let Some(batch) = &mut self.current {
                    batch.phase = RecoveryPhase::Stale;
                }
                self.error = Some(format!(
                    "Latest staged edits are NOT recoverable: {error:#}"
                ));
            }
        }
    }
}
/// The mutation future is not polled until the single writer acknowledges durable intent.
pub async fn after_durable_intent<T>(
    ack: tokio::sync::oneshot::Receiver<Result<(), String>>,
    mutation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    ack.await
        .map_err(|_| "Apply did not start: recovery acknowledgement unavailable".to_string())?
        .map_err(|e| format!("Apply did not start: {e}"))?;
    mutation.await
}
pub struct Validated {
    id: String,
    target: RowTarget,
    result: Result<Vec<RowChange>, String>,
}
impl Workbench {
    /// Called only after the shell acknowledges a checkpoint with these batches
    /// removed. Clear the sources too, so a retained entity cannot recapture them.
    pub(crate) fn discard_recovery_on_close(&mut self) {
        debug_assert!(self.close_requested && !self.busy);
        self.changes.clear();
        self.change_target = None;
        self.query_edit.batch = None;
        self.recovery = RecoveryState::default();
    }
    pub fn recovery_batches(&self) -> Vec<RecoveryBatch> {
        self.recovery
            .detached
            .iter()
            .chain(self.recovery.current.iter())
            .cloned()
            .collect()
    }
    pub fn capture_recovery(&mut self) {
        if self
            .recovery
            .current
            .as_ref()
            .is_some_and(|b| b.phase == RecoveryPhase::ApplyStarted)
        {
            return;
        }
        if self.changes.is_empty() {
            self.recovery.current = None;
            self.recovery.error = None;
            return;
        }
        let captured = (|| {
            ensure!(
                self.recovery.detached.len() < 16,
                "Discard or validate a detached batch before checkpointing another (16-batch bound)"
            );
            let target = self
                .change_target
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Staged batch has no target"))?;
            let (actual, structure) = self.recovery_edit_context()?;
            ensure!(&actual == target, "Staged batch target changed");
            let mut batch = RecoveryBatch::capture(
                self.profile.as_ref().unwrap(),
                target.table.clone(),
                &structure,
                &self.changes,
            )?;
            // An execution proof is deliberately not serialized. Query-origin
            // recovery is evidence only, never a table-browse replay shortcut.
            if self.query_edit.batch.is_some() {
                batch.phase = RecoveryPhase::Stale;
            }
            Ok(batch)
        })();
        self.recovery.record_capture(captured);
    }
    pub fn prepare_apply_marker(&mut self) -> Result<String> {
        ensure!(
            self.recovery.detached.len() < 16,
            "Reconcile a saved recovery batch before Apply (16-batch bound)"
        );
        let target = self
            .change_target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No staged target"))?;
        let (actual, structure) = self.recovery_edit_context()?;
        ensure!(&actual == target, "Staged target changed");
        let profile = self
            .profile
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No profile"))?;
        let mut batch = RecoveryBatch::capture_intent(
            profile,
            target.table.clone(),
            &structure,
            &self.changes,
        )?;
        batch.phase = RecoveryPhase::ApplyStarted;
        batch.validate()?;
        let id = batch.id.clone();
        self.recovery.current = Some(batch);
        self.recovery.error = None;
        Ok(id)
    }
    pub fn has_apply_marker(&self, id: &str) -> bool {
        self.recovery
            .current
            .as_ref()
            .is_some_and(|b| b.id == id && b.phase == RecoveryPhase::ApplyStarted)
    }
    pub fn finish_apply_marker(&mut self, confirmed_autocommit: bool) {
        if let Some(batch) = self.recovery.current.take()
            && !confirmed_autocommit
        {
            self.recovery.detached.push(batch);
            self.recovery.error = Some("Apply outcome needs reconciliation (including transactions not yet committed). The saved batch is detached and cannot be replayed. Inspect the database, then explicitly discard it.".into());
        }
        self.changes.clear();
        self.change_target = None;
    }
    pub fn show_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.busy && !self.has_extension_dialog() {
            self.recovery.open = true;
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }
    fn validate_recovery(&mut self, id: &str, cx: &mut Context<Self>) {
        let result: Result<_> = (|| {
            ensure!(
                !self.busy
                    && !self.close_requested
                    && self.changes.is_empty()
                    && !self.has_row_draft(),
                "Finish pending operations/edits first"
            );
            ensure!(
                self.transaction_known && !self.transaction,
                "Recovery requires known autocommit state"
            );
            let batch = self
                .recovery
                .detached
                .iter()
                .find(|b| b.id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Recovery batch no longer exists"))?;
            let profile = self
                .profile
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Connect the exact saved profile first"))?;
            batch.can_revalidate()?;
            let target = self
                .current_target()
                .ok_or_else(|| anyhow::anyhow!("Explicitly reopen the recovery table first"))?;
            batch.matches(&profile, &target.table, &self.structure)?;
            ensure!(
                !profile.read_only,
                "Recovery staging is disabled on read-only connections"
            );
            let db = self
                .database
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Connect before validating recovery"))?;
            Ok((batch, profile, target, db))
        })();
        match result {
            Ok((batch, profile, target, db)) => {
                self.busy = true;
                self.recovery.pending = Some(batch.id.clone());
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    let result = revalidate(&db, &profile, &batch)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Recovery(Box::new(Validated {
                        id: batch.id,
                        target,
                        result,
                    })));
                });
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
        cx.notify();
    }
    pub fn receive_recovery(&mut self, loaded: Validated, cx: &mut Context<Self>) {
        if self.recovery.pending.as_ref() != Some(&loaded.id) {
            return;
        }
        self.busy = false;
        self.recovery.pending = None;
        if self.current_target().as_ref() != Some(&loaded.target)
            || !self.changes.is_empty()
            || self.close_requested
        {
            self.fail("Recovery target changed; batch remains detached".into());
            return;
        }
        match loaded.result {
            Ok(changes) => {
                self.recovery.detached.retain(|b| b.id != loaded.id);
                self.changes = changes;
                self.change_target = Some(loaded.target);
                self.recovery.open = false;
                self.capture_recovery();
                self.review_pending(cx);
                self.status = "Recovered edits revalidated and staged · inspect Review, then explicit Apply (conflicts checked again)".into();
            }
            Err(e) => self.fail(e),
        }
        cx.notify();
    }
    pub fn render_recovery(&self, cx: &mut Context<Self>) -> AnyElement {
        use gpui_omarchy::{self as ui, ActiveTheme};
        let theme = cx.omarchy().clone();
        let mut body = column().gap_3().text_size(px(13.))
            .child(row().child(ui::icon(ui::IconName::Layers).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Recover staged edits")))
            .child(label("Private staged-edit recovery · detached batches", cx))
            .child(div().h(px(1.)).bg(theme.divider()))
            .child(label("Connect the exact profile and explicitly open its table, then validate. Stale and ApplyStarted batches (including legacy checkpoints) cannot be replayed: inspect the database and reconcile externally, then explicitly discard the saved batch. No mutation runs during validation. Original/draft cells are stored locally (0600), at most 200 changes / 1 MiB per batch. Inserts require explicit primary keys.", cx));
        if let Some(error) = &self.recovery.error {
            body = body.child(label(error.clone(), cx));
        }
        for batch in self.recovery.detached.iter().take(16) {
            let id = batch.id.clone();
            let discard = id.clone();
            let copy = id.clone();
            let changes = batch.changes.clone();
            body = body.child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.divider())
                    .child(label(
                        format!(
                            "{}.{} · {} · {:?}",
                            batch.table.schema, batch.table.name, batch.owner, batch.phase
                        ),
                        cx,
                    ))
                    .child(action(
                        format!("validate-recovery-{id}"),
                        "Validate → Review",
                        false,
                        !self.busy && batch.phase == RecoveryPhase::Staged,
                        cx,
                        move |s, _, cx| s.validate_recovery(&id, cx),
                    ))
                    .child(action(
                        format!("copy-recovery-{copy}"),
                        "Copy original/draft JSON",
                        false,
                        !self.busy,
                        cx,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(changes.clone()))
                        },
                    ))
                    .child(action(
                        format!("discard-recovery-{discard}"),
                        "Reconciled / discard saved batch",
                        false,
                        !self.busy,
                        cx,
                        move |s, _, cx| {
                            s.recovery.detached.retain(|b| b.id != discard);
                            cx.notify();
                        },
                    )),
            );
        }
        if self.recovery.detached.is_empty() {
            body = body.child(label(
                "No detached batches. Current staged edits are checkpointed with SQL buffers.",
                cx,
            ));
        }
        body = body.child(action(
            "close-recovery",
            "Close",
            false,
            !self.busy,
            cx,
            |s, w, cx| {
                s.recovery.open = false;
                w.focus(&s.focus, cx);
                cx.notify();
            },
        ));
        self.modal(body.w(px(720.)).max_w_full().p_4().into_any_element(), cx)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn failed_capture_marks_old_checkpoint_stale_across_restart_and_blocks_revalidation() {
        let (profile, structure, change) = fixture();
        let mut state = super::RecoveryState::default();
        state.record_capture(RecoveryBatch::capture(
            &profile,
            table(),
            &structure,
            &vec![change.clone(); 200],
        ));
        assert!(state.error.is_none());
        let id = state.current.as_ref().unwrap().id.clone();
        state.record_capture(RecoveryBatch::capture(
            &profile,
            table(),
            &structure,
            &vec![change.clone(); 201],
        ));
        assert!(state.error.as_ref().unwrap().contains("NOT recoverable"));
        let stale = state.current.as_ref().unwrap();
        assert_eq!(stale.id, id);
        assert_eq!(stale.phase, super::RecoveryPhase::Stale);
        let recovered = roundtrip(&profile, stale.clone());
        assert!(recovered.can_revalidate().is_err());
        state.record_capture(RecoveryBatch::capture(
            &profile,
            table(),
            &structure,
            std::slice::from_ref(&change),
        ));
        assert!(state.error.is_none());
        assert!(state.current.as_ref().unwrap().can_revalidate().is_ok());
        let mut large = change;
        if let RowChange::Update { values, .. } = &mut large {
            values[0].1 = Cell::Text("a".repeat(MAX_BATCH_BYTES));
        }
        state.record_capture(RecoveryBatch::capture(
            &profile,
            table(),
            &structure,
            &[large],
        ));
        assert!(state.error.is_some());
        assert!(
            roundtrip(&profile, state.current.unwrap())
                .can_revalidate()
                .is_err()
        );
    }
    fn roundtrip(profile: &ConnectionProfile, batch: RecoveryBatch) -> RecoveryBatch {
        let directory = tempfile::tempdir().unwrap();
        let state = SessionState {
            version: crate::session::FORMAT_VERSION,
            active_tab: 1,
            tabs: vec![TabBuffer {
                id: 1,
                connection_id: Some(profile.id.clone()),
                title: "SQL".into(),
                sql: "select 1".into(),
            }],
            workspaces: vec![WorkspaceBuffer {
                documents: Default::default(),
                connection_id: Some(profile.id.clone()),
                active_tab: 1,
                split_tab: None,
                focused_pane: 0,
                open: true,
                staged: vec![batch],
            }],
        };
        {
            SessionStore::at(directory.path())
                .unwrap()
                .save(&state)
                .unwrap();
        }
        SessionStore::at(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .workspaces
            .remove(0)
            .staged
            .remove(0)
    }
    #[test]
    fn capture_rejects_generated_null_duplicate_and_incomplete_composite_insert_keys() {
        let (profile, structure, _) = fixture();
        for values in [
            vec![],
            vec![("id".into(), Cell::Text("1".into()))],
            vec![
                ("id".into(), Cell::Text("1".into())),
                ("tenant".into(), Cell::Null),
            ],
            vec![
                ("id".into(), Cell::Text("1".into())),
                ("tenant".into(), Cell::Text("1".into())),
                ("tenant".into(), Cell::Text("2".into())),
            ],
        ] {
            assert!(
                RecoveryBatch::capture(
                    &profile,
                    table(),
                    &structure,
                    &[RowChange::Insert { values }]
                )
                .is_err()
            );
        }
        let complete = RowChange::Insert {
            values: vec![
                ("id".into(), Cell::Text("9007199254740993".into())),
                ("tenant".into(), Cell::Text("1".into())),
            ],
        };
        assert!(RecoveryBatch::capture(&profile, table(), &structure, &[complete]).is_ok());
    }
    #[test]
    fn durable_apply_intent_blocks_canonical_noop_replay_and_legacy_ambiguity() {
        let (profile, structure, mut change) = fixture();
        if let RowChange::Update { original, values } = &mut change {
            original[2].1 = Cell::Text("1".into());
            values[0].1 = Cell::Text("01".into());
        }
        let RowChange::Update { original, .. } = &change else {
            unreachable!()
        };
        let result = QueryResult {
            columns: structure.columns.clone(),
            rows: vec![original.iter().map(|(_, v)| v.clone()).collect()],
            ..Default::default()
        };
        assert!(check_original(&change, &result).is_ok()); // Database canonicalization cannot prove non-commit.
        let mut batch = RecoveryBatch::capture(&profile, table(), &structure, &[change]).unwrap();
        batch.phase = super::RecoveryPhase::ApplyStarted;
        let recovered = roundtrip(&profile, batch.clone());
        assert_eq!(recovered.phase, super::RecoveryPhase::ApplyStarted);
        assert!(recovered.can_revalidate().is_err());
        let mut legacy = serde_json::to_value(batch).unwrap();
        legacy.as_object_mut().unwrap().remove("phase");
        assert!(
            serde_json::from_value::<RecoveryBatch>(legacy)
                .unwrap()
                .can_revalidate()
                .is_err()
        );
    }
    #[tokio::test]
    async fn mutation_is_never_polled_before_durable_ack_or_after_failed_ack() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        for outcome in [Some(Ok(())), Some(Err("disk full".into())), None] {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let count = calls.clone();
            let before = calls.load(Ordering::SeqCst);
            let mut task = Box::pin(super::after_durable_intent(rx, async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));
            assert!(
                std::future::Future::poll(
                    task.as_mut(),
                    &mut std::task::Context::from_waker(std::task::Waker::noop())
                )
                .is_pending()
            );
            assert_eq!(calls.load(Ordering::SeqCst), before);
            let success = matches!(outcome, Some(Ok(())));
            if let Some(outcome) = outcome {
                tx.send(outcome).unwrap();
            } else {
                drop(tx);
            }
            assert_eq!(task.await.is_ok(), success);
            assert_eq!(calls.load(Ordering::SeqCst), before + usize::from(success));
        }
    }
    use super::{MAX_BATCH_BYTES, MAX_CHANGES, RecoveryBatch, check_original};
    use crate::session::{SessionState, SessionStore, TabBuffer, WorkspaceBuffer};
    use rowboat_core::{Cell, ConnectionProfile, QueryResult, RowChange, Structure, TableRef};
    async fn live_recovery(engine: rowboat_core::Engine) -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture, table};
        use rowboat_core::{BrowseRequest, sql::qualified_table};
        let (db, profile, schema) = fixture(engine).await?;
        let result=async {
            let table=table(&schema,"recovery_rows");let sql=qualified_table(engine,&table);
            db.query(&format!("CREATE TABLE {sql}(tenant BIGINT NOT NULL,id BIGINT NOT NULL,payload VARCHAR(40),PRIMARY KEY(tenant,id)); INSERT INTO {sql} VALUES(1,9007199254740993,'original')"),0).await?;
            let structure=db.structure(&table).await?;let rows=db.browse(&table,&BrowseRequest{limit:2,..Default::default()}).await?;
            let original=rows.columns.iter().zip(&rows.rows[0]).map(|(c,v)|(c.name.clone(),v.clone())).collect();
            let change=RowChange::Update{original,values:vec![("payload".into(),Cell::Text("draft".into()))]};
            let batch=RecoveryBatch::capture(&profile,table.clone(),&structure,&[change])?;
            anyhow::ensure!(super::revalidate(&db,&profile,&batch).await?.len()==1);
            let unchanged=db.browse(&table,&BrowseRequest{limit:2,..Default::default()}).await?;
            anyhow::ensure!(unchanged.rows==rows.rows,"Validation mutated data");
            db.begin().await?;anyhow::ensure!(super::revalidate(&db,&profile,&batch).await.is_err());db.rollback().await?;
            db.query(&format!("UPDATE {sql} SET payload='external' WHERE tenant=1"),0).await?;
            anyhow::ensure!(super::revalidate(&db,&profile,&batch).await.is_err(),"Conflicting original was recovered");
            let insert=RecoveryBatch::capture(&profile,table,&structure,&[RowChange::Insert{values:vec![("tenant".into(),Cell::Text("2".into())),("id".into(),Cell::Text("3".into())),("payload".into(),Cell::Null)]}])?;
            anyhow::ensure!(super::revalidate(&db,&profile,&insert).await?.len()==1);
            db.query(&format!("INSERT INTO {sql} VALUES(2,3,NULL)"),0).await?;
            anyhow::ensure!(super::revalidate(&db,&profile,&insert).await.is_err(),"Commit-before-crash insert replayed");
            // Original-value validation alone cannot detect a canonical no-op commit
            // whose UPDATE trigger has already produced an external effect.
            let canonical=crate::pass5_tests::table(&schema,"canonical_rows");
            let canonical_sql=qualified_table(engine,&canonical);
            let effects=qualified_table(engine,&crate::pass5_tests::table(&schema,"canonical_effects"));
            db.query(&format!("CREATE TABLE {canonical_sql}(id INTEGER PRIMARY KEY,payload INTEGER); INSERT INTO {canonical_sql} VALUES(1,1); CREATE TABLE {effects}(n INTEGER); INSERT INTO {effects} VALUES(0)"),0).await?;
            let trigger=qualified_table(engine,&crate::pass5_tests::table(&schema,"count_update"));
            let trigger_sql=match engine {
                rowboat_core::Engine::Postgres=>format!("CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE {effects} SET n=n+1; RETURN NEW; END $$; CREATE TRIGGER count_update AFTER UPDATE ON {canonical_sql} FOR EACH ROW EXECUTE FUNCTION {trigger}()"),
                rowboat_core::Engine::MySql=>format!("CREATE TRIGGER {trigger} AFTER UPDATE ON {canonical_sql} FOR EACH ROW UPDATE {effects} SET n=n+1"),
                _ => unreachable!("This fixture exercises network trigger providers"),
            };
            db.query(&trigger_sql,0).await?;
            let structure=db.structure(&canonical).await?;
            let before=db.browse(&canonical,&BrowseRequest{limit:2,..Default::default()}).await?;
            let original=before.columns.iter().zip(&before.rows[0]).map(|(c,v)|(c.name.clone(),v.clone())).collect();
            let change=RowChange::Update{original,values:vec![("payload".into(),Cell::Text("01".into()))]};
            let mut marker=RecoveryBatch::capture(&profile,canonical.clone(),&structure,std::slice::from_ref(&change))?;
            marker.phase=super::RecoveryPhase::ApplyStarted;
            let persisted=roundtrip(&profile,marker);
            let (ack,wait)=tokio::sync::oneshot::channel();ack.send(Ok(())).unwrap();
            super::after_durable_intent(wait,async{db.apply_changes(&canonical,std::slice::from_ref(&change)).await.map_err(|e|e.to_string())}).await.map_err(anyhow::Error::msg)?;
            let after=db.browse(&canonical,&BrowseRequest{limit:2,..Default::default()}).await?;
            check_original(&change,&after)?;
            anyhow::ensure!(super::revalidate(&db,&profile,&persisted).await.is_err(),"Ambiguous committed update was offered for replay");
            let count=db.query(&format!("SELECT n FROM {effects}"),2).await?;
            anyhow::ensure!(count[0].rows[0][0].display()=="1","Canonical UPDATE trigger executed more than once");
            anyhow::Ok(())
        }.await;
        cleanup(&db, &schema).await?;
        result
    }
    #[tokio::test]
    #[ignore = "Requires explicit disposable PostgreSQL on 15435"]
    async fn gui_pass5_recovery_postgres_exact_keys_and_crash_conflicts() -> anyhow::Result<()> {
        live_recovery(rowboat_core::Engine::Postgres).await
    }
    #[tokio::test]
    #[ignore = "Requires explicit disposable MySQL on 13309"]
    async fn gui_pass5_recovery_mysql_exact_keys_and_crash_conflicts() -> anyhow::Result<()> {
        live_recovery(rowboat_core::Engine::MySql).await
    }
    fn fixture() -> (ConnectionProfile, Structure, RowChange) {
        let profile = ConnectionProfile {
            id: "private-profile".into(),
            ..Default::default()
        };
        let columns = ["tenant", "id", "payload"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| rowboat_core::Column {
                name: name.into(),
                data_type: "text".into(),
                primary_key: i < 2,
                nullable: false,
                default_value: None,
            })
            .collect();
        let original = vec![
            ("tenant".into(), Cell::Text("1".into())),
            ("id".into(), Cell::Text("9007199254740993".into())),
            ("payload".into(), Cell::Binary(vec![0, 255])),
        ];
        (
            profile,
            Structure {
                columns,
                ..Default::default()
            },
            RowChange::Update {
                original,
                values: vec![("payload".into(), Cell::Null)],
            },
        )
    }
    fn table() -> TableRef {
        TableRef {
            schema: "private".into(),
            name: "rows".into(),
        }
    }
    #[test]
    fn crash_checkpoint_is_private_detached_lossless_and_does_not_serialize_results_or_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let (profile, structure, change) = fixture();
        let batch = RecoveryBatch::capture(&profile, table(), &structure, &[change]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let state = SessionState {
            version: crate::session::FORMAT_VERSION,
            active_tab: 1,
            tabs: vec![TabBuffer {
                id: 1,
                connection_id: Some(profile.id.clone()),
                title: "SQL".into(),
                sql: "select :secret_parameter".into(),
            }],
            workspaces: vec![WorkspaceBuffer {
                documents: Default::default(),
                staged: vec![batch.clone()],
                connection_id: Some(profile.id.clone()),
                active_tab: 1,
                split_tab: None,
                focused_pane: 0,
                open: true,
            }],
        };
        {
            let store = SessionStore::at(directory.path()).unwrap();
            store.save(&state).unwrap();
            // Simulates dropping the process before any UI could explicitly reopen/revalidate.
        }
        let recovered = SessionStore::at(directory.path()).unwrap().load().unwrap();
        assert_eq!(recovered, state);
        assert_eq!(recovered.workspaces[0].staged[0].changes, batch.changes);
        let path = directory.path().join("session.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            !text.contains("password") && !text.contains("results") && !text.contains("parameters")
        );
        assert!(text.contains("9007199254740993"));
    }
    #[test]
    fn recovery_rejects_changed_profile_schema_missing_composite_key_and_oversized_payload() {
        let (mut profile, mut structure, change) = fixture();
        let batch =
            RecoveryBatch::capture(&profile, table(), &structure, std::slice::from_ref(&change))
                .unwrap();
        profile.host = "another-host".into();
        assert!(batch.matches(&profile, &table(), &structure).is_err());
        profile.host = ConnectionProfile::default().host;
        structure.columns[0].primary_key = false;
        assert!(batch.matches(&profile, &table(), &structure).is_err());
        assert!(
            batch
                .key_filter(&RowChange::Insert {
                    values: vec![("id".into(), Cell::Text("1".into()))]
                })
                .is_err()
        );
        let too_large = RowChange::Insert {
            values: vec![("payload".into(), Cell::Text("a".repeat(MAX_BATCH_BYTES)))],
        };
        assert!(RecoveryBatch::capture(&profile, table(), &structure, &[too_large]).is_err());
        assert!(
            RecoveryBatch::capture(
                &profile,
                table(),
                &structure,
                &vec![change; MAX_CHANGES + 1]
            )
            .is_err()
        );
    }
    #[test]
    fn original_conflicts_and_committed_before_crash_are_never_automatically_replayed() {
        let (_, structure, change) = fixture();
        let RowChange::Update { original, .. } = &change else {
            unreachable!()
        };
        let mut result = QueryResult {
            columns: structure.columns,
            rows: vec![original.iter().map(|(_, v)| v.clone()).collect()],
            ..Default::default()
        };
        assert!(check_original(&change, &result).is_ok());
        result.rows[0][2] = Cell::Null;
        assert!(check_original(&change, &result).is_err());
        let insert = RowChange::Insert {
            values: original.clone(),
        };
        assert!(check_original(&insert, &result).is_err());
        result.rows.clear();
        assert!(check_original(&change, &result).is_err());
        assert!(check_original(&insert, &result).is_ok());
    }
}
