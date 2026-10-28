//! Reviewable, workspace-bound local-agent advice. This module never executes
//! suggested SQL or refreshes database evidence on behalf of an agent.
use crate::{
    app::{Event, Workbench},
    forms,
    view::{ai_action, column, icon, label, outlined_action, row},
};
use gpui_kit::{
    base::input::{InputEvent, TextareaState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_services::ai::{self, AgentControl};

pub struct Analysis {
    pub(crate) context: Entity<TextareaState>,
    pub(crate) follow_up: Entity<TextareaState>,
    engine: rowboat_core::Engine,
    sql: String,
    plan: String,
    schema: Option<String>,
    metrics: Option<String>,
    include_schema: bool,
    include_metrics: bool,
    pub(crate) response: String,
    pub(crate) error: Option<String>,
    pub(crate) request: Option<uuid::Uuid>,
    control: AgentControl,
    _subscriptions: Vec<Subscription>,
    view: AiView,
    generated_context: String,
    options_changed: bool,
    review_note: Option<String>,
    source_label: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum AiView {
    Review,
    Advice,
}
impl Drop for Analysis {
    fn drop(&mut self) {
        self.control.cancel();
    }
}
pub enum AiEvent {
    Text {
        request: uuid::Uuid,
        text: String,
    },
    Finished {
        request: uuid::Uuid,
        result: Result<String, String>,
    },
}
impl Workbench {
    pub(crate) fn prepare_ai_analysis(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.shared.borrow().state.preferences.ai.clone();
        let Some(profile) = &self.profile else {
            return;
        };
        if !settings.allowed(&profile.id) {
            self.fail(
                "Enable AI for this connection in Settings → AI before requesting analysis.".into(),
            );
            cx.notify();
            return;
        }
        let Some(state) = &mut self.explain else {
            return;
        };
        if state.pending || state.route != self.tx.route {
            return;
        }
        let Some(report) = &state.report else {
            return;
        };
        let schema = self.table.as_ref().filter(|_| !self.structure.columns.is_empty()).and_then(|table| {
            serde_json::to_string_pretty(&serde_json::json!({"selected_table":table, "structure":self.structure, "scope":"Already loaded selected table only; not necessarily every relation in the query. Metadata may be stale."})).ok()
        });
        let metrics = self.advanced.metrics.as_ref().filter(|sample| sample.engine == profile.engine).and_then(|sample| {
            serde_json::to_string_pretty(&serde_json::json!({"sampled_at_unix_ms":sample.sampled_at_unix_ms, "scope":sample.scope, "counters_reset_at":sample.counters_reset_at, "metrics":sample.metrics, "limits":"Existing point-in-time sample only; not a rate or proof of this query's resource use. Raw rows excluded."})).ok()
        });
        let plan = match serde_json::to_string_pretty(report) {
            Ok(plan) => plan,
            Err(_) => {
                self.fail("Unable to prepare captured plan context".into());
                cx.notify();
                return;
            }
        };
        let context = match ai::performance_context(
            profile.engine,
            &state.sql,
            &plan,
            schema.as_deref().filter(|_| settings.include_schema),
            metrics.as_deref().filter(|_| settings.include_metrics),
        ) {
            Ok(context) => context,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        let generated_context = context.clone();
        let context = forms::textarea(context, window, cx);
        let follow_up = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(2)
                .placeholder("Ask a follow-up about this query…")
        });
        let subscriptions = [&context, &follow_up]
            .into_iter()
            .map(|input| cx.subscribe(input, |_, _, _: &InputEvent, cx| cx.notify()))
            .collect();
        state.ai = Some(Analysis {
            context,
            follow_up,
            engine: profile.engine,
            sql: state.sql.clone(),
            plan,
            schema,
            metrics,
            include_schema: settings.include_schema,
            include_metrics: settings.include_metrics,
            response: String::new(),
            error: None,
            request: None,
            control: AgentControl::default(),
            _subscriptions: subscriptions,
            view: AiView::Review,
            generated_context,
            options_changed: false,
            review_note: None,
            source_label: None,
        });
        state.page = crate::explain_ui::ExplainPage::Ai;
        cx.notify();
    }
    fn rebuild_ai_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ai) = self.explain.as_mut().and_then(|state| state.ai.as_mut()) else {
            return;
        };
        if ai.request.is_some() {
            return;
        }
        match ai::performance_context(
            ai.engine,
            &ai.sql,
            &ai.plan,
            ai.schema.as_deref().filter(|_| ai.include_schema),
            ai.metrics.as_deref().filter(|_| ai.include_metrics),
        ) {
            Ok(context) => {
                ai.generated_context = context.clone();
                ai.context
                    .update(cx, |input, cx| input.set_value(context, window, cx));
                ai.error = None;
                ai.options_changed = false;
                ai.review_note = None;
            }
            Err(error) => ai.error = Some(error.to_string()),
        }
        cx.notify();
    }
    fn toggle_ai_evidence(&mut self, schema: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ai) = self.explain.as_mut().and_then(|state| state.ai.as_mut()) else {
            return;
        };
        if ai.request.is_some() {
            return;
        }
        if schema {
            ai.include_schema = !ai.include_schema;
        } else {
            ai.include_metrics = !ai.include_metrics;
        }
        if ai.context.read(cx).value().as_ref() != ai.generated_context {
            ai.options_changed = true;
            ai.review_note = Some("You edited the preview. Update preview to use these evidence options (replaces those edits).".into());
        } else {
            self.rebuild_ai_context(window, cx);
        }
        cx.notify();
    }
    pub(crate) fn send_ai_analysis(&mut self, cx: &mut Context<Self>) {
        let settings = self.shared.borrow().state.preferences.ai.clone();
        let Some(profile) = &self.profile else {
            return;
        };
        if !settings.allowed(&profile.id) {
            self.fail("AI is disabled for this connection.".into());
            cx.notify();
            return;
        }
        let Some(state) = &mut self.explain else {
            return;
        };
        if state.pending || state.route != self.tx.route || !state.open {
            return;
        }
        let Some(ai) = &mut state.ai else {
            return;
        };
        if ai.request.is_some() {
            return;
        }
        if ai.options_changed {
            ai.error =
                Some("Update the context preview before sending these evidence options.".into());
            cx.notify();
            return;
        }
        let context = ai.context.read(cx).value().to_string();
        if context.trim().is_empty() || context.len() > ai::CONTEXT_LIMIT {
            ai.error = Some("Context must be nonempty and no larger than 192 KiB.".into());
            cx.notify();
            return;
        }
        let request = uuid::Uuid::new_v4();
        ai.control.cancel();
        ai.control = AgentControl::default();
        ai.request = Some(request);
        ai.response.clear();
        ai.error = None;
        ai.view = AiView::Advice;
        ai.source_label = Some(format!(
            "{} · {}",
            settings.provider.label(),
            if settings.model.is_empty() {
                "default model"
            } else {
                &settings.model
            }
        ));
        let control = ai.control.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let updates = tx.clone();
            let result = ai::analyze(&settings, &context, &control, move |text| {
                let _ = updates.send(Event::Ai(Box::new(AiEvent::Text { request, text })));
            })
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(Event::Ai(Box::new(AiEvent::Finished { request, result })));
        });
        cx.notify();
    }
    pub(crate) fn receive_ai(&mut self, event: AiEvent, cx: &mut Context<Self>) {
        let Some(state) = &mut self.explain else {
            return;
        };
        if !state.open || state.route != self.tx.route {
            return;
        }
        let Some(ai) = &mut state.ai else {
            return;
        };
        match event {
            AiEvent::Text { request, text } if ai.request == Some(request) => {
                if ai.response.len().saturating_add(text.len()) <= ai::RESPONSE_LIMIT {
                    ai.response.push_str(&text);
                } else {
                    ai.control.cancel();
                    ai.error = Some("AI response exceeded its bounded limit.".into());
                }
            }
            AiEvent::Finished { request, result } if ai.request == Some(request) => {
                ai.request = None;
                match result {
                    Ok(response) => ai.response = response,
                    Err(error) => ai.error = Some(error),
                }
            }
            _ => return,
        }
        cx.notify();
    }
    pub(crate) fn cancel_ai_analysis(&mut self, cx: &mut Context<Self>) {
        if let Some(ai) = self.explain.as_mut().and_then(|state| state.ai.as_mut()) {
            ai.control.cancel();
            // Invalidate immediately; late chunks/completion cannot revive it.
            ai.request = None;
            ai.error =
                Some("Request cancelled. Any displayed partial advice is incomplete.".into());
        }
        cx.notify();
    }
    pub(crate) fn review_ai_follow_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ai) = self.explain.as_mut().and_then(|state| state.ai.as_mut()) else {
            return;
        };
        if ai.request.is_some() || ai.response.is_empty() {
            return;
        }
        let question = ai.follow_up.read(cx).value();
        if question.trim().is_empty() {
            return;
        }
        let context = format!(
            "{}\n\nPREVIOUS UNTRUSTED AGENT ADVICE:\n{}\n\nFOLLOW-UP QUESTION:\n{}",
            ai.context.read(cx).value(),
            ai.response,
            question
        );
        if context.len() > ai::CONTEXT_LIMIT {
            ai.error = Some(
                "Follow-up exceeds 192 KiB. Trim the reviewed context before continuing.".into(),
            );
        } else {
            ai.context
                .update(cx, |input, cx| input.set_value(context, window, cx));
            ai.follow_up
                .update(cx, |input, cx| input.set_value("", window, cx));
            ai.error = None;
            ai.view = AiView::Review;
            ai.review_note =
                Some("Follow-up added. Review what will be shared, then send when ready.".into());
        }
        cx.notify();
    }
    pub(crate) fn open_ai_suggestion(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &self.explain else {
            return;
        };
        if state.route != self.tx.route || self.close_requested {
            return;
        }
        let Some(ai) = &state.ai else {
            return;
        };
        if ai.request.is_some() || ai.error.is_some() {
            return;
        }
        let Some(sql) = ai::suggested_sql(&ai.response).get(index).cloned() else {
            return;
        };
        self.dismiss_explain();
        let tabs = self.tabs.len();
        self.new_query(Some(format!("-- AI-generated suggestion: unverified. Review semantics, target and operational risks before running.\n{sql}")), window, cx);
        if self.tabs.len() > tabs {
            self.tabs[self.active_tab].title = "AI suggestion · unverified".into();
            self.status = "AI SQL opened for review only. Nothing executed; verify semantics, target and index tradeoffs before running.".into();
        }
        cx.notify();
    }
    pub(crate) fn render_ai_analysis(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.explain.as_ref().unwrap();
        let settings = self.shared.borrow().state.preferences.ai.clone();
        let theme = cx.omarchy().clone();
        let allowed = self
            .profile
            .as_ref()
            .is_some_and(|profile| settings.allowed(&profile.id));
        let Some(ai) = &state.ai else {
            let mut card = column().p_6().gap_4().max_w(px(660.)).w_full()
                .child(div().size(px(44.)).flex().items_center().justify_center().rounded(px(12.)).bg(theme.accent.opacity(0.12)).child(icon(ui::IconName::Sparkles).size(px(24.)).text_color(theme.accent)))
                .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(if allowed { "Find out what's slowing this query down." } else { "Your agent. Your query. You're in control." }))
                .child(div().text_color(theme.secondary).child(if allowed { "Review a query and its execution plan with your installed agent. Get explanations, rewrite ideas and index candidates—not automatic changes." } else { "Connect an installed Codex, Claude Code or OpenCode agent in AI settings. Rowboat never applies the agent's suggestions for you." }));
            if !allowed {
                card = card.child(
                    ai_action(
                        "configure-ai",
                        "AI settings",
                        true,
                        !state.pending,
                        cx,
                        |s, w, cx| s.show_ai_settings(w, cx),
                    )
                    .debug_selector(|| "configure-ai".into()),
                );
            } else if state.pending {
                card = card
                    .child(
                        row()
                            .child(crate::loading_ui::status(window, cx))
                            .child("Preparing estimated plan…"),
                    )
                    .child(outlined_action(
                        "cancel-ai-plan",
                        "Cancel",
                        true,
                        cx,
                        |s, _, cx| s.cancel(cx),
                    ));
            } else if state.report.is_some() {
                card = card.child(ai_action(
                    "review-ai-context",
                    "Review query context",
                    true,
                    true,
                    cx,
                    |s, w, cx| s.prepare_ai_analysis(w, cx),
                ));
            } else {
                card = card.child(column().p_3().gap_2().bg(theme.inset).rounded(px(6.))
                    .child(label("Captured SQL editor query", cx))
                    .child(div().font_family(theme.mono_font.clone()).text_size(px(12.)).max_h(px(100.)).overflow_hidden().child(if state.sql.trim().is_empty() { "Write a query in the SQL editor first.".to_owned() } else { state.sql.clone() })))
                    .child(ai_action("ai-prepare-plan", "Prepare query analysis", true, !self.busy && !state.sql.trim().is_empty(), cx, |s, _, cx| s.load_captured_plan(cx)).debug_selector(|| "ai-prepare-plan".into()))
                    .child(label("Loads an estimated EXPLAIN plan, not EXPLAIN ANALYZE. Nothing is sent to your agent until you review the context and choose Analyze query.", cx));
            }
            if let Some(error) = &state.error {
                card = card.child(
                    div()
                        .p_3()
                        .rounded(px(6.))
                        .bg(theme.danger.opacity(0.1))
                        .text_color(theme.danger)
                        .child(error.clone()),
                );
            }
            return div()
                .id("ai-welcome-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().flex().justify_center().py_5().px_4().child(card))
                .into_any_element();
        };
        let running = ai.request.is_some();
        if ai.view == AiView::Review {
            let bytes = ai.context.read(cx).value().len();
            let mut preview = column().gap_3().p_4()
                .child(row().flex_wrap().child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Review what you're sharing"))
                    .child(div().flex_1()).child(label("SQL + captured plan", cx)))
                .child(div().p_3().rounded(px(6.)).bg(theme.accent.opacity(0.08)).child(label("Your agent may use a cloud model and retain logs. Redact sensitive SQL literals and plan predicates. No credentials or result rows are supplied.", cx)));
            let mut evidence = row().flex_wrap().gap_3();
            for (schema, id, title, included, enabled) in [
                (
                    true,
                    "ai-context-schema",
                    "Selected table & indexes",
                    ai.include_schema,
                    ai.schema.is_some(),
                ),
                (
                    false,
                    "ai-context-metrics",
                    "Existing metric sample",
                    ai.include_metrics,
                    ai.metrics.is_some(),
                ),
            ] {
                evidence = evidence.child(
                    ui::checkbox(
                        id,
                        title,
                        if included {
                            gpui_kit::base::CheckboxState::Checked
                        } else {
                            gpui_kit::base::CheckboxState::Unchecked
                        },
                        cx,
                    )
                    .debug_selector(move || id.into())
                    .disabled(!enabled)
                    .on_click(cx.listener(move |s, _, w, cx| s.toggle_ai_evidence(schema, w, cx))),
                );
            }
            preview = preview.child(evidence);
            if let Some(note) = &ai.review_note {
                preview = preview.child(label(note.clone(), cx));
            }
            if let Some(error) = &ai.error {
                preview = preview.child(div().text_color(theme.danger).child(error.clone()));
            }
            let footer = row()
                .flex_wrap()
                .flex_shrink_0()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(theme.divider())
                .child(
                    column()
                        .gap_1()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(settings.provider.label()),
                        )
                        .child(label(
                            format!(
                                "{} · {} KiB / 192 KiB",
                                if settings.model.is_empty() {
                                    "Default model"
                                } else {
                                    &settings.model
                                },
                                bytes.div_ceil(1024)
                            ),
                            cx,
                        )),
                )
                .when(
                    ai.options_changed
                        || ai.context.read(cx).value().as_ref() != ai.generated_context,
                    |row| {
                        row.child(
                            outlined_action(
                                "ai-rebuild-context",
                                "Update preview (replaces edits)",
                                true,
                                cx,
                                |s, w, cx| s.rebuild_ai_context(w, cx),
                            )
                            .debug_selector(|| "ai-rebuild-context".into()),
                        )
                    },
                )
                .when(!ai.response.is_empty(), |row| {
                    row.child(outlined_action(
                        "back-to-ai-advice",
                        "Back to advice",
                        true,
                        cx,
                        |s, _, cx| {
                            if let Some(ai) = s.explain.as_mut().and_then(|state| state.ai.as_mut())
                            {
                                ai.view = AiView::Advice;
                            }
                            cx.notify();
                        },
                    ))
                })
                .child(
                    ai_action(
                        "send-ai-analysis",
                        "Analyze query",
                        true,
                        allowed
                            && !state.pending
                            && !ai.options_changed
                            && bytes > 0
                            && bytes <= ai::CONTEXT_LIMIT,
                        cx,
                        |s, _, cx| s.send_ai_analysis(cx),
                    )
                    .debug_selector(|| "send-ai-analysis".into()),
                );
            return column()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .id("ai-review-options-scroll")
                        .max_h(px(210.))
                        .overflow_y_scroll()
                        .flex_shrink_0()
                        .child(preview),
                )
                .child(
                    div().flex_1().min_h(px(100.)).px_4().pb_4().child(
                        ui::textarea("ai-reviewed-context", &ai.context, window, cx)
                            .h_full()
                            .debug_selector(|| "ai-reviewed-context".into()),
                    ),
                )
                .child(footer)
                .into_any_element();
        }
        let mut toolbar = row().flex_wrap().px_4().py_3().flex_shrink_0().child(
            column()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(if running {
                    "Analyzing your query…"
                } else {
                    "Performance advice"
                }))
                .child(label(
                    format!(
                        "{} · unverified, not applied",
                        ai.source_label
                            .as_deref()
                            .unwrap_or(settings.provider.label())
                    ),
                    cx,
                )),
        );
        if running {
            toolbar = toolbar
                .child(crate::loading_ui::agent_status(
                    "ai-analysis-working",
                    "Agent working…",
                    window,
                    cx,
                ))
                .child(
                    outlined_action(
                        "cancel-ai-analysis",
                        "Cancel request",
                        true,
                        cx,
                        |s, _, cx| s.cancel_ai_analysis(cx),
                    )
                    .debug_selector(|| "cancel-ai-analysis".into()),
                );
        } else {
            toolbar = toolbar.child(outlined_action(
                "review-ai-context",
                "Review context",
                true,
                cx,
                |s, _, cx| {
                    if let Some(ai) = s.explain.as_mut().and_then(|state| state.ai.as_mut()) {
                        ai.view = AiView::Review;
                    }
                    cx.notify();
                },
            ));
            if !ai.response.is_empty() {
                let response = ai.response.clone();
                toolbar = toolbar.child(outlined_action(
                    "copy-ai-advice",
                    "Copy advice",
                    true,
                    cx,
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(response.clone()))
                    },
                ));
            }
        }
        let mut answer = column().p_4().pt_0().gap_3();
        if let Some(error) = &ai.error {
            answer = answer.child(
                div()
                    .p_3()
                    .bg(theme.danger.opacity(0.08))
                    .rounded(px(6.))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if running && ai.response.is_empty() {
            answer = answer.child(
                column()
                    .py_8()
                    .gap_3()
                    .items_center()
                    .child(
                        icon(ui::IconName::Sparkles)
                            .size(px(28.))
                            .text_color(theme.accent),
                    )
                    .child("Your agent is reviewing the evidence.")
                    .child(label("Nothing will be applied to the database.", cx)),
            );
        }
        for (ordinal, block) in advice_blocks(&ai.response).into_iter().enumerate() {
            match block {
                AdviceBlock::Heading(text) => {
                    answer = answer.child(
                        div()
                            .pt_2()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(text),
                    )
                }
                AdviceBlock::Text(text) => {
                    answer = answer.child(
                        div()
                            .text_size(px(13.))
                            .line_height(relative(1.5))
                            .child(text),
                    )
                }
                AdviceBlock::Code {
                    language,
                    sql_index,
                    text,
                } => {
                    let copied = text.clone();
                    let mut tools = row()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.divider())
                        .child(label(
                            if sql_index.is_some() {
                                "SQL suggestion · review before running".to_owned()
                            } else {
                                language
                            },
                            cx,
                        ))
                        .child(div().flex_1())
                        .child(
                            outlined_action(
                                SharedString::from(format!("copy-ai-code-{ordinal}")),
                                "Copy",
                                true,
                                cx,
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()))
                                },
                            )
                            .min_h(px(26.)),
                        );
                    if let Some(index) = sql_index {
                        let selector = format!("open-ai-sql-{index}");
                        tools = tools.child(
                            outlined_action(
                                SharedString::from(selector.clone()),
                                "Open SQL in editor",
                                !running && ai.error.is_none(),
                                cx,
                                move |s, w, cx| s.open_ai_suggestion(index, w, cx),
                            )
                            .min_h(px(26.))
                            .text_color(theme.accent)
                            .debug_selector(move || selector.clone()),
                        );
                    }
                    answer = answer.child(
                        column()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.inset)
                            .overflow_hidden()
                            .child(tools)
                            .child(
                                div()
                                    .id(SharedString::from(format!("ai-code-scroll-{ordinal}")))
                                    .overflow_x_scroll()
                                    .p_3()
                                    .font_family(theme.mono_font.clone())
                                    .text_size(px(12.))
                                    .child(text),
                            ),
                    );
                }
            }
        }
        let mut body = column().flex_1().min_h_0().child(toolbar).child(
            div()
                .id("ai-advice-scroll")
                .debug_selector(|| "ai-advice-scroll".into())
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(answer),
        );
        if !running && ai.error.is_none() && !ai.response.is_empty() {
            body = body.child(
                column()
                    .flex_shrink_0()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.divider())
                    .bg(theme.inset)
                    .child(
                        ui::textarea("ai-follow-up", &ai.follow_up, window, cx)
                            .h(px(64.))
                            .debug_selector(|| "ai-follow-up".into()),
                    )
                    .child(
                        row()
                            .child(label("Ask about this evidence. Review before sending.", cx))
                            .child(div().flex_1())
                            .child(
                                ai_action(
                                    "review-ai-follow-up",
                                    "Review follow-up",
                                    false,
                                    !ai.follow_up.read(cx).value().trim().is_empty(),
                                    cx,
                                    |s, w, cx| s.review_ai_follow_up(w, cx),
                                )
                                .min_h(px(30.)),
                            ),
                    ),
            );
        }
        body.into_any_element()
    }
}

