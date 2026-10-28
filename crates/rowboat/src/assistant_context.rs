//! One projection of workspace state for every assistant entry point. Capturing
//! context is local-only: no database queries, metric sampling or agent calls.
use crate::app::{Page, Workbench};
use gpui_kit::App;
use rowboat_core::{Cell, Engine};
use rowboat_services::ai_context::{
    QueryContext, ResultContext, Scope, SelectedCell, WorkspaceContext,
};
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy)]
pub(crate) struct ContextOptions {
    pub sql: bool,
    pub schema: bool,
    pub metrics: bool,
    pub cells: bool,
}
impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            sql: true,
            schema: false,
            metrics: false,
            cells: false,
        }
    }
}
fn bounded(text: &str, limit: usize) -> (String, bool) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), end < text.len())
}
fn evidence<T: serde::Serialize>(
    value: &T,
    limit: usize,
    name: &str,
    notes: &mut Vec<String>,
) -> Option<serde_json::Value> {
    // Serialization is bounded by already retained evidence. Oversized sections
    // are omitted whole rather than presenting a silently incomplete definition.
    match serde_json::to_vec(value) {
        Ok(bytes) if bytes.len() <= limit => serde_json::from_slice(&bytes).ok(),
        _ => {
            notes.push(format!("{name} omitted: exceeds context budget"));
            None
        }
    }
}
impl Workbench {
    pub(crate) fn assistant_scope(&self) -> Scope {
        Scope {
            workspace: self.workspace_id,
            session_generation: self.session_generation,
            connection_id: self.profile.as_ref().map(|p| p.id.clone()),
            engine: self.profile.as_ref().map(|p| p.engine),
            database: self.profile.as_ref().and_then(|p| {
                if p.engine == Engine::Sqlite {
                    Some("SQLite file (path not shared)".into())
                } else {
                    self.namespace
                        .snapshot
                        .as_ref()
                        .map(|s| s.database.clone())
                        .or_else(|| Some(p.database.clone()))
                }
            }),
        }
    }
    pub(crate) fn capture_assistant_context(
        &self,
        options: ContextOptions,
        cx: &App,
    ) -> WorkspaceContext {
        let mut limits = vec![
            "Only already loaded evidence; no query is executed by context capture. No credentials, saved profiles, query history or parameter input controls attached. Executed SQL/plan predicates can contain expanded parameter values.".into(),
            "Explorer selection is not SQL search_path. Metadata/metrics may be stale. Selected cells are original loaded values, not staged edits. Result source may differ from current editor SQL.".into(),
        ];
        if self.database.is_none() {
            limits.push("Disconnected workspace. No live investigation target.".into());
        }
        if !self.namespace.known {
            limits.push("Current database context is unverified. Scope database is the last observed/requested target, not proof of the live session context.".into());
        }
        let tab = &self.tabs[self.active_tab];
        let query = (self.page == Page::Query).then(|| {
            let editor = tab.editor.read(cx);
            let full = editor.value();
            let selected = editor.selected_range();
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            full.hash(&mut hash);
            selected.start.hash(&mut hash);
            selected.end.hash(&mut hash);
            let text = if selected.is_empty() {
                full.as_ref()
            } else {
                &full[selected.clone()]
            };
            let (sql, truncated) = bounded(text, 32 * 1024);
            QueryContext {
                tab: tab.id,
                title: tab.title.clone(),
                revision: hash.finish(),
                sql: options.sql.then_some(sql),
                selected_bytes: (!selected.is_empty()).then_some((selected.start, selected.end)),
                truncated: options.sql && truncated,
                error: options
                    .sql
                    .then(|| tab.error.as_deref().map(|s| bounded(s, 4096).0))
                    .flatten(),
            }
        });
        let pin = self.results.selected_snapshot();
        let result = if !self.grid_is_rendered() {
            None
        } else if self.results_read_only() && pin.is_none() {
            limits.push(
                "Result comparison is visible; underlying live result rows were not attached."
                    .into(),
            );
            None
        } else {
            pin.map(|p| &p.result).or_else(|| self.current_result()).map(|result| {
                let origin = pin.map(|p| &p.origin).or_else(|| if self.page == Page::Table { self.results.table_origin.as_ref() } else { self.results.query_origins.get(&tab.id) });
                let source = options.sql.then(|| origin.map(|o| bounded(&o.source, 32 * 1024))).flatten();
                if source.as_ref().is_some_and(|(_, cut)| *cut) { limits.push("Executed result source was truncated to 32 KiB.".into()); }
                let mut selected_cells = vec![];
                let mut omitted = 0usize;
                if options.cells {
                    let positions: Vec<(usize, usize)> = if pin.is_some() {
                        vec![(self.results.selected_row, self.results.selected_column)]
                    } else {
                        let visible = self.visible_grid_rows(cx);
                        self.current_origin().as_ref().and_then(|o| self.selected.as_ref()?.range(o, &visible)).map(|range| {
                            let start = range.start(); let end = range.end();
                            let total = (end.row - start.row + 1).saturating_mul(end.column - start.column + 1);
                            let mut positions = Vec::new();
                            for &row in visible.iter().take(end.row + 1).skip(start.row).take(20) {
                                for col in start.column..=end.column {
                                    if positions.len() == 100 { break; }
                                    positions.push((row, col));
                                }
                            }
                            omitted = total.saturating_sub(positions.len());
                            positions
                        }).unwrap_or_default()
                    };
                    let mut bytes = 0usize;
                    for (row, column) in positions {
                        if let Some(value) = result.rows.get(row).and_then(|r| r.get(column)) {
                            let cost = match value { Cell::Null => 8, Cell::Text(s) => s.len().saturating_mul(6), Cell::Binary(b) => b.len().saturating_mul(5) };
                            if cost > 4096 || bytes + cost > 24 * 1024 { omitted += 1; continue; }
                            bytes += cost;
                            selected_cells.push(SelectedCell { row, column, value: value.clone() });
                        }
                    }
                }
                if result.columns.len() > 128 { limits.push("Only the first 128 result columns are described.".into()); }
                let completeness = pin.map(|p| p.completeness.clone()).unwrap_or_else(|| {
                    if self.page == Page::Query && tab.query_progress.as_ref().is_some_and(|p| p.result_pending(tab.result_index)) { "Pending/incomplete execution" }
                    else { "Loaded result; may be a bounded page or partial batch. Not proof of a whole table." }.into()
                });
                ResultContext {
                    kind: if pin.is_some() { "pinned" } else if self.page == Page::Table { "table page" } else { "query" }.into(),
                    result_index: pin.map_or(tab.result_index, |p| p.result_index),
                    columns: result.columns.iter().take(128).cloned().map(|mut c| { c.default_value = None; c }).collect(),
                    retained_rows: result.rows.len(), truncated: result.truncated, completeness,
                    source: source.map(|(s, _)| s), database: origin.filter(|o| o.engine != Engine::Sqlite).map(|o| o.database.clone()),
                    selected_cells, omitted_selected_cells: omitted,
                }
            })
        };
        let schema_and_indexes = options
            .schema
            .then(|| self.table.as_ref().map(|table| (table, &self.structure)))
            .flatten()
            .and_then(|data| evidence(&data, 48 * 1024, "Selected table schema", &mut limits));
        let existing_metric_sample = options
            .metrics
            .then_some(self.advanced.metrics.as_ref())
            .flatten()
            .filter(|m| Some(m.engine) == self.profile.as_ref().map(|p| p.engine))
            .and_then(|m| evidence(&serde_json::json!({"engine":m.engine,"sampled_at_unix_ms":m.sampled_at_unix_ms,"scope":m.scope,"counters_reset_at":m.counters_reset_at,"metrics":m.metrics,"limits":"Existing counters only, not rates or proof of query resource use. Raw metric result rows excluded."}), 16 * 1024, "Metrics", &mut limits));
        let captured_plan = self
            .assistant
            .plan
            .as_ref()
            .and_then(|plan| evidence(plan, 48 * 1024, "Captured plan", &mut limits));
        WorkspaceContext {
            version: 1,
            captured_at: chrono::Utc::now().to_rfc3339(),
            scope: self.assistant_scope(),
            destination: match self.page {
                Page::Query => "SQL editor",
                Page::Table => "Table",
                Page::History => "Query library",
                Page::Tools => "Tools",
                Page::Objects => "Objects",
                Page::Dashboard => "Dashboard",
            }
            .into(),
            sql_context: self
                .namespace
                .snapshot
                .as_ref()
                .filter(|_| self.namespace.known)
                .and_then(|s| s.current_schema.clone()),
            selected_table: self.table.clone(),
            query,
            result,
            schema_and_indexes,
            captured_plan,
            existing_metric_sample,
            transaction: self.transaction_known.then_some(self.transaction),
            pending_edits: self.changes.len(),
            limits,
        }
    }
}
