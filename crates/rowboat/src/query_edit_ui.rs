//! Query updates retain execution proof; explorer context never supplies a target.
use crate::{
    app::{Event, Page, Workbench},
    forms::{RowEditor, RowTarget},
    workspace_model::Route,
};
use gpui_kit::*;
use rowboat_core::{QueryEditPlan, QueryEditSource, QueryResult, RowChange, Structure};
use std::collections::HashMap;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub route: Route,
    pub tab: usize,
    pub execution: u64,
}
pub struct ResultEdit {
    pub binding: Binding,
    pub capture_requested: bool,
    pub plan: Option<QueryEditPlan>,
    pub reason: String,
}
#[derive(Default)]
pub struct State {
    pub results: HashMap<usize, ResultEdit>,
    next: u64,
    pub draft: Option<Binding>,
    pub batch: Option<Binding>,
    pub review: bool,
}
pub struct Applied {
    pub binding: Binding,
    pub result: Result<u64, String>,
}
impl Workbench {
    pub fn begin_query_edit_capture(&mut self, tab: usize, for_editing: bool) {
        self.query_edit.next = self.query_edit.next.wrapping_add(1);
        let binding = Binding {
            route: self.tx.route,
            tab,
            execution: self.query_edit.next,
        };
        self.query_edit.results.insert(
            tab,
            ResultEdit {
                binding,
                capture_requested: for_editing,
                plan: None,
                reason: if for_editing {
                    "Run for editing · capturing a fresh editable snapshot"
                } else {
                    "Editing not requested"
                }
                .into(),
            },
        );
    }
    pub fn finish_query_edit_capture(
        &mut self,
        tab: usize,
        source: Option<QueryEditSource>,
        failure: Option<&str>,
    ) {
        if self
            .query_edit
            .results
            .get(&tab)
            .is_none_or(|entry| !entry.capture_requested)
        {
            return;
        }
        let plan = (|| {
            if let Some(failure) = failure {
                anyhow::bail!("{failure}");
            }
            let source = source.ok_or_else(|| anyhow::anyhow!("Source proof unavailable"))?;
            if let Some(reason) = source.read_only_reason() {
                anyhow::bail!("{reason}");
            }
            let tab = self
                .tabs
                .iter()
                .find(|t| t.id == tab)
                .ok_or_else(|| anyhow::anyhow!("Query tab closed"))?;
            anyhow::ensure!(
                tab.results.len() == 1,
                "Only a single SELECT result can be edited"
            );
            source.plan(&tab.results[0])
        })();
        if let Some(entry) = self.query_edit.results.get_mut(&tab) {
            match plan {
                Ok(plan) => {
                    entry.reason = format!(
                        "Updates → {}.{} · staged → Review SQL → Apply",
                        plan.table().schema,
                        plan.table().name
                    );
                    entry.plan = Some(plan);
                }
                Err(e) => {
                    entry.plan = None;
                    entry.reason = e.to_string();
                }
            }
        }
    }
    pub fn query_entry(&self) -> Option<&ResultEdit> {
        let tab = &self.tabs[self.active_tab];
        self.query_edit
            .results
            .get(&tab.id)
            .filter(|e| e.binding.route == self.tx.route && tab.result_index == 0)
    }
    pub fn query_plan(&self) -> Option<&QueryEditPlan> {
        self.query_entry()?.plan.as_ref()
    }
    pub fn query_effective_cell(&self, row: usize, column: usize) -> Option<&rowboat_core::Cell> {
        if !self.query_batch_matches() {
            return None;
        }
        let plan = self.query_plan()?;
        let values = self.current_result()?.rows.get(row)?;
        let name = &plan.columns().get(column)?.base_name;
        self.changes.iter().find_map(|change| match change {
            RowChange::Update {
                original,
                values: updated,
            } if original.len() == values.len()
                && plan
                    .result()
                    .columns
                    .iter()
                    .zip(values)
                    .all(|(c, v)| original.iter().any(|(n, o)| n == &c.name && o == v)) =>
            {
                updated.iter().find(|(n, _)| n == name).map(|(_, v)| v)
            }
            _ => None,
        })
    }
    pub fn query_binding(&self) -> Option<Binding> {
        Some(self.query_entry()?.binding.clone())
    }
    pub fn query_batch_matches(&self) -> bool {
        self.query_edit
            .batch
            .as_ref()
            .map_or(self.changes.is_empty(), |b| {
                Some(b) == self.query_entry().map(|e| &e.binding)
            })
    }
    pub fn query_edit_notice(&self) -> String {
        if self.busy {
            return "Read-only while an operation is running".into();
        }
        if !self.query_batch_matches() {
            return "Read-only here · Review the staged batch in its original SQL result".into();
        }
        self.query_entry().map_or_else(
            || "Read-only · run a single base-table SELECT with its complete primary key".into(),
            |e| {
                if e.plan.is_some() {
                    self.tabs[self.active_tab]
                        .query_progress
                        .as_ref()
                        .and_then(|p| p.byte_limit())
                        .map_or_else(
                            || e.reason.clone(),
                            |bytes| {
                                format!(
                                    "{} · {} MiB editing snapshot limit",
                                    e.reason,
                                    bytes / (1024 * 1024)
                                )
                            },
                        )
                } else {
                    format!("Read-only · {}", e.reason)
                }
            },
        )
    }
    pub fn run_for_editing_enabled(&self) -> bool {
        self.page == Page::Query
            && self.database.is_some()
            && self.session_ready
            && !self.busy
            && !self.close_requested
            && !self.has_row_draft()
            && self.changes.is_empty()
            && self.transaction_known
            && !self.transaction
            && self.profile.as_ref().is_some_and(|p| {
                !p.read_only
                    && self.tabs[self.active_tab].connection_id.as_deref() == Some(p.id.as_str())
            })
    }
    pub fn run_query_for_editing(&mut self, cx: &mut Context<Self>) {
        if self.run_for_editing_enabled() {
            self.run_query_with_intent(true, cx);
        }
    }
    pub fn editing_target(&self) -> Option<RowTarget> {
        if self.page != Page::Query {
            return self.current_target();
        }
        Some(RowTarget {
            connection_id: self.profile.as_ref()?.id.clone(),
            session_generation: self.session_generation,
            table: self.query_plan()?.table().clone(),
        })
    }
    pub fn editing_structure(&self) -> Structure {
        if self.page == Page::Query
            && let Some(plan) = self.query_plan()
        {
            return Structure {
                columns: plan.result().columns.clone(),
                ..Default::default()
            };
        }
        self.structure.clone()
    }
    pub fn recovery_edit_context(&self) -> anyhow::Result<(RowTarget, Structure)> {
        if let Some(binding) = &self.query_edit.batch {
            anyhow::ensure!(
                binding.route == self.tx.route,
                "Query batch belongs to an older session"
            );
            let entry = self
                .query_edit
                .results
                .get(&binding.tab)
                .filter(|e| &e.binding == binding)
                .ok_or_else(|| anyhow::anyhow!("Query execution proof changed"))?;
            let plan = entry
                .plan
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Query proof unavailable"))?;
            plan.preview_changes(&self.changes)?;
            return Ok((
                RowTarget {
                    connection_id: self
                        .profile
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("No profile"))?
                        .id
                        .clone(),
                    session_generation: self.session_generation,
                    table: plan.table().clone(),
                },
                Structure {
                    columns: plan.result().columns.clone(),
                    ..Default::default()
                },
            ));
        }
        Ok((
            self.current_target()
                .ok_or_else(|| anyhow::anyhow!("No table target"))?,
            self.structure.clone(),
        ))
    }
    /// Canonical names and cells in the current filtered/displayed row order.
    pub fn editing_result(&self, cx: &App) -> QueryResult {
        if self.page != Page::Query {
            return self.table_result.clone();
        }
        let Some(plan) = self.query_plan() else {
            return QueryResult::default();
        };
        let Some(display) = self.current_result() else {
            return QueryResult::default();
        };
        QueryResult {
            columns: plan.result().columns.clone(),
            rows: self
                .visible_grid_rows(cx)
                .into_iter()
                .map(|i| display.rows[i].clone())
                .collect(),
            ..Default::default()
        }
    }
    pub fn validate_query_changes(&self, changes: &[RowChange]) -> Result<(), String> {
        if self.page != Page::Query {
            return Ok(());
        }
        if !self.query_batch_matches() {
            return Err("Changes belong to another query execution".into());
        }
        self.query_plan()
            .ok_or_else(|| self.query_edit_notice())?
            .preview_changes(changes)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn bind_query_changes(&mut self) {
        if self.page == Page::Query {
            self.query_edit.batch = if self.changes.is_empty() {
                None
            } else {
                self.query_binding()
            };
            self.query_edit.draft = None;
        }
    }
    pub fn query_draft_current(&self) -> bool {
        match &self.query_edit.draft {
            Some(b) => {
                self.page == Page::Query && Some(b) == self.query_entry().map(|e| &e.binding)
            }
            None => self.page != Page::Query,
        }
    }
    pub fn edit_query_row(&mut self, insert: bool, w: &mut Window, cx: &mut Context<Self>) {
        if insert || !self.grid_writable() || self.has_row_draft() {
            return;
        }
        let Some((row, column)) = self.selected_cell() else {
            return;
        };
        let Some(plan) = self.query_plan() else {
            return;
        };
        let Some(values) = self.current_result().and_then(|r| r.rows.get(row)) else {
            return;
        };
        let original: Vec<_> = plan
            .result()
            .columns
            .iter()
            .zip(values)
            .map(|(c, v)| (c.name.clone(), v.clone()))
            .collect();
        let structure = Structure {
            columns: plan
                .result()
                .columns
                .iter()
                .zip(plan.columns())
                .filter(|(_, m)| m.editable)
                .map(|(c, _)| c.clone())
                .collect(),
            ..Default::default()
        };
        let name = plan.columns().get(column).map(|c| c.base_name.clone());
        let mut form = RowEditor::new(
            self.editing_target().unwrap(),
            &structure,
            Some(original.clone()),
            w,
            cx,
        );
        for change in &self.changes {
            if let RowChange::Update {
                original: old,
                values,
            } = change
                && old == &original
            {
                for (name, value) in values {
                    if let Some(field) = form.fields.iter_mut().find(|f| &f.name == name) {
                        field.null = matches!(value, rowboat_core::Cell::Null);
                        field.value.update(cx, |e, cx| {
                            e.set_value(
                                if field.null {
                                    String::new()
                                } else {
                                    value.display()
                                },
                                w,
                                cx,
                            )
                        });
                    }
                }
            }
        }
        if let Some(f) = form
            .fields
            .iter()
            .find(|f| Some(&f.name) == name.as_ref())
            .or_else(|| form.fields.first())
        {
            f.value.update(cx, |e, cx| e.focus(w, cx));
        }
        self.query_edit.draft = self.query_binding();
        self.row_editor = Some(form);
        cx.notify();
    }
    pub fn review_query_changes(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(b) = self.query_edit.batch.clone() else {
            return false;
        };
        if b.route != self.tx.route {
            self.fail("Staged query belongs to an older session".into());
            return true;
        }
        self.focus_query_tab(b.tab, cx);
        self.tabs[self.active_tab].result_index = 0;
        self.page = Page::Query;
        self.query_edit.review = true;
        self.dismiss_completions(cx);
        cx.notify();
        true
    }
    pub fn apply_query_edits(&mut self, cx: &mut Context<Self>) {
        if !self.query_edit.review
            || !self.grid_writable()
            || self.has_row_draft()
            || self.changes.is_empty()
        {
            return;
        }
        let Some(binding) = self.query_edit.batch.clone() else {
            return;
        };
        let Some(plan) = self.query_plan().cloned() else {
            return;
        };
        if let Err(e) = self.validate_query_changes(&self.changes) {
            self.fail(e);
            return;
        }
        let marker = match self.prepare_apply_marker() {
            Ok(id) => id,
            Err(e) => {
                self.fail(format!("Apply blocked: {e:#}"));
                return;
            }
        };
        let (ack, persisted) = tokio::sync::oneshot::channel();
        if self
            .shell_tx
            .send(crate::workspace_shell::Command::PersistApply {
                route: self.tx.route,
                marker,
                ack,
            })
            .is_err()
        {
            self.finish_apply_marker(false);
            self.fail("Apply did not start: recovery coordinator unavailable".into());
            return;
        }
        let db = self.database.clone().unwrap();
        let changes = self.changes.clone();
        let tx = self.tx.clone();
        self.busy = true;
        self.status = "Persisting query Apply intent…".into();
        self.error = None;
        self.runtime.spawn(async move {
            let result = crate::recovery_ui::after_durable_intent(persisted, async {
                db.apply_query_changes(&plan, &changes)
                    .await
                    .map_err(|e| format!("{e:#}"))
            })
            .await;
            let _ = tx.send(Event::TransactionState(
                db.in_transaction().await.map_err(|e| format!("{e:#}")),
            ));
            let _ = tx.send(Event::QueryApplied(Box::new(Applied { binding, result })));
        });
        cx.notify();
    }
    pub fn receive_query_applied(&mut self, applied: Applied, cx: &mut Context<Self>) {
        self.busy = false;
        let confirmed = applied.result.is_ok() && self.transaction_known && !self.transaction;
        self.finish_apply_marker(confirmed);
        self.query_edit.batch = None;
        self.query_edit.draft = None;
        self.query_edit.review = false;
        if let Some(e) = self.query_edit.results.get_mut(&applied.binding.tab)
            && e.binding == applied.binding
        {
            e.plan = None;
            e.reason=if confirmed{"Applied · displayed result is stale; explicitly rerun SQL to refresh"}else{"Apply requires reconciliation · displayed result is stale; inspect before rerunning"}.into();
        }
        match applied.result {
            Ok(rows) => self.status = format!("Applied {rows} query updates · SQL was not rerun"),
            Err(e) => self.fail(e),
        }
        cx.notify();
    }
}
