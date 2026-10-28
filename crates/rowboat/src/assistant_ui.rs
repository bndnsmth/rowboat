//! Non-modal, workspace-local assistant. Local context follows the UI; each send
//! freezes its evidence. No database operation is launched by opening the panel.
use crate::{
    ai_ui::{AdviceBlock, advice_blocks},
    app::{Event, Page, SendAssistant, Workbench},
    assistant_context::ContextOptions,
    view::{ai_action, column, icon, label, outlined_action, row},
};
use gpui_kit::{
    base::input::{InputEvent, TextareaState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_services::{
    ai::{self, AgentControl, AgentUpdate},
    ai_context::{self, Message, Scope},
};

pub(crate) struct Assistant {
    pub open: bool,
    pub width: f32,
    pub resize: Option<(Pixels, f32)>,
    pub prompt: Entity<TextareaState>,
    pub options: ContextOptions,
    pub turns: Vec<Turn>,
    pub request: Option<uuid::Uuid>,
    pub plan: Option<serde_json::Value>,
    pub focus: FocusHandle,
    pub(crate) context_expanded: bool,
    sharing_initialized: bool,
    scope: Option<Scope>,
    preview: Option<Entity<TextareaState>>,
    confirm_trust: bool,
    control: AgentControl,
    error: Option<String>,
    notice: Option<String>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}
pub(crate) struct Turn {
    pub id: uuid::Uuid,
    pub question: String,
    pub context: String,
    pub response: String,
    pub error: Option<String>,
    pub complete: bool,
    pub trusted: bool,
    pub tools: usize,
    label: String,
    agent: String,
    scope: Scope,
}
impl Assistant {
    pub fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(2)
                .placeholder("Ask a question or describe the SQL you need…")
        });
        let subscription = cx.subscribe(&prompt, |_, _, _: &InputEvent, cx| cx.notify());
        Self {
            open: false,
            width: 440.,
            resize: None,
            prompt,
            options: Default::default(),
            turns: vec![],
            request: None,
            plan: None,
            focus: cx.focus_handle(),
            context_expanded: true,
            sharing_initialized: false,
            scope: None,
            preview: None,
            confirm_trust: false,
            control: Default::default(),
            error: None,
            notice: None,
            scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }
    pub fn cancel(&mut self) {
        self.control.cancel();
        if let Some(request) = self.request.take()
            && let Some(turn) = self.turns.iter_mut().find(|t| t.id == request)
        {
            turn.error = Some("Request cancelled; partial advice is incomplete. Cancellation is not proof that a database operation stopped.".into());
        }
    }
}
impl Drop for Assistant {
    fn drop(&mut self) {
        self.control.cancel();
    }
}
pub(crate) enum AssistantEvent {
    Update {
        request: uuid::Uuid,
        update: AgentUpdate,
    },
    Finished {
        request: uuid::Uuid,
        result: Result<String, String>,
    },
}

