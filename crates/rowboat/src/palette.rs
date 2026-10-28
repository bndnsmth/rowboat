//! One search/dispatch model for keyboard and mouse command selection.
use crate::{
    app::{Page, Workbench},
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::TableRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Run,
    RunForEditing,
    NewQuery,
    Format,
    Complete,
    Split,
    OpenSql,
    CloseTab,
    Favorite,
    SaveQueryAs,
    SavedQueries,
    SaveSqlFile,
    SaveSqlFileAs,
    NewConnection,
    Connections,
    Reconnect,
    Disconnect,
    RefreshCatalog,
    Cancel,
    QueryPage,
    History,
    Tools,
    ReviewRows,
    Recovery,
    Snippets,
    ExpandTrigger,
    MoveWorkspace,
    Explorer,
    FocusGrid,
    AccessMatrix,
    CopyRange,
    Preferences,
    Objects,
    Dashboard,
    Explain,
    Ai,
}
impl Command {
    fn all() -> &'static [(Self, &'static str)] {
        &[
            (Self::Run, "Run query in active SQL pane"),
            (
                Self::RunForEditing,
                "Run for editing · run current SELECT / selection again for a fresh snapshot",
            ),
            (Self::NewQuery, "New SQL tab"),
            (Self::Format, "Format active SQL editor"),
            (Self::Complete, "Complete SQL from live catalog"),
            (Self::Split, "Toggle split SQL editors"),
            (Self::OpenSql, "Open SQL file…"),
            (Self::CloseTab, "Close SQL tab…"),
            (Self::Favorite, "Save active SQL query"),
            (Self::SaveQueryAs, "Save SQL query as a named copy…"),
            (Self::SavedQueries, "Show saved SQL query library"),
            (Self::SaveSqlFile, "Save file-backed SQL document"),
            (Self::SaveSqlFileAs, "Save SQL file as a new path…"),
            (Self::NewConnection, "New saved connection…"),
            (Self::Connections, "Manage saved connections…"),
            (Self::Reconnect, "Connection settings / reconnect…"),
            (Self::Disconnect, "Disconnect this workspace"),
            (Self::RefreshCatalog, "Refresh database catalog"),
            (Self::Cancel, "Cancel running query"),
            (Self::QueryPage, "Show SQL editor"),
            (Self::History, "Show query history"),
            (Self::Tools, "Show database tools"),
            (Self::ReviewRows, "Review staged row changes"),
            (Self::Recovery, "Detached staged-edit recovery…"),
            (Self::Snippets, "Reusable snippet templates…"),
            (
                Self::ExpandTrigger,
                "Expand exact snippet trigger at SQL caret · Ctrl+Shift+Space",
            ),
            (
                Self::MoveWorkspace,
                "Move this live workspace between main / satellite window",
            ),
            (Self::Explorer, "Focus table explorer · arrows and Enter"),
            (Self::FocusGrid, "Focus results / data grid · F6"),
            (
                Self::AccessMatrix,
                "Effective permissions matrix / reviewed grants…",
            ),
            (Self::CopyRange, "Copy selected grid range"),
            (Self::Preferences, "Application preferences / shortcuts…"),
            (Self::Objects, "Show grouped object explorer"),
            (Self::Dashboard, "Show saved metrics dashboards"),
            (Self::Explain, "Explain query / AI performance review…"),
            (Self::Ai, "Open AI query assistant…"),
        ]
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Command(Command),
    Table(TableRef),
    Favorite(String),
    Connection(String),
    WindowConnection(String),
    Snippet(String),
}
#[derive(Clone)]
pub struct Item {
    pub target: Target,
    pub title: String,
    pub detail: String,
    pub enabled: bool,
}
#[derive(Default)]
pub struct Cursor {
    pub index: usize,
}
impl Cursor {
    pub fn step(&mut self, items: &[Item], forward: bool) {
        if items.is_empty() {
            self.index = 0;
            return;
        }
        let len = items.len();
        self.index = self.index.min(len - 1);
        for _ in 0..len {
            self.index = if forward {
                (self.index + 1) % len
            } else {
                (self.index + len - 1) % len
            };
            if items[self.index].enabled {
                return;
            }
        }
    }
}
#[derive(Default)]
pub struct PaletteState {
    pub cursor: Cursor,
    pub route: Option<Route>,
    pub scroll: ScrollHandle,
    return_focus: Option<ReturnFocus>,
}
struct ReturnFocus {
    handle: FocusHandle,
    route: Route,
    host_epoch: u64,
    page: Page,
    table_page: crate::app::TablePage,
    tab: usize,
}
fn search(items: Vec<Item>, filter: &str) -> Vec<Item> {
    let filter = filter.trim().to_lowercase();
    let words = filter.split_whitespace().collect::<Vec<_>>();
    let mut found = items
        .into_iter()
        .filter_map(|item| {
            let title = item.title.to_lowercase();
            let text = format!("{title} {}", item.detail.to_lowercase());
            if words.iter().all(|word| text.contains(word)) {
                let rank = if title == filter {
                    0
                } else if title.starts_with(&filter) {
                    1
                } else {
                    2
                };
                Some((rank, item))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().take(80).map(|(_, item)| item).collect()
}
impl Workbench {
    fn command_enabled(&self, command: Command, cx: &App) -> bool {
        if self.library.file_pending {
            return false;
        }
        let idle = !self.busy && !self.close_requested && !self.has_row_draft();
        let clean = idle && self.changes.is_empty();
        let connected = self.database.is_some();
        match command {
            Command::RunForEditing => self.run_for_editing_enabled(),
            Command::Run => clean && connected && self.page == Page::Query,
            Command::Cancel => self.busy && connected && !self.transfer_running,
            Command::Reconnect => self.connection_settings_enabled(),
            Command::Disconnect => {
                clean
                    && self.transaction_known
                    && !self.transaction
                    && (connected || self.owner.is_some())
            }
            Command::RefreshCatalog => clean && connected,
            Command::CopyRange => self.grid_range(cx).is_some(),
            Command::ReviewRows => idle && !self.changes.is_empty(),
            Command::Recovery => idle,
            Command::Snippets => idle && self.page == Page::Query,
            Command::ExpandTrigger => idle && self.page == Page::Query,
            Command::MoveWorkspace => !self.close_requested,
            Command::Explorer => !self.close_requested,
            Command::FocusGrid => {
                !self.close_requested && !self.has_row_draft() && self.grid_is_rendered()
            }
            Command::AccessMatrix => clean && connected,
            Command::Favorite
            | Command::SaveQueryAs
            | Command::SaveSqlFile
            | Command::SaveSqlFileAs => idle && self.loaded,
            Command::CloseTab | Command::Format | Command::Complete | Command::Split => {
                idle && self.page == Page::Query
            }
            Command::NewQuery
            | Command::OpenSql
            | Command::QueryPage
            | Command::History
            | Command::SavedQueries
            | Command::Tools => clean,
            Command::NewConnection => self.loaded && !self.close_requested,
            Command::Connections => !self.close_requested && !self.has_row_draft(),
            Command::Preferences => !self.close_requested,
            Command::Objects => clean && connected,
            Command::Dashboard => clean && connected,
            Command::Explain => {
                clean
                    && connected
                    && self.page == Page::Query
                    && self.admin_supported(rowboat_admin::providers::Operation::Explain)
            }
            Command::Ai => self.ai_entry_enabled(),
        }
    }
    pub fn palette_items(&self, cx: &App) -> Vec<Item> {
        let navigation = !self.busy
            && !self.has_row_draft()
            && self.changes.is_empty()
            && !self.close_requested
            && !self.library.file_pending;
        let mut items = Command::all()
            .iter()
            .map(|(command, title)| Item {
                target: Target::Command(*command),
                title: (*title).into(),
                detail: "Command".into(),
                enabled: self.command_enabled(*command, cx),
            })
            .collect::<Vec<_>>();
        items.extend(self.catalog.iter().map(|t| Item {
            target: Target::Table(t.table.clone()),
            title: format!("{}.{}", t.table.schema, t.table.name),
            detail: format!("{} · current workspace", t.kind),
            enabled: navigation,
        }));
        let shared = self.shared.borrow();
        items.extend(shared.state.snippets.iter().map(|s| Item {
            target: Target::Favorite(s.id.clone()),
            title: s.name.clone(),
            detail: format!(
                    "Saved query · {} · {} · opens for review",
                    shared
                        .state
                        .query_metadata
                        .get(&s.id)
                        .map(|m| m.folder.as_str())
                        .unwrap_or(""),
                    s.sql
                ),
            enabled: navigation,
        }));
        items.extend(self.state.connections.iter().map(|p| Item {
            target: Target::Connection(p.id.clone()),
            title: p.name.clone(),
            detail: format!(
                "Connection · {:?} · {}",
                p.engine,
                crate::profiles_ui::connection_target(p).compact
            ),
            enabled: !self.close_requested,
        }));
        items.extend(self.state.connections.iter().map(|p| Item {
            target: Target::WindowConnection(p.id.clone()),
            title: format!("Open window: {}", p.name),
            detail: "Existing workspace is focused instead of duplicating its session".into(),
            enabled: self.loaded && !self.close_requested,
        }));
        items.extend(self.state.snippet_templates.iter().map(|t| Item {
            target: Target::Snippet(t.id.clone()),
            title: format!("Snippet: {} · {}", t.name, t.trigger),
            detail: "Expand literal template at the active editor selection".into(),
            enabled: !self.busy && !self.close_requested && self.page == Page::Query,
        }));
        search(items, &self.quick_filter.read(cx).value())
    }
    pub fn show_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.library.file_pending {
            return;
        }
        if self.has_extension_dialog() {
            return;
        }
        if self.url_input.is_some()
            || self.management.is_some()
            || self.has_row_draft()
            || self.form.is_some()
            || self.close_requested
            || self.transfer.is_some()
            || self.grid.inspector.is_some()
            || self.advanced.designer.is_some()
        {
            return;
        }
        if !self.quick_open {
            self.palette.return_focus = window.focused(cx).map(|handle| ReturnFocus {
                handle,
                route: self.tx.route,
                host_epoch: self.host_epoch,
                page: self.page,
                table_page: self.table_page,
                tab: self.tabs[self.active_tab].id,
            });
        }
        self.tabs[self.active_tab].show_parameters = false;
        self.dismiss_completions(cx);
        self.quick_open = true;
        self.palette.route = Some(self.tx.route);
        self.palette.cursor.index = 0;
        self.quick_filter.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        self.palette.cursor.index = self
            .palette_items(cx)
            .iter()
            .position(|item| item.enabled)
            .unwrap_or(0);
        cx.notify();
    }
    /// Cancellation restores the initiator. Executing a command deliberately
    /// does not use this path: the command owns its destination focus.
    pub fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.quick_open {
            return false;
        }
        self.quick_open = false;
        let target = self.palette.return_focus.take().filter(|saved| {
            saved.route == self.tx.route
                && saved.host_epoch == self.host_epoch
                && saved.page == self.page
                && saved.table_page == self.table_page
                && saved.tab == self.tabs[self.active_tab].id
                && self.focus.contains(&saved.handle, window)
                && !self.modal_focus.contains(&saved.handle, window)
                && (saved.handle != self.grid_focus || self.grid_is_rendered())
        });
        window.focus(
            target.as_ref().map_or(&self.focus, |saved| &saved.handle),
            cx,
        );
        self.repair_keyboard_focus(window, cx);
        cx.notify();
        true
    }
    pub fn accept_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_open {
            return;
        }
        if let Some(item) = self
            .palette_items(cx)
            .get(self.palette.cursor.index)
            .cloned()
        {
            self.dispatch_palette(item.target, window, cx);
        }
    }
    fn dispatch_palette(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if !self.quick_open || self.palette.route != Some(self.tx.route) {
            self.quick_open = false;
            self.palette.return_focus = None;
            window.focus(&self.focus, cx);
            self.status = "Workspace session changed; reopen the command palette.".into();
            cx.notify();
            return;
        }
        if !self
            .palette_items(cx)
            .iter()
            .any(|item| item.target == target && item.enabled)
        {
            self.status =
                "Command unavailable with the current connection, operation or pending edits."
                    .into();
            cx.notify();
            return;
        }
        self.quick_open = false;
        self.palette.return_focus = None;
        window.focus(&self.focus, cx);
        match target {
            Target::Snippet(id) => self.insert_snippet(&id, window, cx),
            Target::WindowConnection(id) => {
                if let Some(profile) = self.state.connections.iter().find(|p| p.id == id).cloned() {
                    let _ = self
                        .shell_tx
                        .send(crate::workspace_shell::Command::OpenWindow {
                            profile: Box::new(profile),
                        });
                }
            }
            Target::Table(table) => self.open_table(table, cx),
            Target::Favorite(id) => self.open_library_query(&id, window, cx),
            Target::Connection(id) => {
                if let Some(profile) = self.state.connections.iter().find(|p| p.id == id).cloned() {
                    self.show_connection(Some(profile), window, cx);
                }
            }
            Target::Command(command) => match command {
                Command::Run => self.run_query(cx),
                Command::RunForEditing => self.run_query_for_editing(cx),
                Command::NewQuery => self.new_query(None, window, cx),
                Command::Format => self.format_query(window, cx),
                Command::Complete => self.complete_sql(window, cx),
                Command::Split => self.toggle_editor_split(window, cx),
                Command::OpenSql => self.import_sql(window, cx),
                Command::CloseTab => self.close_query(window, cx),
                Command::Favorite => self.favorite(window, cx),
                Command::SaveQueryAs => self.save_library_query_as(window, cx),
                Command::SaveSqlFile => self.save_library_file(false, cx),
                Command::SaveSqlFileAs => self.save_library_file(true, cx),
                Command::SavedQueries => {
                    self.choose_menu(crate::chrome_ui::Command::SavedQueries, window, cx)
                }
                Command::NewConnection => self.show_connection(None, window, cx),
                Command::Connections => {
                    self.choose_menu(crate::chrome_ui::Command::Connections, window, cx)
                }
                Command::Reconnect => self.connection_settings(window, cx),
                Command::Disconnect => self.disconnect(cx),
                Command::RefreshCatalog => self.refresh_catalog(cx),
                Command::Cancel => self.cancel(cx),
                Command::QueryPage => {
                    self.choose_menu(crate::chrome_ui::Command::SqlEditor, window, cx)
                }
                Command::History => {
                    self.choose_menu(crate::chrome_ui::Command::History, window, cx)
                }
                Command::Tools => self.page = Page::Tools,
                Command::ReviewRows => self.review_pending(cx),
                Command::Recovery => self.show_recovery(window, cx),
                Command::Snippets => self.show_snippets(window, cx),
                Command::ExpandTrigger => self.expand_snippet_trigger(window, cx),
                Command::MoveWorkspace => self.request_move(cx),
                Command::FocusGrid => self.focus_grid(window, cx),
                Command::Explorer => {
                    self.explorer_filter.update(cx, |e, cx| e.focus(window, cx));
                }
                Command::AccessMatrix => self.show_access_matrix(window, cx),
                Command::CopyRange => self.copy_grid(false, cx),
                Command::Preferences => self.show_preferences(window, cx),
                Command::Objects => self.show_objects_page(cx),
                Command::Dashboard => self.show_dashboards(cx),
                Command::Explain => self.explain_query(window, cx),
                Command::Ai => self.open_ai_assistant(window, cx),
            },
        }
        cx.notify();
    }
    pub fn render_palette(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let entries = self.palette_items(cx);
        let mut items = column().gap_1();
        for (index, item) in entries.iter().enumerate() {
            let target = item.target.clone();
            items = items.child(
                action(
                    ("palette-item", index),
                    item.title.clone(),
                    false,
                    item.enabled,
                    cx,
                    move |s, w, cx| s.dispatch_palette(target.clone(), w, cx),
                )
                .w_full()
                .justify_start()
                .selected(index == self.palette.cursor.index)
                .child(div().flex_1())
                .child(label(
                    if item.enabled {
                        item.detail
                            .replace(['\r', '\n'], " ")
                            .chars()
                            .take(160)
                            .collect::<String>()
                    } else {
                        format!(
                            "{} · unavailable",
                            item.detail
                                .replace(['\r', '\n'], " ")
                                .chars()
                                .take(160)
                                .collect::<String>()
                        )
                    },
                    cx,
                )),
            );
        }
        if entries.is_empty() {
            items = items.child(label("No matching commands or database objects", cx));
        }
        self.modal(column().id("command-palette").key_context("RowboatPalette").w(px(780.)).max_h(px(650.)).p_4().gap_3()
            .capture_key_down(cx.listener(|s,e:&KeyDownEvent,_,cx|{
                if !e.keystroke.modifiers.control&&!e.keystroke.modifiers.alt && matches!(e.keystroke.key.as_str(),"up"|"down"){
                    cx.stop_propagation();let items=s.palette_items(cx);s.palette.cursor.step(&items,e.keystroke.key=="down");s.palette.scroll.scroll_to_item(s.palette.cursor.index);cx.notify();
                }
            }))
            .child(row().child(label("COMMAND PALETTE",cx)).child(div().flex_1()).child(action("close-palette","Esc",false,true,cx,|s,w,cx|{s.dismiss_palette(w,cx);}).debug_selector(||"close-palette".into())))
            .child(ui::input("quick-filter",&self.quick_filter,window,cx))
            .child(items.id("palette-results").min_h_0().overflow_y_scroll().track_scroll(&self.palette.scroll))
            .child(label("↑ / ↓ select · Enter run · Esc close · Commands, tables, favorites, connections · first 80 matches",cx).border_t_1().border_color(theme.divider()).pt_2())
            .into_any_element(),cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Cursor, Item, Target, search};
    fn item(title: &str, enabled: bool) -> Item {
        Item {
            target: Target::Command(Command::Run),
            title: title.into(),
            detail: "Command query".into(),
            enabled,
        }
    }
    #[test]
    fn keyboard_navigation_wraps_and_skips_disabled_actions() {
        let items = vec![item("a", true), item("b", false), item("c", true)];
        let mut cursor = Cursor::default();
        cursor.step(&items, true);
        assert_eq!(cursor.index, 2);
        cursor.step(&items, true);
        assert_eq!(cursor.index, 0);
        cursor.step(&items, false);
        assert_eq!(cursor.index, 2);
        cursor.step(&[], true);
        assert_eq!(cursor.index, 0);
    }
    #[test]
    fn palette_search_ranks_exact_then_prefix_and_matches_all_words_without_reordering_equal_scores()
     {
        let result = search(
            vec![
                item("Show query history", true),
                item("Query", true),
                item("Query tools", false),
                item("Other", true),
            ],
            "query",
        );
        assert_eq!(
            result.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["Query", "Query tools", "Show query history", "Other"]
        );
        assert!(!result[1].enabled);
        assert_eq!(
            search(
                vec![item("Show query history", true), item("Query tools", true)],
                "HISTORY query"
            )
            .len(),
            1
        );
    }
}
