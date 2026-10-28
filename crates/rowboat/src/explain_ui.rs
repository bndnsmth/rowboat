//! Estimate-only EXPLAIN and explicitly executed ANALYZE, with exact native metrics.
use crate::{
    app::{Event, Workbench},
    forms,
    view::{action, ai_action, column, icon, label, outlined_action, row},
    workspace_model::Route,
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::ActiveTheme;
use rowboat_admin::explain::{AnalyzeIntent, ExplainReport, PlanNode, ReportMode};
use std::collections::BTreeSet;

pub struct ExplainState {
    request: uuid::Uuid,
    pub(crate) route: Route,
    tab: usize,
    title: String,
    pub(crate) sql: String,
    execution: String,
    analyzing: bool,
    pub open: bool,
    pub(crate) pending: bool,
    pub(crate) ai: Option<crate::ai_ui::Analysis>,
    pub(crate) report: Option<ExplainReport>,
    pub(crate) error: Option<String>,
    collapsed: BTreeSet<Vec<usize>>,
    details: BTreeSet<Vec<usize>>,
    pub(crate) page: ExplainPage,
    show_sql: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplainPage {
    Plan,
    Ai,
}
pub struct Explained {
    request: uuid::Uuid,
    tab: usize,
    result: Result<ExplainReport, String>,
    transaction: Result<bool, String>,
}
impl Workbench {
    pub fn explain_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_performance(ExplainPage::Plan, window, cx);
    }
    pub(crate) fn ai_entry_enabled(&self) -> bool {
        !self.close_requested
            && (!self.modal_open()
                || self.quick_open
                || self
                    .explain
                    .as_ref()
                    .is_some_and(|state| state.open && !state.pending))
            && self.session_ready
    }
    pub(crate) fn open_ai_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ai_entry_enabled() {
            return;
        }
        if self
            .explain
            .as_ref()
            .is_some_and(|state| state.open && state.report.is_some())
        {
            self.ask_about_plan(window, cx);
        } else {
            self.toggle_assistant(window, cx);
        }
    }
    fn open_performance(&mut self, page: ExplainPage, window: &mut Window, cx: &mut Context<Self>) {
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
            pending: false,
            ai: None,
            report: None,
            error: None,
            collapsed: BTreeSet::new(),
            details: BTreeSet::new(),
            page,
            show_sql: false,
        });
        if page == ExplainPage::Plan {
            self.load_captured_plan(cx);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub(crate) fn load_captured_plan(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.explain else {
            return;
        };
        if self.busy
            || state.pending
            || !state.open
            || state.route != self.tx.route
            || self.close_requested
        {
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        let request = uuid::Uuid::new_v4();
        let execution = state.execution.clone();
        let tab_id = state.tab;
        state.request = request;
        state.pending = true;
        state.error = None;
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
        cx.notify();
    }
    pub fn receive_explained(
        &mut self,
        event: Explained,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if self
            .explain
            .as_ref()
            .is_some_and(|state| state.page == ExplainPage::Ai && state.report.is_some())
            && self.profile.as_ref().is_some_and(|profile| {
                self.shared
                    .borrow()
                    .state
                    .preferences
                    .ai
                    .allowed(&profile.id)
            })
        {
            self.prepare_ai_analysis(window, cx);
        }
        cx.notify();
    }
    pub(crate) fn show_plan_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &mut self.explain else {
            return;
        };
        state.page = ExplainPage::Ai;
        if state.ai.is_none()
            && state.report.is_some()
            && self.profile.as_ref().is_some_and(|profile| {
                self.shared
                    .borrow()
                    .state
                    .preferences
                    .ai
                    .allowed(&profile.id)
            })
        {
            self.prepare_ai_analysis(window, cx);
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
        state.ai = None;
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
            state.ai = None;
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
        let title = format!(
            "{}{}",
            node.operation,
            node.relation
                .as_ref()
                .map(|name| format!(" · {name}"))
                .unwrap_or_default()
        );
        let heading = if node.children.is_empty() {
            // A leaf is information, not a disabled action. Keep it at full
            // contrast and out of the focus order.
            row()
                .id(SharedString::from(format!("plan-node-{id}")))
                .debug_selector(move || node_selector.clone())
                .flex_1()
                .min_w_0()
                .child(
                    icon(gpui_omarchy::IconName::FileCode)
                        .size(px(14.))
                        .text_color(theme.accent),
                )
                .child(div().font_weight(FontWeight::MEDIUM).child(title))
                .into_any_element()
        } else {
            action(
                SharedString::from(format!("plan-node-{id}")),
                format!("{} {title}", if collapsed { "▸" } else { "▾" }),
                false,
                true,
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
            .font_weight(FontWeight::MEDIUM)
            .debug_selector(move || node_selector.clone())
            .into_any_element()
        };
        let mut body = column()
            .gap_2()
            .pl_3()
            .border_l_2()
            .border_color(theme.accent.opacity(0.4))
            .child(
                row()
                    .flex_wrap()
                    .p_2()
                    .bg(theme.inset)
                    .rounded(px(6.))
                    .child(heading)
                    .when_some(node.access.as_ref(), |row, access| {
                        row.child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(px(4.))
                                .bg(theme.accent.opacity(0.1))
                                .text_color(theme.accent)
                                .text_size(px(11.))
                                .child(access.clone()),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        outlined_action(
                            SharedString::from(format!("plan-detail-{id}")),
                            if details { "Hide detail" } else { "Details" },
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
                        )
                        .min_h(px(28.)),
                    )
                    .child(
                        outlined_action(
                            SharedString::from(format!("plan-copy-{id}")),
                            "Copy JSON",
                            true,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(raw.clone()))
                            },
                        )
                        .min_h(px(28.))
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
        let title = column()
            .gap_1()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Query performance"),
            )
            .child(label(
                format!(
                    "{} · {} · captured query",
                    self.profile
                        .as_ref()
                        .map(|p| p.engine.descriptor().name)
                        .unwrap_or("SQL"),
                    state.title
                ),
                cx,
            ));
        let mut header = row()
            .flex_shrink_0()
            .px_4()
            .py_3()
            .gap_3()
            .child(
                div()
                    .p_2()
                    .rounded(px(8.))
                    .bg(theme.accent.opacity(0.1))
                    .child(
                        icon(if state.page == ExplainPage::Ai {
                            ui::IconName::Sparkles
                        } else {
                            ui::IconName::Layers
                        })
                        .size(px(20.))
                        .text_color(theme.accent),
                    ),
            )
            .child(title);
        if state.page == ExplainPage::Plan {
            header = header.child(
                ai_action(
                    "prepare-ai-analysis",
                    "Ask AI",
                    true,
                    !state.pending && state.report.is_some(),
                    cx,
                    |s, w, cx| s.show_plan_ai(w, cx),
                )
                .debug_selector(|| "prepare-ai-analysis".into()),
            );
            header = header.child(
                outlined_action(
                    "plan-to-sidebar",
                    "Ask in sidebar",
                    !state.pending && state.report.is_some(),
                    cx,
                    |s, w, cx| s.ask_about_plan(w, cx),
                )
                .debug_selector(|| "plan-to-sidebar".into()),
            );
        }
        header = header.child(
            outlined_action(
                "close-visual-explain",
                "Close",
                !state.pending,
                cx,
                |s, w, cx| {
                    s.dismiss_explain();
                    w.focus(&s.focus, cx);
                    cx.notify();
                },
            )
            .debug_selector(|| "close-visual-explain".into()),
        );
        let mut tabs = row()
            .flex_shrink_0()
            .px_4()
            .gap_3()
            .border_b_1()
            .border_color(theme.divider());
        for (page, id, title, glyph) in [
            (
                ExplainPage::Plan,
                "performance-tab-plan",
                "Execution plan",
                ui::IconName::Layers,
            ),
            (
                ExplainPage::Ai,
                "performance-tab-ai",
                "AI assistant",
                ui::IconName::Sparkles,
            ),
        ] {
            tabs = tabs.child(
                action(id, "", false, true, cx, move |s, w, cx| {
                    if page == ExplainPage::Ai {
                        s.show_plan_ai(w, cx);
                    } else if let Some(state) = &mut s.explain {
                        state.page = page;
                        cx.notify();
                    }
                })
                .accessibility_label(title)
                .rounded_none()
                .px_3()
                .h(px(38.))
                .border_b_2()
                .border_color(if state.page == page {
                    theme.accent
                } else {
                    transparent_black()
                })
                .text_color(if state.page == page {
                    theme.accent
                } else {
                    theme.secondary
                })
                .child(icon(glyph).size(px(14.)))
                .child(title)
                .debug_selector(move || id.into()),
            );
        }
        let mut body = column()
            .w_full()
            .h_full()
            .text_size(px(13.))
            .child(header)
            .child(tabs);
        if state.page == ExplainPage::Ai {
            body = body.child(self.render_ai_analysis(window, cx));
        } else {
            let mut toolbar = row()
                .flex_wrap()
                .flex_shrink_0()
                .px_4()
                .py_3()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .bg(theme.inset)
                        .text_size(px(11.))
                        .child(if sqlite {
                            "SQLite query strategy"
                        } else if state.analyzing {
                            "Actual execution"
                        } else {
                            "Estimated plan"
                        }),
                )
                .child(outlined_action(
                    "show-captured-sql",
                    if state.show_sql {
                        "Hide SQL"
                    } else {
                        "Captured SQL"
                    },
                    true,
                    cx,
                    |s, _, cx| {
                        if let Some(state) = &mut s.explain {
                            state.show_sql = !state.show_sql;
                        }
                        cx.notify();
                    },
                ))
                .child(div().flex_1());
            if let Some(report) = &state.report {
                let raw = report.raw_report.clone();
                toolbar = toolbar.child(
                    outlined_action(
                        "copy-plan-json",
                        "Copy raw plan",
                        true,
                        cx,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(raw.clone()))
                        },
                    )
                    .child(icon(ui::IconName::Copy).size(px(14.)))
                    .debug_selector(|| "copy-plan-json".into()),
                );
            }
            if analyze_supported {
                toolbar = toolbar.child(
                    outlined_action(
                        "execute-analyze",
                        "Run EXPLAIN ANALYZE",
                        !self.busy
                            && !self.close_requested
                            && self.transaction_known
                            && state.ai.as_ref().is_none_or(|ai| ai.request.is_none()),
                        cx,
                        |s, _, cx| s.analyze_captured_query(cx),
                    )
                    .text_color(theme.warning)
                    .debug_selector(|| "execute-analyze".into()),
                );
            }
            body = body.child(toolbar);
            let mut plan = column().p_4().pt_0().gap_3()
                .child(div().p_3().rounded(px(6.)).bg(theme.inset).child(label(if sqlite {
                    "Query strategy only. SQLite does not report actual runtime or row-count estimates here."
                } else if state.analyzing {
                    "Actual times are milliseconds (often per-loop averages). Optimizer costs are not time; node timings cannot be added."
                } else {
                    "Estimates only. EXPLAIN ANALYZE really runs the captured SQL; SELECT functions can have side effects. Cancellation does not prove rollback."
                }, cx)));
            if state.show_sql {
                plan = plan.child(
                    column()
                        .p_3()
                        .gap_2()
                        .bg(theme.inset)
                        .rounded(px(6.))
                        .child(label(
                            "Captured SQL · parameter values may be sensitive",
                            cx,
                        ))
                        .child(
                            div()
                                .font_family(theme.mono_font.clone())
                                .text_size(px(12.))
                                .child(state.execution.clone()),
                        ),
                );
            }
            if state.pending {
                plan = plan.child(
                    row()
                        .child(crate::loading_ui::status(window, cx))
                        .child("Loading query plan…")
                        .child(outlined_action(
                            "cancel-profile",
                            "Cancel",
                            true,
                            cx,
                            |s, _, cx| s.cancel(cx),
                        )),
                );
            }
            if let Some(error) = &state.error {
                plan = plan.child(
                    div()
                        .text_color(theme.danger)
                        .child(format!("EXPLAIN failed: {error}")),
                );
            }
            if let Some(report) = &state.report {
                if report.mode == ReportMode::Analyze {
                    let mut metrics = row().flex_wrap();
                    for (name, value) in &report.timing {
                        metrics = metrics.child(
                            div()
                                .p_2()
                                .bg(theme.inset)
                                .rounded(px(4.))
                                .child(format!("{name}: {} ms", value.0)),
                        );
                    }
                    plan = plan.child(metrics);
                }
                for warning in &report.warnings {
                    plan = plan.child(label(warning.clone(), cx));
                }
                if report.roots.is_empty() {
                    plan = plan.child(label(
                        "No recognized plan nodes. Use Copy raw plan to inspect the native report.",
                        cx,
                    ));
                }
                for (index, node) in report.roots.iter().enumerate() {
                    plan = plan.child(self.render_plan_node(node, vec![index], cx));
                }
            } else if !state.pending {
                plan = plan.child(outlined_action(
                    "load-estimated-plan",
                    "Load estimated plan",
                    true,
                    cx,
                    |s, _, cx| s.load_captured_plan(cx),
                ));
            }
            body = body.child(
                div()
                    .id("explain-tree-scroll")
                    .min_h_0()
                    .flex_1()
                    .overflow_scroll()
                    .child(plan),
            );
        }
        self.modal(body.into_any_element(), cx)
    }
}
