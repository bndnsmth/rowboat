//! Display-order selection and review-only grid mutations, owned by one workspace.
use crate::{
    app::{Page, Workbench},
    forms::{self, RowTarget},
    staging::GridSelection,
    view::{action, column, label, row},
};
use gpui_kit::{base::input::TextareaState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::{
    Cell, QueryResult, RowChange,
    grid::{self, ClipboardDialect, GridLimits, GridPosition, GridRange},
};

#[path = "record_inspector.rs"]
pub mod record_inspector;

#[derive(Default)]
pub struct GridState {
    pub row_selection: bool,
    pub lossless: bool,
    pub draft: Option<CellDraft>,
    pub inspector: Option<CellInspector>,
    pub record: Option<record_inspector::RecordInspector>,
}
pub struct CellDraft {
    pub target: RowTarget,
    pub range: GridRange,
    pub snapshot: QueryResult,
    pub value: Entity<TextareaState>,
    pub kind: ValueKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Binary,
    Null,
}
impl ValueKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Binary => "Binary hex",
            Self::Null => "SQL NULL",
        }
    }
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Text => Self::Binary,
            Self::Binary => Self::Null,
            Self::Null => Self::Text,
        }
    }
    pub(crate) fn parse(self, value: &str) -> Result<Cell, String> {
        match self {
            Self::Text => Ok(Cell::Text(value.into())),
            Self::Binary => forms::parse_binary(value).map_err(|e| e.to_string()),
            Self::Null => Ok(Cell::Null),
        }
    }
}
pub struct CellInspector {
    title: String,
    text: Entity<TextareaState>,
    raw: Cell,
    preview: String,
    formatted: Option<String>,
    showing_formatted: bool,
}