/// Passive formatting only: no HTML, remote images, link activation or scripts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdviceBlock {
    Heading(String),
    Text(String),
    Code {
        language: String,
        sql_index: Option<usize>,
        text: String,
    },
}
pub(crate) fn advice_blocks(markdown: &str) -> Vec<AdviceBlock> {
    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut fence: Option<(String, String)> = None;
    let mut sql_count = 0usize;
    let flush = |paragraph: &mut String, blocks: &mut Vec<AdviceBlock>| {
        if !paragraph.trim().is_empty() {
            blocks.push(AdviceBlock::Text(
                std::mem::take(paragraph)
                    .trim()
                    .replace("**", "")
                    .replace('`', ""),
            ));
        }
    };
    for line in markdown.lines() {
        if let Some((_, text)) = &mut fence {
            if line.trim() == "```" {
                let (language, text) = fence.take().unwrap();
                let sql_index = if language.eq_ignore_ascii_case("sql")
                    && !text.trim().is_empty()
                    && sql_count < 16
                {
                    let index = sql_count;
                    sql_count += 1;
                    Some(index)
                } else {
                    None
                };
                blocks.push(AdviceBlock::Code {
                    language,
                    sql_index,
                    text: text.trim().to_owned(),
                });
            } else {
                text.push_str(line);
                text.push('\n');
            }
        } else if let Some(language) = line.trim().strip_prefix("```") {
            flush(&mut paragraph, &mut blocks);
            fence = Some((language.to_owned(), String::new()));
        } else if line.trim().is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else if let Some(title) = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "))
            .or_else(|| line.strip_prefix("# "))
        {
            flush(&mut paragraph, &mut blocks);
            blocks.push(AdviceBlock::Heading(title.replace("**", "")));
        } else {
            if !paragraph.is_empty() {
                paragraph.push('\n');
            }
            if let Some(item) = line
                .trim_start()
                .strip_prefix("- ")
                .or_else(|| line.trim_start().strip_prefix("* "))
            {
                paragraph.push_str("• ");
                paragraph.push_str(item);
            } else {
                paragraph.push_str(line);
            }
        }
    }
    flush(&mut paragraph, &mut blocks);
    if let Some((language, text)) = fence {
        blocks.push(AdviceBlock::Code {
            language,
            sql_index: None,
            text,
        });
    }
    blocks
}

#[cfg(test)]
mod presentation_tests {
    use super::{AdviceBlock, advice_blocks};
    #[test]
    fn advice_is_passive_and_only_complete_sql_fences_are_actionable() {
        let blocks = advice_blocks(
            "## Evidence\n\n- **Observed** scan\n\n```sql\nSELECT 1;\n```\n![untrusted](https://example.invalid/private)\n```text\n```sql\nnot SQL\n```\n```sql\nunclosed",
        );
        assert!(matches!(&blocks[0], AdviceBlock::Heading(title) if title == "Evidence"));
        assert!(blocks.iter().any(|b| matches!(b, AdviceBlock::Code { sql_index: Some(0), text, .. } if text == "SELECT 1;")));
        assert_eq!(
            blocks
                .iter()
                .filter(|b| matches!(
                    b,
                    AdviceBlock::Code {
                        sql_index: Some(_),
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(blocks.iter().any(
            |b| matches!(b, AdviceBlock::Text(text) if text.contains("https://example.invalid"))
        ));
    }
}
