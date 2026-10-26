//! Estimate-only EXPLAIN and explicitly executed ANALYZE, with exact native metrics.
use crate::{
    app::{Event, Workbench},
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::ActiveTheme;
use rowboat_admin::explain::{AnalyzeIntent, ExplainReport, PlanNode, ReportMode};
use std::collections::BTreeSet;

pub struct ExplainState {
    request: uuid::Uuid,
    route: Route,
    tab: usize,
    title: String,
    sql: String,
    execution: String,
    analyzing: bool,
    pub open: bool,
    pending: bool,
    pub(crate) report: Option<ExplainReport>,
    pub(crate) error: Option<String>,
    collapsed: BTreeSet<Vec<usize>>,
    details: BTreeSet<Vec<usize>>,
}
pub struct Explained {
    request: uuid::Uuid,
    tab: usize,
    result: Result<ExplainReport, String>,
    transaction: Result<bool, String>,
}
impl Workbench {
    pub fn explain_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::Explain, cx) {
            return;
        }
        if !self.can_leave() || self.has_extension_dialog() {
            cx.notify();
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        let tab = &self.tabs[self.active_tab];
        if tab.connection_id.as_ref() != self.profile.as_ref().map(|p| &p.id) {
            self.fail("This editor belongs to another connection".into());
            cx.notify();
            return;
        }
        let selected = tab.editor.read(cx).selected_value();
        let sql = if selected.is_empty() {
            tab.editor.read(cx).value().to_string()
        } else {
            selected.to_string()
        };
        let execution = match forms::parameters(&tab.parameters.read(cx).value())
            .and_then(|values| rowboat_admin::query::expand_named(db.engine(), &sql, &values))
        {
            Ok(sql) => sql,
            Err(e) => {
                self.fail(format!("EXPLAIN parameters: {e:#}"));
                cx.notify();
                return;
            }
        };
        let request = uuid::Uuid::new_v4();
        let tab_id = tab.id;
        self.explain = Some(ExplainState {
            request,
            route: self.tx.route,
            tab: tab_id,
            title: tab.title.clone(),
            sql,
            execution: execution.clone(),
            analyzing: false,
            open: true,
            pending: true,
            report: None,
            error: None,
            collapsed: BTreeSet::new(),
            details: BTreeSet::new(),
        });
        self.busy = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = rowboat_admin::explain::execute(&db, &execution)
                .await
                .map_err(|e| format!("{e:#}"));
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Explained(Box::new(Explained {
                request,
                tab: tab_id,
                result,
                transaction,
            })));
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub fn receive_explained(&mut self, event: Explained, cx: &mut Context<Self>) {
        let Some(state) = &mut self.explain else {
            return;
        };
        if state.request != event.request || state.tab != event.tab || state.route != self.tx.route
        {
            return;
        }
        state.pending = false;
        self.busy = false;
        match event.result {
            Ok(report) => {
                self.status = if report.engine == rowboat_core::Engine::Sqlite {
                    "SQLite query strategy loaded · inspect native plan rows"
                } else {
                    "Query plan report completed · inspect metrics and raw report"
                }
                .into();
                state.report = Some(report);
            }
            Err(e) => {
                self.status =
                    "Query plan operation ended with an error · see report details".into();
                state.error = Some(e);
            }
        }
        match event.transaction {
            Ok(active) => {
                self.transaction = active;
                self.transaction_known = true;
            }
            Err(e) => {
                self.transaction_known = false;
                self.fail(e);
            }
        }
        cx.notify();
    }
    fn analyze_captured_query(&mut self, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::Analyze, cx) {
            return;
        }
        let Some(state) = &mut self.explain else {
            return;
        };
        if self.busy
            || self.close_requested
            || state.pending
            || state.route != self.tx.route
            || !self.tabs.iter().any(|t| t.id == state.tab)
            || !self.transaction_known
        {
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        let request = uuid::Uuid::new_v4();
        state.request = request;
        state.pending = true;
        state.analyzing = true;
        state.report = None;
        state.error = None;
        state.collapsed.clear();
        state.details.clear();
        let execution = state.execution.clone();
        let tab = state.tab;
        self.busy = true;
        self.status = "ANALYZE executing captured SQL once…".into();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result =
                rowboat_admin::explain::analyze(&db, &execution, AnalyzeIntent::ExecuteQuery)
                    .await
                    .map_err(|e| format!("{e:#}"));
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Explained(Box::new(Explained {
                request,
                tab,
                result,
                transaction,
            })));
        });
        cx.notify();
    }
    pub fn dismiss_explain(&mut self) {
        if let Some(state) = &mut self.explain
            && !state.pending
        {
            state.open = false;
        }
    }
    fn render_plan_node(
        &self,
        node: &PlanNode,
        path: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.explain.as_ref().unwrap();
        let collapsed = state.collapsed.contains(&path);
        let details = state.details.contains(&path);
        let theme = cx.omarchy().clone();
        let id = path
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("-");
        let toggle = path.clone();
        let node_selector = format!("plan-node-{id}");
        let copy_selector = format!("plan-copy-{id}");
        let detail_path = path.clone();
        let raw = serde_json::to_string_pretty(&node.raw).unwrap_or_default();
        let mut body = column()
            .gap_1()
            .pl_3()
            .border_l_2()
            .border_color(theme.accent.opacity(0.4))
            .child(
                row()
                    .child(
                        action(
                            SharedString::from(format!("plan-node-{id}")),
                            format!(
                                "{} {}{}{}",
                                if node.children.is_empty() {
                                    "·"
                                } else if collapsed {
                                    "▸"
                                } else {
                                    "▾"
                                },
                                node.operation,
                                node.relation
                                    .as_ref()
                                    .map(|v| format!(" · {v}"))
                                    .unwrap_or_default(),
                                node.access
                                    .as_ref()
                                    .map(|v| format!(" · {v}"))
                                    .unwrap_or_default()
                            ),
                            false,
                            !node.children.is_empty(),
                            cx,
                            move |s, _, cx| {
                                if let Some(e) = &mut s.explain
                                    && !e.collapsed.remove(&toggle)
                                {
                                    e.collapsed.insert(toggle.clone());
                                }
                                cx.notify();
                            },
                        )
                        .debug_selector(move || node_selector.clone()),
                    )
                    .child(action(
                        SharedString::from(format!("plan-detail-{id}")),
                        if details { "Hide detail" } else { "Details" },
                        false,
                        true,
                        cx,
                        move |s, _, cx| {
                            if let Some(e) = &mut s.explain
                                && !e.details.remove(&detail_path)
                            {
                                e.details.insert(detail_path.clone());
                            }
                            cx.notify();
                        },
                    ))
                    .child(
                        action(
                            SharedString::from(format!("plan-copy-{id}")),
                            if self
                                .profile
                                .as_ref()
                                .is_some_and(|p| p.engine == rowboat_core::Engine::Sqlite)
                            {
                                "Copy native row (JSON)"
                            } else {
                                "Copy node JSON"
                            },
                            false,
                            true,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(raw.clone()))
                            },
                        )
                        .debug_selector(move || copy_selector.clone()),
                    ),
            );
        if !node.estimates.is_empty() {
            body = body.child(label(
                format!(
                    "Estimates · {}",
                    node.estimates
                        .iter()
                        .map(|(k, v)| format!("{k}: {}", v.0))
                        .collect::<Vec<_>>()
                        .join(" · ")
                ),
                cx,
            ));
        }
        if !node.actual.is_empty() {
            body = body.child(label(
                format!(
                    "Observed · {}",
                    node.actual
                        .iter()
                        .map(|(name, value)| format!("{name}: {}", value.0))
                        .collect::<Vec<_>>()
                        .join(" · ")
                ),
                cx,
            ));
        }
        if details {
            for (name, value) in &node.predicates {
                body = body.child(label(format!("{name}: {value}"), cx));
            }
            body = body.child(
                div()
                    .p_2()
                    .bg(theme.inset)
                    .font_family(theme.mono_font.clone())
                    .text_size(px(12.))
                    .child(serde_json::to_string_pretty(&node.raw).unwrap_or_default()),
            );
        }
        if !collapsed {
            for (index, child) in node.children.iter().enumerate() {
                let mut child_path = path.clone();
                child_path.push(index);
                body = body.child(self.render_plan_node(child, child_path, cx));
            }
        }
        body.into_any_element()
    }
    pub fn render_explain(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use gpui_omarchy as ui;
        let theme = cx.omarchy().clone();
        let state = self.explain.as_ref().unwrap();
        let sqlite = self
            .profile
            .as_ref()
            .is_some_and(|p| p.engine == rowboat_core::Engine::Sqlite);
        let analyze_supported = self.admin_supported(rowboat_admin::providers::Operation::Analyze);
        let mut body=column().p_4().gap_3().text_size(px(13.))
            .child(row().flex_wrap().child(ui::icon(ui::IconName::Layers).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Query plan")).child(div().flex_1()).child(action("close-visual-explain","Close",false,!state.pending,cx,|s,w,cx|{s.dismiss_explain();w.focus(&s.focus,cx);cx.notify();}).debug_selector(|| "close-visual-explain".into())))
            .child(label(format!("EXPLAIN · {} · tab {}",state.title,state.tab), cx))
            .child(div().h(px(1.)).bg(theme.divider()))
            .child(label(if sqlite { "SQLite EXPLAIN QUERY PLAN · query strategy only; the captured statement is not executed. Native plan rows contain no actual runtime or row-count estimates. ANALYZE profiling is unavailable." } else if state.analyzing { "ANALYZE · actual times are milliseconds, typically per-loop averages. Optimizer costs are not estimated milliseconds; node times are not summed." } else { "Plain EXPLAIN · estimates only. Analyze below really executes the captured statement; SELECT functions can have side effects. Cancellation does not prove rollback." },cx))
            .child(div().p_3().bg(theme.inset).text_size(px(12.)).font_family(theme.mono_font.clone()).child(state.sql.clone()));
        body = body.child(label(
            format!(
                "Captured SQL (parameters expanded in memory only): {}",
                state.execution
            ),
            cx,
        ));
        if analyze_supported {
            body = body.child(
                action(
                    "execute-analyze",
                    "Analyze — execute captured SQL",
                    true,
                    !self.busy && !self.close_requested && self.transaction_known,
                    cx,
                    |s, _, cx| s.analyze_captured_query(cx),
                )
                .debug_selector(|| "execute-analyze".into()),
            );
        }
        if state.pending {
            body = body
                .child(if state.analyzing {
                    "Running ANALYZE…"
                } else {
                    "Loading plan…"
                })
                .child(action(
                    "cancel-profile",
                    "Cancel plan request",
                    false,
                    true,
                    cx,
                    |s, _, cx| s.cancel(cx),
                ));
        }
        if let Some(error) = &state.error {
            body = body.child(label(format!("EXPLAIN failed: {error}"), cx));
        }
        if let Some(report) = &state.report {
            let raw = report.raw_report.clone();
            body = body.child(label(
                format!(
                    "Mode: {:?} · raw format: {:?}",
                    report.mode, report.raw_format
                ),
                cx,
            ));
            if report.mode == ReportMode::Analyze {
                for (name, value) in &report.timing {
                    body = body.child(label(format!("{name}: {} ms", value.0), cx));
                }
            }
            body = body.child(
                action(
                    "copy-plan-json",
                    if sqlite {
                        "Copy native plan rows (JSON)"
                    } else {
                        "Copy complete raw plan (JSON / TREE)"
                    },
                    false,
                    true,
                    cx,
                    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(raw.clone())),
                )
                .debug_selector(|| "copy-plan-json".into()),
            );
            for warning in &report.warnings {
                body = body.child(label(warning.clone(), cx));
            }
            if report.roots.is_empty() {
                body = body.child(label(
                    if sqlite {
                        "No native plan rows returned; SQLite may omit a plan for this statement."
                    } else {
                        "No recognized tree nodes; complete raw report is available above."
                    },
                    cx,
                ));
            }
            for (index, node) in report.roots.iter().enumerate() {
                body = body.child(self.render_plan_node(node, vec![index], cx));
            }
        }
        self.modal(
            div()
                .id("explain-tree-scroll")
                .w(window.viewport_size().width - px(48.))
                .max_h(window.viewport_size().height - px(60.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}