impl Workbench {
    /// The displayed result may be reordered; its captured QueryEditPlan is immutable.
    /// Use ordinals here because unrelated query columns can share a display name.
    pub fn sort_grid_column(&mut self, column: usize, cx: &mut Context<Self>) {
        if self.results_read_only() || self.busy || self.close_requested || self.has_row_draft() {
            return;
        }
        if self.page == Page::Table {
            if let Some(name) = self
                .current_result()
                .and_then(|r| r.columns.get(column))
                .map(|c| c.name.clone())
            {
                self.sort_column(name, cx);
            }
            return;
        }
        if self.page != Page::Query {
            return;
        }
        let descending = self
            .query_sort
            .is_some_and(|(sorted, descending)| sorted == column && !descending);
        let tab = &mut self.tabs[self.active_tab];
        let Some(result) = tab.results.get_mut(tab.result_index) else {
            return;
        };
        let Some(metadata) = result.columns.get(column) else {
            return;
        };
        result.rows.sort_by(|left, right| {
            let order = rowboat_core::sorting::compare_cells(
                &metadata.data_type,
                left.get(column).unwrap_or(&Cell::Null),
                right.get(column).unwrap_or(&Cell::Null),
            );
            if descending { order.reverse() } else { order }
        });
        self.status = format!(
            "Sorted displayed result by {} (typed numeric / local byte order)",
            metadata.name
        );
        self.query_sort = Some((column, descending));
        self.selected = None;
        cx.notify();
    }
    pub fn grid_sort_arrow(&self, column: usize, name: &str) -> &'static str {
        let descending = if self.page == Page::Table {
            self.browse_request
                .sort
                .as_ref()
                .filter(|(sorted, _)| sorted == name)
                .map(|(_, ascending)| !ascending)
        } else if self.page == Page::Query {
            self.query_sort
                .filter(|(sorted, _)| *sorted == column)
                .map(|(_, descending)| descending)
        } else {
            None
        };
        descending.map_or("", |descending| if descending { " ↓" } else { " ↑" })
    }
    pub fn has_row_draft(&self) -> bool {
        self.row_editor.is_some() || self.grid.draft.is_some()
    }
    pub fn visible_grid_rows(&self, cx: &App) -> Vec<usize> {
        let filter = if self.page == Page::Query {
            self.result_filter.read(cx).value().to_lowercase()
        } else {
            String::new()
        };
        self.current_result()
            .map(|r| {
                r.rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| {
                        filter.is_empty()
                            || row
                                .iter()
                                .any(|c| c.display().to_lowercase().contains(&filter))
                    })
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn select_grid_cell(&mut self, row: usize, column: usize, extend: bool, cx: &App) {
        if self.results_read_only() || self.grid.draft.is_some() {
            return;
        }
        let extend = extend && !self.grid.row_selection;
        self.grid.row_selection = false;
        let Some(origin) = self.current_origin() else {
            return;
        };
        let visible_rows = self.visible_grid_rows(cx);
        if extend
            && let Some(selection) = self.selected.as_mut()
            && selection.origin == origin
            && selection.visible_rows == visible_rows
        {
            selection.focus = GridPosition { row, column };
            return;
        }
        self.selected = Some(GridSelection {
            origin,
            row,
            column,
            focus: GridPosition { row, column },
            visible_rows,
        });
    }
    pub fn select_grid_rows(&mut self, row: usize, extend: bool, cx: &App) {
        if self.results_read_only()
            || self.has_row_draft()
            || self.busy
            || self.close_requested
            || self.modal_open()
            || self.shared.borrow().shell_modal_open
        {
            return;
        }
        let Some(origin) = self.current_origin() else {
            return;
        };
        let visible_rows = self.visible_grid_rows(cx);
        let columns = self.current_result().map_or(0, |r| r.columns.len());
        if columns == 0 || !visible_rows.contains(&row) {
            return;
        }
        let anchor = self
            .selected
            .as_ref()
            .filter(|s| {
                extend
                    && self.grid.row_selection
                    && s.origin == origin
                    && s.visible_rows == visible_rows
            })
            .map_or(row, |s| s.row);
        self.selected = Some(GridSelection {
            origin,
            row: anchor,
            column: 0,
            focus: GridPosition {
                row,
                column: columns - 1,
            },
            visible_rows,
        });
        self.grid.row_selection = true;
    }
    pub fn grid_range(&self, cx: &App) -> Option<GridRange> {
        if self.results_read_only() {
            return None;
        }
        self.selected
            .as_ref()?
            .range(&self.current_origin()?, &self.visible_grid_rows(cx))
    }
    fn clipboard_dialect(&self) -> ClipboardDialect {
        if self.grid.lossless {
            ClipboardDialect::Rowboat
        } else {
            ClipboardDialect::Spreadsheet
        }
    }
    pub fn copy_grid(&mut self, whole_row: bool, cx: &mut Context<Self>) {
        let Some(mut range) = self.grid_range(cx) else {
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        if whole_row {
            let Ok(r) = GridRange::from_size(
                GridPosition {
                    row: range.start().row,
                    column: 0,
                },
                range.rows(),
                result.columns.len(),
            ) else {
                return;
            };
            range = r;
        }
        let limits = GridLimits::default();
        let copied = self
            .copy_projection(range, limits, cx)
            .and_then(|projected| {
                let local = GridRange::from_size(
                    GridPosition { row: 0, column: 0 },
                    range.rows(),
                    range.columns(),
                )?;
                grid::export_tsv(&projected, local, self.clipboard_dialect(), limits)
            });
        match copied {
            Ok(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.status = format!(
                    "Copied {} × {} · {} TSV",
                    range.rows(),
                    range.columns(),
                    if self.grid.lossless {
                        "Rowboat lossless"
                    } else {
                        "plain / display"
                    }
                );
            }
            Err(e) => self.fail(e.to_string()),
        }
        cx.notify();
    }
    fn copy_projection(
        &self,
        range: GridRange,
        limits: GridLimits,
        cx: &App,
    ) -> Result<QueryResult, grid::GridError> {
        use grid::GridError;
        let cells = range
            .rows()
            .checked_mul(range.columns())
            .ok_or(GridError::Overflow)?;
        if range.rows() > limits.max_rows
            || range.columns() > limits.max_columns
            || cells > limits.max_cells
        {
            return Err(GridError::Limit);
        }
        let result = self.current_result().ok_or(GridError::Empty)?;
        let visible = self.visible_grid_rows(cx);
        let selected = visible
            .get(range.start().row..=range.end().row)
            .ok_or(GridError::Bounds)?;
        let columns = result
            .columns
            .get(range.start().column..=range.end().column)
            .ok_or(GridError::Bounds)?;
        let cell = |row: usize, column: usize| {
            let original = result.rows.get(row)?.get(column)?;
            Some(if self.page == Page::Table {
                effective_cell(result, &self.changes, row, column)
            } else {
                self.query_effective_cell(row, column).unwrap_or(original)
            })
        };
        // Preflight borrowed payloads before allocating any copied cells. Encoded
        // TSV can be larger (quoting/hex); the core exporter enforces that final cap.
        let mut bytes = 0usize;
        for &row in selected {
            for column in range.start().column..=range.end().column {
                let size = match cell(row, column).ok_or(GridError::Ragged)? {
                    Cell::Null => 0,
                    Cell::Text(value) => value.len(),
                    Cell::Binary(value) => value.len(),
                };
                bytes = bytes.checked_add(size).ok_or(GridError::Overflow)?;
                if bytes > limits.max_bytes {
                    return Err(GridError::Limit);
                }
            }
        }
        Ok(QueryResult {
            columns: columns.to_vec(),
            rows: selected
                .iter()
                .map(|&row| {
                    (range.start().column..=range.end().column)
                        .map(|column| cell(row, column).expect("preflight validated cell").clone())
                        .collect()
                })
                .collect(),
            ..Default::default()
        })
    }
    pub fn grid_writable(&self) -> bool {
        if self.results_read_only() {
            return false;
        }
        if self.page == Page::Query {
            return self.database.is_some()
                && !self.busy
                && !self.close_requested
                && !self.transaction
                && self.transaction_known
                && self.profile.as_ref().is_some_and(|p| !p.read_only)
                && self.query_plan().is_some()
                && self.query_batch_matches();
        }
        self.page == Page::Table
            && self.query_edit.batch.is_none()
            && self.database.is_some()
            && !self.busy
            && !self.close_requested
            && self.profile.as_ref().is_some_and(|p| !p.read_only)
            && self.table.as_ref().is_some_and(|table| {
                self.catalog.iter().any(|t| {
                    &t.table == table
                        && matches!(
                            t.kind.as_str(),
                            "TABLE" | "BASE TABLE" | "PARTITIONED TABLE"
                        )
                })
            })
    }
    fn grid_target(&self) -> Result<RowTarget, String> {
        if !self.grid_writable() {
            return Err("Editing requires an idle, writable base-table session.".into());
        }
        if self.row_editor.is_some() {
            return Err("Stage or cancel the row editor first.".into());
        }
        let target = self.editing_target().ok_or("No verified edit source")?;
        if self.change_target.as_ref().is_some_and(|t| t != &target) {
            return Err("Staged changes belong to another table/session.".into());
        }
        Ok(target)
    }
    fn accept_grid_plan(
        &mut self,
        target: RowTarget,
        plan: Result<Vec<RowChange>, grid::GridError>,
        cx: &mut Context<Self>,
    ) {
        match plan {
            Ok(changes) => {
                if let Err(error) = self.validate_query_changes(&changes) {
                    self.fail(error);
                    cx.notify();
                    return;
                }
                self.changes = changes;
                self.bind_query_changes();
                self.change_target = (!self.changes.is_empty()).then_some(target);
                self.grid.draft = None;
                self.status = format!("{} row changes staged · Review → Apply", self.changes.len());
            }
            Err(e) => self.fail(e.to_string()),
        }
        cx.notify();
    }
    pub fn paste_grid(&mut self, cx: &mut Context<Self>) {
        let outcome = (|| {
            let target = self.grid_target()?;
            if self.grid.draft.is_some() {
                return Err("Stage or cancel the cell draft first.".into());
            }
            let range = self
                .grid_range(cx)
                .ok_or("Select a current table cell first")?;
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .ok_or("Clipboard contains no text")?;
            let data = grid::parse_tsv(&text, self.clipboard_dialect(), GridLimits::default())
                .map_err(|e| e.to_string())?;
            let plan = grid::stage_paste(
                &self.editing_result(cx),
                &self.editing_structure(),
                range.start(),
                &data,
                &self.changes,
                GridLimits::default(),
            );
            Ok((target, plan))
        })();
        match outcome {
            Ok((target, plan)) => self.accept_grid_plan(target, plan, cx),
            Err(e) => {
                self.fail(e);
                cx.notify();
            }
        }
    }
    pub fn clear_grid(&mut self, cx: &mut Context<Self>) {
        let outcome = (|| {
            let target = self.grid_target()?;
            if self.grid.draft.is_some() {
                return Err("Stage or cancel the cell draft first.".into());
            }
            let range = self
                .grid_range(cx)
                .ok_or("Select a current table range first")?;
            Ok((
                target,
                grid::stage_clear(
                    &self.editing_result(cx),
                    &self.editing_structure(),
                    range,
                    &self.changes,
                    GridLimits::default(),
                ),
            ))
        })();
        match outcome {
            Ok((target, plan)) => self.accept_grid_plan(target, plan, cx),
            Err(e) => {
                self.fail(e);
                cx.notify();
            }
        }
    }
    pub fn begin_cell_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.draft.is_some() {
            return;
        }
        let target = match self.grid_target() {
            Ok(t) => t,
            Err(e) => {
                self.fail(e);
                cx.notify();
                return;
            }
        };
        let Some(range) = self.grid_range(cx) else {
            return;
        };
        let result = self.editing_result(cx);
        if range.validate(&result).is_err() {
            return;
        }
        if self.page == Page::Query
            && self.query_plan().is_some_and(|p| {
                p.columns().iter().enumerate().any(|(i, c)| {
                    i >= range.start().column && i <= range.end().column && !c.editable
                })
            })
        {
            self.fail("Selected range includes a generated/noneditable source column".into());
            cx.notify();
            return;
        }
        let position = range.start();
        let cell = effective_cell(&result, &self.changes, position.row, position.column).clone();
        let kind = match cell {
            Cell::Null => ValueKind::Null,
            Cell::Binary(_) => ValueKind::Binary,
            _ => ValueKind::Text,
        };
        let text = if matches!(cell, Cell::Null) {
            String::new()
        } else {
            cell.display()
        };
        let value = forms::textarea(text, window, cx);
        value.update(cx, |v, cx| v.focus(window, cx));
        self.query_edit.draft = if self.page == Page::Query {
            self.query_binding()
        } else {
            None
        };
        self.grid.draft = Some(CellDraft {
            target,
            range,
            snapshot: result,
            value,
            kind,
        });
        cx.notify();
    }
    pub fn cell_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.cell_edit_command(
            &event.keystroke.key,
            event.keystroke.modifiers.control,
            event.keystroke.modifiers.shift,
            window,
            cx,
        )
    }
    pub fn cell_edit_command(
        &mut self,
        key: &str,
        control: bool,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.results_read_only() {
            return false;
        }
        let Some(draft) = self.grid.draft.as_ref() else {
            return false;
        };
        if self.close_requested
            || self.busy
            || self.modal_open()
            || !draft.value.read(cx).focus_handle(cx).is_focused(window)
        {
            return false;
        }
        if key == "escape" {
            self.grid.draft = None;
            window.focus(&self.grid_focus, cx);
            cx.notify();
            return true;
        }
        if key != "tab" && !(key == "enter" && control) {
            return false;
        }
        let position = draft.range.start();
        self.stage_cell_draft(cx);
        if self.grid.draft.is_some() {
            return true;
        }
        window.focus(&self.grid_focus, cx);
        if key == "tab" {
            let columns = self.current_result().map_or(0, |r| r.columns.len());
            let rows = self.visible_grid_rows(cx);
            let count = rows.len() * columns;
            if count > 0 {
                let current = position.row * columns + position.column;
                let next = if shift {
                    current.saturating_sub(1)
                } else {
                    (current + 1).min(count - 1)
                };
                self.select_grid_cell(rows[next / columns], next % columns, false, cx);
                self.begin_cell_edit(window, cx);
            }
        }
        true
    }
    pub fn stage_cell_draft(&mut self, cx: &mut Context<Self>) {
        let outcome = (|| {
            let target = self.grid_target()?;
            let draft = self.grid.draft.as_ref().ok_or("No cell draft")?;
            let result = self.editing_result(cx);
            if !self.query_draft_current()
                || !draft_matches(&draft.target, &draft.snapshot, &target, &result)
            {
                return Err(
                    "Cell draft belongs to an older table result. Cancel and select again.".into(),
                );
            }
            let value = draft.kind.parse(&draft.value.read(cx).value())?;
            Ok((
                target,
                grid::stage_fill(
                    &result,
                    &self.editing_structure(),
                    draft.range,
                    value,
                    &self.changes,
                    GridLimits::default(),
                ),
            ))
        })();
        match outcome {
            Ok((target, plan)) => self.accept_grid_plan(target, plan, cx),
            Err(e) => {
                self.fail(e);
                cx.notify();
            }
        }
    }
    pub fn inspect_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid_range(cx).is_none() {
            return;
        }
        let Some((row, column)) = self.selected_cell() else {
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        let Some(cell) = result.rows.get(row).and_then(|r| r.get(column)) else {
            return;
        };
        let cell = if self.page == Page::Table {
            effective_cell(result, &self.changes, row, column)
        } else {
            self.query_effective_cell(row, column).unwrap_or(cell)
        };
        let title = format!(
            "{} · {}",
            result.columns[column].name, result.columns[column].data_type
        );
        self.inspect_record_value(title, cell.clone(), window, cx);
    }
    /// Shared read-only text/hex modal, including immutable pinned record values.
    pub fn inspect_record_value(
        &mut self,
        title: String,
        raw: Cell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (description, preview) = inspector_text(&raw);
        let formatted = formatted_json(&raw);
        let title = format!("{title} · {description}");
        let text = forms::textarea(preview.clone(), window, cx);
        text.update(cx, |t, cx| {
            t.set_readonly(true, cx);
            t.focus(window, cx);
        });
        self.grid.inspector = Some(CellInspector {
            title,
            text,
            raw,
            preview,
            formatted,
            showing_formatted: false,
        });
        cx.notify();
    }
    pub fn render_grid_tools(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.grid.draft.is_none() {
            return div().into_any_element();
        }
        let mut tools = row().flex_wrap().px_3().py_1().bg(cx.omarchy().surface);
        if let Some(range) = self.grid_range(cx) {
            tools = tools.child(label(
                format!(
                    "{} × {} · Shift-click to extend",
                    range.rows(),
                    range.columns()
                ),
                cx,
            ));
        }
        if let Some(draft) = &self.grid.draft {
            tools = tools
                .child(label(
                    format!("Fill {} × {}", draft.range.rows(), draft.range.columns()),
                    cx,
                ))
                .child(action(
                    "cell-kind",
                    draft.kind.label(),
                    false,
                    true,
                    cx,
                    |s, _, cx| {
                        if let Some(d) = &mut s.grid.draft {
                            d.kind = d.kind.next();
                        }
                        cx.notify();
                    },
                ))
                .child(action(
                    "stage-cell",
                    "Stage · Ctrl+Enter",
                    true,
                    !self.busy,
                    cx,
                    |s, w, cx| {
                        s.stage_cell_draft(cx);
                        if s.grid.draft.is_none() {
                            w.focus(&s.grid_focus, cx);
                        }
                    },
                ))
                .child(action(
                    "cancel-cell",
                    "Cancel · Esc",
                    false,
                    true,
                    cx,
                    |s, w, cx| {
                        s.grid.draft = None;
                        w.focus(&s.grid_focus, cx);
                        cx.notify();
                    },
                ));
        }
        tools.into_any_element()
    }
    pub fn render_cell_inspector(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let inspector = self.grid.inspector.as_ref().unwrap();
        self.modal(column().w(px(820.)).max_h(window.viewport_size().height-px(70.)).p_6().gap_3()
            .child(row().child(div().flex_1().child(inspector.title.clone())).child(action("close-inspector","Close",false,true,cx,|s,w,cx|{s.grid.inspector=None;w.focus(&s.focus,cx);cx.notify();})))
            .child(label("Read-only snapshot · Copy original preserves exact text / bytes; formatting is display-only.",cx))
            .child(row().child(action("inspector-format",if inspector.showing_formatted{"Formatted JSON · show raw"}else{"Raw · show formatted JSON"},false,inspector.formatted.is_some(),cx,|s,w,cx|{
                if let Some(i)=&mut s.grid.inspector {i.showing_formatted = !i.showing_formatted;let text=if i.showing_formatted{i.formatted.as_ref().unwrap_or(&i.preview)}else{&i.preview};i.text.update(cx,|v,cx|v.set_value(text.clone(),w,cx));}cx.notify();
            })).child(action("inspector-copy-raw","Copy original text / hex",false,true,cx,|s,_,cx|{if let Some(i)=&s.grid.inspector{cx.write_to_clipboard(ClipboardItem::new_string(i.raw.display()));}cx.notify();})))
            .child(div().h(px(430.)).font_family(cx.omarchy().mono_font.clone()).child(ui::textarea("cell-inspector",&inspector.text,window,cx)))
            .into_any_element(),cx)
    }
}

