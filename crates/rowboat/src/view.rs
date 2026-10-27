use crate::app::*;
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme, ButtonVariant};
use rowboat_core::{Cell, Engine};

pub(crate) fn action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    cx: &mut Context<Workbench>,
    handler: impl Fn(&mut Workbench, &mut Window, &mut Context<Workbench>) + 'static,
) -> ui::Button {
    let t = cx.omarchy().clone();
    let label: SharedString = label.into();
    let accessible = label.clone();
    let mut text = label.to_string();
    let mut glyph = None;
    for (prefix, name) in [
        ("☑", ui::IconName::SquareCheck),
        ("☐", ui::IconName::Square),
        ("←", ui::IconName::ChevronLeft),
        ("▴", ui::IconName::ChevronUp),
        ("▾", ui::IconName::ChevronDown),
        ("✕", ui::IconName::Close),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            glyph = Some(name);
            text = rest.trim_start().to_string();
            break;
        }
    }
    for (suffix, name) in [
        (" ▾", ui::IconName::ChevronDown),
        (" ▴", ui::IconName::ChevronUp),
        (" →", ui::IconName::ChevronRight),
    ] {
        if let Some(rest) = text.strip_suffix(suffix) {
            glyph = Some(name);
            text = rest.to_string();
            break;
        }
    }
    let destructive = label.starts_with("Drop")
        || label.starts_with("Delete")
        || label.starts_with("Discard")
        || label == "Clear history"
        || label == "Remove";
    ui::button(
        id,
        "",
        if primary {
            ButtonVariant::Primary
        } else if destructive {
            ButtonVariant::Danger
        } else {
            ButtonVariant::Secondary
        },
        cx,
    )
    .accessibility_label(accessible)
    .when_some(glyph, |b, glyph| b.child(icon(glyph).size(px(14.))))
    .when(!text.is_empty(), |b| b.child(text))
    .text_size(px(13.))
    .min_h(px(28.))
    .rounded(px(4.))
    .when(primary, |b| {
        b.bg(t.accent.opacity(1.))
            .text_color(t.on_accent)
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|s| s.bg(t.accent.opacity(1.)).border_color(t.on_accent))
            .active(|s| s.bg(t.accent.opacity(1.)).border_color(t.on_accent))
            .focus_visible(|s| s.bg(t.accent.opacity(1.)).border_color(t.on_accent))
    })
    .disabled(!enabled)
    .on_click(cx.listener(move |this, _, window, cx| handler(this, window, cx)))
}
pub(crate) fn row() -> Div {
    div().flex().items_center().gap_2()
}
pub(crate) fn column() -> Div {
    div().flex().flex_col().min_w_0().min_h_0()
}
pub(crate) fn label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(11.))
        .text_color(cx.omarchy().secondary)
        .child(text.into())
}

pub(crate) fn icon(name: ui::IconName) -> ui::Icon {
    ui::icon(name).size(px(16.))
}

pub(crate) fn icon_action(
    id: impl Into<ElementId>,
    name: ui::IconName,
    title: &'static str,
    enabled: bool,
    cx: &mut Context<Workbench>,
    handler: impl Fn(&mut Workbench, &mut Window, &mut Context<Workbench>) + 'static,
) -> AnyElement {
    ui::with_tooltip(
        action(id, "", false, enabled, cx, handler)
            .accessibility_label(title)
            .size(px(28.))
            .p_1()
            .child(icon(name)),
        title,
    )
    .into_any_element()
}
pub(crate) fn section(text: impl Into<SharedString>, cx: &App) -> Div {
    label(text, cx)
        .font_weight(FontWeight::SEMIBOLD)
        .mt_4()
        .mb_2()
}

