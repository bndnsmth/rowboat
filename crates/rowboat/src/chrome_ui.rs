//! Compact daily-driver menus. Commands are revalidated at selection time.
use crate::{
    app::*,
    view::{action, icon, icon_action, label, row},
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_services::TransferFormat;

#[derive(Clone, Copy)]
pub enum Menu {
    Connection,
    Query,
    Selection,
    Transfer,
    Navigate,
}
#[derive(Clone, Copy, PartialEq)]
pub enum Command {
    NewConnection,
    Connections,
    Url,
    ImportProfiles,
    ExportProfiles,
    Settings,
    Disconnect,
    Move,
    Preferences,
    Recovery,
    Format,
    RunForEditing,
    RunCurrent,
    RunSelection,
    RunAll,
    Complete,
    RefreshCompletions,
    Parameters,
    Explain,
    Favorite,
    OpenSql,
    Snippets,
    Split,
    CloseQuery,
    CopyCell,
    CopyRow,
    SelectRow,
    CopyRange,
    Lossless,
    Inspect,
    EditRow,
    DeleteRow,
    Fill,
    Paste,
    Null,
    Related,
    Csv,
    Json,
    Sql,
    StreamCsv,
    StreamJson,
    StreamSql,
    ImportCsv,
    ImportJson,
    ImportArray,
    History,
    SavedQueries,
    Database,
    SqlEditor,
    Tools,
    Objects,
    Dashboard,
    Drafts,
}
impl Menu {
    fn entries(self) -> &'static [(Command, &'static str)] {
        use Command::*;
        match self {
            Self::Connection => &[
                (Connections, "Manage saved connections…"),
                (NewConnection, "New connection…"),
                (Url, "Connect from URL…"),
                (Settings, "Settings / reconnect…"),
                (Disconnect, "Disconnect"),
                (Move, "Move to other window"),
                (ImportProfiles, "Import connections…"),
                (ExportProfiles, "Export connections…"),
                (Preferences, "Preferences & shortcuts…"),
                (Recovery, "Recover staged edits…"),
            ],
            Self::Query => &[
                (RunCurrent, "Run Current · statement at cursor"),
                (RunSelection, "Run Selection · selected SQL only"),
                (RunAll, "Run All · entire SQL buffer"),
                (
                    RunForEditing,
                    "Run for editing · selection or all, single SELECT",
                ),
                (Format, "Format SQL"),
                (Complete, "Complete SQL · Ctrl+Space"),
                (RefreshCompletions, "Refresh SQL suggestions"),
                (Parameters, "Query parameters…"),
                (Explain, "Explain query…"),
                (Favorite, "Save query"),
                (OpenSql, "Open SQL file…"),
                (Snippets, "Snippet templates…"),
                (Split, "Toggle split editors"),
                (CloseQuery, "Close query tab"),
            ],
            Self::Selection => &[
                (CopyCell, "Copy cell"),
                (CopyRow, "Copy row"),
                (SelectRow, "Select whole row · Shift+Space"),
                (CopyRange, "Copy selected range"),
                (Inspect, "Inspect full cell…"),
                (EditRow, "Edit row…"),
                (Fill, "Edit / fill selected cells"),
                (Paste, "Paste selection → staged"),
                (Null, "Set NULL → staged"),
                (DeleteRow, "Delete row → staged"),
                (Related, "Related rows…"),
                (Lossless, "Lossless TSV copy / paste"),
            ],
            Self::Transfer => &[
                (Csv, "Loaded rows → CSV…"),
                (Json, "Loaded rows → JSON…"),
                (Sql, "Loaded rows → SQL…"),
                (StreamCsv, "Stream all → CSV…"),
                (StreamJson, "Stream all → JSON Lines…"),
                (StreamSql, "Stream all → INSERT SQL…"),
                (ImportCsv, "Import CSV…"),
                (ImportJson, "Import JSON Lines…"),
                (ImportArray, "Import JSON array…"),
            ],
            Self::Navigate => &[
                (Database, "Database"),
                (SqlEditor, "SQL editor"),
                (SavedQueries, "Saved queries"),
                (History, "History"),
                (Objects, "Views, routines & objects"),
                (Dashboard, "Metrics dashboards"),
                (Tools, "Database tools"),
                (Drafts, "Unassigned SQL drafts"),
                (Preferences, "Preferences & shortcuts…"),
                (Recovery, "Recover staged edits…"),
            ],
        }
    }
}
impl Workbench {
    pub fn menu_enabled(&self, command: Command, cx: &App) -> bool {
        use Command::*;
        if self.close_requested
            || self.shared.borrow().shell_modal_open
            || self.modal_open()
            || self.library.file_pending
        {
            return false;
        }
        if self.results_read_only()
            && matches!(
                command,
                CopyCell
                    | CopyRow
                    | Inspect
                    | SelectRow
                    | CopyRange
                    | Lossless
                    | EditRow
                    | Fill
                    | Paste
                    | Null
                    | DeleteRow
                    | Related
                    | Csv
                    | Json
                    | Sql
                    | StreamCsv
                    | StreamJson
                    | StreamSql
                    | ImportCsv
                    | ImportJson
                    | ImportArray
            )
        {
            return false;
        }
        let idle = !self.busy;
        let clean = idle && !self.has_row_draft() && self.changes.is_empty();
        let selected = self.selected_cell().is_some();
        let writable = self.grid_writable() && !self.has_row_draft();
        let connected = self.database.is_some();
        let table = self.page == Page::Table;
        match command {
            Connections => !self.has_row_draft(),
            RunForEditing => self.run_for_editing_enabled(),
            RunCurrent | RunAll => self.database.is_some() && !self.busy && self.changes.is_empty(),
            RunSelection => {
                self.database.is_some()
                    && !self.busy
                    && self.changes.is_empty()
                    && !self.tabs[self.active_tab]
                        .editor
                        .read(cx)
                        .selected_range()
                        .is_empty()
            }
            NewConnection | Url | ImportProfiles => idle && self.loaded && !self.has_row_draft(),
            ExportProfiles => idle && self.loaded && !self.state.connections.is_empty(),
            Settings => self.connection_settings_enabled(),
            Disconnect => clean && connected,
            Move => true,
            Preferences | Recovery => idle && !self.has_row_draft(),
            Format => {
                idle && self.page == Page::Query
                    && !self.has_row_draft()
                    && rowboat_services::service_support(
                        self.profile.as_ref().map(|p| p.engine).unwrap_or_default(),
                        rowboat_services::ServiceOperation::FormatSql,
                    )
                    .is_ok()
            }
            Parameters | Favorite | OpenSql | Snippets | Split | CloseQuery => {
                idle && self.page == Page::Query && !self.has_row_draft()
            }
            Complete | RefreshCompletions => {
                idle && self.page == Page::Query && !self.has_row_draft()
            }
            Explain => {
                idle && connected
                    && self.page == Page::Query
                    && !self.has_row_draft()
                    && self.admin_supported(rowboat_admin::providers::Operation::Explain)
            }
            CopyCell | CopyRow | Inspect => selected && !self.has_row_draft(),
            SelectRow => idle && selected && !self.has_row_draft(),
            CopyRange => self.grid_range(cx).is_some() && !self.has_row_draft(),
            Lossless => !self.has_row_draft(),
            EditRow | Fill | Paste | Null => idle && writable && selected,
            DeleteRow => table && idle && writable && selected,
            Related => clean && table && selected,
            Csv | Json => self.current_result().is_some_and(|r| !r.columns.is_empty()),
            Sql => table && self.current_result().is_some_and(|r| !r.columns.is_empty()),
            StreamCsv | StreamJson => clean && connected,
            StreamSql => clean && connected && table,
            ImportCsv | ImportJson | ImportArray => {
                table && clean && writable && !self.transaction && self.transaction_known
            }
            History | SavedQueries | Database | SqlEditor | Tools => !self.has_row_draft(),
            Objects => {
                !self.has_row_draft()
                    && self.admin_supported(rowboat_admin::providers::Operation::Namespaces)
            }
            Dashboard => {
                !self.has_row_draft()
                    && self.admin_supported(rowboat_admin::providers::Operation::ServerMetrics)
            }
            Drafts => !self.has_row_draft() && self.tabs.iter().any(|t| t.connection_id.is_none()),
        }
    }
    pub fn choose_menu(&mut self, command: Command, w: &mut Window, cx: &mut Context<Self>) {
        if !self.menu_enabled(command, cx) {
            return;
        }
        use Command::*;
        match command {
            Connections => {
                let _ = self
                    .shell_tx
                    .send(crate::workspace_shell::Command::Connections {
                        new_connection: false,
                    });
            }
            RunForEditing => self.run_query_for_editing(cx),
            RunCurrent => {
                self.run_query_scope(rowboat_core::execution::RunScope::Current, false, cx)
            }
            RunSelection => {
                self.run_query_scope(rowboat_core::execution::RunScope::Selection, false, cx)
            }
            RunAll => self.run_query_scope(rowboat_core::execution::RunScope::All, false, cx),
            NewConnection => self.show_connection(None, w, cx),
            Url => self.show_connection_url(w, cx),
            ImportProfiles => self.import_profiles(cx),
            ExportProfiles => self.export_profiles(cx),
            Settings => {
                self.connection_settings(w, cx);
            }
            Disconnect => self.disconnect(cx),
            Move => self.request_move(cx),
            Preferences => self.show_preferences(w, cx),
            Recovery => self.show_recovery(w, cx),
            Format => self.format_query(w, cx),
            Complete => self.complete_sql(w, cx),
            RefreshCompletions => self.refresh_completions(w, cx),
            Parameters => {
                self.tabs[self.active_tab].show_parameters = true;
                self.pending_focus = Some(
                    self.tabs[self.active_tab]
                        .parameters
                        .read(cx)
                        .focus_handle(cx),
                );
            }
            Explain => self.explain_query(w, cx),
            Favorite => self.favorite(w, cx),
            OpenSql => self.import_sql(w, cx),
            Snippets => self.show_snippets(w, cx),
            Split => self.toggle_editor_split(w, cx),
            CloseQuery => self.close_query(w, cx),
            CopyCell => self.copy_cell(cx),
            CopyRow => self.copy_row(cx),
            SelectRow => {
                if let Some((r, _)) = self.selected_cell() {
                    self.select_grid_rows(r, false, cx);
                    w.focus(&self.grid_focus, cx);
                }
            }
            CopyRange => self.copy_grid(false, cx),
            Lossless => self.grid.lossless = !self.grid.lossless,
            Inspect => self.inspect_cell(w, cx),
            EditRow => self.edit_row(false, w, cx),
            DeleteRow => self.stage_delete(cx),
            Fill => self.begin_cell_edit(w, cx),
            Paste => self.paste_grid(cx),
            Null => self.clear_grid(cx),
            Related => self.show_relationships(w, cx),
            Csv => self.export("csv", cx),
            Json => self.export("json", cx),
            Sql => self.export("sql", cx),
            StreamCsv => self.show_data_transfer(false, TransferFormat::Csv, cx),
            StreamJson => self.show_data_transfer(false, TransferFormat::JsonLines, cx),
            StreamSql => self.show_data_transfer(false, TransferFormat::Sql, cx),
            ImportCsv => self.import("csv", cx),
            ImportJson => self.import("json", cx),
            ImportArray => self.show_data_transfer(true, TransferFormat::JsonArray, cx),
            History => {
                self.saved_queries_view = false;
                self.page = Page::History;
                w.focus(&self.focus, cx);
            }
            SavedQueries => {
                self.saved_queries_view = true;
                self.page = Page::History;
                w.focus(&self.focus, cx);
            }
            Database => {
                self.database_destination = true;
                self.page = if self.table.is_some() {
                    Page::Table
                } else {
                    Page::Query
                };
                w.focus(&self.focus, cx);
            }
            SqlEditor => {
                self.database_destination = false;
                self.page = Page::Query;
                self.selected = None;
                self.tabs[self.active_tab]
                    .editor
                    .update(cx, |e, cx| e.focus(w, cx));
            }
            Tools => {
                self.page = Page::Tools;
                w.focus(&self.focus, cx);
            }
            Objects => self.show_objects_page(cx),
            Dashboard => self.show_dashboards(cx),
            Drafts => {
                if let Some(i) = self.tabs.iter().position(|t| t.connection_id.is_none()) {
                    self.active_tab = i;
                    self.page = Page::Query;
                    self.selected = None;
                    self.tabs[i].editor.update(cx, |e, cx| e.focus(w, cx));
                }
            }
        }
        cx.notify();
    }
    pub fn action_menu(
        &self,
        id: &'static str,
        title: &'static str,
        menu: Menu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entries = menu.entries();
        let items = entries
            .iter()
            .map(|(command, title)| {
                let item = ui::MenuItem::new(*title).disabled(!self.menu_enabled(*command, cx));
                if *command == Command::Lossless {
                    item.checked(self.grid.lossless)
                } else {
                    item
                }
            })
            .collect();
        let owner = self.tx.route;
        let epoch = self.host_epoch;
        let entity = cx.weak_entity();
        ui::menu(
            id,
            ui::button((id, 0usize), "", ui::ButtonVariant::Secondary, cx)
                .text_size(px(13.))
                .rounded(px(4.))
                .min_h(px(28.))
                .accessibility_label(title.trim_end_matches(" ▾").to_string())
                .when(matches!(menu, Menu::Navigate), |b| {
                    b.child(icon(ui::IconName::Wrench).size(px(14.)))
                })
                .child(title.trim_end_matches(" ▾").to_string())
                .child(icon(ui::IconName::ChevronDown).size(px(12.)))
                .debug_selector(move || id.into()),
            items,
            move |index, w, cx| {
                let Some((command, _)) = entries.get(index) else {
                    return;
                };
                let _ = entity.update(cx, |s, cx| {
                    if s.tx.route == owner && s.host_epoch == epoch {
                        s.choose_menu(*command, w, cx);
                    }
                });
            },
        )
        .into_any_element()
    }
    pub fn compact_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.omarchy();
        let connected = self.database.is_some();
        let target = self
            .profile
            .as_ref()
            .map(crate::profiles_ui::connection_target);
        let name = self
            .profile
            .as_ref()
            .map(|p| format!("{}  /  {}", p.name, p.engine.descriptor().name))
            .unwrap_or_else(|| "No connection".into());
        row()
            .h(px(36.))
            .px_3()
            .flex_shrink_0()
            .bg(t.surface.opacity(1.))
            .border_b_1()
            .border_color(t.divider())
            .child(icon(ui::IconName::Database).text_color(t.secondary))
            .child(
                div()
                    .id("connection-target")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name)
                    .map(|label| {
                        if let Some(target) = target {
                            ui::with_tooltip(label, target.file_path.unwrap_or(target.compact))
                                .into_any_element()
                        } else {
                            label.into_any_element()
                        }
                    }),
            )
            .child(
                row()
                    .gap_1()
                    .child(div().size(px(6.)).rounded_full().bg(if connected {
                        t.success
                    } else {
                        t.secondary
                    }))
                    .child(label(
                        if connected {
                            "Connected"
                        } else {
                            "Disconnected"
                        },
                        cx,
                    )),
            )
            .when(self.profile.as_ref().is_some_and(|p| p.read_only), |d| {
                d.child(label("Read only", cx))
            })
            .when(!self.transaction_known || self.transaction, |d| {
                d.child(
                    div()
                        .text_color(cx.omarchy().warning)
                        .child(if self.transaction_known {
                            "Transaction open"
                        } else {
                            "Transaction unknown"
                        }),
                )
            })
            .child(action(
                "quick-open",
                "Commands",
                false,
                !self.has_row_draft(),
                cx,
                |s, w, cx| s.show_palette(w, cx),
            ))
            .child(self.action_menu(
                "connection-menu",
                "Current connection",
                Menu::Connection,
                cx,
            ))
            .into_any_element()
    }
    pub fn compact_grid_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let table = self.page == Page::Table;
        let free = !self.busy && !self.has_row_draft();
        let mut bar = row()
            .px_3()
            .py_1()
            .min_h(px(36.))
            .bg(cx.omarchy().surface.opacity(1.))
            .flex_wrap()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.omarchy().divider());
        if table {
            bar = bar
                .child(action(
                    "refresh-table",
                    "Refresh",
                    false,
                    free && self.changes.is_empty(),
                    cx,
                    |s, _, cx| s.refresh_table(cx),
                ))
                .child(action(
                    "insert-row",
                    "Add row",
                    false,
                    free && self.grid_writable(),
                    cx,
                    |s, w, cx| s.edit_row(true, w, cx),
                ))
                .child(action(
                    "nested-filters",
                    if self.filter_expression().is_some() {
                        "Filter active"
                    } else {
                        "Filter…"
                    },
                    false,
                    free,
                    cx,
                    |s, w, cx| s.show_filters(w, cx),
                ))
                .child(action(
                    "review-pending",
                    format!("Review ({})", self.changes.len()),
                    !self.changes.is_empty(),
                    free && !self.changes.is_empty(),
                    cx,
                    |s, _, cx| s.review_pending(cx),
                ));
        } else {
            let tab = &self.tabs[self.active_tab];
            bar = bar
                .child(
                    icon(ui::IconName::Table)
                        .size(px(14.))
                        .text_color(cx.omarchy().secondary),
                )
                .child(
                    label(
                        if tab.results.is_empty() {
                            "Results".into()
                        } else {
                            let result = &tab.results[tab.result_index];
                            let identity = if result.columns.is_empty() {
                                format!("{} affected", result.rows_affected)
                            } else {
                                result
                                    .columns
                                    .iter()
                                    .take(2)
                                    .map(|c| c.name.chars().take(20).collect::<String>())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            };
                            format!(
                                "Results {} / {} · {identity}",
                                tab.result_index + 1,
                                tab.results.len()
                            )
                        },
                        cx,
                    )
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(12.)),
                )
                .when(tab.results.len() > 1, |d| {
                    d.child(icon_action(
                        "previous-result",
                        ui::IconName::ChevronLeft,
                        "Previous result",
                        true,
                        cx,
                        |s, _, cx| s.navigate_result(false, cx),
                    ))
                    .child(icon_action(
                        "next-result",
                        ui::IconName::ChevronRight,
                        "Next result",
                        true,
                        cx,
                        |s, _, cx| s.navigate_result(true, cx),
                    ))
                })
                .child(div().w(px(160.)).child(ui::input(
                    "result-filter",
                    &self.result_filter,
                    window,
                    cx,
                )));
        }
        bar.when(!table && !self.changes.is_empty(), |d| {
            d.child(
                action(
                    "review-query-pending",
                    format!("Review ({})", self.changes.len()),
                    true,
                    free,
                    cx,
                    |s, _, cx| s.review_pending(cx),
                )
                .debug_selector(|| "review-query-pending".into()),
            )
        })
        .child(div().flex_1())
        .when_some(self.grid_range(cx), |d, range| {
            d.child(label(
                format!("{} × {} selected", range.rows(), range.columns()),
                cx,
            ))
        })
        .child(self.action_menu("selection-menu", "Selection ▾", Menu::Selection, cx))
        .child(self.action_menu("transfer-menu", "Transfer ▾", Menu::Transfer, cx))
        .into_any_element()
    }
}
