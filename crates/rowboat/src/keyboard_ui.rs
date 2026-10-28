//! More-specific input bindings prevent native indentation/Enter from swallowing commands.
use crate::app::*;
use gpui_kit::*;
pub fn bind(cx: &mut App) {
    bind_assistant(cx);
    cx.bind_keys([
        KeyBinding::new("up", NamespaceUp, Some("RowboatNamespace > Input")),
        KeyBinding::new("down", NamespaceDown, Some("RowboatNamespace > Input")),
        KeyBinding::new("home", NamespaceFirst, Some("RowboatNamespace > Input")),
        KeyBinding::new("end", NamespaceLast, Some("RowboatNamespace > Input")),
        KeyBinding::new("enter", NamespaceChoose, Some("RowboatNamespace > Input")),
        KeyBinding::new("f6", FocusGrid, Some("Rowboat")),
        KeyBinding::new("f6", FocusGrid, Some("Rowboat > Input")),
        KeyBinding::new("ctrl-enter", StageCell, Some("RowboatGridDraft > Input")),
        KeyBinding::new("tab", NextCell, Some("RowboatGridDraft > Input")),
        KeyBinding::new("shift-tab", PreviousCell, Some("RowboatGridDraft > Input")),
        KeyBinding::new("escape", CancelCell, Some("RowboatGridDraft > Input")),
        KeyBinding::new("tab", ModalNext, Some("RowboatModal")),
        KeyBinding::new("tab", ModalNext, Some("RowboatModal > Input")),
        KeyBinding::new("shift-tab", ModalPrevious, Some("RowboatModal")),
        KeyBinding::new("shift-tab", ModalPrevious, Some("RowboatModal > Input")),
        KeyBinding::new("escape", Dismiss, Some("RowboatModal > Input")),
        KeyBinding::new("up", ExplorerUp, Some("RowboatExplorer > Input")),
        KeyBinding::new("down", ExplorerDown, Some("RowboatExplorer > Input")),
        KeyBinding::new("home", ExplorerFirst, Some("RowboatExplorer > Input")),
        KeyBinding::new("end", ExplorerLast, Some("RowboatExplorer > Input")),
        KeyBinding::new("enter", ExplorerOpen, Some("RowboatExplorer > Input")),
    ]);
}
pub fn bind_assistant(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "ctrl-enter",
            SendAssistant,
            Some("RowboatAssistant > Input"),
        ),
        KeyBinding::new("ctrl-enter", SendAssistant, Some("RowboatAssistant")),
    ]);
}
pub fn modal_focus(reverse: bool, handle: &FocusHandle, w: &mut Window, cx: &mut App) {
    for _ in 0..256 {
        if reverse {
            w.focus_prev(cx);
        } else {
            w.focus_next(cx);
        }
        if handle.contains_focused(w, cx) {
            return;
        }
    }
    w.focus(handle, cx);
}
impl Workbench {
    /// Retained results can exist while Review or another page hides their grid.
    pub fn grid_is_rendered(&self) -> bool {
        (self.page == Page::Query && !self.query_edit.review
            || self.page == Page::Table && self.table_page == TablePage::Content)
            && (self.results_read_only()
                || self.current_result().is_some_and(|r| !r.columns.is_empty()))
    }

    /// Repair only regions we know the next render removes; do not steal focus
    /// from an editor just because its result set changed.
    pub fn repair_keyboard_focus(&self, w: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() || self.has_row_draft() || self.shared.borrow().shell_modal_open {
            return;
        }
        let hidden_grid = self.grid_focus.is_focused(w) && !self.grid_is_rendered();
        let hidden_editor = self.tabs.iter().any(|tab| {
            tab.editor.read(cx).focus_handle(cx).is_focused(w)
                && (self.page != Page::Query
                    || self.query_edit.review
                    || !self.panes.tabs.contains(&Some(tab.id)))
        });
        if hidden_grid || hidden_editor {
            w.focus(&self.focus, cx);
        }
    }

    pub fn dismiss_grid_inspector(&mut self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.grid.inspector.take().is_none() {
            return false;
        }
        if self.grid_is_rendered() && !self.close_requested {
            w.focus(&self.grid_focus, cx);
        } else {
            w.focus(&self.focus, cx);
        }
        cx.notify();
        true
    }