impl Workbench {
    pub(crate) fn sync_assistant_scope(&mut self) {
        let scope = self.assistant_scope();
        if self
            .assistant
            .scope
            .as_ref()
            .is_some_and(|old| old != &scope)
        {
            self.assistant.cancel();
            self.assistant.turns.clear();
            self.assistant.preview = None;
            self.assistant.plan = None;
            self.assistant.options.cells = false;
            self.assistant.confirm_trust = false;
            self.assistant.context_expanded = true;
            self.assistant.notice = Some("Database/session changed. Started a new conversation; the unsent question was kept.".into());
        }
        self.assistant.scope = Some(scope);
        if self.assistant.request.is_some()
            && (self.close_requested
                || !self.assistant_allowed()
                || self
                    .assistant
                    .turns
                    .last()
                    .is_some_and(|t| t.trusted && !self.assistant_trusted()))
        {
            self.assistant.cancel();
        }
    }
    pub(crate) fn toggle_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_requested || !self.session_ready {
            return;
        }
        self.sync_assistant_scope();
        self.assistant.open = !self.assistant.open;
        if self.assistant.open {
            self.initialize_assistant_sharing();
            self.assistant
                .prompt
                .update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.assistant.cancel();
            self.assistant.resize = None;
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }
    fn initialize_assistant_sharing(&mut self) {
        if !self.assistant.sharing_initialized {
            let prefs = &self.shared.borrow().state.preferences.ai;
            self.assistant.options.schema = prefs.include_schema;
            self.assistant.options.metrics = prefs.include_metrics;
            self.assistant.sharing_initialized = true;
        }
    }
    pub(crate) fn ask_about_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.explain else {
            return;
        };
        if state.pending || state.route != self.tx.route {
            return;
        }
        let Some(report) = &state.report else {
            return;
        };
        let plan = serde_json::json!({"captured_sql":state.sql,"report":report,"scope":"Captured plan, not necessarily the current editor query. It can contain expanded parameter values."});
        self.sync_assistant_scope();
        self.dismiss_explain();
        self.assistant.open = true;
        self.initialize_assistant_sharing();
        self.assistant.plan = Some(plan);
        self.assistant.preview = None;
        self.assistant.prompt.update(cx, |input, cx| {
            input.set_value("Review this captured query plan. Explain the evidence, likely bottlenecks, and suggested next measurements or SQL changes.", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }
    fn assistant_allowed(&self) -> bool {
        let settings = self.shared.borrow().state.preferences.ai.clone();
        settings.enabled
            && self
                .profile
                .as_ref()
                .is_none_or(|p| settings.allowed(&p.id))
    }
    fn assistant_trusted(&self) -> bool {
        self.profile.as_ref().is_some_and(|p| {
            self.shared
                .borrow()
                .state
                .preferences
                .ai
                .investigation_allowed(&p.id)
        })
    }
    fn set_assistant_trust(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(id) = self.profile.as_ref().map(|p| p.id.clone()) else {
            return;
        };
        self.assistant.cancel();
        if !self.shared.borrow().writable {
            self.assistant.error =
                Some("Preferences are read-only; investigation permission was not changed.".into());
        } else {
            self.edit_state(|state| {
                if enabled {
                    state.preferences.ai.investigation_connections.insert(id);
                } else {
                    state.preferences.ai.investigation_connections.remove(&id);
                }
            });
            self.assistant.confirm_trust = false;
        }
        cx.notify();
    }
    fn review_assistant_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_assistant_scope();
        // Review is not permission to rebuild a pinned/redacted snapshot. Only
        // sending or explicitly discarding it resumes live context capture.
        if let Some(preview) = &self.assistant.preview {
            preview.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            return;
        }
        match self
            .capture_assistant_context(self.assistant.options, cx)
            .to_json()
        {
            Ok(context) => {
                self.assistant.preview = Some(cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .rows(2)
                        .default_value(context)
                }));
                self.assistant.error = None;
            }
            Err(error) => self.assistant.error = Some(error.to_string()),
        }
        cx.notify();
    }
    pub(crate) fn send_assistant_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_assistant_scope();
        if self.close_requested
            || self.shared.borrow().shell_modal_open
            || self.modal_open()
            || !self.assistant.open
            || self.assistant.confirm_trust
            || !self.assistant_allowed()
            || self.assistant.request.is_some()
        {
            return;
        }
        let outcome = (|| {
            anyhow::ensure!(
                self.assistant.turns.len() < 8,
                "This conversation has reached 8 turns. Choose New chat to continue."
            );
            let question = self.assistant.prompt.read(cx).value().to_string();
            anyhow::ensure!(question.len() <= 16 * 1024, "Question exceeds 16 KiB");
            let context = if let Some(preview) = &self.assistant.preview {
                preview.read(cx).value().to_string()
            } else {
                self.capture_assistant_context(self.assistant.options, cx)
                    .to_json()?
            };
            let history = self
                .assistant
                .turns
                .iter()
                .filter(|t| t.complete && t.error.is_none())
                .flat_map(|t| {
                    [
                        Message {
                            role: "user".into(),
                            text: t.question.clone(),
                        },
                        Message {
                            role: "assistant".into(),
                            text: t.response.clone(),
                        },
                    ]
                })
                .collect::<Vec<_>>();
            let input = ai_context::conversation(&context, &history, &question)?;
            let target = if self.assistant_trusted() {
                anyhow::ensure!(
                    self.database.is_some() && self.namespace.known && !self.busy,
                    "Wait for an idle, connected workspace with known database context before investigating."
                );
                let mut target = self
                    .profile
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("No database target"))?;
                if target.engine != rowboat_core::Engine::Sqlite {
                    target.database = self
                        .namespace
                        .snapshot
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Active database context is unknown"))?
                        .database
                        .clone();
                }
                Some(target)
            } else {
                None
            };
            Ok::<_, anyhow::Error>((question, context, input, target))
        })();
        let (question, context, input, target) = match outcome {
            Ok(value) => value,
            Err(error) => {
                self.assistant.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let settings = self.shared.borrow().state.preferences.ai.clone();
        let request = uuid::Uuid::new_v4();
        let scope = self.assistant_scope();
        let label = format!(
            "{} · {}",
            scope.database.as_deref().unwrap_or("No live database"),
            if self.assistant.preview.is_some() {
                "reviewed snapshot"
            } else {
                "workspace snapshot"
            }
        );
        self.assistant.turns.push(Turn {
            id: request,
            question,
            context,
            response: String::new(),
            error: None,
            complete: false,
            trusted: target.is_some(),
            tools: 0,
            label,
            agent: format!(
                "{} · {}",
                settings.provider.label(),
                if settings.model.is_empty() {
                    "default model"
                } else {
                    &settings.model
                }
            ),
            scope,
        });
        self.assistant.control = AgentControl::default();
        self.assistant.request = Some(request);
        // Once context is captured, give the conversation the room. This only
        // changes presentation; the toolbar can expand even during a request.
        self.assistant.context_expanded = false;
        self.assistant.error = None;
        self.assistant.preview = None;
        self.assistant
            .prompt
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.assistant.scroll.scroll_to_bottom();
        let control = self.assistant.control.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let updates = tx.clone();
            let result = ai::assist(&settings, &input, target, &control, move |update| {
                let _ = updates.send(Event::Assistant(Box::new(AssistantEvent::Update {
                    request,
                    update,
                })));
            })
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Assistant(Box::new(AssistantEvent::Finished {
                request,
                result,
            })));
        });
        cx.notify();
    }
    pub(crate) fn receive_assistant(&mut self, event: AssistantEvent, cx: &mut Context<Self>) {
        self.sync_assistant_scope();
        let follow_bottom =
            self.assistant.scroll.offset().y <= -self.assistant.scroll.max_offset().y + px(24.);
        let request = match &event {
            AssistantEvent::Update { request, .. } | AssistantEvent::Finished { request, .. } => {
                *request
            }
        };
        if self.assistant.request != Some(request) {
            return;
        }
        let Some(turn) = self.assistant.turns.iter_mut().find(|t| t.id == request) else {
            return;
        };
        match event {
            AssistantEvent::Update {
                update: AgentUpdate::Text(text),
                ..
            } => {
                if turn.response.len().saturating_add(text.len()) <= ai::RESPONSE_LIMIT {
                    turn.response.push_str(&text);
                } else {
                    self.assistant.control.cancel();
                    turn.error = Some("Response exceeded the bounded limit".into());
                }
            }
            AssistantEvent::Update {
                update: AgentUpdate::Tool(_),
                ..
            } => turn.tools = turn.tools.saturating_add(1),
            AssistantEvent::Finished { result, .. } => {
                self.assistant.request = None;
                match result {
                    Ok(text) if turn.error.is_none() => {
                        turn.response = text;
                        turn.complete = true;
                    }
                    Err(error) => turn.error = Some(error),
                    _ => {}
                }
            }
        }
        if follow_bottom {
            self.assistant.scroll.scroll_to_bottom();
        }
        cx.notify();
    }
    pub(crate) fn open_assistant_sql(
        &mut self,
        turn_id: uuid::Uuid,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_assistant_scope();
        let Some(turn) = self.assistant.turns.iter().find(|t| t.id == turn_id) else {
            return;
        };
        if !turn.complete || turn.error.is_some() || turn.scope != self.assistant_scope() {
            return;
        }
        if let Some(sql) = ai::suggested_sql(&turn.response).get(index) {
            self.new_query(Some(format!("-- AI-generated suggestion: unverified. Review target, semantics and risks before running.\n{sql}")), window, cx);
        }
    }
    fn render_assistant_turn(
        &self,
        turn: &Turn,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.omarchy().clone();
        let id = turn.id;
        let mut body = column()
            .gap_3()
            .p_3()
            .border_b_1()
            .border_color(theme.divider())
            .child(
                div()
                    .p_3()
                    .bg(theme.inset)
                    .rounded(px(6.))
                    .child(turn.question.clone()),
            )
            .child(label(format!("{} · {}", turn.agent, turn.label), cx));
        if turn.trusted {
            body = body.child(label(
                format!(
                    "Trusted investigation · {} tool events · instructions, not confinement",
                    turn.tools
                ),
                cx,
            ));
        }
        if let Some(error) = &turn.error {
            body = body.child(div().text_color(theme.danger).child(error.clone()));
        }
        if self.assistant.request == Some(id) {
            body = body.child(crate::loading_ui::agent_status(
                "assistant-working",
                if turn.tools > 0 {
                    "Agent is investigating…"
                } else if turn.response.is_empty() {
                    "Working with your agent…"
                } else {
                    "Agent is responding…"
                },
                window,
                cx,
            ));
        }
        for (ordinal, block) in advice_blocks(&turn.response).into_iter().enumerate() {
            match block {
                AdviceBlock::Heading(text) => {
                    body = body.child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(15.))
                            .child(text),
                    )
                }
                AdviceBlock::Text(text) => {
                    body = body.child(div().line_height(relative(1.5)).child(text))
                }
                AdviceBlock::Code {
                    language,
                    sql_index,
                    text,
                } => {
                    let copied = text.clone();
                    let selector = format!("assistant-sql-{id}-{ordinal}");
                    let mut tools = row()
                        .flex_wrap()
                        .p_2()
                        .child(label(language, cx))
                        .child(div().flex_1())
                        .child(
                            outlined_action(
                                SharedString::from(format!("copy-{selector}")),
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
                        tools = tools.child(
                            outlined_action(
                                SharedString::from(selector.clone()),
                                "New SQL tab",
                                turn.complete && turn.error.is_none() && !self.modal_open(),
                                cx,
                                move |s, w, cx| s.open_assistant_sql(id, index, w, cx),
                            )
                            .min_h(px(26.))
                            .debug_selector(move || selector.clone()),
                        );
                    }
                    body = body.child(
                        column()
                            .bg(theme.inset)
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border)
                            .child(tools)
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "assistant-code-{id}-{ordinal}"
                                    )))
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
        if !turn.response.is_empty() {
            let response = turn.response.clone();
            let context = turn.context.clone();
            body = body.child(
                row()
                    .flex_wrap()
                    .child(label(
                        "Unverified · review suggested SQL before running",
                        cx,
                    ))
                    .child(
                        outlined_action(
                            SharedString::from(format!("assistant-copy-{id}")),
                            "Copy",
                            true,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(response.clone()))
                            },
                        )
                        .min_h(px(26.)),
                    )
                    .child(
                        outlined_action(
                            SharedString::from(format!("assistant-context-{id}")),
                            "Copy context",
                            true,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(context.clone()))
                            },
                        )
                        .min_h(px(26.)),
                    ),
            );
        }
        body.into_any_element()
    }
    pub(crate) fn render_assistant(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.omarchy().clone();
        let running = self.assistant.request.is_some();
        let trusted = self.assistant_trusted();
        let scope = self.assistant_scope();
        let mut header = row()
            .p_3()
            .flex_shrink_0()
            .gap_2()
            .child(icon(ui::IconName::Sparkles).text_color(theme.accent))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Workspace AI"),
            )
            .child(div().flex_1())
            .child(
                outlined_action(
                    "assistant-new",
                    "New chat",
                    !self.assistant.turns.is_empty(),
                    cx,
                    |s, _, cx| {
                        s.assistant.cancel();
                        s.assistant.turns.clear();
                        s.assistant.preview = None;
                        s.assistant.plan = None;
                        s.assistant.error = None;
                        s.assistant.context_expanded = true;
                        cx.notify();
                    },
                )
                .min_h(px(28.))
                .debug_selector(|| "assistant-new".into()),
            );
        header = header.child(
            outlined_action("assistant-close", "✕", true, cx, |s, w, cx| {
                s.toggle_assistant(w, cx)
            })
            .accessibility_label("Close AI sidebar")
            .min_h(px(28.))
            .debug_selector(|| "assistant-close".into()),
        );
        let database = scope.database.unwrap_or_else(|| "No live database".into());
        let mut summary = vec![database.clone()];
        if let Some(table) = &self.table {
            summary.push(format!("{}.{}", table.schema, table.name));
        }
        if self.page == Page::Query {
            summary.push(self.tabs[self.active_tab].title.clone());
        }
        if self.assistant.plan.is_some() {
            summary.push("plan attached".into());
        }
        let summary = summary.join(" · ");
        let mut shared = vec!["result metadata"];
        if self.assistant.options.sql {
            shared.push("SQL");
        }
        if self.assistant.options.schema {
            shared.push("schema");
        }
        if self.assistant.options.metrics {
            shared.push("metrics");
        }
        if self.assistant.options.cells {
            shared.push("selected values");
        }
        if self.assistant.plan.is_some() {
            shared.push("captured plan + SQL");
        }
        let summary_hint = if self.assistant.preview.is_some() {
            "The next send uses your pinned reviewed snapshot, not the current selection. Expand Context to review sharing controls.".to_owned()
        } else {
            format!(
                "{summary}\nShared on Send: {}. Expand Context to change sharing or review the snapshot.",
                shared.join(", ")
            )
        };
        let toolbar = row()
            .id("assistant-context-toolbar")
            .debug_selector(|| "assistant-context-toolbar".into())
            .h(px(40.))
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .overflow_hidden()
            .border_y_1()
            .border_color(theme.divider())
            .bg(theme.inset)
            .child(
                outlined_action(
                    "assistant-context-toggle",
                    "Context",
                    true,
                    cx,
                    |s, _, cx| {
                        s.assistant.context_expanded = !s.assistant.context_expanded;
                        cx.notify();
                    },
                )
                .min_h(px(28.))
                .h(px(28.))
                .py_0()
                .self_center()
                .accessibility_label(if self.assistant.context_expanded {
                    "Collapse context settings"
                } else {
                    "Expand context settings"
                })
                .child(
                    icon(if self.assistant.context_expanded {
                        ui::IconName::ChevronDown
                    } else {
                        ui::IconName::ChevronRight
                    })
                    .size(px(14.)),
                )
                .debug_selector(|| "assistant-context-toggle".into()),
            )
            .child(ui::with_tooltip(
                div()
                    .id("assistant-context-summary")
                    .debug_selector(|| "assistant-context-summary".into())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .child(if self.assistant.preview.is_some() {
                        "Reviewed snapshot".to_owned()
                    } else {
                        summary
                    }),
                summary_hint,
            ))
            .when(self.assistant.preview.is_some(), |r| {
                r.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(theme.accent)
                        .child("Pinned"),
                )
            })
            .when(self.assistant.options.cells, |r| {
                r.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(theme.warning)
                        .child("Values"),
                )
            })
            .when(trusted, |r| {
                r.child(ui::with_tooltip(
                    div()
                        .id("assistant-trusted-badge")
                        .debug_selector(|| "assistant-trusted-badge".into())
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(theme.warning)
                        .child("Trusted"),
                    "Agent tools enabled · read-only instructions, not confinement",
                ))
            });
        let mut context = column()
            .p_3()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.divider())
            .bg(theme.inset)
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .bg(theme.accent.opacity(0.1))
                            .child(database),
                    )
                    .when_some(self.table.as_ref(), |r, t| {
                        r.child(label(format!("{}.{}", t.schema, t.name), cx))
                    })
                    .when(self.page == Page::Query, |r| {
                        r.child(label(self.tabs[self.active_tab].title.clone(), cx))
                    }),
            )
            .child(label(
                if self.assistant.preview.is_some() {
                    "Reviewed snapshot pinned · not following selection"
                } else {
                    "Follows your workspace · sent only when you press Send"
                },
                cx,
            ));
        let mut options = row().flex_wrap().gap_2();
        for (key, caption, included) in [
            ("sql", "SQL", self.assistant.options.sql),
            ("schema", "Schema", self.assistant.options.schema),
            ("metrics", "Metrics", self.assistant.options.metrics),
            ("cells", "Selected values", self.assistant.options.cells),
        ] {
            let selector = format!("assistant-share-{key}");
            options = options.child(
                ui::checkbox(
                    SharedString::from(selector.clone()),
                    caption,
                    if included {
                        gpui_kit::base::CheckboxState::Checked
                    } else {
                        gpui_kit::base::CheckboxState::Unchecked
                    },
                    cx,
                )
                .disabled(running || self.assistant.preview.is_some())
                .debug_selector(move || selector.clone())
                .on_click(cx.listener(move |s, _, _, cx| {
                    let options = &mut s.assistant.options;
                    match key {
                        "sql" => options.sql = !options.sql,
                        "schema" => options.schema = !options.schema,
                        "metrics" => options.metrics = !options.metrics,
                        _ => options.cells = !options.cells,
                    }
                    cx.notify();
                })),
            );
        }
        context = context
            .child(options)
            .child(
                row()
                    .flex_wrap()
                    .child(
                        outlined_action(
                            "assistant-review",
                            "Review context",
                            !running,
                            cx,
                            |s, w, cx| s.review_assistant_context(w, cx),
                        )
                        .min_h(px(28.))
                        .debug_selector(|| "assistant-review".into()),
                    )
                    .child(
                        outlined_action(
                            "assistant-settings",
                            "AI settings",
                            !running,
                            cx,
                            |s, w, cx| s.show_ai_settings(w, cx),
                        )
                        .min_h(px(28.)),
                    ),
            )
            .child(
                ui::checkbox(
                    "assistant-trust",
                    "Allow agent investigation",
                    if trusted {
                        gpui_kit::base::CheckboxState::Checked
                    } else {
                        gpui_kit::base::CheckboxState::Unchecked
                    },
                    cx,
                )
                .disabled(self.profile.is_none() || running)
                .debug_selector(|| "assistant-trust".into())
                .on_click(cx.listener(move |s, _, _, cx| {
                    if trusted {
                        s.set_assistant_trust(false, cx);
                    } else {
                        s.assistant.confirm_trust = !s.assistant.confirm_trust;
                        cx.notify();
                    }
                })),
            );
        if trusted {
            context = context.child(
                div()
                    .text_color(theme.warning)
                    .text_size(px(11.))
                    .child("Trusted tools enabled · read-only instructions, not confinement"),
            );
        }
        if self.assistant.plan.is_some() {
            context = context.child(
                outlined_action(
                    "assistant-remove-plan",
                    "✕ Captured plan + SQL attached",
                    !running,
                    cx,
                    |s, _, cx| {
                        s.assistant.plan = None;
                        s.assistant.preview = None;
                        cx.notify();
                    },
                )
                .min_h(px(26.)),
            );
        }
        let context = column()
            .id("assistant-context-controls")
            .flex_shrink_0()
            .child(toolbar)
            .when(self.assistant.context_expanded, |d| {
                d.child(
                    div()
                        .id("assistant-context-details")
                        .debug_selector(|| "assistant-context-details".into())
                        .max_h((window.viewport_size().height - px(390.)).clamp(px(90.), px(300.)))
                        .overflow_y_scroll()
                        .child(context),
                )
            });
        let mut messages = column().gap_2();
        if let Some(note) = &self.assistant.notice {
            messages = messages.child(label(note.clone(), cx).p_3());
        }
        if self.assistant.turns.is_empty() {
            messages = messages.child(column().p_4().gap_3()
                .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).child("Work with your database, not around it."))
                .child(label("Ask questions, author SQL, explain a query, or analyze selected results. Your installed agent may use a cloud provider and retain logs. Result values are excluded unless you opt in. Previous messages stay in this conversation; use New chat to remove them.", cx)));
            for (id, title, question) in [
                (
                    "author",
                    "Help write SQL",
                    "Help me write SQL for this database. Ask what I need if the goal is unclear; do not guess missing schema.",
                ),
                (
                    "explain",
                    "Explain current SQL",
                    "Explain the selected/current SQL, its semantics, and any likely issues.",
                ),
                (
                    "results",
                    "Understand these results",
                    "Explain the current result metadata and selected values, if shared. Be explicit about missing values or incomplete results.",
                ),
            ] {
                messages = messages.child(
                    outlined_action(
                        SharedString::from(format!("assistant-preset-{id}")),
                        title,
                        !running,
                        cx,
                        move |s, w, cx| {
                            s.assistant.prompt.update(cx, |input, cx| {
                                input.set_value(question, w, cx);
                                input.focus(w, cx);
                            });
                            cx.notify();
                        },
                    )
                    .mx_4(),
                );
            }
            messages = messages.child(
                outlined_action(
                    "assistant-performance",
                    "Load query plan…",
                    self.database.is_some() && !self.busy && !self.modal_open(),
                    cx,
                    |s, w, cx| s.explain_query(w, cx),
                )
                .mx_4(),
            );
        }
        for turn in &self.assistant.turns {
            messages = messages.child(self.render_assistant_turn(turn, window, cx));
        }
        let content = if let Some(preview) = &self.assistant.preview {
            column().flex_1().min_h_0().p_3().gap_2()
                .child(label("Edit/redact the next message's context. This snapshot stays pinned until sent or discarded.", cx))
                .child(ui::textarea("assistant-context-preview", preview, window, cx).h_full().debug_selector(|| "assistant-context-preview".into()))
                .child(outlined_action("assistant-live", "Discard preview · use live context", true, cx, |s, _, cx| { s.assistant.preview = None; cx.notify(); }).debug_selector(|| "assistant-live".into())).into_any_element()
        } else {
            div()
                .id("assistant-messages")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.assistant.scroll)
                .child(messages)
                .into_any_element()
        };
        let mut composer = column()
            .p_3()
            .gap_2()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.divider());
        if let Some(error) = &self.assistant.error {
            composer = composer.child(div().text_color(theme.danger).child(error.clone()));
        }
        if !self.assistant_allowed() {
            composer = composer.child(label(
                "Enable AI for this workspace in AI settings before sending.",
                cx,
            ));
        }
        composer = composer
            .child(
                ui::textarea("assistant-prompt", &self.assistant.prompt, window, cx)
                    .h(px(72.))
                    .debug_selector(|| "assistant-prompt".into()),
            )
            .child(
                row()
                    .child(label("Ctrl+Enter · send", cx))
                    .child(div().flex_1())
                    .when(running, |row| {
                        row.child(
                            outlined_action("assistant-cancel", "Stop", true, cx, |s, _, cx| {
                                s.assistant.cancel();
                                cx.notify();
                            })
                            .debug_selector(|| "assistant-cancel".into()),
                        )
                    })
                    .when(!running, |row| {
                        row.child(
                            ai_action(
                                "assistant-send",
                                "Send",
                                true,
                                self.assistant_allowed()
                                    && !self.assistant.prompt.read(cx).value().trim().is_empty(),
                                cx,
                                |s, w, cx| s.send_assistant_message(w, cx),
                            )
                            .debug_selector(|| "assistant-send".into()),
                        )
                    }),
            );
        row()
            .id("assistant-panel")
            .relative()
            .debug_selector(|| "assistant-panel".into())
            .key_context("RowboatAssistant")
            .track_focus(&self.assistant.focus)
            .w(px(self.assistant.width).min(window.viewport_size().width - px(40.)))
            .h_full()
            .flex_shrink_0()
            .min_w_0()
            .bg(theme.surface)
            .border_l_1()
            .border_color(theme.border)
            .on_action(cx.listener(|s, _: &SendAssistant, w, cx| s.send_assistant_message(w, cx)))
            // Broad user-configured SQL shortcuts must never execute the hidden
            // editor while focus belongs to the assistant's text inputs.
            .capture_action(cx.listener(|_, _: &crate::app::RunQuery, _, cx| cx.stop_propagation()))
            .capture_action(cx.listener(|_, _: &crate::app::RunCurrent, _, cx| cx.stop_propagation()))
            .capture_action(cx.listener(|_, _: &crate::app::RunSelection, _, cx| cx.stop_propagation()))
            .capture_action(cx.listener(|_, _: &crate::app::RunAll, _, cx| cx.stop_propagation()))
            .child(
                div()
                    .id("assistant-resize")
                    .debug_selector(|| "assistant-resize".into())
                    .w(px(6.))
                    .h_full()
                    .flex_shrink_0()
                    .cursor_col_resize()
                    .hover(|d| d.bg(theme.accent.opacity(0.3)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|s, event: &MouseDownEvent, _, cx| {
                            s.assistant.resize = Some((event.position.x, s.assistant.width));
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                column()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(header)
                    .child(context)
                    .child(content)
                    .child(composer),
            )
            .when(self.assistant.confirm_trust, |panel| panel.child(
                div().id("assistant-trust-confirmation").absolute().inset_0().occlude().flex().flex_col().justify_center().p_4().bg(theme.surface)
                    .child(column().gap_3()
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(18.)).text_color(theme.warning).child("Allow trusted investigation?"))
                        .child("Trusted mode runs agent shell/read tools under your account. It is instructed to use rowboatctl read-only, but can ignore those instructions. It may access files, keyring-backed databases and network services. No sandbox guarantee. Enable only for a trusted agent and this connection.")
                        .child(outlined_action("assistant-enable-trust", "Enable for this connection", true, cx, |s, _, cx| s.set_assistant_trust(true, cx)).text_color(theme.warning).debug_selector(|| "assistant-enable-trust".into()))
                        .child(outlined_action("assistant-cancel-trust", "Cancel", true, cx, |s, _, cx| { s.assistant.confirm_trust = false; cx.notify(); })))
            ))
            .into_any_element()
    }
}
