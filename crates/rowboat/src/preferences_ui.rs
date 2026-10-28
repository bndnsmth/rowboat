//! App-data-only preferences and scoped keymap replacement; native editing keys stay owned by inputs.
use crate::{
    app::*,
    forms,
    view::{action, column, label, row},
};
use gpui_kit::{
    base::input::{InputEvent, InputState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_services::Preferences as AppPreferences;
use std::collections::BTreeMap;

pub struct PreferencesForm {
    original: AppPreferences,
    theme: Entity<ui::ChoiceState>,
    fields: BTreeMap<&'static str, Entity<InputState>>,
    shortcuts: BTreeMap<String, Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
    pub(crate) section: PreferenceSection,
    ai: rowboat_services::ai::AiPreferences,
    probe: Option<(uuid::Uuid, rowboat_services::ai::AgentControl)>,
    probe_result: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreferenceSection {
    Appearance,
    Editor,
    Shortcuts,
    Ai,
}
impl PreferencesForm {
    fn ai_settings(&self, cx: &App) -> anyhow::Result<rowboat_services::ai::AiPreferences> {
        let mut settings = self.ai.clone();
        settings.executable = self.fields["ai_executable"]
            .read(cx)
            .value()
            .trim()
            .to_string();
        settings.model = self.fields["ai_model"].read(cx).value().trim().to_string();
        settings.timeout_secs = self.fields["ai_timeout"].read(cx).value().trim().parse()?;
        settings.validate()?;
        Ok(settings)
    }
    fn invalidate_probe(&mut self) {
        if let Some((_, control)) = self.probe.take() {
            control.cancel();
        }
        self.probe_result = None;
    }
}
impl Drop for PreferencesForm {
    fn drop(&mut self) {
        self.invalidate_probe();
    }
}
pub struct AgentChecked {
    request: uuid::Uuid,
    result: Result<String, String>,
}
fn action_for(id: &str) -> Option<Box<dyn Action>> {
    Some(match id {
        "run_query" => Box::new(RunQuery),
        "new_query" => Box::new(NewQuery),
        "format_query" => Box::new(FormatQuery),
        "quick_open" => Box::new(QuickOpen),
        "new_connection" => Box::new(NewConnection),
        "copy_cell" => Box::new(CopyCell),
        "complete_sql" => Box::new(CompleteSql),
        "close_query" => Box::new(CloseQuery),
        "command_palette" => Box::new(CommandPalette),
        "preferences" => Box::new(ShowPreferences),
        _ => return None,
    })
}
fn shortcut_bindings(
    previous: &BTreeMap<String, String>,
    next: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<KeyBinding>> {
    let mut bindings = vec![];
    for context in ["Rowboat", "Rowboat > Input", "RowboatSqlEditor > Input"] {
        let predicate = Some(std::rc::Rc::new(KeyBindingContextPredicate::parse(
            context,
        )?));
        for (id, chord) in previous {
            let action =
                action_for(id).ok_or_else(|| anyhow::anyhow!("Unknown shortcut action: {id}"))?;
            bindings.push(KeyBinding::load(
                chord,
                Box::new(Unbind(action.name().into())),
                predicate.clone(),
                false,
                None,
                &DummyKeyboardMapper,
            )?);
        }
        for (id, chord) in next {
            let action =
                action_for(id).ok_or_else(|| anyhow::anyhow!("Unknown shortcut action: {id}"))?;
            bindings.push(KeyBinding::load(
                chord,
                action,
                predicate.clone(),
                false,
                None,
                &DummyKeyboardMapper,
            )?);
        }
    }
    Ok(bindings)
}
pub fn bind_shortcuts(
    previous: &BTreeMap<String, String>,
    preferences: &AppPreferences,
    cx: &mut App,
) -> anyhow::Result<BTreeMap<String, String>> {
    preferences.validate()?;
    let next = preferences.effective_shortcuts()?;
    if previous != &next {
        cx.bind_keys(shortcut_bindings(previous, &next)?);
        // GPUI resolves bindings at equal input depth by installation order.
        // Keep assistant Send ahead of broad user-configured SQL shortcuts.
        crate::keyboard_ui::bind_assistant(cx);
    }
    Ok(next)
}
impl Workbench {
    pub(crate) fn show_ai_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .explain
            .as_ref()
            .is_some_and(|state| state.open && state.pending)
        {
            return;
        }
        self.dismiss_explain();
        self.show_preferences(window, cx);
        if let Some(form) = &mut self.preferences_form {
            form.section = PreferenceSection::Ai;
        }
        cx.notify();
    }
    pub fn shortcut_label(&self, id: &str) -> String {
        self.state
            .preferences
            .effective_shortcuts()
            .ok()
            .and_then(|map| map.get(id).cloned())
            .unwrap_or_default()
    }
    pub fn show_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_extension_dialog() {
            return;
        }
        if self.close_requested
            || self.form.is_some()
            || self.has_row_draft()
            || self.transfer.is_some()
            || self.management.is_some()
            || self.grid.inspector.is_some()
            || self.url_input.is_some()
            || self.advanced.designer.is_some()
        {
            return;
        }
        self.quick_open = false;
        self.error = None;
        let original = self.shared.borrow().state.preferences.clone();
        let fields = [
            ("editor_font_size", original.editor_font_size.to_string()),
            ("grid_font_size", original.grid_font_size.to_string()),
            ("grid_row_height", original.grid_row_height.to_string()),
            ("page_size", original.page_size.to_string()),
            ("query_row_limit", original.query_row_limit.to_string()),
            (
                "query_result_limit_mib",
                original.query_result_limit_mib.to_string(),
            ),
            ("ai_executable", original.ai.executable.clone()),
            ("ai_model", original.ai.model.clone()),
            ("ai_timeout", original.ai.timeout_secs.to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name, forms::input(value, "Value", window, cx)))
        .collect::<BTreeMap<_, _>>();
        let shortcuts = rowboat_services::default_shortcuts()
            .into_keys()
            .map(|id| {
                let value = original.shortcuts.get(&id).cloned().unwrap_or_default();
                (id, forms::input(value, "Default (leave empty)", window, cx))
            })
            .collect::<BTreeMap<_, _>>();
        let mut subscriptions: Vec<_> = fields
            .values()
            .chain(shortcuts.values())
            .map(|input| {
                cx.subscribe(input, |s, _, _: &InputEvent, cx| {
                    if let Some(form) = &mut s.preferences_form {
                        form.invalidate_probe();
                    }
                    cx.notify();
                })
            })
            .collect();
        let mut items = vec![ui::ChoiceItem::new("system", "System (default)")];
        items.extend(
            crate::theme_controller::PALETTES
                .iter()
                .map(|p| ui::ChoiceItem::new(p.id, p.name)),
        );
        let selected = items
            .iter()
            .position(|item| item.value.as_ref() == original.theme)
            .unwrap_or_else(|| {
                items.push(ui::ChoiceItem::new(
                    original.theme.clone(),
                    format!("Unavailable: {} (using System)", original.theme),
                ));
                items.len() - 1
            });
        let theme = cx.new(|cx| {
            ui::ChoiceState::new(items, window, cx)
                .label("Application theme")
                .default_selected(selected)
        });
        subscriptions.push(cx.observe(&theme, |_, _, cx| cx.notify()));
        self.preferences_form = Some(PreferencesForm {
            ai: original.ai.clone(),
            original,
            theme,
            fields,
            shortcuts,
            _subscriptions: subscriptions,
            section: PreferenceSection::Appearance,
            probe: None,
            probe_result: None,
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub fn dismiss_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.preferences_form.take().is_none() {
            return false;
        }
        // A focused input disappears with the form. Restore the Rowboat shortcut context.
        window.focus(&self.focus, cx);
        cx.notify();
        true
    }
    pub(crate) fn preferences_theme_open(&self, cx: &App) -> bool {
        self.preferences_form
            .as_ref()
            .is_some_and(|f| f.theme.read(cx).is_open())
    }
    fn save_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = (|| {
            let form = self
                .preferences_form
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No preferences draft"))?;
            let shared = self.shared.borrow();
            anyhow::ensure!(
                shared.writable,
                "App data is not writable; preferences were not saved"
            );
            anyhow::ensure!(
                serde_json::to_value(&form.original)?
                    == serde_json::to_value(&shared.state.preferences)?,
                "Preferences changed in another workspace. Reopen this dialog before editing."
            );
            let value = |name| form.fields[name].read(cx).value().trim().to_string();
            let mut next = form.original.clone();
            next.theme = form
                .theme
                .read(cx)
                .selected()
                .ok_or_else(|| anyhow::anyhow!("Choose an application theme"))?
                .value
                .to_string();
            next.editor_font_size = value("editor_font_size").parse()?;
            next.grid_font_size = value("grid_font_size").parse()?;
            next.grid_row_height = value("grid_row_height").parse()?;
            next.page_size = value("page_size").parse()?;
            next.query_row_limit = value("query_row_limit").parse()?;
            next.query_result_limit_mib = value("query_result_limit_mib").parse()?;
            next.ai = form.ai_settings(cx)?;
            next.shortcuts.clear();
            for (id, input) in &form.shortcuts {
                let chord = input.read(cx).value();
                if !chord.trim().is_empty() {
                    next.shortcuts.insert(
                        id.clone(),
                        rowboat_services::canonical_key_chord(chord.trim())?,
                    );
                }
            }
            next.validate()?;
            let _ = shortcut_bindings(&BTreeMap::new(), &next.effective_shortcuts()?)?;
            Ok::<_, anyhow::Error>(next)
        })();
        match outcome {
            Ok(preferences) => {
                let theme = preferences.theme.clone();
                self.edit_state(|state| state.preferences = preferences);
                crate::theme_controller::set_choice(&theme, cx);
                self.dismiss_preferences(window, cx);
                self.status="Preferences saved · theme and fonts apply live; limits apply to the next execution / page refresh".into();
            }
            Err(e) => self.fail(format!("Preferences: {e:#}")),
        }
        cx.notify();
    }
    fn check_ai_agent(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.preferences_form else {
            return;
        };
        let settings = match form.ai_settings(cx) {
            Ok(settings) => settings,
            Err(error) => {
                form.probe_result = Some(format!("Invalid AI settings: {error}"));
                cx.notify();
                return;
            }
        };
        form.invalidate_probe();
        let request = uuid::Uuid::new_v4();
        let control = rowboat_services::ai::AgentControl::default();
        form.probe = Some((request, control.clone()));
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = rowboat_services::ai::check_agent(&settings, &control)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::AgentChecked(Box::new(AgentChecked {
                request,
                result,
            })));
        });
        cx.notify();
    }
    pub fn receive_agent_checked(&mut self, event: AgentChecked, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.preferences_form
            && form
                .probe
                .as_ref()
                .is_some_and(|(request, _)| *request == event.request)
        {
            form.probe = None;
            form.probe_result = Some(event.result.unwrap_or_else(|error| error));
            cx.notify();
        }
    }
    pub fn render_preferences(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let form = self.preferences_form.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let body = column()
            .p_4()
            .gap_3()
            .text_size(px(13.))
            .child(
                row()
                    .child(ui::icon(ui::IconName::Settings).text_color(theme.accent))
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Application preferences"))
                    .child(div().flex_1())
                    .child(action(
                        "close-preferences",
                        "Cancel",
                        false,
                        true,
                        cx,
                        |s, w, cx| {
                            s.dismiss_preferences(w, cx);
                        },
                    ).debug_selector(|| "close-preferences".into())),
            )
            .child(label(
                "Stored in Rowboat's private app data. Preferences apply across retained workspaces.",
                cx,
            ));
        let mut tabs = row()
            .flex_wrap()
            .gap_2()
            .pb_3()
            .border_b_1()
            .border_color(theme.divider());
        for (section, id, title) in [
            (
                PreferenceSection::Appearance,
                "preferences-tab-appearance",
                "Appearance",
            ),
            (
                PreferenceSection::Editor,
                "preferences-tab-editor",
                "Editor & Results",
            ),
            (
                PreferenceSection::Shortcuts,
                "preferences-tab-shortcuts",
                "Shortcuts",
            ),
            (PreferenceSection::Ai, "preferences-tab-ai", "AI"),
        ] {
            tabs = tabs.child(
                action(
                    id,
                    title,
                    form.section == section,
                    true,
                    cx,
                    move |s, _, cx| {
                        if let Some(form) = &mut s.preferences_form {
                            form.section = section;
                        }
                        cx.notify();
                    },
                )
                .when(section == PreferenceSection::Ai, |button| {
                    button.child(ui::icon(ui::IconName::Sparkles).size(px(14.)))
                })
                .debug_selector(move || id.into()),
            );
        }
        let header = body.child(tabs);
        let mut body = column().p_4().gap_3().text_size(px(13.));
        if form.section == PreferenceSection::Appearance {
            body = body.child(column().gap_2().pt_2()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Appearance"))
                .child(div().w_full().debug_selector(|| "preferences-theme".into())
                    .child(ui::select("preferences-theme-select", &form.theme, window, cx)))
                .child(label("System follows the current Omarchy palette when available, otherwise your desktop's light/dark appearance. Embedded themes work on every machine. Changes apply on Save.", cx))
                .child(label(crate::theme_controller::description(cx), cx)));
        }
        if form.section == PreferenceSection::Editor {
            for (id, title, hint) in [
                ("editor_font_size", "SQL editor font size", "10–32 px"),
                ("grid_font_size", "Result grid font size", "10–32 px"),
                ("grid_row_height", "Minimum grid row height", "20–64 px"),
                ("page_size", "Table page size", "1–10,000 rows"),
                ("query_row_limit", "Query rows per result", "1–100,000 rows"),
                (
                    "query_result_limit_mib",
                    "Retained result budget",
                    "16–512 MiB per connection workspace",
                ),
            ] {
                if id == "editor_font_size" || id == "page_size" {
                    body = body.child(
                        column()
                            .gap_1()
                            .pt_4()
                            .pb_2()
                            .border_t_1()
                            .border_color(theme.divider())
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(
                                if id == "editor_font_size" {
                                    "Typography"
                                } else {
                                    "Results"
                                },
                            ))
                            .child(label(
                                if id == "editor_font_size" {
                                    "Tune editor text and grid density. Changes apply live."
                                } else {
                                    "Limits apply to the next query run or table refresh."
                                },
                                cx,
                            )),
                    );
                }
                body = body.child(
                    row()
                        .flex_wrap()
                        .gap_3()
                        .child(
                            column()
                                .flex_1()
                                .min_w(px(220.))
                                .gap_1()
                                .child(title)
                                .child(label(hint, cx)),
                        )
                        .child(
                            div()
                                .w(px(180.))
                                .max_w_full()
                                .debug_selector(move || id.into())
                                .child(ui::input(
                                    SharedString::from(id),
                                    &form.fields[id],
                                    window,
                                    cx,
                                )),
                        ),
                );
            }
            body = body.child(label(
            "Shared by retained tabs, table results, pins and comparison work in each workspace. Query streams use half the remaining allowance to leave room for delivery. Editing also has a 64 MiB proof limit. This is a retained-data budget; total process RAM is higher.",
            cx,
        ));
        }
        if form.section == PreferenceSection::Shortcuts {
            body = body.child(
            column().gap_1().pt_4().pb_2().border_t_1().border_color(theme.divider())
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Keyboard"))
                .child(label("Leave an override empty to use its default. Text editing, clipboard and undo shortcuts stay protected.", cx)),
        );
            for (id, default) in rowboat_services::default_shortcuts() {
                let title = match id.as_str() {
                    "run_query" => "Run query",
                    "new_query" => "New query tab",
                    "format_query" => "Format SQL",
                    "quick_open" => "Quick open",
                    "new_connection" => "New connection",
                    "copy_cell" => "Copy selected cell",
                    "complete_sql" => "Show SQL completions",
                    "close_query" => "Close query tab",
                    "command_palette" => "Open command palette",
                    "preferences" => "Open preferences",
                    _ => "Shortcut",
                };
                body = body.child(
                    row()
                        .flex_wrap()
                        .gap_3()
                        .child(
                            column()
                                .flex_1()
                                .min_w(px(220.))
                                .gap_1()
                                .child(title)
                                .child(label(format!("Default: {default}"), cx)),
                        )
                        .child(div().w(px(230.)).max_w_full().child(ui::input(
                            SharedString::from(format!("shortcut-{id}")),
                            &form.shortcuts[&id],
                            window,
                            cx,
                        ))),
                );
            }
        }
        if form.section == PreferenceSection::Ai {
            body = body
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Bring your own agent"))
                .child(label("Use an installed, signed-in agent. Rowboat does not install agents, manage API keys or call model APIs. The sidebar shows sharing options and offers an editable context preview. Your agent may use a cloud provider and retain logs.", cx))
                .child(action("ai-enabled", if form.ai.enabled { "☑ AI assistance enabled" } else { "☐ Enable AI assistance" }, false, true, cx, |s, _, cx| {
                    if let Some(form) = &mut s.preferences_form { form.ai.enabled = !form.ai.enabled; }
                    cx.notify();
                }).debug_selector(|| "ai-enabled".into()));
            let mut providers = row().flex_wrap();
            for provider in rowboat_services::ai::AgentProvider::ALL {
                let id = SharedString::from(format!("ai-provider-{}", provider.executable()));
                let selector = id.to_string();
                providers = providers.child(
                    action(
                        id,
                        provider.label(),
                        form.ai.provider == provider,
                        true,
                        cx,
                        move |s, w, cx| {
                            if let Some(form) = &mut s.preferences_form
                                && form.ai.provider != provider
                            {
                                form.ai.provider = provider;
                                // These overrides belong to the old provider,
                                // not to whichever adapter is selected next.
                                for id in ["ai_executable", "ai_model"] {
                                    form.fields[id]
                                        .update(cx, |input, cx| input.set_value("", w, cx));
                                }
                                form.invalidate_probe();
                            }
                            cx.notify();
                        },
                    )
                    .debug_selector(move || selector.clone()),
                );
            }
            body = body.child(providers);
            for (id, title, hint) in [
                (
                    "ai_executable",
                    "Executable override",
                    "Empty: detect the selected agent on PATH. Otherwise use an absolute executable path, not command arguments.",
                ),
                (
                    "ai_model",
                    "Model override",
                    "Optional agent-specific model ID. Empty: agent default. Codex uses clean session defaults, not user tool configuration.",
                ),
                (
                    "ai_timeout",
                    "Request deadline (seconds)",
                    "15–600 seconds. Cancellation stops only this request's process group.",
                ),
            ] {
                body = body.child(
                    column()
                        .gap_1()
                        .child(title)
                        .child(label(hint, cx))
                        .child(ui::input(id, &form.fields[id], window, cx)),
                );
            }
            body = body.child(
                row()
                    .flex_wrap()
                    .child(action(
                        "ai-default-schema",
                        if form.ai.include_schema {
                            "☑ Include selected table metadata by default"
                        } else {
                            "☐ Include selected table metadata by default"
                        },
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            if let Some(form) = &mut s.preferences_form {
                                form.ai.include_schema = !form.ai.include_schema;
                            }
                            cx.notify();
                        },
                    ))
                    .child(action(
                        "ai-default-metrics",
                        if form.ai.include_metrics {
                            "☑ Include existing metrics by default"
                        } else {
                            "☐ Include existing metrics by default"
                        },
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            if let Some(form) = &mut s.preferences_form {
                                form.ai.include_metrics = !form.ai.include_metrics;
                            }
                            cx.notify();
                        },
                    )),
            );
            if let Some(connection) = &self.owner {
                let connection = connection.clone();
                let blocked = form.ai.disabled_connections.contains(&connection);
                body = body.child(
                    action(
                        "ai-block-connection",
                        if blocked {
                            "☑ AI disabled for this connection"
                        } else {
                            "☐ Disable AI for this connection"
                        },
                        false,
                        true,
                        cx,
                        move |s, _, cx| {
                            if let Some(form) = &mut s.preferences_form
                                && !form.ai.disabled_connections.remove(&connection)
                            {
                                form.ai.disabled_connections.insert(connection.clone());
                            }
                            cx.notify();
                        },
                    )
                    .debug_selector(|| "ai-block-connection".into()),
                );
            }
            body = body.child(
                action(
                    "ai-check-agent",
                    if form.probe.is_some() {
                        "Checking installation…"
                    } else {
                        "Check agent availability (no model request)"
                    },
                    false,
                    form.probe.is_none(),
                    cx,
                    |s, _, cx| s.check_ai_agent(cx),
                )
                .debug_selector(|| "ai-check-agent".into()),
            );
            if let Some(connection) = self.owner.clone() {
                let trusted = form.ai.investigation_connections.contains(&connection);
                body = body.child(action("ai-trusted-connection", if trusted { "☑ Trusted investigation enabled for this connection" } else { "☐ Allow trusted agent investigation for this connection" }, false, true, cx, move |s, _, cx| {
                    if let Some(form) = &mut s.preferences_form
                        && !form.ai.investigation_connections.remove(&connection) {
                        form.ai.investigation_connections.insert(connection.clone());
                    }
                    cx.notify();
                }).debug_selector(|| "ai-trusted-connection".into()))
                    .child(div().text_color(theme.warning).child("Trusted mode enables shell/read tools under your account. The agent is instructed to use rowboatctl --read-only, but can ignore instructions and access files, network or databases. This is not an enforced sandbox. Enable only for a trusted agent/connection. Session-only database passwords are not forwarded."));
            }
            if let Some(result) = &form.probe_result {
                body = body.child(label(result.clone(), cx));
            }
            body = body.child(label("Open Workspace AI from the top-right sparkles button. Values are excluded unless you opt in; SQL, definitions and plans can still be sensitive. Advice-only mode disables tools. Performance review remains available from a query plan, with an optional handoff to the sidebar.", cx));
        }
        let mut footer = column()
            .gap_2()
            .p_4()
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.divider());
        if let Some(error) = &self.error {
            footer = footer.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        footer = footer.child(
            row().flex_wrap().child(div().flex_1()).child(
                action(
                    "save-preferences",
                    "Save preferences",
                    true,
                    self.loaded,
                    cx,
                    |s, w, cx| s.save_preferences(w, cx),
                )
                .debug_selector(|| "save-preferences".into()),
            ),
        );
        self.modal(
            column()
                .w(px(720.).min(window.viewport_size().width - px(48.)))
                // Leave room for workspace chrome, the modal focus strip and margins.
                .max_h(window.viewport_size().height - px(136.))
                .when(
                    self.dialogs
                        .size(
                            "preferences",
                            size(self.viewport.width, self.viewport.height - px(40.)),
                        )
                        .is_some(),
                    |body| body.w_full().h_full().max_w_full().max_h_full(),
                )
                .child(header.flex_shrink_0())
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "preferences-scroll-{}",
                            match form.section {
                                PreferenceSection::Appearance => "appearance",
                                PreferenceSection::Editor => "editor",
                                PreferenceSection::Shortcuts => "shortcuts",
                                PreferenceSection::Ai => "ai",
                            }
                        )))
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                )
                .child(footer.flex_shrink_0())
                .into_any_element(),
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::shortcut_bindings;
    use crate::app::{CopyCell, Dismiss, RunQuery};
    use gpui_kit::{KeyBinding, Keymap};
    use std::collections::BTreeMap;
    #[test]
    fn changing_shortcuts_unbinds_only_prior_rowboat_actions_and_preserves_native_copy() {
        let old = rowboat_services::default_shortcuts();
        let mut keymap = Keymap::new(vec![KeyBinding::new("ctrl-c", Dismiss, Some("Input"))]);
        keymap.add_bindings(shortcut_bindings(&BTreeMap::new(), &old).unwrap());
        let mut next = old.clone();
        next.insert("run_query".into(), "alt-r".into());
        keymap.add_bindings(shortcut_bindings(&old, &next).unwrap());
        let bindings = keymap.bindings_for_action(&RunQuery).collect::<Vec<_>>();
        assert_eq!(bindings.len(), 3);
        assert!(bindings.iter().all(|b| b.keystrokes()[0].key() == "r"));
        assert_eq!(keymap.bindings_for_action(&Dismiss).count(), 1);
        assert_eq!(keymap.bindings_for_action(&CopyCell).count(), 3);
    }
}
