use crate::{
    app::*,
    view::{action, column, row},
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
impl Workbench {
    pub fn reconcile_panes(&mut self) {
        let ids: Vec<_> = self.tabs.iter().map(|t| t.id).collect();
        self.panes
            .reconcile(&ids, self.tabs[self.active_tab.min(self.tabs.len() - 1)].id);
        if let Some(index) = self
            .tabs
            .iter()
            .position(|t| Some(t.id) == self.panes.tabs[self.panes.focused])
        {
            self.active_tab = index;
        }
    }
    pub fn focus_query_tab(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|t| t.id == id) {
            if self.active_tab != index {
                self.dismiss_completions(cx);
                self.selected = None;
                self.widths.clear();
                self.query_sort = None;
            }
            self.active_tab = index;
            self.panes.select(id);
            cx.notify();
        }
    }
    pub fn observe_editor_focus(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.install_completion(id, cx);
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let handle = tab.editor.read(cx).focus_handle(cx);
        let editor = tab.editor.clone();
        let epoch = self.host_epoch;
        let subscription = cx.on_focus(&handle, window, move |s, w, cx| {
            if s.host_epoch != epoch {
                return;
            }
            s.focus_query_tab(id, cx);
            editor.update(cx, |e, cx| e.focus(w, cx));
        });
        self.editor_focus.insert(id, subscription);
    }
    pub fn toggle_editor_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.tabs[1].is_some() {
            self.panes.collapse();
            self.reconcile_panes();
            cx.notify();
            return;
        }
        let current = self.tabs[self.active_tab].id;
        let other = self.tabs.iter().find(|t| t.id != current).map(|t| t.id);
        let other = if let Some(id) = other {
            id
        } else {
            let before = self.tabs.len();
            self.new_query(None, window, cx);
            if self.tabs.len() == before {
                return;
            }
            self.tabs[self.active_tab].id
        };
        self.panes.tabs = [Some(current), None];
        self.panes.split(other);
        self.panes.focus(0);
        self.focus_query_tab(current, cx);
        self.tabs[self.active_tab]
            .editor
            .update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }
    pub fn render_editor_panes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.reconcile_panes();
        let left = self.render_editor_pane(0, window, cx);
        if self.panes.tabs[1].is_none() {
            return left;
        }
        let right = self.render_editor_pane(1, window, cx);
        ui::resizable("sql-pane-split", Axis::Horizontal, cx)
            .with_state(&self.split_layout)
            .child(
                ui::resizable_panel()
                    .size(px(560.))
                    .size_range(px(240.)..px(4000.))
                    .child(left),
            )
            .child(
                ui::resizable_panel()
                    .size(px(560.))
                    .size_range(px(240.)..px(4000.))
                    .child(right),
            )
            .into_any_element()
    }
    fn render_editor_pane(
        &self,
        pane: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self.panes.tabs[pane].unwrap();
        let tab = self.tabs.iter().find(|t| t.id == id).unwrap();
        let editor = tab.editor.clone();
        let t = cx.omarchy().clone();
        let active = self.panes.focused == pane;
        column()
            .size_full()
            .min_w_0()
            .when(self.panes.tabs[1].is_some(), |d| {
                d.border_1()
                    .border_color(if active { t.accent } else { t.divider() })
            })
            .when(self.panes.tabs[1].is_some(), |d| {
                d.child(
                    row()
                        .px_2()
                        .child(action(
                            ("focus-sql-pane", pane),
                            format!("{}{}", tab.title, if active { " · active" } else { "" }),
                            false,
                            true,
                            cx,
                            move |s, w, cx| {
                                s.focus_query_tab(id, cx);
                                s.tabs[s.active_tab]
                                    .editor
                                    .update(cx, |v, cx| v.focus(w, cx));
                            },
                        ))
                        .child(div().flex_1()),
                )
            })
            .child(
                div()
                    .id(("sql-editor-wrap", pane))
                    .key_context(if self.snippet_active(tab.id) {
                        "RowboatSqlEditor RowboatSnippet"
                    } else {
                        "RowboatSqlEditor"
                    })
                    .capture_action(cx.listener(
                        move |s, _: &gpui_kit::base::input::IndentInline, w, cx| {
                            if !s.snippet_active(id)
                                && s.inline_completion_allowed(id, s.host_epoch)
                                && let Some(menu) = s.advanced.completion_menus.get(&id)
                                && s.inline_offer_current(&menu.borrow())
                                && let Some(tab) = s.tabs.iter().find(|t| t.id == id)
                                && crate::completion_popup::accept_tab(&tab.editor, menu, w, cx)
                            {
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .capture_action(cx.listener(move |s, _: &Dismiss, _, cx| {
                        if !s.modal_open() && !s.snippet_active(id) {
                            s.dismiss_completions(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(
                        move |s, action: &gpui_kit::base::input::MoveDown, w, cx| {
                            if s.completion_action(id, action, w, cx) {
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .capture_action(cx.listener(
                        move |s, action: &gpui_kit::base::input::MoveUp, w, cx| {
                            if s.completion_action(id, action, w, cx) {
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .capture_action(cx.listener(
                        move |s, action: &gpui_kit::base::input::Enter, w, cx| {
                            if action.secondary
                                && !action.shift
                                && s.shortcut_label("run_query") == "ctrl-enter"
                            {
                                cx.stop_propagation();
                                cx.defer_in(w, move |s, _, cx| {
                                    s.focus_query_tab(id, cx);
                                    s.run_query(cx);
                                });
                            } else if !action.secondary && s.completion_action(id, action, w, cx) {
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .on_action(cx.listener(move |_, _: &RunQuery, w, cx| {
                        cx.stop_propagation();
                        cx.defer_in(w, move |s, _, cx| {
                            s.focus_query_tab(id, cx);
                            s.run_query(cx);
                        });
                    }))
                    .on_action(cx.listener(move |_, _: &RunCurrent, w, cx| {
                        cx.stop_propagation();
                        cx.defer_in(w, move |s, _, cx| {
                            s.focus_query_tab(id, cx);
                            s.run_query_scope(
                                rowboat_core::execution::RunScope::Current,
                                false,
                                cx,
                            );
                        });
                    }))
                    .on_action(cx.listener(move |_, _: &RunSelection, w, cx| {
                        cx.stop_propagation();
                        cx.defer_in(w, move |s, _, cx| {
                            s.focus_query_tab(id, cx);
                            s.run_query_scope(
                                rowboat_core::execution::RunScope::Selection,
                                false,
                                cx,
                            );
                        });
                    }))
                    .on_action(cx.listener(move |_, _: &RunAll, w, cx| {
                        cx.stop_propagation();
                        cx.defer_in(w, move |s, _, cx| {
                            s.focus_query_tab(id, cx);
                            s.run_query_scope(rowboat_core::execution::RunScope::All, false, cx);
                        });
                    }))
                    .on_action(cx.listener(move |_, _: &FormatQuery, w, cx| {
                        cx.stop_propagation();
                        cx.defer_in(w, move |s, w, cx| {
                            s.focus_query_tab(id, cx);
                            s.format_query(w, cx);
                        });
                    }))
                    .on_action(cx.listener(move |_, _: &CompleteSql, w, cx| {
                        cx.stop_propagation();
                        cx.defer_in(w, move |s, w, cx| {
                            s.focus_query_tab(id, cx);
                            s.complete_sql(w, cx);
                        });
                    }))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p_2()
                    .child(crate::sql_editor::render(
                        &editor,
                        self.state.preferences.editor_font_size,
                        cx,
                    ))
                    .when_some(self.advanced.completion_menus.get(&id), |d, menu| {
                        if self.inline_completion_allowed(id, self.host_epoch) {
                            d.child(crate::completion_popup::render(
                                &editor,
                                menu,
                                cx.entity().downgrade(),
                                id,
                                cx,
                            ))
                        } else {
                            editor.update(cx, |e, cx| e.dismiss_lsp_overlays(cx));
                            d
                        }
                    }),
            )
            .into_any_element()
    }
}