    pub fn focus_grid(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.close_requested
            || self.shared.borrow().shell_modal_open
            || self.modal_open()
            || self.has_row_draft()
            || !self.grid_is_rendered()
        {
            return;
        }
        if self.page == Page::Query && self.grid_focus.is_focused(w) {
            self.tabs[self.active_tab]
                .editor
                .update(cx, |e, cx| e.focus(w, cx));
        } else if self.results_read_only() {
            // The snapshot renderer owns its selection and arrow/Page handlers.
            // Never synthesize a selection in the hidden live result.
            self.selected = None;
            w.focus(&self.grid_focus, cx);
        } else {
            let rows = self.visible_grid_rows(cx);
            if !rows.is_empty() {
                let columns = self.current_result().map_or(0, |r| r.columns.len());
                let selected = self.selected.as_ref().filter(|s| {
                    Some(&s.origin) == self.current_origin().as_ref()
                        && s.visible_rows == rows
                        && s.focus.column < columns
                });
                let position = selected.and_then(|s| {
                    rows.iter()
                        .position(|r| *r == s.focus.row)
                        .map(|r| (r, s.focus.column))
                });
                let (r, c) = position.unwrap_or((0, 0));
                if position.is_none() {
                    self.select_grid_cell(rows[r], c, false, cx);
                }
                self.reveal_grid_cell(r, c);
            }
            w.focus(&self.grid_focus, cx);
        }
        cx.notify();
    }
    pub fn grid_key(
        &mut self,
        event: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.results_read_only()
            || !self.grid_focus.is_focused(w)
            || !self.grid_is_rendered()
            || self.modal_open()
            || self.has_row_draft()
            || self.busy
            || self.close_requested
            || self.shared.borrow().shell_modal_open
        {
            return false;
        }
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        if mods.alt || mods.platform || mods.control && !matches!(key, "home" | "end") {
            return false;
        }
        if key == "escape" {
            self.selected = None;
            cx.notify();
            return true;
        }
        let rows = self.visible_grid_rows(cx);
        let columns = self.current_result().map_or(0, |r| r.columns.len());
        if rows.is_empty() || columns == 0 {
            return false;
        }
        let position = self
            .selected
            .as_ref()
            .filter(|s| Some(&s.origin) == self.current_origin().as_ref() && s.visible_rows == rows)
            .map(|s| s.focus);
        let mut r = position
            .and_then(|p| rows.iter().position(|r| *r == p.row))
            .unwrap_or(0);
        let mut c = position.map_or(0, |p| p.column).min(columns - 1);
        if key == "space" && mods.shift {
            self.select_grid_rows(rows[r], false, cx);
            cx.notify();
            return true;
        }
        match key {
            "up" => r = r.saturating_sub(1),
            "down" => r = (r + 1).min(rows.len() - 1),
            "left" => c = c.saturating_sub(1),
            "right" => c = (c + 1).min(columns - 1),
            "home" => {
                c = 0;
                if mods.control {
                    r = 0;
                }
            }
            "end" => {
                c = columns - 1;
                if mods.control {
                    r = rows.len() - 1;
                }
            }
            "pageup" => r = r.saturating_sub(self.grid_page_step()),
            "pagedown" => r = r.saturating_add(self.grid_page_step()).min(rows.len() - 1),
            "enter" => {}
            _ => return false,
        }
        if self.grid.row_selection
            && mods.shift
            && (matches!(key, "up" | "down" | "pageup" | "pagedown")
                || mods.control && matches!(key, "home" | "end"))
        {
            self.select_grid_rows(rows[r], true, cx);
        } else {
            self.select_grid_cell(rows[r], c, mods.shift, cx);
        }
        self.reveal_grid_cell(r, c);
        if key == "enter" {
            if self.grid_writable() {
                self.begin_cell_edit(w, cx);
            } else {
                self.inspect_cell(w, cx);
            }
        }
        cx.notify();
        true
    }

    fn grid_page_step(&self) -> usize {
        let height = f32::from(self.grid_scroll.0.borrow().base_handle.bounds().size.height);
        let row_height = self
            .state
            .preferences
            .grid_row_height
            .max(self.state.preferences.grid_font_size * 1.5 + 2.);
        if height > 0. {
            ((height / row_height).floor() as usize)
                .saturating_sub(1)
                .max(1)
        } else {
            20
        }
    }

    fn reveal_grid_cell(&self, r: usize, c: usize) {
        self.grid_scroll.scroll_to_item(r, ScrollStrategy::Nearest);
        let left = px((52. * self.state.preferences.grid_font_size / 12.).max(52.)
            + self.widths.iter().take(c).sum::<f32>());
        let right = left + px(self.widths.get(c).copied().unwrap_or(150.));
        let mut offset = self.grid_x_scroll.offset();
        let width = self.grid_x_scroll.bounds().size.width;
        if c == 0 {
            offset.x = px(0.);
        } else if left + offset.x < px(0.) || right - left > width {
            offset.x = -left;
        } else if right + offset.x > width {
            offset.x = width - right;
        }
        self.grid_x_scroll.set_offset(offset);
    }
    pub fn explorer_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.modal_open() || self.close_requested {
            return;
        }
        let items = self.explorer_items(cx);
        let kinds = items
            .iter()
            .map(|i| crate::namespace_ui::is_view(i))
            .collect::<Vec<_>>();
        let tables = items
            .into_iter()
            .map(|i| i.table.clone())
            .collect::<Vec<_>>();
        if tables.is_empty() {
            return;
        }
        self.explorer_cursor = self.explorer_cursor.min(tables.len() - 1);
        match key {
            "up" => self.explorer_cursor = self.explorer_cursor.saturating_sub(1),
            "down" => self.explorer_cursor = (self.explorer_cursor + 1).min(tables.len() - 1),
            "home" => self.explorer_cursor = 0,
            "end" => self.explorer_cursor = tables.len() - 1,
            _ => {
                self.open_table(tables[self.explorer_cursor].clone(), cx);
            }
        }
        self.explorer_page = self.explorer_cursor / 200;
        let start = self.explorer_page * 200;
        let headers = if kinds[self.explorer_cursor] { 2 } else { 1 };
        self.explorer_scroll
            .scroll_to_item(1 + self.explorer_cursor - start + headers);
        cx.notify();
    }
}