fn same_snapshot(a: &QueryResult, b: &QueryResult) -> bool {
    a.columns.len() == b.columns.len()
        && a.columns
            .iter()
            .zip(&b.columns)
            .all(|(a, b)| a.name == b.name && a.data_type == b.data_type)
        && a.rows == b.rows
}
fn draft_matches(
    target: &RowTarget,
    snapshot: &QueryResult,
    current: &RowTarget,
    result: &QueryResult,
) -> bool {
    target == current && same_snapshot(snapshot, result)
}
pub fn effective_cell<'a>(
    result: &'a QueryResult,
    changes: &'a [RowChange],
    row: usize,
    column: usize,
) -> &'a Cell {
    let values = &result.rows[row];
    let name = &result.columns[column].name;
    for change in changes {
        if let RowChange::Update {
            original,
            values: updated,
        } = change
            && original.len() == result.columns.len()
            && result
                .columns
                .iter()
                .zip(values)
                .all(|(col, value)| original.iter().any(|(n, v)| n == &col.name && v == value))
            && let Some((_, value)) = updated.iter().find(|(n, _)| n == name)
        {
            return value;
        }
    }
    &values[column]
}
fn inspector_text(cell: &Cell) -> (String, String) {
    const MAX: usize = 64 * 1024;
    match cell {
        Cell::Null => (
            "SQL NULL".into(),
            "SQL NULL (distinct from empty text and the text NULL)".into(),
        ),
        Cell::Text(text) => {
            let preview: String = text.chars().take(MAX).collect();
            (
                format!("Text · {} bytes", text.len()),
                if preview.len() < text.len() {
                    format!("{preview}\n… preview truncated; use Copy range for the full value")
                } else {
                    preview
                },
            )
        }
        Cell::Binary(bytes) => {
            let mut text = String::new();
            for (index, chunk) in bytes.chunks(16).take(MAX / 16).enumerate() {
                let hex = chunk
                    .iter()
                    .map(|b| format!("{b:02x} "))
                    .collect::<String>();
                let ascii = chunk
                    .iter()
                    .map(|b| {
                        if b.is_ascii_graphic() || *b == b' ' {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect::<String>();
                text.push_str(&format!("{:08x}  {hex:48}  |{ascii}|\n", index * 16));
            }
            if bytes.len() > MAX {
                text.push_str(
                    "… preview truncated; use Rowboat lossless Copy range for all bytes\n",
                );
            }
            if bytes.len() <= MAX
                && let Ok(utf8) = std::str::from_utf8(bytes)
            {
                text.push_str(&format!("\nUTF-8:\n{utf8}"));
            }
            (
                format!("Binary · {} bytes · hex / ASCII", bytes.len()),
                text,
            )
        }
    }
}

fn formatted_json(cell: &Cell) -> Option<String> {
    let Cell::Text(text) = cell else {
        return None;
    };
    if text.len() > 64 * 1024 {
        return None;
    }
    let json = serde_json::from_str::<serde_json::Value>(text).ok()?;
    serde_json::to_string_pretty(&json).ok()
}

#[cfg(test)]
mod tests {
    use super::{ValueKind, draft_matches, effective_cell, formatted_json, inspector_text};
    use crate::forms::RowTarget;
    use rowboat_core::{
        Cell, QueryResult, RowChange,
        grid::{self, ClipboardDialect, GridLimits, GridPosition, GridRange},
    };
    use rowboat_core::{Column, Structure, TableRef};
    fn result() -> QueryResult {
        let column = |name: &str, key: bool| Column {
            name: name.into(),
            data_type: "text".into(),
            nullable: !key,
            primary_key: key,
            default_value: None,
        };
        QueryResult {
            columns: vec![
                column("tenant", true),
                column("id", true),
                column("value", false),
                column("other", false),
            ],
            rows: vec![vec![
                Cell::Text("a".into()),
                Cell::Text("1".into()),
                Cell::Text("original".into()),
                Cell::Null,
            ]],
            ..Default::default()
        }
    }
    fn target() -> RowTarget {
        RowTarget {
            connection_id: "pg".into(),
            session_generation: 3,
            table: TableRef {
                schema: "public".into(),
                name: "items".into(),
            },
        }
    }
    #[test]
    fn cell_draft_rejects_reconnected_replaced_sorted_or_remapped_results() {
        let original = result();
        let owner = target();
        assert!(draft_matches(&owner, &original, &owner, &original));
        let mut changed = owner.clone();
        changed.session_generation += 1;
        assert!(!draft_matches(&owner, &original, &changed, &original));
        changed = owner.clone();
        changed.connection_id = "mysql".into();
        assert!(!draft_matches(&owner, &original, &changed, &original));
        changed = owner.clone();
        changed.table.name = "other".into();
        assert!(!draft_matches(&owner, &original, &changed, &original));
        let mut refreshed = original.clone();
        refreshed.rows[0][2] = Cell::Text("server changed".into());
        assert!(!draft_matches(&owner, &original, &owner, &refreshed));
        refreshed = original.clone();
        refreshed.columns.swap(2, 3);
        assert!(!draft_matches(&owner, &original, &owner, &refreshed));
    }
    #[test]
    fn staged_display_overlay_keeps_full_original_and_lossless_copy_retains_types() {
        let result = result();
        let structure = Structure {
            columns: result.columns.clone(),
            ..Default::default()
        };
        let range = GridRange::from_size(GridPosition { row: 0, column: 2 }, 1, 1).unwrap();
        let first = grid::stage_fill(
            &result,
            &structure,
            range,
            Cell::Binary(vec![0, 255]),
            &[],
            GridLimits::default(),
        )
        .unwrap();
        let range = GridRange::from_size(GridPosition { row: 0, column: 3 }, 1, 1).unwrap();
        let changes = grid::stage_fill(
            &result,
            &structure,
            range,
            Cell::Text("\\N".into()),
            &first,
            GridLimits::default(),
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert!(
            matches!(&changes[0],RowChange::Update{original,values} if original.len()==4 && values.len()==2)
        );
        assert_eq!(result.rows[0][2], Cell::Text("original".into()));
        let shown = QueryResult {
            columns: result.columns.clone(),
            rows: vec![
                (0..4)
                    .map(|c| effective_cell(&result, &changes, 0, c).clone())
                    .collect(),
            ],
            ..Default::default()
        };
        let range = GridRange::from_size(GridPosition { row: 0, column: 0 }, 1, 4).unwrap();
        let copied = grid::export_tsv(
            &shown,
            range,
            ClipboardDialect::Rowboat,
            GridLimits::default(),
        )
        .unwrap();
        assert_eq!(
            grid::parse_tsv(&copied, ClipboardDialect::Rowboat, GridLimits::default())
                .unwrap()
                .rows(),
            shown.rows
        );
        assert_eq!(
            ValueKind::Text.parse("NULL").unwrap(),
            Cell::Text("NULL".into())
        );
        assert_eq!(ValueKind::Text.parse("").unwrap(), Cell::Text("".into()));
        assert_eq!(ValueKind::Null.parse("ignored").unwrap(), Cell::Null);
        assert!(ValueKind::Binary.parse("zz").is_err());
    }
    #[test]
    fn json_view_preserves_large_integer_decimal_and_raw_spelling() {
        let raw = r#"{"id":1234567890123456789012345678901234567890,"decimal":0.123456789012345678901234567890,"escaped":"\u0041"}"#;
        let cell = Cell::Text(raw.into());
        let formatted = formatted_json(&cell).unwrap();
        assert!(formatted.contains("1234567890123456789012345678901234567890"));
        assert!(formatted.contains("0.123456789012345678901234567890"));
        assert_eq!(inspector_text(&cell).1, raw);
        assert_eq!(cell.display(), raw);
    }
    #[test]
    fn binary_inspection_is_bounded_and_keeps_raw_bytes_untouched() {
        let bytes = vec![255; 128 * 1024];
        let cell = Cell::Binary(bytes.clone());
        let (title, preview) = inspector_text(&cell);
        assert!(title.contains("131072 bytes"));
        assert!(preview.contains("preview truncated"));
        assert!(preview.len() < 400_000);
        assert_eq!(cell, Cell::Binary(bytes));
    }
}