impl Workbench {
    fn destination(
        &self,
        id: &'static str,
        title: &'static str,
        glyph: ui::IconName,
        command: crate::chrome_ui::Command,
        active: bool,
        cx: &mut Context<Self>,
    ) -> ui::Button {
        let t = cx.omarchy().clone();
        action(
            id,
            "",
            false,
            self.menu_enabled(command, cx),
            cx,
            move |s, w, cx| {
                s.choose_menu(command, w, cx);
            },
        )
        .accessibility_label(title)
        .debug_selector(move || id.into())
        .flex_1()
        .min_w_0()
        .h(px(32.))
        .px_2()
        .text_size(px(12.))
        .justify_start()
        .bg(if active {
            t.selection.opacity(1.)
        } else {
            transparent_black()
        })
        .border_l_2()
        .border_color(if active {
            t.accent
        } else {
            transparent_black()
        })
        .font_weight(if active {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .child(icon(glyph).text_color(if active { t.accent } else { t.secondary }))
        .child(ui::with_tooltip(
            div().id((id, 1usize)).min_w_0().truncate().child(title),
            title,
        ))
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use crate::chrome_ui::Command;
        let t = cx.omarchy().clone();
        let navigation = column()
            .flex_shrink_0()
            .p_2()
            .gap_1()
            .border_b_1()
            .border_color(t.divider())
            .child(
                row()
                    .gap_1()
                    .child(self.destination(
                        "database-page",
                        "Database",
                        ui::IconName::Database,
                        Command::Database,
                        self.page == Page::Table
                            || self.page == Page::Query && self.database_destination,
                        cx,
                    ))
                    .child(self.destination(
                        "queries-page",
                        "SQL editor",
                        ui::IconName::FileCode,
                        Command::SqlEditor,
                        self.page == Page::Query && !self.database_destination,
                        cx,
                    )),
            )
            .child(
                row()
                    .gap_1()
                    .child(self.destination(
                        "saved-queries-page",
                        "Saved",
                        ui::IconName::Bookmark,
                        Command::SavedQueries,
                        self.page == Page::History && self.saved_queries_view,
                        cx,
                    ))
                    .child(self.destination(
                        "history-page",
                        "History",
                        ui::IconName::Clock,
                        Command::History,
                        self.page == Page::History && !self.saved_queries_view,
                        cx,
                    )),
            );
        let mut tables = column().gap_0();
        let visible = self.explorer_items(cx);
        let page = self
            .explorer_page
            .min(visible.len().saturating_sub(1) / 200);
        tables = tables.child(
            row()
                .flex_shrink_0()
                .child(label(
                    format!("{} matches · page {}", visible.len(), page + 1),
                    cx,
                ))
                .child(icon_action(
                    "tables-prev",
                    ui::IconName::ChevronLeft,
                    "Previous explorer page",
                    page > 0,
                    cx,
                    move |s, _, cx| {
                        s.explorer_page = page.saturating_sub(1);
                        cx.notify();
                    },
                ))
                .child(icon_action(
                    "tables-next",
                    ui::IconName::ChevronRight,
                    "Next explorer page",
                    (page + 1) * 200 < visible.len(),
                    cx,
                    move |s, _, cx| {
                        s.explorer_page = page + 1;
                        cx.notify();
                    },
                )),
        );
        // Both section controls remain reachable on every page, including when
        // collapsed. Only their children participate in the shared 200-row page.
        for (index, title) in ["Tables", "Views"].iter().enumerate() {
            let count = self
                .catalog
                .iter()
                .filter(|i| usize::from(crate::namespace_ui::is_view(i)) == index)
                .count();
            tables = tables.child(
                action(
                    ("explorer-group", index),
                    "",
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        s.namespace.collapsed[index] = !s.namespace.collapsed[index];
                        s.explorer_page = 0;
                        s.explorer_cursor = 0;
                        cx.notify();
                    },
                )
                .accessibility_label(format!("{title} ({count})"))
                .child(
                    icon(if self.namespace.collapsed[index] {
                        ui::IconName::ChevronRight
                    } else {
                        ui::IconName::ChevronDown
                    })
                    .size(px(12.)),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child(*title))
                .child(label(count.to_string(), cx))
                .w_full()
                .debug_selector(move || format!("explorer-group-{index}"))
                .flex_shrink_0()
                .justify_start()
                .mt_2(),
            );
            for (i, info) in visible
                .iter()
                .enumerate()
                .skip(page * 200)
                .take(200)
                .filter(|(_, info)| usize::from(crate::namespace_ui::is_view(info)) == index)
            {
                let selected = self.page == Page::Table && self.table.as_ref() == Some(&info.table)
                    || self.explorer_cursor == i
                        && self
                            .explorer_filter
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window);
                let table = info.table.clone();
                let symbol = if crate::namespace_ui::is_view(info) {
                    ui::IconName::Eye
                } else {
                    ui::IconName::Table
                };
                tables = tables.child(
                    ui::button(("table", i), "", ButtonVariant::Secondary, cx)
                        .debug_selector(move || format!("explorer-item-{i}"))
                        .w_full()
                        .flex_shrink_0()
                        .justify_start()
                        .pl_5()
                        .h(px(28.))
                        .text_size(px(13.))
                        .bg(if selected {
                            t.selection.opacity(1.)
                        } else {
                            transparent_black()
                        })
                        .border_l_2()
                        .border_color(if selected {
                            t.accent
                        } else {
                            transparent_black()
                        })
                        .child(icon(symbol).text_color(if selected {
                            t.accent
                        } else {
                            t.secondary
                        }))
                        .child(ui::with_tooltip(
                            div()
                                .id(("table-name", i))
                                .min_w_0()
                                .truncate()
                                .child(info.table.name.clone()),
                            format!("{}.{}", info.table.schema, info.table.name),
                        ))
                        .on_click(cx.listener(move |s, _, _, cx| s.open_table(table.clone(), cx))),
                );
            }
        }
        if self.database.is_some() && visible.is_empty() {
            tables = tables.child(label("No matching tables or views", cx).p_3());
        }
        column()
            .size_full()
            .bg(t.inset.opacity(1.))
            .child(navigation)
            .when(self.database.is_some(), |d| {
                d.child(self.render_namespace_selector(cx))
            })
            .child(
                row()
                    .px_4()
                    .mb_2()
                    .child(label("Tables & views", cx).font_weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1())
                    .child(icon_action(
                        "refresh-catalog",
                        ui::IconName::RefreshCw,
                        "Refresh tables and views",
                        self.database.is_some() && !self.busy,
                        cx,
                        |s, _, cx| s.refresh_catalog(cx),
                    )),
            )
            .child(div().key_context("RowboatExplorer").px_3().child(ui::input(
                "explorer-search",
                &self.explorer_filter,
                window,
                cx,
            )))
            .child(
                tables
                    .id("explorer-scroll")
                    .track_scroll(&self.explorer_scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2(),
            )
            .child(
                column()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(t.divider())
                    .when(!self.changes.is_empty(), |d| {
                        d.child(action(
                            "sidebar-review",
                            format!("Review {} changes", self.changes.len()),
                            true,
                            !self.has_row_draft(),
                            cx,
                            |s, _, cx| s.review_pending(cx),
                        ))
                    })
                    .child(
                        row()
                            .child(self.action_menu(
                                "navigation-menu",
                                "Tools",
                                crate::chrome_ui::Menu::Navigate,
                                cx,
                            ))
                            .child(self.destination(
                                "sidebar-preferences",
                                "Preferences",
                                ui::IconName::Settings,
                                Command::Preferences,
                                false,
                                cx,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_query(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.query_edit.review {
            return self.render_changes(cx);
        }
        let t = cx.omarchy().clone();
        let mut tabs = row().gap_0().h(px(34.));
        let workspace = self.tabs[self.active_tab].connection_id.clone();
        let active_id = self.tabs[self.active_tab].id;
        let tab_count = self.tabs.len();
        let index = self.tabs[..self.active_tab]
            .iter()
            .filter(|tab| tab.connection_id == workspace)
            .count();
        let previous_reveal = self.query_tab_reveal;
        let tab_scroll = self.query_tab_scroll.clone();
        let reveal_owner = cx.weak_entity();
        // GPUI 0.3.9 consumes scroll_to_item before refreshing its viewport and
        // overflow. Measure after prepaint instead, when both are current. The
        // actual strip width also catches restored/resized split-pane layouts.
        tabs = tabs.on_children_prepainted(move |_, _, cx| {
            let viewport = tab_scroll.bounds();
            let reveal = (active_id, tab_count, viewport.size.width);
            if previous_reveal == Some(reveal) || viewport.size.width <= px(0.) {
                return;
            }
            let Some(bounds) = tab_scroll.bounds_for_item(index) else {
                return;
            };
            let mut offset = tab_scroll.offset();
            if bounds.left() + offset.x < viewport.left() {
                offset.x = viewport.left() - bounds.left();
            } else if bounds.right() + offset.x > viewport.right() {
                offset.x = viewport.right() - bounds.right();
            }
            offset.x = offset.x.clamp(-tab_scroll.max_offset().x, px(0.));
            let owner = reveal_owner.clone();
            cx.defer(move |cx| {
                let _ = owner.update(cx, |s, cx| {
                    if s.page != Page::Query
                        || s.tabs[s.active_tab].id != active_id
                        || s.tabs.len() != tab_count
                        || s.query_tab_reveal == Some(reveal)
                    {
                        return;
                    }
                    s.query_tab_reveal = Some(reveal);
                    if s.query_tab_scroll.offset() != offset {
                        s.query_tab_scroll.set_offset(offset);
                        cx.notify();
                    }
                });
            });
        });
        for (i, tab) in self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| t.connection_id == workspace)
        {
            tabs = tabs.child(
                ui::button(("query-tab", tab.id), "", ButtonVariant::Secondary, cx)
                    .h_full()
                    .px_3()
                    .min_w(px(112.))
                    .max_w(px(200.))
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .accessibility_label(tab.title.clone())
                    .when(i == self.active_tab, |b| {
                        b.debug_selector(|| "active-query-tab".into())
                    })
                    .disabled(!self.menu_enabled(crate::chrome_ui::Command::SqlEditor, cx))
                    .bg(if i == self.active_tab {
                        t.background.opacity(1.)
                    } else {
                        transparent_black()
                    })
                    .text_color(if i == self.active_tab {
                        t.foreground
                    } else {
                        t.secondary
                    })
                    .font_weight(if i == self.active_tab {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .border_b_2()
                    .border_color(if i == self.active_tab {
                        t.accent
                    } else {
                        transparent_black()
                    })
                    .child(icon(ui::IconName::FileCode).size(px(14.)))
                    .child(ui::with_tooltip(
                        div()
                            .id(("query-tab-title", tab.id))
                            .truncate()
                            .child(tab.title.clone()),
                        tab.title.clone(),
                    ))
                    .on_click(cx.listener(move |s, _, _, cx| {
                        if !s.menu_enabled(crate::chrome_ui::Command::SqlEditor, cx) {
                            return;
                        }
                        let id = s.tabs[i].id;
                        s.focus_query_tab(id, cx);
                        s.database_destination = false;
                    })),
            );
        }
        let tab_choices = self
            .tabs
            .iter()
            .filter(|tab| tab.connection_id == workspace)
            .map(|tab| (tab.id, tab.title.clone()))
            .collect::<Vec<_>>();
        let tab_owner = cx.weak_entity();
        let tab_route = self.tx.route;
        let tab_epoch = self.host_epoch;
        let tabs_enabled = self.menu_enabled(crate::chrome_ui::Command::SqlEditor, cx);
        let tab_menu = ui::menu(
            "query-tabs-overflow",
            ui::icon_button(
                "query-tabs-overflow-button",
                ui::IconName::ChevronDown,
                "All query tabs",
                ButtonVariant::Secondary,
                cx,
            ),
            tab_choices
                .iter()
                .map(|(id, title)| {
                    ui::MenuItem::new(if *id == self.tabs[self.active_tab].id {
                        format!("{title} · Active")
                    } else {
                        title.clone()
                    })
                    .disabled(!tabs_enabled)
                })
                .collect(),
            move |index, _, cx| {
                let _ = tab_owner.update(cx, |s, cx| {
                    if s.tx.route != tab_route
                        || s.host_epoch != tab_epoch
                        || !s.menu_enabled(crate::chrome_ui::Command::SqlEditor, cx)
                    {
                        return;
                    }
                    if let Some((id, _)) = tab_choices.get(index) {
                        s.query_tab_reveal = None;
                        s.focus_query_tab(*id, cx);
                        s.database_destination = false;
                    }
                });
            },
        );
        let free = !self.busy;
        let controls = row()
            .flex_wrap()
            .px_3()
            .py_2()
            .bg(t.surface.opacity(1.))
            .border_b_1()
            .border_color(t.divider())
            .when(
                self.database.is_none()
                    || workspace.as_deref() != self.profile.as_ref().map(|p| p.id.as_str()),
                |d| {
                    d.child(action(
                        "connect-workspace",
                        if workspace.is_none() && self.database.is_some() {
                            "Use active connection"
                        } else {
                            "Connect workspace"
                        },
                        true,
                        free,
                        cx,
                        |s, w, cx| s.connect_workspace(w, cx),
                    ))
                },
            )
            .child(
                action(
                    "run-query",
                    if self.tabs[self.active_tab]
                        .editor
                        .read(cx)
                        .selected_value()
                        .is_empty()
                    {
                        "Run all"
                    } else {
                        "Run selection"
                    },
                    true,
                    free && self.database.is_some()
                        && self.tabs[self.active_tab].connection_id.as_deref()
                            == self.profile.as_ref().map(|p| p.id.as_str()),
                    cx,
                    |s, _, cx| s.run_query(cx),
                )
                .child(icon(ui::IconName::Play).size(px(14.))),
            )
            .when(self.busy, |d| {
                d.child(
                    action(
                        "cancel-query",
                        "Cancel",
                        false,
                        self.busy && self.database.is_some() && !self.transfer_running,
                        cx,
                        |s, _, cx| s.cancel(cx),
                    )
                    .child(icon(ui::IconName::Square).size(px(12.))),
                )
            })
            .child(label(self.shortcut_label("run_query"), cx))
            .child(
                action(
                    "run-current",
                    "Run current",
                    false,
                    self.menu_enabled(crate::chrome_ui::Command::RunCurrent, cx),
                    cx,
                    |s, _, cx| {
                        s.run_query_scope(rowboat_core::execution::RunScope::Current, false, cx)
                    },
                )
                .debug_selector(|| "run-current".into()),
            )
            .child(div().flex_1())
            .child(
                action(
                    "save-query",
                    "Save query",
                    false,
                    self.menu_enabled(crate::chrome_ui::Command::Favorite, cx),
                    cx,
                    |s, w, cx| s.choose_menu(crate::chrome_ui::Command::Favorite, w, cx),
                )
                .child(icon(ui::IconName::Bookmark).size(px(14.))),
            )
            .when(self.panes.tabs[1].is_some(), |d| {
                d.child(label(
                    format!("Active: {}", self.tabs[self.active_tab].title),
                    cx,
                ))
            })
            .child(self.action_menu("query-menu", "Query", crate::chrome_ui::Menu::Query, cx));
        let editor_panes = self.render_editor_panes(window, cx);
        let editor_area = column()
            .id("query-editor-region")
            .debug_selector(|| "query-editor-region".into())
            .size_full()
            .bg(t.background.opacity(1.))
            .child(controls)
            .child(self.render_execution_scope(cx))
            .child(self.render_library_document(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(editor_panes),
            );
        let results = column()
            .id("query-results-region")
            .debug_selector(|| "query-results-region".into())
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_results(window, cx)),
            );
        column()
            .size_full()
            .child(
                row()
                    .gap_0()
                    .h(px(34.))
                    .bg(t.surface.opacity(1.))
                    .border_b_1()
                    .border_color(t.divider())
                    .flex_shrink_0()
                    .child(
                        tabs.id("query-tabs-scroll")
                            .debug_selector(|| "query-tabs-scroll".into())
                            .track_scroll(&self.query_tab_scroll)
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll(),
                    )
                    .child(tab_menu)
                    .child(icon_action(
                        "add-query",
                        ui::IconName::Plus,
                        "New query",
                        !self.has_row_draft() && !self.close_requested,
                        cx,
                        |s, w, cx| s.new_query(None, w, cx),
                    )),
            )
            .child(
                div().flex_1().min_h_0().child(
                    ui::resizable("query-split", Axis::Vertical, cx)
                        .with_state(&self.editor_split)
                        .child(
                            ui::resizable_panel()
                                .size(px((f32::from(window.viewport_size().height) - 180.)
                                    .max(320.)
                                    * 0.42))
                                .size_range(px(120.)..px(4000.))
                                .child(editor_area),
                        )
                        .child(
                            ui::resizable_panel()
                                .size(px((f32::from(window.viewport_size().height) - 180.)
                                    .max(320.)
                                    * 0.58))
                                .size_range(px(180.)..px(4000.))
                                .child(results),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_table(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.omarchy().clone();
        let free = !self.busy && !self.has_row_draft();
        let title = self
            .table
            .as_ref()
            .map(|t| format!("{}  /  {}", t.schema, t.name))
            .unwrap_or_default();
        let tabs = row()
            .px_3()
            .py_1()
            .min_h(px(40.))
            .flex_wrap()
            .bg(t.surface.opacity(1.))
            .border_b_1()
            .border_color(t.divider())
            .child(icon(ui::IconName::Table))
            .child(ui::with_tooltip(
                div()
                    .id("table-title")
                    .min_w_0()
                    .max_w(px(260.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.clone()),
                title,
            ))
            .child(div().flex_1())
            .child(
                action("table-content", "Data", false, free, cx, |s, w, cx| {
                    s.table_page = TablePage::Content;
                    w.focus(&s.grid_focus, cx);
                    cx.notify();
                })
                .selected(self.table_page == TablePage::Content)
                .border_b_2()
                .border_color(if self.table_page == TablePage::Content {
                    t.accent
                } else {
                    transparent_black()
                }),
            )
            .child(
                action(
                    "table-diagram",
                    "Diagram",
                    false,
                    free && self.database.is_some() && self.changes.is_empty(),
                    cx,
                    |s, w, cx| s.show_diagram(w, cx),
                )
                .debug_selector(|| "table-diagram".into()),
            )
            .child(
                action(
                    "table-structure",
                    "Structure",
                    false,
                    free,
                    cx,
                    |s, _, cx| {
                        s.table_page = TablePage::Structure;
                        cx.notify();
                    },
                )
                .selected(self.table_page == TablePage::Structure)
                .border_b_2()
                .border_color(if self.table_page == TablePage::Structure {
                    t.accent
                } else {
                    transparent_black()
                }),
            )
            .child(
                action(
                    "table-changes",
                    format!("Review ({})", self.changes.len()),
                    false,
                    free,
                    cx,
                    |s, _, cx| {
                        s.table_page = TablePage::Changes;
                        cx.notify();
                    },
                )
                .selected(self.table_page == TablePage::Changes)
                .border_b_2()
                .border_color(if self.table_page == TablePage::Changes {
                    t.accent
                } else {
                    transparent_black()
                }),
            );
        let body = match self.table_page {
            TablePage::Content => self.render_results(window, cx),
            TablePage::Structure => self.render_structure(cx),
            TablePage::Changes => self.render_changes(cx),
        };
        column()
            .size_full()
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_results(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tools = self.render_results_tools(window, cx);
        let record_toggle = self.render_record_toolbar(window, cx);
        let grid = self
            .render_results_comparison(window, cx)
            .unwrap_or_else(|| self.render_live_results(window, cx));
        let body = if self.record_panel_open() {
            let panel = self.render_record_panel(window, cx);
            let grid = div()
                .size_full()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .child(grid);
            match self.record_panel_dock(window) {
                crate::grid_ui::record_inspector::RecordDock::Right => row()
                    .gap_0()
                    .size_full()
                    .min_h_0()
                    .child(grid)
                    .child(panel)
                    .into_any_element(),
                crate::grid_ui::record_inspector::RecordDock::Bottom => column()
                    .size_full()
                    .min_h_0()
                    .child(grid)
                    .child(panel)
                    .into_any_element(),
            }
        } else {
            grid
        };
        column()
            .size_full()
            .min_h_0()
            .child(tools)
            .child(row().px_3().py_1().child(record_toggle))
            .child(div().flex_1().min_h_0().overflow_hidden().child(body))
            .into_any_element()
    }

    fn render_live_results(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.omarchy().clone();
        let table = self.page == Page::Table;
        let free = !self.busy && !self.has_row_draft();
        let toolbar = self.compact_grid_toolbar(window, cx);
        let mut content = column()
            .size_full()
            .bg(t.background.opacity(1.))
            .child(toolbar)
            .child(self.render_grid_tools(window, cx));
        if table {
            content = content.child(self.render_filter_toolbar(cx));
        } else if let Some(context) = self
            .namespace
            .result_context
            .get(&self.tabs[self.active_tab].id)
        {
            content = content.child(label(context.clone(), cx).px_3().py_1());
        }
        let tab = &self.tabs[self.active_tab];
        let query_loading = (!table && self.query_running_for(tab.id)).then(|| {
            tab.query_progress
                .as_ref()
                .map(|p| p.loading_label(&tab.results))
                .unwrap_or("Waiting for results…")
        });
        if let Some(phase) = query_loading {
            if self.current_result().is_none_or(|result| {
                result.rows.is_empty()
                    && tab
                        .query_progress
                        .as_ref()
                        .is_none_or(|p| p.result_pending(tab.result_index))
            }) {
                return content
                    .child(crate::loading_ui::rail(window, cx))
                    .child(crate::loading_ui::waiting(phase, window, cx))
                    .into_any_element();
            }
            content = content.child(crate::loading_ui::header(phase, window, cx));
        }
        let Some(result) = self.current_result() else {
            return content.child(self.empty_results(cx)).into_any_element();
        };
        if !table && !result.columns.is_empty() {
            content = content.child(
                row()
                    .px_3()
                    .py_1()
                    .flex_wrap()
                    .child(icon(ui::IconName::Info).size(px(12.)).text_color(t.secondary))
                    .child(
                        label(self.query_edit_notice(), cx)
                            .debug_selector(|| "query-edit-notice".into()),
                    )
                    .when(self.query_plan().is_none(), |d| {
                        d.child(ui::with_tooltip(
                            action(
                                "run-for-editing",
                                "Run again for editing",
                                false,
                                self.run_for_editing_enabled(),
                                cx,
                                |s, _, cx| s.run_query_for_editing(cx),
                            )
                            .text_size(px(11.))
                            .min_h(px(24.))
                            .py_0()
                            .debug_selector(|| "run-for-editing".into()),
                            "Runs current SELECT / selection again for a fresh editable snapshot (up to 64 MiB).",
                        ))
                    }),
            );
        }
        if result.columns.is_empty() {
            if table {
                return content
                    .child(
                        column()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(if self.busy {
                                "Loading table…"
                            } else {
                                "Table content is unavailable. Refresh to retry."
                            }),
                    )
                    .into_any_element();
            }
            return content
                .child(self.render_statement_result(result, cx))
                .into_any_element();
        }
        let total_rows = result.rows.len();
        let visible_rows = self.visible_grid_rows(cx);
        let rows = visible_rows.len();
        let tab = &self.tabs[self.active_tab];
        let (timing, result_notice) = crate::query_stream_ui::result_presentation(
            if table {
                None
            } else {
                tab.query_progress.as_ref()
            },
            tab.result_index,
            result,
            !table && self.query_running_for(tab.id),
        );
        let row_count = crate::query_stream_ui::result_row_count(
            if table {
                None
            } else {
                tab.query_progress.as_ref()
            },
            tab.result_index,
            result,
        );
        let columns = result.columns.clone();
        if self.widths.len() != columns.len() {
            self.widths = columns
                .iter()
                .map(|c| {
                    ((c.name.len().max(c.data_type.len()) as f32 * 8.) + 45.).clamp(150., 290.)
                })
                .collect();
        }
        let grid_font_size = self.state.preferences.grid_font_size;
        let grid_detail_size = (grid_font_size - 2.).max(10.);
        let gutter_width = (52. * grid_font_size / 12.).max(52.);
        let width = self.widths.iter().sum::<f32>() + gutter_width;
        let mut header = row()
            .id("grid-header")
            .debug_selector(|| "grid-header".into())
            .on_scroll_wheel(cx.listener(|s, e: &ScrollWheelEvent, w, cx| {
                let delta = e.delta.pixel_delta(w.line_height());
                if delta.x == px(0.) && delta.y != px(0.) {
                    let handle = &s.grid_scroll.0.borrow().base_handle;
                    let mut offset = handle.offset();
                    offset.y += delta.y;
                    handle.set_offset(offset);
                    cx.notify();
                }
            }))
            .gap_0()
            .h(px(44_f32.max(grid_font_size * 3. + 8.)))
            .w(px(width))
            .bg(t.surface.opacity(1.))
            .border_b_1()
            .border_color(t.border)
            .flex_shrink_0()
            .child(
                div()
                    .w(px(gutter_width))
                    .px_3()
                    .text_size(px(grid_detail_size))
                    .text_color(t.secondary)
                    .child("#"),
            );
        for (i, col) in columns.iter().enumerate() {
            let name = col.name.clone();
            let sort = self.grid_sort_arrow(i, &name);
            header = header.child(
                div()
                    .id(("column-head", i))
                    .debug_selector(move || format!("column-head-{i}"))
                    .relative()
                    .h_full()
                    .w(px(self.widths[i]))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .border_r_1()
                    .border_color(t.divider())
                    .cursor_pointer()
                    .child(
                        row()
                            .gap_1()
                            .when(col.primary_key, |d| {
                                d.child(
                                    icon(ui::IconName::KeyRound)
                                        .size(px(12.))
                                        .text_color(t.accent),
                                )
                            })
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(grid_font_size))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("{name}{sort}")),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(grid_detail_size))
                            .text_color(t.secondary)
                            .child(col.data_type.clone()),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| s.sort_grid_column(i, cx)))
                    .child(
                        div()
                            .id(("resize-column", i))
                            .absolute()
                            .right_0()
                            .top_0()
                            .h_full()
                            .w(px(6.))
                            .cursor_col_resize()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |s, e: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    s.resizing = Some((i, f32::from(e.position.x), s.widths[i]));
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }
        let grid = div()
            .id("grid-horizontal")
            .track_focus(&self.grid_focus)
            .track_scroll(&self.grid_x_scroll)
            .debug_selector(||"data-grid".into())
            .on_key_down(cx.listener(|s,e:&KeyDownEvent,w,cx|{if s.grid_key(e,w,cx){cx.stop_propagation();}}))
            .on_action(cx.listener(|s,_:&Dismiss,w,cx|{if s.grid_focus.is_focused(w){s.selected=None;cx.notify();}else{cx.propagate();}}))
            .flex_1()
            .min_h_0()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .child(
                column().w(px(width)).h_full().child(header).child(
                    uniform_list(
                        "result-rows",
                        rows,
                        cx.processor(move |s, range: std::ops::Range<usize>, window, cx| {
                            let t = cx.omarchy().clone();
                            let mut rendered = Vec::new();
                            let selected_range = s.grid_range(cx);
                            for position in range {
                                let r = visible_rows[position];
                                let Some(values) = s
                                    .current_result()
                                    .and_then(|result| result.rows.get(r))
                                    .cloned()
                                else {
                                    continue;
                                };
                                let mut line = row()
                                    .id(("grid-row", r))
                                    .gap_0()
                                    .h(px(s
                                        .state
                                        .preferences
                                        .grid_row_height
                                        .max(if s.grid.draft.is_some() { 112. } else { 0. })
                                        .max(grid_font_size * 1.5 + 2.)))
                                    .w(px(width))
                                    .border_b_1()
                                    .border_color(t.divider().opacity(0.45))
                                    .bg(t.background.opacity(1.))
                                    .child(
                                        div()
                                            .id(("row-header",r)).debug_selector(move||format!("row-header-{r}"))
                                            .h_full().cursor_pointer()
                                            .bg(if s.grid.row_selection&&selected_range.is_some_and(|range|position>=range.start().row&&position<=range.end().row){t.accent.opacity(0.18)}else{t.background.opacity(0.)})
                                            .on_click(cx.listener(move|s,e:&ClickEvent,w,cx|{s.select_grid_rows(r,e.modifiers().shift,cx);if !s.has_row_draft(){w.focus(&s.grid_focus,cx);}cx.notify();}))
                                            .w(px(gutter_width))
                                            .flex_shrink_0()
                                            .px_3()
                                            .text_size(px(grid_detail_size))
                                            .text_color(t.secondary)
                                            .child(
                                                (r + 1
                                                    + if s.page == Page::Table {
                                                        s.browse_request.offset as usize
                                                    } else {
                                                        0
                                                    })
                                                .to_string(),
                                            ),
                                    );
                                for (c, cell) in values.iter().enumerate() {
                                    let selected = selected_range.is_some_and(|range| {
                                        position >= range.start().row
                                            && position <= range.end().row
                                            && c >= range.start().column
                                            && c <= range.end().column
                                    });
                                    let effective = if table {
                                        crate::grid_ui::effective_cell(
                                            &s.table_result,
                                            &s.changes,
                                            r,
                                            c,
                                        )
                                    } else {
                                        s.query_effective_cell(r,c).unwrap_or(cell)
                                    };
                                    let staged = effective != cell;
                                    let text =
                                        effective.display().replace('\n', " ↵ ").replace('\r', "");
                                    let text = if text.chars().count() > 500 {
                                        format!("{}…", text.chars().take(500).collect::<String>())
                                    } else {
                                        text
                                    };
                                    line = line.child(
                                        div()
                                            .id(("grid-cell", r * columns.len() + c))
                                            .h_full()
                                            .w(px(s.widths.get(c).copied().unwrap_or(170.)))
                                            .flex_shrink_0()
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .border_r_1()
                                            .border_color(t.divider().opacity(0.5))
                                            .bg(if selected {
                                                t.selection
                                            } else {
                                                gpui_kit::transparent_black()
                                            })
                                            .text_color(if staged {
                                                t.warning
                                            } else if matches!(effective, Cell::Null) {
                                                t.secondary
                                            } else {
                                                t.foreground
                                            })
                                            .font_family(t.mono_font.clone())
                                            .text_size(px(grid_font_size))
                                            .cursor_pointer()
                                            .child(
                                                if let Some(draft) =
                                                    s.grid.draft.as_ref().filter(|d| {
                                                        d.range.start()
                                                            == rowboat_core::grid::GridPosition {
                                                                row: position,
                                                                column: c,
                                                            }
                                                            && s.editing_target()
                                                                == Some(d.target.clone())
                                                            && s.query_draft_current()
                                                    })
                                                {
                                                    div()
                                                        .key_context("RowboatGridDraft")
                                                        // Native input actions and the later-installed
                                                        // application shortcut layer can win key dispatch.
                                                        // Handle both at the draft boundary, before Input
                                                        // consumes secondary Enter or RunQuery bubbles out.
                                                        .capture_action(cx.listener(|_,a:&gpui_kit::base::input::Enter,w,cx|{
                                                            if a.secondary && !a.shift {
                                                                cx.stop_propagation();
                                                                cx.defer_in(w,|s,w,cx|{s.cell_edit_command("enter",true,false,w,cx);});
                                                            }
                                                        }))
                                                        .capture_action(cx.listener(|s,_:&RunQuery,w,cx|{
                                                            cx.stop_propagation();
                                                            if s.shortcut_label("run_query")=="ctrl-enter" {
                                                                cx.defer_in(w,|s,w,cx|{s.cell_edit_command("enter",true,false,w,cx);});
                                                            }
                                                        }))
                                                        .w_full()
                                                        .child(
                                                            ui::textarea(
                                                                "inline-cell-value",
                                                                &draft.value,
                                                                window,
                                                                cx,
                                                            )
                                                            .text_size(px(grid_font_size))
                                                            .font_family(t.mono_font.clone())
                                                            .py(px(0.))
                                                            .h_full(),
                                                        )
                                                        .into_any_element()
                                                } else {
                                                    div()
                                                        .truncate()
                                                        .child(if staged {
                                                            format!("● {text}")
                                                        } else {
                                                            text
                                                        })
                                                        .into_any_element()
                                                },
                                            )
                                            .on_click(cx.listener(
                                                move |s, e: &ClickEvent, w, cx| {
                                                    s.select_grid_cell(
                                                        r,
                                                        c,
                                                        e.modifiers().shift,
                                                        cx,
                                                    );
                                                    if s.grid.draft.is_none(){w.focus(&s.grid_focus,cx);}
                                                    if e.click_count() == 2 {
                                                        if s.grid_writable() {
                                                            s.begin_cell_edit(w, cx);
                                                        } else {
                                                            s.inspect_cell(w, cx);
                                                        }
                                                    }
                                                    cx.notify();
                                                },
                                            )),
                                    );
                                }
                                rendered.push(line);
                            }
                            rendered
                        }),
                    )
                    .track_scroll(&self.grid_scroll)
                    .map(|mut list|{list.style().restrict_scroll_to_axis=Some(true);list})
                    .flex_1()
                    .min_h_0()
                    .w(px(width)),
                ),
            );
        let mut footer = row()
            .min_h(px(36.))
            .flex_wrap()
            .py_1()
            .px_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(t.divider())
            .bg(t.surface.opacity(1.))
            .text_size(px(11.))
            .child(if rows == total_rows {
                format!("{row_count} · {timing}")
            } else {
                format!("{rows} shown · {row_count} · {timing}")
            })
            .when(!result_notice.is_empty(), |d| {
                d.child(div().text_color(t.warning).child(result_notice))
            })
            .child(div().flex_1());
        if table {
            footer = footer
                .child(action(
                    "previous-page",
                    "← Previous",
                    false,
                    free && self.changes.is_empty() && self.browse_request.offset > 0,
                    cx,
                    |s, _, cx| {
                        s.browse_request.offset = s
                            .browse_request
                            .offset
                            .saturating_sub(s.browse_request.limit as u64);
                        s.refresh_table(cx);
                    },
                ))
                .child(label(
                    format!(
                        "Page {} · {} / page",
                        self.browse_request.offset / self.browse_request.limit as u64 + 1,
                        self.browse_request.limit
                    ),
                    cx,
                ))
                .child(action(
                    "next-page",
                    "Next →",
                    false,
                    free && self.changes.is_empty() && rows >= self.browse_request.limit as usize,
                    cx,
                    |s, _, cx| {
                        s.browse_request.offset += s.browse_request.limit as u64;
                        s.refresh_table(cx);
                    },
                ));
        } else {
            footer = footer.child(label("F6 focus · Shift+Space row · Enter inspect", cx));
        }
        // Scrolling alone does not paint scrollbars in GPUI. Keep both overlays
        // outside the horizontally scrolled content so the vertical track stays
        // at the visible right edge, even when the grid is thousands of pixels wide.
        let header_height = 44_f32.max(grid_font_size * 3. + 8.);
        let grid = div()
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .pr(px(12.))
                    .pb(px(12.))
                    .flex()
                    .flex_col()
                    .child(grid),
            )
            .child(
                div()
                    .absolute()
                    .top(px(header_height))
                    .bottom(px(12.))
                    .right_0()
                    .w(px(12.))
                    .debug_selector(|| "results-scrollbar-y".into())
                    .child(
                        ui::scrollbar(
                            "results-scroll-y",
                            gpui_kit::base::ScrollbarAxis::Vertical,
                            &self.grid_scroll,
                            window,
                            cx,
                        )
                        .viewport_from_layout(),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right(px(12.))
                    .bottom_0()
                    .h(px(12.))
                    .debug_selector(|| "results-scrollbar-x".into())
                    .child(
                        ui::scrollbar(
                            "results-scroll-x",
                            gpui_kit::base::ScrollbarAxis::Horizontal,
                            &self.grid_x_scroll,
                            window,
                            cx,
                        )
                        .viewport_from_layout(),
                    ),
            );
        content.child(grid).child(footer).into_any_element()
    }

    fn empty_results(&self, cx: &App) -> AnyElement {
        let t = cx.omarchy();
        column()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .child(icon(ui::IconName::Table).size(px(32.)).text_color(t.accent))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.bright)
                    .child(if self.database.is_some() {
                        "Run a query to see results"
                    } else {
                        "Connect to get started"
                    }),
            )
            .child(div().text_size(px(13.)).text_color(t.secondary).child(
                if self.database.is_some() {
                    "Open a table in the explorer, or run a SQL query above."
                } else {
                    "Add a PostgreSQL, MySQL or SQLite connection."
                },
            ))
            .into_any_element()
    }

    fn render_structure(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::management::Operation;
        let sqlite = self
            .profile
            .as_ref()
            .is_some_and(|p| p.engine == Engine::Sqlite);
        let t = cx.omarchy().clone();
        let mut content = column()
            .p_4()
            .gap_1()
            .child(
                row()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Table structure"),
                    )
                    .child(div().flex_1())
                    .child(action(
                        "copy-ddl",
                        "Copy DDL",
                        false,
                        !self.structure.definition.is_empty(),
                        cx,
                        |s, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                s.structure.definition.clone(),
                            ));
                            s.status = "Definition copied".into();
                            cx.notify();
                        },
                    )),
            )
            .child(
                row()
                    .flex_wrap()
                    .child(action(
                        "manage-structure",
                        if sqlite {
                            "Columns & indexes…"
                        } else {
                            "Alter columns, indexes & keys…"
                        },
                        true,
                        self.structure_action_enabled(Operation::AddColumn),
                        cx,
                        |s, w, cx| {
                            s.show_management(crate::management::Operation::AddColumn, None, w, cx)
                        },
                    ))
                    .child(action(
                        "rename-table",
                        "Rename table…",
                        false,
                        self.structure_action_enabled(Operation::RenameTable),
                        cx,
                        |s, w, cx| {
                            s.show_management(
                                crate::management::Operation::RenameTable,
                                None,
                                w,
                                cx,
                            )
                        },
                    ))
                    .child(action(
                        "drop-table",
                        "Drop table…",
                        false,
                        self.structure_action_enabled(Operation::DropTable),
                        cx,
                        |s, w, cx| {
                            s.show_management(crate::management::Operation::DropTable, None, w, cx)
                        },
                    ))
                    .child(action(
                        "inspect-schema",
                        "Inspect schema / indexes / FKs…",
                        false,
                        self.structure_action_enabled(Operation::InspectConstraints),
                        cx,
                        |s, w, cx| s.show_management(Operation::InspectConstraints, None, w, cx),
                    )),
            )
            .child(section("COLUMNS", cx));
        if sqlite {
            content = content.child(label("SQLite native schema. Type/default/nullability and constraint alterations require an unsupported table rebuild; index rename is unavailable. Mutations support ordinary tables only, excluding views, virtual/shadow/internal objects.", cx));
        }
        for col in &self.structure.columns {
            let name = col.name.clone();
            content = content.child(
                row()
                    .py_2()
                    .flex_wrap()
                    .border_b_1()
                    .border_color(t.divider())
                    .child(
                        row()
                            .gap_1()
                            .w(px(220.))
                            .font_family(t.mono_font.clone())
                            .when(col.primary_key, |d| {
                                d.child(
                                    icon(ui::IconName::KeyRound)
                                        .size(px(14.))
                                        .text_color(t.accent),
                                )
                            })
                            .child(div().truncate().child(col.name.clone())),
                    )
                    .child(
                        div()
                            .w(px(180.))
                            .text_color(t.accent)
                            .child(col.data_type.clone()),
                    )
                    .child(
                        div()
                            .w(px(100.))
                            .child(if col.nullable { "NULL" } else { "NOT NULL" }),
                    )
                    .child(ui::with_tooltip(
                        div()
                            .id(format!("column-default-{}", col.name))
                            .max_w(px(260.))
                            .truncate()
                            .text_color(t.secondary)
                            .child(col.default_value.clone().unwrap_or_default()),
                        col.default_value.clone().unwrap_or_default(),
                    ))
                    .child(div().flex_1())
                    .child(action(
                        format!("alter-column-{name}"),
                        if sqlite {
                            "Rename column…"
                        } else {
                            "Alter / generated…"
                        },
                        false,
                        self.structure_action_enabled(if sqlite {
                            Operation::RenameColumn
                        } else {
                            Operation::AlterColumn
                        }),
                        cx,
                        move |s, w, cx| {
                            if s.profile
                                .as_ref()
                                .is_some_and(|p| p.engine == Engine::MySql)
                            {
                                s.show_advanced_column(name.clone(), w, cx);
                                return;
                            }
                            s.show_management(
                                if sqlite {
                                    Operation::RenameColumn
                                } else {
                                    Operation::AlterColumn
                                },
                                Some(name.clone()),
                                w,
                                cx,
                            )
                        },
                    )),
            );
        }
        content = content.child(section("INDEXES", cx));
        if self.structure.indexes.is_empty() {
            content = content.child(label("No indexes reported", cx));
        }
        for index in &self.structure.indexes {
            let rename = index.name.clone();
            let drop_name = index.name.clone();
            content = content.child(
                column()
                    .gap_1()
                    .py_2()
                    .child(
                        row()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(index.name.clone()),
                            )
                            .child(label(if index.unique { "UNIQUE" } else { "INDEX" }, cx))
                            .child(div().flex_1())
                            .child(action(
                                format!("rename-index-{rename}"),
                                "Rename…",
                                false,
                                self.structure_action_enabled(Operation::RenameIndex),
                                cx,
                                move |s, w, cx| {
                                    s.show_management(
                                        crate::management::Operation::RenameIndex,
                                        Some(rename.clone()),
                                        w,
                                        cx,
                                    )
                                },
                            ))
                            .child(action(
                                format!("drop-index-{drop_name}"),
                                "Drop…",
                                false,
                                self.structure_action_enabled(Operation::DropIndex)
                                    && (!sqlite
                                        || !index.name.to_ascii_lowercase().starts_with("sqlite_")),
                                cx,
                                move |s, w, cx| {
                                    s.show_management(
                                        crate::management::Operation::DropIndex,
                                        Some(drop_name.clone()),
                                        w,
                                        cx,
                                    )
                                },
                            )),
                    )
                    .child(label(index.columns.clone(), cx))
                    .child(
                        div()
                            .font_family(t.mono_font.clone())
                            .text_size(px(11.))
                            .child(index.definition.clone()),
                    ),
            );
        }
        content = content.child(section("FOREIGN KEYS", cx));
        if self.structure.foreign_keys.is_empty() {
            content = content.child(label("No foreign keys reported", cx));
        }
        for key in &self.structure.foreign_keys {
            let name = key.name.clone();
            content = content.child(
                column()
                    .gap_1()
                    .py_2()
                    .child(
                        row()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(key.name.clone()),
                            )
                            .child(div().flex_1())
                            .child(action(
                                format!("drop-fk-{name}"),
                                "Drop foreign key…",
                                false,
                                self.structure_action_enabled(Operation::DropForeignKey),
                                cx,
                                move |s, w, cx| {
                                    s.show_management(
                                        crate::management::Operation::DropForeignKey,
                                        Some(name.clone()),
                                        w,
                                        cx,
                                    )
                                },
                            )),
                    )
                    .child(format!(
                        "{} → {} ({})",
                        key.columns, key.referenced_table, key.referenced_columns
                    ))
                    .child(label(
                        format!("ON UPDATE {} · ON DELETE {}", key.on_update, key.on_delete),
                        cx,
                    )),
            );
        }
        content = content.child(section("DEFINITION", cx)).child(
            div()
                .p_4()
                .bg(t.inset.opacity(1.))
                .font_family(t.mono_font.clone())
                .text_size(px(12.))
                .child(if self.structure.definition.is_empty() {
                    "No definition reported".into()
                } else {
                    self.structure.definition.clone()
                }),
        );
        div()
            .id("structure-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(content)
            .into_any_element()
    }

    fn render_changes(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.omarchy().clone();
        let query = self.query_edit.review;
        let header=column().id("review-actions").debug_selector(||"review-actions".into()).px_4().py_3().gap_2().flex_shrink_0().bg(t.surface.opacity(1.)).border_b_1().border_color(t.divider())
            .child(row().flex_wrap().child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(format!("Review {} changes",self.changes.len()))).child(div().flex_1())
                .child(action("back-content",if query{"Back to query result"}else{"Back to data"},false,!self.busy,cx,|s,w,cx|{s.query_edit.review=false;s.table_page=TablePage::Content;w.focus(&s.grid_focus,cx);cx.notify();}))
                .child(action("revert-changes","Discard all",false,!self.busy&&!self.has_row_draft()&&!self.changes.is_empty(),cx,|s,_,cx|{s.changes.clear();s.change_target=None;s.query_edit.batch=None;s.review_details.clear();s.status="Staged changes discarded".into();cx.notify();}))
                .child(action("apply-changes",if self.busy{"Applying…"}else{"Apply changes"},true,self.grid_writable()&&!self.has_row_draft()&&!self.changes.is_empty(),cx,|s,_,cx|s.apply_changes(cx)).debug_selector(||"apply-changes".into())))
            .child(label("Not yet applied. Original values are checked for conflicts before any row changes.",cx));
        let mut list = column().p_4().gap_2();
        if self.changes.is_empty() {
            list =
                list.child(div().py_8().child(
                    "No pending changes. Edit, insert, delete, or import rows from Content.",
                ));
        } else if let (Some(target), Some(profile)) = (&self.change_target, &self.profile) {
            let offset = self
                .review_offset
                .min((self.changes.len().saturating_sub(1) / 50) * 50);
            list = list.child(label(
                format!(
                    "Bound target: {} / {}.{} · changes {}–{} of {}",
                    profile.name,
                    target.table.schema,
                    target.table.name,
                    offset + 1,
                    (offset + 50).min(self.changes.len()),
                    self.changes.len()
                ),
                cx,
            ));
            for (i, change) in self.changes.iter().enumerate().skip(offset).take(50) {
                let summary = crate::review_ui::summary(change, &self.editing_structure().columns);
                let expanded = self.review_details.contains(&i);
                let sql = if query {
                    self.query_plan()
                        .ok_or_else(|| anyhow::anyhow!("Query proof no longer available"))
                        .and_then(|p| p.preview_changes(std::slice::from_ref(change)))
                } else {
                    rowboat_core::sql::preview_changes(
                        profile.engine,
                        &target.table,
                        std::slice::from_ref(change),
                    )
                }
                .unwrap_or_else(|e| e.to_string());
                let preview = if !query && sql.chars().count() > 4000 {
                    format!(
                        "{}…\n-- Preview shortened; Copy SQL includes the entire statement.",
                        sql.chars().take(4000).collect::<String>()
                    )
                } else {
                    sql.clone()
                };
                list = list.child(
                    column()
                        .p_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(t.divider())
                        .child(
                            row()
                                .child(label(format!("{} · change {}", match change {
                                    rowboat_core::RowChange::Insert { .. } => "Insert",
                                    rowboat_core::RowChange::Update { .. } => "Update",
                                    rowboat_core::RowChange::Delete { .. } => "Delete",
                                }, i + 1), cx).font_weight(FontWeight::SEMIBOLD).text_color(match change {
                                    rowboat_core::RowChange::Delete { .. } => t.danger,
                                    rowboat_core::RowChange::Insert { .. } => t.success,
                                    rowboat_core::RowChange::Update { .. } => t.accent,
                                }))
                                .child(div().flex_1())
                                .child(
                                    action(
                                        ("change-details", i),
                                        if expanded {
                                            "Hide details ▴"
                                        } else if query {
                                            "Originals ▾"
                                        } else {
                                            "SQL & originals ▾"
                                        },
                                        false,
                                        true,
                                        cx,
                                        move |s, _, cx| {
                                            if !s.review_details.remove(&i) {
                                                s.review_details.insert(i);
                                            }
                                            cx.notify();
                                        },
                                    )
                                    .debug_selector(move || format!("change-details-{i}")),
                                )
                                .child(action(
                                    ("copy-change", i),
                                    "Copy SQL",
                                    false,
                                    true,
                                    cx,
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            sql.clone(),
                                        ))
                                    },
                                ))
                                .child(action(
                                    ("remove-change", i),
                                    "Remove",
                                    false,
                                    !self.busy,
                                    cx,
                                    move |s, _, cx| {
                                        s.changes.remove(i);
                                        s.review_details.clear();
                                        if s.changes.is_empty() {
                                            s.change_target = None;
                                            s.query_edit.batch=None;
                                        }
                                        cx.notify();
                                    },
                                )),
                        )
                        .child(div().text_size(px(12.)).child(summary))
                        .when(query,|d|d.child(label("UPDATE PREVIEW · Apply executes bound values against this captured source",cx))
                            .child(div().id(("query-update-preview",i)).overflow_x_scroll().min_w_0().debug_selector(move||format!("query-update-preview-{i}")).font_family(t.mono_font.clone()).text_size(px(11.)).child(preview.clone())))
                        .when(expanded, |d| {
                            d.child(label("Original / draft values (preview)", cx))
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .child(crate::staging::readable(change)),
                                )
                                .child(
                                    action(
                                        ("copy-change-evidence", i),
                                        "Copy full original / draft JSON",
                                        false,
                                        true,
                                        cx,
                                        {
                                            let evidence = serde_json::to_string_pretty(change)
                                                .unwrap_or_default();
                                            move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    evidence.clone(),
                                                ))
                                            }
                                        },
                                    )
                                    .debug_selector(move || format!("copy-change-evidence-{i}")),
                                )
                                .when(!query, |d| d.child(label(
                                    "SQL PREVIEW · literal illustration; Apply uses bound values",
                                    cx,
                                ))
                                .child(
                                    div()
                                        .font_family(t.mono_font.clone())
                                        .text_size(px(11.))
                                        .text_color(t.secondary)
                                        .child(preview),
                                ))
                        }),
                );
            }
            list = list.child(
                row()
                    .child(action(
                        "changes-prev",
                        "← Previous",
                        false,
                        offset > 0,
                        cx,
                        |s, _, cx| {
                            s.review_offset = s.review_offset.saturating_sub(50);
                            cx.notify();
                        },
                    ))
                    .child(div().flex_1())
                    .child(action(
                        "changes-next",
                        "Next →",
                        false,
                        offset + 50 < self.changes.len(),
                        cx,
                        move |s, _, cx| {
                            s.review_offset = offset + 50;
                            cx.notify();
                        },
                    )),
            );
        }
        column()
            .size_full()
            .child(header)
            .child(
                div()
                    .id("changes-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    fn render_connection_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.omarchy().clone();
        let form = self.form.as_ref().unwrap();
        let validation = if form.validation_attempted {
            form.validation_errors(cx)
        } else {
            Vec::new()
        };
        let feedback = form.test_feedback(cx);
        let free = !self.busy;
        let field = |key: &'static str,
                     title: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let error = validation
                .iter()
                .find(|(field, _)| *field == key)
                .map(|(_, message)| *message);
            let reveal = form.reveal_field == Some(key);
            let owner = cx.weak_entity();
            let instance = form.instance;
            column()
                .flex_1()
                .gap_2()
                .child(label(title, cx))
                .child(
                    div()
                        .border_1()
                        .border_color(if error.is_some() {
                            t.danger
                        } else {
                            transparent_black()
                        })
                        .child(
                            ui::input(key, &form.fields[key], window, cx)
                                .debug_selector(move || key.into()),
                        ),
                )
                .when_some(error, |d, message| {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(t.danger)
                            .child(message)
                            .debug_selector(move || format!("connection-error-{key}")),
                    )
                })
                .when(reveal, |d| {
                    d.on_children_prepainted(move |bounds, _, cx| {
                        let (Some(first), Some(last)) = (bounds.first(), bounds.last()) else {
                            return;
                        };
                        let top = first.top();
                        let bottom = last.bottom();
                        let owner = owner.clone();
                        cx.defer(move |cx| {
                            let _ = owner.update(cx, |s, cx| {
                                let Some(f) = s.form.as_mut().filter(|f| {
                                    f.instance == instance && f.reveal_field == Some(key)
                                }) else {
                                    return;
                                };
                                let viewport = f.scroll.bounds();
                                let mut offset = f.scroll.offset();
                                if top < viewport.top() {
                                    offset.y += viewport.top() - top;
                                } else if bottom > viewport.bottom() {
                                    offset.y -= bottom - viewport.bottom();
                                }
                                offset.y = offset.y.clamp(-f.scroll.max_offset().y, px(0.));
                                f.scroll.set_offset(offset);
                                f.reveal_field = None;
                                cx.notify();
                            });
                        });
                    })
                })
        };
        let mut engines = row().gap_3();
        for (id, engine) in [
            ("engine-postgres", Engine::Postgres),
            ("engine-mysql", Engine::MySql),
            ("engine-sqlite", Engine::Sqlite),
        ] {
            engines = engines.child(
                action(
                    id,
                    engine.descriptor().name,
                    false,
                    free,
                    cx,
                    move |s, w, cx| {
                        if let Some(f) = &mut s.form
                            && f.engine != engine
                        {
                            f.select_engine(engine, w, cx);
                            s.startup_connect = None;
                        }
                        cx.notify();
                    },
                )
                .debug_selector(move || id.into())
                .selected(form.engine == engine)
                .flex_1(),
            );
        }
        let mut body = column()
            .gap_4()
            .p_4()
            .child(
                row()
                    .pb_4()
                    .border_b_1()
                    .border_color(t.divider())
                    .child(
                        column()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Connect to a database"),
                            )
                            .child(label("A direct, native connection to your data.", cx)),
                    )
                    .child(div().flex_1())
                    .child(icon_action(
                        "close-connection",
                        ui::IconName::Close,
                        "Close connection form",
                        free,
                        cx,
                        |s, _, cx| {
                            s.form = None;
                            cx.notify();
                        },
                    )),
            )
            .child(engines)
            .child(
                column()
                    .gap_2()
                    .child(
                        row()
                            .flex_wrap()
                            .child(label("CONNECTION URL (OPTIONAL)", cx))
                            .child(div().flex_1())
                            .child(
                                action(
                                    "paste-connection-url",
                                    "Paste URL",
                                    false,
                                    free,
                                    cx,
                                    |s, w, cx| s.paste_connection_url(w, cx),
                                )
                                .debug_selector(|| "paste-connection-url".into()),
                            ),
                    )
                    .child(
                        row()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .capture_key_down(cx.listener(|s, e: &KeyDownEvent, w, cx| {
                                        if e.keystroke.key == "enter"
                                            && !e.keystroke.modifiers.modified()
                                        {
                                            s.fill_connection_from_url(w, cx);
                                            cx.stop_propagation();
                                        }
                                    }))
                                    .child(
                                        ui::input("connection-url-input", &form.url, window, cx)
                                            .border_color(if form.url_error.is_some() {
                                                t.danger
                                            } else {
                                                t.control_border()
                                            })
                                            .debug_selector(|| "connection-url-input".into()),
                                    ),
                            )
                            .child(
                                action(
                                    "fill-connection-url",
                                    "Fill fields",
                                    false,
                                    free,
                                    cx,
                                    |s, w, cx| {
                                        s.fill_connection_from_url(w, cx);
                                    },
                                )
                                .debug_selector(|| "fill-connection-url".into()),
                            ),
                    )
                    .when_some(form.url_error.clone(), |d, error| {
                        d.child(div().text_color(t.danger).child(error))
                    }),
            )
            .child(
                row()
                    .gap_4()
                    .child(field("name", "CONNECTION NAME", window, cx))
                    .child(field("group", "GROUP", window, cx)),
            );
        if form.is_file() {
            use rowboat_core::SqliteOpenMode;
            let create = form.file_mode == SqliteOpenMode::CreateIfMissing;
            body = body
                .child(field("file_path", "DATABASE FILE PATH", window, cx))
                .child(
                    row()
                        .child(
                            action(
                                "sqlite-open-existing",
                                "Open existing",
                                false,
                                free,
                                cx,
                                |s, _, cx| {
                                    if let Some(f) = &mut s.form {
                                        f.file_mode = SqliteOpenMode::OpenExisting;
                                        f.file_request = None;
                                        f.draft_changed();
                                    }
                                    cx.notify();
                                },
                            )
                            .debug_selector(|| "sqlite-open-existing".into())
                            .selected(!create),
                        )
                        .child(
                            action(
                                "sqlite-create",
                                "Create if missing",
                                false,
                                free && !form.read_only,
                                cx,
                                |s, _, cx| {
                                    if let Some(f) = &mut s.form {
                                        f.file_mode = SqliteOpenMode::CreateIfMissing;
                                        f.file_request = None;
                                        f.draft_changed();
                                    }
                                    cx.notify();
                                },
                            )
                            .debug_selector(|| "sqlite-create".into())
                            .selected(create),
                        )
                        .child(
                            action(
                                "sqlite-choose-file",
                                "Choose file…",
                                false,
                                free && form.file_request.is_none(),
                                cx,
                                |s, w, cx| s.choose_database_file(w, cx),
                            )
                            .debug_selector(|| "sqlite-choose-file".into()),
                        ),
                )
                .child(label(
                    if create {
                        "Creates a missing database on Connect. Existing files are preserved."
                    } else {
                        "Opens an existing database. A missing file is an error."
                    },
                    cx,
                ))
                .child(action(
                    "toggle-read-only",
                    if form.read_only {
                        "☑  Read-only file"
                    } else {
                        "☐  Read-only file"
                    },
                    false,
                    free,
                    cx,
                    |s, _, cx| {
                        if let Some(f) = &mut s.form {
                            f.read_only = !f.read_only;
                            if f.read_only {
                                f.file_mode = SqliteOpenMode::OpenExisting;
                            }
                            f.file_request = None;
                            f.draft_changed();
                        }
                        cx.notify();
                    },
                ));
        } else {
            body = body.child(row().gap_4().child(field("host","HOST",window,cx)).child(div().w(px(125.)).child(field("port","PORT",window,cx))))
            .child(row().gap_4().child(field("database","DATABASE",window,cx)).child(field("username","USERNAME",window,cx)))
            .child(column().gap_2().child(label("PASSWORD",cx)).child(ui::input("connection-password",&form.password,window,cx).debug_selector(||"connection-password".into())).child(label("Memory-only by default. Remembered passwords use the system keyring, never connection or recovery JSON files.",cx)))
            .child(row().child(action("toggle-read-only",if form.read_only{"☑  Read-only session"}else{"☐  Read-only session"},false,free,cx,|s,_,cx|{if let Some(f)=&mut s.form{f.read_only = !f.read_only;f.draft_changed();}cx.notify();})).child(div().flex_1())
                .child(action("toggle-advanced",if form.advanced{"▴  TLS & SSH"}else{"▾  TLS & SSH"},false,true,cx,|s,_,cx|{if let Some(f)=&mut s.form{f.advanced = !f.advanced;}cx.notify();})));
        }
        if form.has_exact_password() {
            body = body.child(label("The imported password is retained exactly in memory, including characters this single-line field cannot display. Changing the password replaces the imported value.", cx));
        }
        if !form.is_file() && form.advanced {
            body = body
                .child(section("TLS / SSL", cx))
                .child(field(
                    "ssl_mode",
                    "MODE · disable / prefer / require / verify-ca / verify-full",
                    window,
                    cx,
                ))
                .child(field("ssl_ca", "CA CERTIFICATE PATH", window, cx))
                .child(
                    row()
                        .gap_4()
                        .child(field("ssl_cert", "CLIENT CERTIFICATE PATH", window, cx))
                        .child(field("ssl_key", "CLIENT KEY PATH", window, cx)),
                )
                .child(action(
                    "toggle-ssh",
                    if form.ssh {
                        "☑  Connect through SSH tunnel"
                    } else {
                        "☐  Connect through SSH tunnel"
                    },
                    false,
                    free,
                    cx,
                    |s, _, cx| {
                        if let Some(f) = &mut s.form {
                            f.ssh = !f.ssh;
                            f.draft_changed();
                        }
                        cx.notify();
                    },
                ));
            if form.ssh {
                body=body.child(row().gap_4().child(field("ssh_host","SSH HOST",window,cx)).child(div().w(px(125.)).child(field("ssh_port","SSH PORT",window,cx))))
                .child(row().gap_4().child(field("ssh_username","SSH USERNAME",window,cx)).child(field("ssh_identity","IDENTITY FILE (OPTIONAL)",window,cx))).child(label("Uses your SSH agent or identity file and existing trusted known_hosts entries.",cx));
            }
        }
        if !form.is_file() {
            body = body.child(
                row()
                    .child(action(
                        "remember-password",
                        if form.remember {
                            "☑ Remember password in system keyring"
                        } else {
                            "☐ Remember password in system keyring"
                        },
                        false,
                        free && !form.keyring_loading,
                        cx,
                        |s, _, cx| {
                            if let Some(f) = &mut s.form {
                                f.remember = !f.remember;
                                f.remember_touched = true;
                                f.draft_changed();
                            }
                            cx.notify();
                        },
                    ))
                    .child(label(
                        if form.keyring_loading {
                            "Checking keyring…"
                        } else if form.credential_exists && !form.remember {
                            "Save to remove the stored keyring password."
                        } else {
                            ""
                        },
                        cx,
                    )),
            );
            body=body.child(action("copy-connection-url","Copy password-free URL",false,free,cx,|s,_,cx|s.copy_connection_url(cx)))
            .child(label("Portable file export excludes passwords and machine-local TLS/SSH key paths; configure those locally after import.",cx));
        }
        if !self.transaction_known {
            body = body.child(label("The previous session is unavailable or its transaction state is unknown. Replacement cannot confirm whether earlier writes committed. SQL and parameters stay open; staged edits become detached evidence. Nothing is replayed.", cx))
                .child(action("confirm-session-replacement", if form.confirm_replace { "☑ I understand the previous outcome is unconfirmed" } else { "☐ I understand the previous outcome is unconfirmed" }, false, free, cx, |s, _, cx| {
                    if let Some(form) = &mut s.form { form.confirm_replace = !form.confirm_replace; }
                    cx.notify();
                }).debug_selector(|| "confirm-session-replacement".into()));
        }
        let footer = row()
            .p_4()
            .bg(t.surface.opacity(1.))
            .flex_shrink_0()
            .flex_wrap()
            .border_t_1()
            .border_color(t.divider())
            .child(label(
                if self.loaded {
                    "Connection profiles are stored locally."
                } else {
                    "Local state unavailable; profiles cannot be saved."
                },
                cx,
            ))
            .child(div().flex_1())
            .child(
                action(
                    "test-connection",
                    if form.test_request.is_some() {
                        "Testing…"
                    } else {
                        "Test connection"
                    },
                    false,
                    free && !form.keyring_loading
                        && form.file_request.is_none()
                        && form.test_request.is_none(),
                    cx,
                    |s, w, cx| {
                        if s.prepare_connection_action(w, cx) {
                            s.test_connection(cx);
                        }
                    },
                )
                .debug_selector(|| "test-connection".into()),
            )
            .child(
                action(
                    "save-connection",
                    "Save",
                    false,
                    free && self.loaded && !form.keyring_loading && form.file_request.is_none(),
                    cx,
                    |s, w, cx| {
                        if s.prepare_connection_action(w, cx) {
                            s.save_connection(false, cx);
                        }
                    },
                )
                .debug_selector(|| "save-connection".into()),
            )
            .child(
                action(
                    "connect",
                    if self.transaction_known {
                        "Save & Connect"
                    } else {
                        "Save & Replace session"
                    },
                    true,
                    free && self.loaded
                        && !form.keyring_loading
                        && form.file_request.is_none()
                        && (self.transaction_known || form.confirm_replace),
                    cx,
                    |s, w, cx| {
                        if !s.prepare_connection_action(w, cx) {
                            return;
                        }
                        if s.transaction_known {
                            s.save_connection(true, cx);
                        } else {
                            s.replace_unknown_session(cx);
                        }
                    },
                )
                .debug_selector(|| "connect".into()),
            );
        let mut footer = column()
            .flex_shrink_0()
            .when(self.profile_editor, |d| {
                d.when_some(self.error.clone(), |d, error| {
                    d.child(div().px_4().py_2().text_color(t.danger).child(error))
                })
            })
            .child(footer);
        if let Some(success) = feedback
            && let Some((_, message)) = &form.test_result
        {
            footer = footer.child(
                column()
                    .id("connection-test-result")
                    .debug_selector(|| "connection-test-result".into())
                    .max_h(px(130.))
                    .overflow_y_scroll()
                    .p_3()
                    .gap_1()
                    .border_t_1()
                    .border_color(if success { t.success } else { t.danger })
                    .bg(if success {
                        t.success.opacity(0.10)
                    } else {
                        t.danger.opacity(0.10)
                    })
                    .child(
                        row()
                            .child(icon(if success {
                                ui::IconName::Check
                            } else {
                                ui::IconName::Info
                            }))
                            .text_color(if success { t.success } else { t.danger })
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(if success {
                                "Connection verified"
                            } else {
                                "Connection test failed"
                            }))
                            .debug_selector(move || {
                                if success {
                                    "connection-test-success".into()
                                } else {
                                    "connection-test-failure".into()
                                }
                            }),
                    )
                    .child(label(message.clone(), cx)),
            );
        } else if form.test_request.is_some() {
            footer = footer.child(crate::loading_ui::header(
                "Testing a temporary connection · nothing saved",
                window,
                cx,
            ));
        } else if form.test_stale || form.test_result.is_some() {
            footer = footer.child(
                label(
                    "Connection settings changed · test again to verify this target.",
                    cx,
                )
                .px_3()
                .pb_3(),
            );
        }
        self.modal(
            column()
                .id("connection-form-frame")
                .debug_selector(|| "connection-form-frame".into())
                .border_2()
                .border_color(match feedback {
                    Some(true) => t.success.opacity(0.8),
                    Some(false) => t.danger.opacity(0.8),
                    None => t.divider(),
                })
                .w(px(680.).min(window.viewport_size().width - px(64.)))
                .h((window.viewport_size().height - px(120.)).max(px(240.)))
                .child(
                    div()
                        .id("connection-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&form.scroll)
                        .child(body),
                )
                .child(footer)
                .into_any_element(),
            cx,
        )
    }

    fn render_row_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.omarchy().clone();
        let editor = self.row_editor.as_ref().unwrap();
        let insert = editor.original.is_none();
        let mut body = column()
            .p_6()
            .gap_3()
            .child(
                row()
                    .child(
                        div()
                            .text_size(px(21.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if insert { "Insert row" } else { "Edit row" }),
                    )
                    .child(div().flex_1())
                    .child(action(
                        "close-row-editor",
                        "✕",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            s.row_editor = None;
                            cx.notify();
                        },
                    )),
            )
            .child(label(
                if insert {
                    "Enable columns to supply a value. Omitted columns use server defaults."
                } else {
                    "Changes are staged first. SQL NULL is distinct from empty text."
                },
                cx,
            ));
        for (i, field) in editor.fields.iter().enumerate() {
            body = body.child(
                row()
                    .gap_3()
                    .child(
                        column()
                            .w(px(170.))
                            .gap_1()
                            .child(div().truncate().child(field.name.clone()))
                            .child(label(field.data_type.clone(), cx)),
                    )
                    .child(action(
                        ("row-include", i),
                        if field.include { "☑" } else { "☐" },
                        false,
                        true,
                        cx,
                        move |s, _, cx| {
                            if let Some(e) = &mut s.row_editor {
                                e.fields[i].include = !e.fields[i].include;
                            }
                            cx.notify();
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .opacity(if field.include && !field.null {
                                1.
                            } else {
                                0.4
                            })
                            .child(
                                ui::textarea(("row-value", i), &field.value, window, cx)
                                    .min_h(px(36.))
                                    .h(px(62.)),
                            ),
                    )
                    .child(
                        action(("row-null", i), "NULL", false, true, cx, move |s, _, cx| {
                            if let Some(e) = &mut s.row_editor {
                                e.fields[i].null = !e.fields[i].null;
                                e.fields[i].include = true;
                            }
                            cx.notify();
                        })
                        .selected(field.null),
                    ),
            );
        }
        body = body
            .child(label(
                "Text preserves line breaks. Binary columns accept hexadecimal bytes, e.g. 0x00ff. Empty binary and SQL NULL are distinct.",
                cx,
            ))
            .child(
                row()
                    .pt_3()
                    .border_t_1()
                    .border_color(t.divider())
                    .child(div().flex_1())
                    .child(action(
                        "stage-row",
                        "Stage change",
                        true,
                        true,
                        cx,
                        |s, _, cx| s.stage_editor(cx),
                    )),
            );
        self.modal(
            div()
                .id("row-edit-scroll")
                .w(px(800.))
                .max_h(px(750.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }

    pub(crate) fn modal(&self, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        div()
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(black().opacity(0.55))
            .child(
                column()
                    .max_w((self.viewport.width - px(32.)).max(px(240.)))
                    .key_context("RowboatModal")
                    // The workbench sits below the 40px workspace band. Reserve
                    // that band as well as margins when clamping window-sized dialogs.
                    .max_h((self.viewport.height - px(80.)).max(px(160.)))
                    .min_h_0()
                    .bg(cx.omarchy().surface.opacity(1.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(cx.omarchy().border)
                    .shadow_lg()
                    .child(
                        row()
                            .flex_shrink_0()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.omarchy().divider())
                            .child(label("Tab / Shift+Tab · move focus", cx))
                            .child(div().flex_1())
                            .child(action(
                                "modal-dismiss",
                                "Close · Esc",
                                false,
                                !self.busy,
                                cx,
                                |_, w, cx| w.dispatch_action(Box::new(Dismiss), cx),
                            )),
                    )
                    .child(
                        div()
                            .id("modal-viewport-scroll")
                            .min_h_0()
                            .flex_1()
                            .overflow_scroll()
                            .child(content),
                    )
                    .track_focus(&self.modal_focus),
            )
            .into_any_element()
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.viewport = window.viewport_size();
        if self.profile_editor {
            return column()
                .relative()
                .size_full()
                .track_focus(&self.focus)
                .when(self.form.is_some(), |d| {
                    d.child(self.render_connection_form(window, cx))
                })
                .into_any_element();
        }
        if self.close_requested {
            self.pending_focus = None;
            return div()
                .size_full()
                .track_focus(&self.focus)
                .occlude()
                .p_6()
                .bg(cx.omarchy().background)
                .text_color(cx.omarchy().foreground)
                .child("Workspace actions are paused while the main window finishes its operation.")
                .into_any_element();
        }
        if let Some(focus) = self.pending_focus.take() {
            window.focus(&focus, cx);
        }
        self.repair_keyboard_focus(window, cx);
        if self.modal_open() && (self.focus.is_focused(window) || window.focused(cx).is_none()) {
            window.focus(&self.modal_focus, cx);
        } else if !self.modal_open()
            && (self.modal_focus.is_focused(window) || window.focused(cx).is_none())
        {
            window.focus(&self.focus, cx);
        }
        let t = cx.omarchy().clone();
        let header = self.compact_header(cx);
        let sidebar = self.render_sidebar(window, cx);
        let body = match self.page {
            Page::Query => self.render_query(window, cx),
            Page::Table => self.render_table(window, cx),
            Page::History => self.render_library(window, cx),
            Page::Tools => self.render_tools(window, cx),
            Page::Objects => self.render_objects_page(window, cx),
            Page::Dashboard => self.render_dashboard(cx),
        };
        let free = !self.busy;
        let connected = self.database.is_some();
        let status = row()
            .h(px(33.))
            .px_4()
            .flex_shrink_0()
            .border_t_1()
            .border_color(t.divider())
            .bg(t.surface.opacity(1.))
            .text_size(px(11.))
            .when(self.busy, |d| {
                d.child(crate::loading_ui::status(window, cx))
            })
            .when(!self.busy, |d| {
                d.child(
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(if self.error.is_some() {
                            t.danger
                        } else if connected {
                            t.success
                        } else {
                            t.secondary
                        }),
                )
            })
            .child(div().truncate().flex_1().child(self.status.clone()))
            .when_some(
                (self.page == Page::Query)
                    .then(|| {
                        self.tabs[self.active_tab]
                            .query_progress
                            .as_ref()
                            .and_then(|p| p.timing.as_ref())
                            .map(|t| t.detail())
                    })
                    .flatten(),
                |d, detail| {
                    d.child(ui::with_tooltip(
                        div()
                            .id("query-timing-detail")
                            .debug_selector(|| "query-timing-detail".into())
                            .cursor_pointer()
                            .child(
                                row()
                                    .gap_1()
                                    .child(icon(ui::IconName::Clock).size(px(12.)))
                                    .child("Timing"),
                            ),
                        detail,
                    ))
                },
            )
            .when(connected && !self.transaction, |d| {
                d.child(action(
                    "begin",
                    "Begin",
                    false,
                    connected && free && !self.transaction,
                    cx,
                    |s, _, cx| s.transaction("BEGIN", cx),
                ))
            })
            .when(
                connected && (self.transaction || !self.transaction_known),
                |d| {
                    d.child(action(
                        "commit",
                        "Commit",
                        self.transaction,
                        connected && free,
                        cx,
                        |s, _, cx| s.transaction("COMMIT", cx),
                    ))
                    .child(action(
                        "rollback",
                        "Rollback",
                        false,
                        connected && free,
                        cx,
                        |s, _, cx| s.transaction("ROLLBACK", cx),
                    ))
                },
            );
        let mut root = column()
            .id("rowboat-workbench")
            .relative()
            .key_context("Rowboat")
            .track_focus(&self.focus)
            .size_full()
            .bg(t.background.opacity(1.))
            .text_color(t.foreground)
            .font_family(t.font.clone())
            .text_size(px(13.))
            .capture_action(cx.listener(|s, _: &Dismiss, window, cx| {
                if s.dismiss_diagram(window, cx) {
                    cx.stop_propagation();
                } else if s.preferences_theme_open(cx) {
                    window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .capture_action(
                cx.listener(|s, _: &gpui_kit::base::actions::Cancel, window, cx| {
                    if s.preferences_form.is_some() && !s.preferences_theme_open(cx) {
                        s.dismiss_preferences(window, cx);
                        cx.stop_propagation();
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .capture_key_down(cx.listener(|s, event: &KeyDownEvent, window, cx| {
                if s.preferences_theme_open(cx) {
                    if event.keystroke.key == "escape" {
                        window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
                        cx.stop_propagation();
                    }
                    // The choice popup owns navigation and Tab until it closes.
                    return;
                }
                if !s.modal_open()
                    && s.explorer_filter
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                    && matches!(
                        event.keystroke.key.as_str(),
                        "up" | "down" | "home" | "end" | "enter"
                    )
                {
                    s.explorer_key(&event.keystroke.key, cx);
                    cx.stop_propagation();
                    return;
                }
                if s.cell_edit_key(event, window, cx) {
                    cx.stop_propagation();
                    return;
                }
                if s.modal_open() && event.keystroke.key == "escape" {
                    window.dispatch_action(Box::new(Dismiss), cx);
                    cx.stop_propagation();
                    return;
                }
                if s.modal_open()
                    && event.keystroke.key == "tab"
                    && !event.keystroke.modifiers.control
                {
                    let before = window.focused(cx);
                    for _ in 0..256 {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev(cx);
                        } else {
                            window.focus_next(cx);
                        }
                        if s.modal_focus.contains_focused(window, cx)
                            || window.focused(cx) == before
                        {
                            break;
                        }
                    }
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape" && s.dismiss_access(window, cx) {
                    cx.stop_propagation();
                    return;
                }
                let key = event.keystroke.key.as_str();
                let modifiers = event.keystroke.modifiers;
                if modifiers.control
                    && rowboat_services::environment_os("ROWBOAT_TRACE_KEYS").is_some()
                {
                    eprintln!(
                        "rowboat-key capture key={key:?} ctrl={} shift={} alt={} platform={}",
                        modifiers.control, modifiers.shift, modifiers.alt, modifiers.platform
                    );
                }
            }))
            .on_action(cx.listener(|s, _: &RunQuery, _, cx| s.run_query(cx)))
            .on_action(cx.listener(|s, _: &RunCurrent, _, cx| {
                s.run_query_scope(rowboat_core::execution::RunScope::Current, false, cx)
            }))
            .on_action(cx.listener(|s, _: &RunSelection, _, cx| {
                s.run_query_scope(rowboat_core::execution::RunScope::Selection, false, cx)
            }))
            .on_action(cx.listener(|s, _: &RunAll, _, cx| {
                s.run_query_scope(rowboat_core::execution::RunScope::All, false, cx)
            }))
            .on_action(cx.listener(|s, _: &NamespaceUp, w, cx| s.namespace_key("up", w, cx)))
            .on_action(cx.listener(|s, _: &NamespaceDown, w, cx| s.namespace_key("down", w, cx)))
            .on_action(cx.listener(|s, _: &NamespaceFirst, w, cx| s.namespace_key("home", w, cx)))
            .on_action(cx.listener(|s, _: &NamespaceLast, w, cx| s.namespace_key("end", w, cx)))
            .on_action(cx.listener(|s, _: &NamespaceChoose, w, cx| s.namespace_key("enter", w, cx)))
            .on_action(cx.listener(|s, _: &FocusGrid, w, cx| s.focus_grid(w, cx)))
            .on_action(cx.listener(|s, _: &StageCell, w, cx| {
                s.cell_edit_command("enter", true, false, w, cx);
            }))
            .on_action(cx.listener(|s, _: &NextCell, w, cx| {
                s.cell_edit_command("tab", false, false, w, cx);
            }))
            .on_action(cx.listener(|s, _: &PreviousCell, w, cx| {
                s.cell_edit_command("tab", false, true, w, cx);
            }))
            .on_action(cx.listener(|s, _: &CancelCell, w, cx| {
                s.cell_edit_command("escape", false, false, w, cx);
            }))
            .on_action(cx.listener(|s, _: &ModalNext, w, cx| {
                if s.preferences_theme_open(cx) {
                    w.dispatch_action(Box::new(gpui_kit::base::input::IndentInline), cx);
                } else {
                    crate::keyboard_ui::modal_focus(false, &s.modal_focus, w, cx)
                }
            }))
            .on_action(cx.listener(|s, _: &ModalPrevious, w, cx| {
                if s.preferences_theme_open(cx) {
                    w.dispatch_action(Box::new(gpui_kit::base::input::OutdentInline), cx);
                } else {
                    crate::keyboard_ui::modal_focus(true, &s.modal_focus, w, cx)
                }
            }))
            .on_action(cx.listener(|s, _: &ExplorerUp, _, cx| s.explorer_key("up", cx)))
            .on_action(cx.listener(|s, _: &ExplorerDown, _, cx| s.explorer_key("down", cx)))
            .on_action(cx.listener(|s, _: &ExplorerFirst, _, cx| s.explorer_key("home", cx)))
            .on_action(cx.listener(|s, _: &ExplorerLast, _, cx| s.explorer_key("end", cx)))
            .on_action(cx.listener(|s, _: &ExplorerOpen, _, cx| s.explorer_key("enter", cx)))
            .on_action(cx.listener(|s, _: &NextSnippet, w, cx| {
                if !s.advance_snippet(w, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|s, _: &PreviousSnippet, w, cx| {
                if !s.previous_snippet(w, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|s, _: &ExpandSnippet, w, cx| s.expand_snippet_trigger(w, cx)))
            .on_action(cx.listener(|s, _: &PreviousResult, _, cx| s.navigate_result(false, cx)))
            .on_action(cx.listener(|s, _: &NextResult, _, cx| s.navigate_result(true, cx)))
            .on_action(cx.listener(|s, _: &NewQuery, w, cx| s.new_query(None, w, cx)))
            .on_action(cx.listener(|s, _: &FormatQuery, w, cx| s.format_query(w, cx)))
            .on_action(cx.listener(|s, _: &NewConnection, w, cx| s.show_connection(None, w, cx)))
            .on_action(cx.listener(|s, _: &CopyCell, _, cx| s.copy_cell(cx)))
            .on_action(cx.listener(|s, _: &CompleteSql, w, cx| s.complete_sql(w, cx)))
            .on_action(cx.listener(|s, _: &CloseQuery, w, cx| s.close_query(w, cx)))
            .on_action(cx.listener(|s, _: &QuickOpen, w, cx| {
                s.show_palette(w, cx);
            }))
            .on_action(cx.listener(|s, _: &CommandPalette, w, cx| s.show_palette(w, cx)))
            .on_action(cx.listener(|s, _: &ShowPreferences, w, cx| s.show_preferences(w, cx)))
            .on_action(cx.listener(|s, _: &Dismiss, w, cx| {
                if s.dismiss_diagram(w, cx) {
                    return;
                }
                if s.busy || s.close_requested {
                    return;
                }
                if s.dismiss_palette(w, cx) {
                    return;
                }
                if s.dismiss_library(w, cx) {
                    return;
                }
                if s.namespace.picker.take().is_some() {
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.custom_metrics.editor.take().is_some() {
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.snippet_active(s.tabs[s.active_tab].id)
                    && s.tabs[s.active_tab]
                        .editor
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(w)
                {
                    s.snippet_session = None;
                    cx.notify();
                    return;
                }
                if s.dashboards.editor.take().is_some() {
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.data_transfer.is_some() {
                    s.dismiss_data_transfer();
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.explain.as_ref().is_some_and(|e| e.open) {
                    s.dismiss_explain();
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.objects.dialog.is_some() {
                    if !s.busy {
                        s.objects.dialog = None;
                        w.focus(&s.focus, cx);
                    }
                    cx.notify();
                    return;
                }
                if s.relationships.is_some() {
                    s.dismiss_relationships();
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.filters.editor.take().is_some() {
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.column_form.is_some() && !s.busy {
                    s.column_form = None;
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.dismiss_access(w, cx) {
                    return;
                }
                if s.snippet_form.take().is_some() {
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.recovery.open && !s.busy {
                    s.recovery.open = false;
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.dismiss_preferences(w, cx) {
                    return;
                }
                if s.dismiss_grid_inspector(w, cx) {
                    return;
                }
                if s.grid.draft.take().is_some() {
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.url_input.is_some() {
                    s.url_input = None;
                    w.focus(&s.focus, cx);
                    cx.notify();
                    return;
                }
                if s.management.is_some() {
                    if !s.busy {
                        s.management = None;
                        w.focus(&s.focus, cx);
                        cx.notify();
                    }
                    return;
                }
                if s.advanced.designer.is_some() {
                    if !s.busy {
                        s.advanced.designer = None;
                        w.focus(&s.focus, cx);
                        cx.notify();
                    }
                    return;
                }
                if !s.busy {
                    s.form = None;
                    s.row_editor = None;
                }
                s.quick_open = false;
                s.dismiss_completions(cx);
                s.close_tab_requested = None;
                s.tabs[s.active_tab].show_parameters = false;
                s.error = None;
                w.focus(&s.focus, cx);
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|s, e: &MouseMoveEvent, _, cx| {
                if let Some((i, start, width)) = s.resizing {
                    if e.dragging() {
                        s.widths[i] = (width + f32::from(e.position.x) - start).clamp(75., 800.);
                        cx.notify();
                    } else {
                        s.resizing = None;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|s, _, _, _| s.resizing = None),
            )
            .child(header)
            .child(
                div().flex_1().min_h_0().child(
                    ui::resizable("workspace-split", Axis::Horizontal, cx)
                        .with_state(&self.sidebar_split)
                        .child(
                            ui::resizable_panel()
                                .size(px(280.))
                                .size_range(px(245.)..px(450.))
                                .child(sidebar),
                        )
                        .child(
                            ui::resizable_panel()
                                .size(px(1195.))
                                .size_range(px(400.)..px(4000.))
                                .child(body),
                        ),
                ),
            )
            .child(status);
        if !self.recovery.detached.is_empty() || self.recovery.error.is_some() {
            root = root.child(
                row()
                    .px_4()
                    .py_2()
                    .child(label(
                        self.recovery.error.clone().unwrap_or_else(|| {
                            format!(
                                "{} detached staged-edit recovery batches · no mutations restored",
                                self.recovery.detached.len()
                            )
                        }),
                        cx,
                    ))
                    .child(action(
                        "open-recovered-edits",
                        "Recovery…",
                        false,
                        !self.busy,
                        cx,
                        |s, w, cx| s.show_recovery(w, cx),
                    )),
            );
        }
        if self.form.is_some() {
            root = root.child(self.render_connection_form(window, cx));
        }
        if self.namespace.picker.is_some() {
            root = root.child(self.render_namespaces(window, cx));
        }
        if self.preferences_form.is_some() {
            root = root.child(self.render_preferences(window, cx));
        }
        if self.library.form.is_some() {
            root = root.child(self.render_library_form(window, cx));
        }
        if self.diagram.open {
            root = root.child(self.render_diagram(window, cx));
        }
        if self.recovery.open {
            root = root.child(self.render_recovery(cx));
        }
        if self.snippet_form.is_some() {
            root = root.child(self.render_snippets(window, cx));
        }
        if self.access_matrix.is_some() {
            root = root.child(self.render_access_matrix(window, cx));
        }
        if self.custom_metrics.editor.is_some() {
            root = root.child(self.render_custom_editor(window, cx));
        }
        if self.column_form.is_some() {
            root = root.child(self.render_advanced_column(window, cx));
        }
        if self.filters.editor.is_some() {
            root = root.child(self.render_filters(window, cx));
        }
        if self.relationships.is_some() {
            root = root.child(self.render_relationships(cx));
        }
        if self.dashboards.editor.is_some() {
            root = root.child(self.render_dashboard_editor(window, cx));
        }
        if self.data_transfer.is_some() {
            root = root.child(self.render_data_transfer(window, cx));
        }
        if self.explain.as_ref().is_some_and(|e| e.open) {
            root = root.child(self.render_explain(window, cx));
        }
        if self.objects.dialog.is_some() {
            root = root.child(self.render_object_definition(window, cx));
        }
        if self.url_input.is_some() {
            root = root.child(self.render_connection_url(window, cx));
        }
        if self.row_editor.is_some() {
            root = root.child(self.render_row_editor(window, cx));
        }
        if self.grid.inspector.is_some() {
            root = root.child(self.render_cell_inspector(window, cx));
        }
        if self.transfer.is_some() {
            root = root.child(self.render_transfer(window, cx));
        }
        if self.quick_open {
            root = root.child(self.render_palette(window, cx));
        }
        if self.advanced.designer.is_some() {
            root = root.child(self.render_designer(window, cx));
        }
        if self.management.is_some() {
            root = root.child(self.render_management(window, cx));
        }
        if self.tabs[self.active_tab].show_parameters {
            root = root.child(self.render_parameters(window, cx));
        }
        if self.close_tab_requested.is_some() {
            root = root.child(self.render_close_guard(cx));
        }
        if let Some(error) = self.error.clone() {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(45.))
                    .right(px(18.))
                    .w(px(620.))
                    .max_h(px(240.))
                    .border_1()
                    .border_color(t.danger)
                    .bg(t.background.opacity(1.))
                    .shadow_lg()
                    .child(
                        row()
                            .px_4()
                            .py_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(t.danger)
                                    .child("Unable to complete operation"),
                            )
                            .child(div().flex_1())
                            .child(action(
                                "dismiss-error",
                                "✕",
                                false,
                                true,
                                cx,
                                |s, _, cx| {
                                    s.error = None;
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        div()
                            .id("error-scroll")
                            .max_h(px(170.))
                            .overflow_y_scroll()
                            .px_4()
                            .pb_4()
                            .text_size(px(12.))
                            .child(error),
                    ),
            );
        }
        root.into_any_element()
    }
}
