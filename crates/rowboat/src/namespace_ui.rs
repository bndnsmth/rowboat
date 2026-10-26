//! Explicit explorer/session context, owned by the retained physical workspace.
use crate::{
    app::*,
    forms,
    view::{action, column, icon, label, row},
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui};
use rowboat_core::{NamespaceBehavior, NamespaceCatalog, QueryResult, Structure, TableInfo};
use std::collections::HashMap;

#[derive(Default)]
pub struct NamespaceState {
    pub snapshot: Option<NamespaceCatalog>,
    pub selected: Option<String>,
    pub known: bool,
    pub request: u64,
    pub picker: Option<Picker>,
    pub collapsed: [bool; 2],
    pub result_context: HashMap<usize, String>,
}
pub struct Picker {
    pub filter: Entity<gpui_kit::base::input::InputState>,
    pub cursor: usize,
    scroll: ScrollHandle,
    route: crate::workspace_model::Route,
    epoch: u64,
    _subscription: Subscription,
}
pub struct Loaded {
    pub query_timing: Option<(usize, u64)>,
    pub request: u64,
    pub selected: Option<String>,
    pub error: Option<String>,
    pub result: Result<NamespaceCatalog, String>,
}
pub fn is_view(info: &TableInfo) -> bool {
    info.kind.to_ascii_lowercase().contains("view")
}
impl Workbench {
    pub(crate) fn namespace_behavior(&self) -> NamespaceBehavior {
        self.profile
            .as_ref()
            .map_or(NamespaceBehavior::Schemas, |p| {
                p.engine.descriptor().namespaces
            })
    }
    pub fn namespace_scope(&self) -> Option<&str> {
        if self.namespace_behavior() == NamespaceBehavior::Databases {
            self.namespace
                .snapshot
                .as_ref()
                .map(|s| s.database.as_str())
        } else {
            self.namespace.selected.as_deref().or_else(|| {
                self.namespace
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.current_schema.as_deref())
            })
        }
    }
    pub fn namespace_label(&self) -> String {
        let scope = self.namespace_scope().unwrap_or("No namespace");
        format!(
            "{}{}",
            scope,
            if self.namespace.snapshot.is_some() && !self.namespace.known {
                " · unverified"
            } else {
                ""
            }
        )
    }
    pub fn namespace_switch_allowed(&self) -> bool {
        self.database.is_some()
            && self.namespace.known
            && !self.busy
            && !self.transaction
            && self.transaction_known
            && !self.has_row_draft()
            && self.changes.is_empty()
            && !self.close_requested
            && !self.shared.borrow().shell_modal_open
            && (!self.modal_open() || self.namespace.picker.is_some())
    }
    pub fn show_namespaces(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.namespace_switch_allowed() || self.modal_open() {
            return;
        }
        let filter = forms::input("", "Search database / schema…", w, cx);
        filter.update(cx, |e, cx| e.focus(w, cx));
        let subscription = cx.subscribe(
            &filter,
            |s, _, event: &gpui_kit::base::input::InputEvent, cx| {
                if matches!(event, gpui_kit::base::input::InputEvent::Change) {
                    if let Some(p) = s.namespace.picker.as_mut() {
                        p.cursor = 0;
                        p.scroll.set_offset(point(px(0.), px(0.)));
                    }
                    cx.notify();
                }
            },
        );
        let cursor = self
            .namespace
            .snapshot
            .as_ref()
            .and_then(|s| {
                s.namespaces
                    .iter()
                    .position(|n| Some(n.as_str()) == self.namespace_scope())
            })
            .unwrap_or(0);
        self.namespace.picker = Some(Picker {
            filter,
            cursor,
            scroll: ScrollHandle::new(),
            route: self.tx.route,
            epoch: self.host_epoch,
            _subscription: subscription,
        });
        if let Some(p) = &self.namespace.picker {
            p.scroll.scroll_to_item(cursor % 50);
        }
        cx.notify();
    }
    pub fn namespace_choices(&self, cx: &App) -> Vec<String> {
        let filter = self
            .namespace
            .picker
            .as_ref()
            .map(|p| p.filter.read(cx).value().to_lowercase())
            .unwrap_or_default();
        self.namespace
            .snapshot
            .as_ref()
            .map(|s| {
                s.namespaces
                    .iter()
                    .filter(|n| n.to_lowercase().contains(&filter))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn namespace_key(&mut self, key: &str, w: &mut Window, cx: &mut Context<Self>) {
        let names = self.namespace_choices(cx);
        let Some(p) = self.namespace.picker.as_mut() else {
            return;
        };
        p.cursor = p.cursor.min(names.len().saturating_sub(1));
        match key {
            "up" => p.cursor = p.cursor.saturating_sub(1),
            "down" => p.cursor = (p.cursor + 1).min(names.len().saturating_sub(1)),
            "home" => p.cursor = 0,
            "end" => p.cursor = names.len().saturating_sub(1),
            "enter" => {
                if let Some(name) = names.get(p.cursor) {
                    self.choose_namespace(name.clone(), false, w, cx);
                }
            }
            _ => {}
        }
        if let Some(p) = &self.namespace.picker {
            p.scroll.scroll_to_item(p.cursor % 50);
        }
        cx.notify();
    }
    pub fn choose_namespace(
        &mut self,
        name: String,
        new_workspace: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.namespace_switch_allowed() {
            return;
        }
        let Some(p) = self.namespace.picker.as_ref() else {
            return;
        };
        if p.route != self.tx.route
            || p.epoch != self.host_epoch
            || !self
                .namespace
                .snapshot
                .as_ref()
                .is_some_and(|s| s.namespaces.contains(&name))
        {
            return;
        }
        let switches_database = self.namespace_behavior() == NamespaceBehavior::Databases;
        if new_workspace {
            if !switches_database || !self.shared.borrow().writable {
                return;
            }
            let _ = self
                .shell_tx
                .send(crate::workspace_shell::Command::OpenDatabase {
                    route: self.tx.route,
                    host_epoch: self.host_epoch,
                    database: name,
                });
            self.namespace.picker = None;
            w.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        self.namespace.picker = None;
        w.focus(&self.focus, cx);
        self.load_namespace(Some(name), switches_database, cx);
    }
    pub fn load_namespace(
        &mut self,
        selected: Option<String>,
        switch: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.close_requested {
            return;
        }
        if switch && !self.namespace_switch_allowed() {
            return;
        }
        self.start_namespace_load(selected, switch, None, cx);
    }
    /// Required completion work, not a user context change: close freeze must
    /// not suppress the actual-context read or prepared-cache synchronization.
    /// Keep working preserves `busy` until this request's reply is consumed.
    pub(crate) fn sync_terminal_namespace(&mut self, tab: usize, cx: &mut Context<Self>) {
        self.namespace.known = false;
        self.start_namespace_load(self.namespace.selected.clone(), false, Some(tab), cx);
    }
    fn start_namespace_load(
        &mut self,
        selected: Option<String>,
        switch: bool,
        query_tab: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_completions(cx);
        let Some(db) = self.database.clone() else {
            return;
        };
        self.namespace.request = self.namespace.request.wrapping_add(1);
        let request = self.namespace.request;
        let tx = self.tx.clone();
        self.busy = true;
        self.namespace.known = false;
        let started = std::time::Instant::now();
        self.runtime.spawn(async move {
            let error = if switch {
                match selected.as_deref() {
                    Some(name) => db
                        .select_database(name)
                        .await
                        .err()
                        .map(|e| format!("Database switch failed: {e:#}")),
                    None => Some("No database selected".into()),
                }
            } else {
                None
            };
            // Read the actual context even after a rejected USE. Never display the
            // requested name optimistically; transport failure makes it unverified.
            let scope = if db.engine().descriptor().namespaces == NamespaceBehavior::Databases {
                None
            } else {
                selected.as_deref()
            };
            // Bound completion even while the close dialog is waiting for busy
            // work. Core's cancellation lease retires uncertain wire state.
            let result = match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                db.namespace_catalog(scope),
            )
            .await
            {
                Ok(result) => result.map_err(|e| format!("{e:#}")),
                Err(_) => Err("Context refresh timed out; reconnect required".into()),
            };
            let _ = tx.send(Event::Namespace(Box::new(Loaded {
                query_timing: query_tab.map(|tab| (tab, started.elapsed().as_millis() as u64)),
                request,
                selected,
                error,
                result,
            })));
        });
        cx.notify();
    }
    pub fn receive_namespace(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        if loaded.request != self.namespace.request {
            return;
        }
        self.busy = false;
        match loaded.result {
            Ok(snapshot) => self.accept_namespace(snapshot, loaded.selected, cx),
            Err(error) => {
                self.namespace.known = false;
                self.fail(format!(
                    "Database/schema context could not be verified: {error}"
                ));
            }
        }
        if let Some(error) = loaded.error {
            self.fail(error);
        }
        if let Some((tab, context_ms)) = loaded.query_timing
            && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab)
            && let Some(timing) = tab.query_progress.as_mut().and_then(|p| p.timing.as_mut())
        {
            timing.context_ms = Some(context_ms);
            let total = timing.elapsed();
            timing.total_ms = Some(total);
            if tab.error.is_none() && self.error.is_none() {
                let rows: u64 = tab
                    .results
                    .iter()
                    .map(|r| {
                        if r.columns.is_empty() {
                            r.rows_affected
                        } else {
                            r.rows.len() as u64
                        }
                    })
                    .sum();
                let fetch_ms = timing.result_fetch_ms;
                let count = if tab.results.len() == 1 && !tab.results[0].columns.is_empty() {
                    crate::query_stream_ui::result_row_count(
                        tab.query_progress.as_ref(),
                        0,
                        &tab.results[0],
                    )
                } else if tab.results.iter().any(|r| r.truncated) {
                    format!("{rows} retained / affected rows · partial display")
                } else {
                    format!("{rows} rows")
                };
                self.status =
                    format!("Finished · {count} · {total} ms total · {fetch_ms} ms SQL/fetch");
            }
        }
        cx.notify();
    }
    pub fn accept_namespace(
        &mut self,
        snapshot: NamespaceCatalog,
        selected: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let switches_database = self.namespace_behavior() == NamespaceBehavior::Databases;
        let scope = if switches_database {
            Some(snapshot.database.clone())
        } else {
            selected.or_else(|| snapshot.current_schema.clone())
        };
        let changed = self.namespace_scope() != scope.as_deref()
            || self
                .namespace
                .snapshot
                .as_ref()
                .is_some_and(|s| s.database != snapshot.database);
        if changed {
            // Only explicit clean switching or a completed SQL batch reaches here.
            // Keep SQL buffers/results, but never relabel an old table as new context.
            if !self.has_row_draft() && self.changes.is_empty() {
                self.table = None;
                self.table_result = QueryResult::default();
                self.structure = Structure::default();
                self.filters = Default::default();
                self.relationship_origin = None;
                self.relationship_return = None;
                self.selected = None;
                if self.page == Page::Table {
                    self.page = Page::Query;
                }
            }
            self.objects = Default::default();
            self.advanced.reset_connection();
            self.activity_revision = self.activity_revision.wrapping_add(1);
        }
        // These are live connection copies only; saved source profiles are unchanged.
        if let Some(p) = self.profile.as_mut() {
            p.database = snapshot.database.clone();
        }
        if let Some(p) = self.endpoint.as_mut() {
            p.database = snapshot.database.clone();
        }
        self.catalog = snapshot.tables.clone();
        self.namespace.snapshot = Some(snapshot);
        self.namespace.selected = scope;
        self.namespace.known = true;
        self.namespace.collapsed = [false, false];
        self.explorer_page = 0;
        self.explorer_cursor = 0;
        self.explorer_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
    pub fn scope_catalog(&self, mut catalog: Vec<TableInfo>) -> Vec<TableInfo> {
        if let Some(scope) = self.namespace_scope() {
            catalog.retain(|i| i.table.schema == scope);
        } else if self.namespace.snapshot.is_some() {
            catalog.clear();
        }
        catalog
    }
    pub fn explorer_items(&self, cx: &App) -> Vec<&TableInfo> {
        let filter = self.explorer_filter.read(cx).value().to_lowercase();
        let mut items = self
            .catalog
            .iter()
            .filter(|i| {
                !self.namespace.collapsed[usize::from(is_view(i))]
                    && format!("{}.{}", i.table.schema, i.table.name)
                        .to_lowercase()
                        .contains(&filter)
            })
            .collect::<Vec<_>>();
        items.sort_by_key(|i| (is_view(i), i.table.name.as_str()));
        items
    }
    pub fn render_namespace_selector(&self, cx: &mut Context<Self>) -> AnyElement {
        let behavior = self.namespace_behavior();
        column()
            .px_3()
            .gap_1()
            .mb_2()
            .child(label(
                match behavior {
                    NamespaceBehavior::Databases => "Database · SQL context",
                    NamespaceBehavior::Schemas => "Schema · explorer filter",
                    NamespaceBehavior::AttachedDatabases => "Database · explorer filter",
                },
                cx,
            ))
            .child(
                action(
                    "namespace-selector",
                    "",
                    false,
                    self.namespace_switch_allowed(),
                    cx,
                    |s, w, cx| s.show_namespaces(w, cx),
                )
                .accessibility_label(self.namespace_label())
                .justify_start()
                .child(icon(ui::IconName::Database).size(px(14.)))
                .child(ui::with_tooltip(
                    div()
                        .id("namespace-name")
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(self.namespace_label()),
                    self.namespace_label(),
                ))
                .child(icon(ui::IconName::ChevronDown).size(px(12.)))
                .w_full()
                .overflow_hidden()
                .debug_selector(|| "namespace-selector".into()),
            )
            .into_any_element()
    }
    pub fn render_namespaces(&self, w: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = &self.namespace.picker else {
            return div().into_any_element();
        };
        let switches_database = self.namespace_behavior() == NamespaceBehavior::Databases;
        let names = self.namespace_choices(cx);
        let cursor = p.cursor.min(names.len().saturating_sub(1));
        let selected = names.get(cursor).cloned();
        let mut list = column().gap_1();
        let start = cursor / 50 * 50;
        for (index, name) in names.iter().enumerate().skip(start).take(50) {
            let name = name.clone();
            list = list.child(
                action(
                    ("namespace", index),
                    name,
                    false,
                    self.namespace_switch_allowed(),
                    cx,
                    move |s, _, cx| {
                        if let Some(p) = s.namespace.picker.as_mut() {
                            p.cursor = index;
                        }
                        cx.notify();
                    },
                )
                .selected(index == cursor)
                .flex_shrink_0()
                .justify_start()
                .w_full(),
            );
        }
        self.modal(
            column()
                .w(px(540.))
                .p_4()
                .gap_2()
                .child(
                    div()
                        .text_size(px(17.))
                        .child(match self.namespace_behavior() {
                            NamespaceBehavior::Databases => "Choose database",
                            NamespaceBehavior::Schemas => "Choose explorer schema",
                            NamespaceBehavior::AttachedDatabases => "Choose attached database",
                        }),
                )
                .child(label(
                    if switches_database {
                        "Changes this session's SQL database using USE."
                    } else {
                        "Filters explorer only. SQL name-resolution order stays unchanged."
                    },
                    cx,
                ))
                .child(
                    div()
                        .key_context("RowboatNamespace")
                        .capture_key_down(cx.listener(|s, e: &KeyDownEvent, w, cx| {
                            if matches!(
                                e.keystroke.key.as_str(),
                                "up" | "down" | "home" | "end" | "enter"
                            ) && !e.keystroke.modifiers.modified()
                            {
                                s.namespace_key(&e.keystroke.key, w, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(ui::input("namespace-search", &p.filter, w, cx)),
                )
                .child(label(
                    format!(
                        "{} matches · ↑/↓ choose · Enter select · page {}",
                        names.len(),
                        cursor / 50 + 1
                    ),
                    cx,
                ))
                .child(
                    list.id("namespace-list")
                        .track_scroll(&p.scroll)
                        .max_h((w.viewport_size().height - px(400.)).clamp(px(60.), px(210.)))
                        .overflow_y_scroll(),
                )
                .when(names.len() > 50, |d| {
                    d.child(
                        row()
                            .child(action(
                                "namespace-prev",
                                "Previous",
                                false,
                                start > 0,
                                cx,
                                move |s, _, cx| {
                                    if let Some(p) = s.namespace.picker.as_mut() {
                                        p.cursor = start.saturating_sub(50);
                                        p.scroll.set_offset(point(px(0.), px(0.)));
                                    }
                                    cx.notify();
                                },
                            ))
                            .child(action(
                                "namespace-next",
                                "Next",
                                false,
                                start + 50 < names.len(),
                                cx,
                                move |s, _, cx| {
                                    if let Some(p) = s.namespace.picker.as_mut() {
                                        p.cursor = start + 50;
                                        p.scroll.set_offset(point(px(0.), px(0.)));
                                    }
                                    cx.notify();
                                },
                            )),
                    )
                })
                .child(
                    row()
                        .flex_wrap()
                        .child(action(
                            "namespace-apply",
                            if switches_database {
                                "Use database"
                            } else {
                                "Show namespace"
                            },
                            true,
                            selected.is_some() && self.namespace_switch_allowed(),
                            cx,
                            {
                                let name = selected.clone();
                                move |s, w, cx| {
                                    if let Some(name) = &name {
                                        s.choose_namespace(name.clone(), false, w, cx);
                                    }
                                }
                            },
                        ))
                        .when(switches_database, |d| {
                            d.child(action(
                                "namespace-new-workspace",
                                "Open database in new workspace",
                                false,
                                selected.is_some()
                                    && self.namespace_switch_allowed()
                                    && self.shared.borrow().writable,
                                cx,
                                move |s, w, cx| {
                                    if let Some(name) = &selected {
                                        s.choose_namespace(name.clone(), true, w, cx);
                                    }
                                },
                            ))
                        }),
                )
                .into_any_element(),
            cx,
        )
    }
}
