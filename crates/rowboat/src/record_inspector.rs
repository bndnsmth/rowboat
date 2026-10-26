//! Read-only, bounded record inspection. Retained cells are borrowed, never reformatted
//! in place. JSON nodes retain source spans, including exact numeric/string spelling.
use crate::{
    app::{Page, Workbench},
    forms::{self, RowTarget},
    grid_ui::effective_cell,
    view::{action as workbench_action, column, label, row},
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::{Cell, Column, QueryResult, RowChange};
use std::{collections::BTreeSet, ops::Range};

pub const MAX_JSON_BYTES: usize = 64 * 1024;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_JSON_NODES: usize = 512;
const MAX_VISIBLE_NODES: usize = 192;
const MAX_RENDER_BYTES: usize = 256 * 1024;
const MAX_SEARCH_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_FIELDS: usize = 4096;
const FIELDS_PER_PAGE: usize = 16;

fn action(
    id: impl Into<SharedString>,
    caption: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    cx: &mut Context<Workbench>,
    handler: impl Fn(&mut Workbench, &mut Window, &mut Context<Workbench>) + 'static,
) -> ui::Button {
    let id = id.into();
    workbench_action(id.clone(), caption, primary, enabled, cx, handler)
        .flex_shrink_0()
        .debug_selector(move || id.to_string())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecordDock {
    #[default]
    Right,
    Bottom,
}

pub struct RecordInspector {
    pub open: bool,
    pub dock: RecordDock,
    pub search: Entity<InputState>,
    pub field_page: usize,
    expanded: BTreeSet<String>,
}

/// A borrowed immutable view. Pinned results use `retained`, with no overlays.
/// Callers retain their original QueryResult/Cell storage and its memory accounting.
pub struct RecordSnapshot<'a> {
    pub title: String,
    pub columns: &'a [Column],
    pub values: &'a [Cell],
    pub staged: Vec<Option<&'a Cell>>,
    pub deleted: bool,
}
impl<'a> RecordSnapshot<'a> {
    pub fn retained(title: impl Into<String>, result: &'a QueryResult, row: usize) -> Option<Self> {
        let values = result.rows.get(row)?;
        if values.len() != result.columns.len() {
            return None;
        }
        Some(Self {
            title: title.into(),
            columns: &result.columns,
            values,
            staged: vec![],
            deleted: false,
        })
    }

    /// A table overlay is valid only for its exact connection/session/table owner.
    pub fn with_table_changes(
        mut self,
        result: &'a QueryResult,
        row: usize,
        changes: &'a [RowChange],
        owner: Option<&RowTarget>,
        current: Option<&RowTarget>,
    ) -> Self {
        if owner.is_none()
            || owner != current
            || !std::ptr::eq(self.columns, result.columns.as_slice())
            || !result
                .rows
                .get(row)
                .is_some_and(|values| std::ptr::eq(self.values, values.as_slice()))
        {
            return self;
        }
        self.staged = (0..self.columns.len())
            .map(|column| {
                let effective = effective_cell(result, changes, row, column);
                (effective != &self.values[column]).then_some(effective)
            })
            .collect();
        self.deleted = changes.iter().any(|change| {
            matches!(change, RowChange::Delete { original }
                if original.len() == self.columns.len()
                    && self.columns.iter().zip(self.values).all(|(c, v)|
                        original.iter().any(|(name, value)| name == &c.name && value == v)))
        });
        self
    }

    pub fn value(&self, column: usize, staged: bool) -> Option<&'a Cell> {
        if staged && let Some(Some(value)) = self.staged.get(column) {
            return Some(value);
        }
        self.values.get(column)
    }

    fn identity(&self) -> (usize, usize, String) {
        (
            self.columns.as_ptr() as usize,
            self.values.as_ptr() as usize,
            self.title.clone(),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum JsonFallback {
    TooLarge,
    TooDeep,
    Invalid,
}
impl JsonFallback {
    fn message(&self) -> &'static str {
        match self {
            Self::TooLarge => {
                "JSON tree limited to 64 KiB · raw preview; Copy value keeps the full original"
            }
            Self::TooDeep => {
                "JSON tree limited to 32 levels · raw preview; Copy value keeps the full original"
            }
            Self::Invalid => "Raw text · not valid JSON",
        }
    }
}

#[derive(Debug)]
pub struct JsonNode {
    pub pointer: String,
    pub name: String,
    pub span: Range<usize>,
    pub parent: Option<usize>,
    pub depth: usize,
    pub container: bool,
}
#[derive(Debug)]
pub struct JsonTree<'a> {
    pub raw: &'a str,
    pub nodes: Vec<JsonNode>,
    pub truncated: bool,
}
impl<'a> JsonTree<'a> {
    pub fn parse(raw: &'a str) -> Result<Self, JsonFallback> {
        if raw.len() > MAX_JSON_BYTES {
            return Err(JsonFallback::TooLarge);
        }
        // Check depth without recursion before either validation or tree construction.
        let mut depth = 0usize;
        let mut quoted = false;
        let mut escaped = false;
        let mut string_start = 0;
        for (offset, b) in raw.bytes().enumerate() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    quoted = false;
                    // IgnoredAny deliberately permits unpaired surrogate escapes.
                    // Check strings separately before decoding object keys below.
                    serde_json::from_str::<String>(&raw[string_start..=offset])
                        .map_err(|_| JsonFallback::Invalid)?;
                }
            } else {
                match b {
                    b'"' => {
                        quoted = true;
                        string_start = offset;
                    }
                    b'[' | b'{' => {
                        depth += 1;
                        if depth > MAX_JSON_DEPTH {
                            return Err(JsonFallback::TooDeep);
                        }
                    }
                    b']' | b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        // IgnoredAny validates the bounded input without allocating a second value tree.
        // The crate already enables serde_json arbitrary_precision; no f64 conversion
        // or Value reserialization is involved in this view or in nested copying.
        serde_json::from_str::<serde::de::IgnoredAny>(raw).map_err(|_| JsonFallback::Invalid)?;
        let mut tree = Self {
            raw,
            nodes: vec![],
            truncated: false,
        };
        let start = raw.len() - raw.trim_start().len();
        let end = raw.trim_end().len();
        tree.build(start..end, "$".into(), String::new(), None, 0);
        Ok(tree)
    }

    fn build(
        &mut self,
        span: Range<usize>,
        name: String,
        pointer: String,
        parent: Option<usize>,
        depth: usize,
    ) {
        if self.nodes.len() == MAX_JSON_NODES {
            self.truncated = true;
            return;
        }
        let first = self.raw.as_bytes()[span.start];
        let container = first == b'{' || first == b'[';
        let index = self.nodes.len();
        self.nodes.push(JsonNode {
            pointer: pointer.clone(),
            name,
            span: span.clone(),
            parent,
            depth,
            container,
        });
        if !container {
            return;
        }
        let mut cursor = skip_space(self.raw, span.start + 1);
        let mut ordinal = 0usize;
        while cursor < span.end - 1 {
            if self.nodes.len() == MAX_JSON_NODES {
                self.truncated = true;
                break;
            }
            let name = if first == b'{' {
                let end = value_end(self.raw, cursor);
                let name: String =
                    serde_json::from_str(&self.raw[cursor..end]).expect("validated key");
                cursor = skip_space(self.raw, end);
                cursor = skip_space(self.raw, cursor + 1); // colon
                name
            } else {
                ordinal.to_string()
            };
            let end = value_end(self.raw, cursor);
            let token = name.replace('~', "~0").replace('/', "~1");
            self.build(
                cursor..end,
                name,
                format!("{pointer}/{token}"),
                Some(index),
                depth + 1,
            );
            cursor = skip_space(self.raw, end);
            if self.raw.as_bytes().get(cursor) == Some(&b',') {
                cursor = skip_space(self.raw, cursor + 1);
            }
            ordinal += 1;
        }
    }

    pub fn original(&self, node: usize) -> &str {
        &self.raw[self.nodes[node].span.clone()]
    }

    fn search_text(&self, node: usize) -> String {
        let value = self.original(node);
        let decoded = if value.starts_with('"') {
            serde_json::from_str::<String>(value).unwrap_or_else(|_| value.into())
        } else if self.nodes[node].container {
            String::new()
        } else {
            value.into()
        };
        format!("{} {decoded}", self.nodes[node].name).to_lowercase()
    }

    pub fn visible_nodes(
        &self,
        expanded: &BTreeSet<String>,
        search: &str,
        limit: usize,
    ) -> Vec<usize> {
        if limit == 0 {
            return vec![];
        }
        let search = search.to_lowercase();
        let mut matches = vec![search.is_empty(); self.nodes.len()];
        if !search.is_empty() {
            for (index, hit) in matches.iter_mut().enumerate() {
                *hit = self.search_text(index).contains(&search);
            }
            for index in (0..self.nodes.len()).rev() {
                if matches[index]
                    && let Some(parent) = self.nodes[index].parent
                {
                    matches[parent] = true;
                }
            }
        }
        let mut visible = vec![false; self.nodes.len()];
        let mut result = vec![];
        for (index, node) in self.nodes.iter().enumerate() {
            visible[index] = node.parent.is_none_or(|parent| {
                visible[parent]
                    && (!search.is_empty()
                        || expanded.contains(&self.nodes[parent].span.start.to_string()))
            });
            if visible[index] && (matches[index] || index == 0) {
                result.push(index);
                if result.len() == limit {
                    break;
                }
            }
        }
        result
    }
}

fn skip_space(raw: &str, mut cursor: usize) -> usize {
    while raw
        .as_bytes()
        .get(cursor)
        .is_some_and(u8::is_ascii_whitespace)
    {
        cursor += 1;
    }
    cursor
}
// Input is already validated. Locate exact token boundaries without normalizing it.
fn value_end(raw: &str, start: usize) -> usize {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, b) in raw.as_bytes()[start..].iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
                if depth == 0 {
                    return start + offset + 1;
                }
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'[' | b'{' => depth += 1,
                b']' | b'}' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return start + offset + 1;
                    }
                }
                b',' | b']' | b'}' | b':' | b' ' | b'\n' | b'\r' | b'\t' if depth == 0 => {
                    return start + offset;
                }
                _ => {}
            }
        }
    }
    raw.len()
}

fn prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn preview(cell: &Cell) -> String {
    match cell {
        Cell::Null => "SQL NULL".into(),
        Cell::Text(text) if text.is_empty() => "Empty text (0 bytes)".into(),
        Cell::Text(text) => {
            let short = prefix(text, 512);
            format!(
                "{short}{}",
                if short.len() < text.len() {
                    "… [preview truncated]"
                } else {
                    ""
                }
            )
        }
        Cell::Binary(bytes) => format!(
            "Binary · {} bytes · {}{}",
            bytes.len(),
            Cell::Binary(bytes.iter().take(64).copied().collect()).display(),
            if bytes.len() > 64 {
                "… [preview truncated]"
            } else {
                ""
            }
        ),
    }
}

pub struct RecordSearch {
    pub columns: Vec<usize>,
    pub limited: bool,
}
/// Literal, Unicode-aware case-insensitive field/value search. A visible limit
/// notice accompanies bounded-prefix searches; a missing match is never definitive.
pub fn search_record(snapshot: &RecordSnapshot<'_>, search: &str) -> RecordSearch {
    let term = prefix(search, 1024).to_lowercase();
    if term.is_empty() {
        return RecordSearch {
            columns: (0..snapshot.columns.len()).collect(),
            limited: false,
        };
    }
    let mut budget = MAX_SEARCH_BYTES;
    let mut result = RecordSearch {
        columns: vec![],
        limited: snapshot.columns.len() > MAX_SEARCH_FIELDS || search.len() > 1024,
    };
    for (ordinal, column) in snapshot.columns.iter().take(MAX_SEARCH_FIELDS).enumerate() {
        let mut hit = prefix(&column.name, 4096).to_lowercase().contains(&term)
            || prefix(&column.data_type, 4096)
                .to_lowercase()
                .contains(&term);
        for cell in [
            snapshot.values.get(ordinal),
            snapshot.staged.get(ordinal).copied().flatten(),
        ]
        .into_iter()
        .flatten()
        {
            match cell {
                Cell::Text(text) => {
                    let shown = prefix(text, MAX_JSON_BYTES.min(budget));
                    budget -= shown.len();
                    result.limited |= shown.len() < text.len();
                    hit |= shown.to_lowercase().contains(&term);
                    if !hit
                        && shown.len() == text.len()
                        && let Ok(tree) = JsonTree::parse(text)
                    {
                        result.limited |= tree.truncated;
                        hit |= (0..tree.nodes.len()).any(|i| tree.search_text(i).contains(&term));
                    }
                }
                Cell::Null => hit |= "sql null".contains(&term),
                Cell::Binary(bytes) => {
                    result.limited |= bytes.len() > 64;
                    hit |= preview(cell).to_lowercase().contains(&term);
                }
            }
        }
        if hit {
            result.columns.push(ordinal);
        }
    }
    result
}

impl Workbench {
    pub fn toggle_record_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.grid.record {
            state.open = !state.open;
        } else {
            let search = forms::input("", "Search fields and values", window, cx);
            cx.observe(&search, |s, _, cx| {
                if let Some(state) = &mut s.grid.record {
                    state.field_page = 0;
                }
                cx.notify();
            })
            .detach();
            self.grid.record = Some(RecordInspector {
                open: true,
                dock: RecordDock::Right,
                search,
                field_page: 0,
                expanded: BTreeSet::new(),
            });
        }
        // Opening and row updates do not focus any input or alter pending drafts.
        cx.notify();
    }

    pub fn record_panel_open(&self) -> bool {
        self.grid.record.as_ref().is_some_and(|s| s.open)
    }

    /// Parent layout places the panel beside the grid for Right, below for Bottom.
    /// Small windows automatically use Bottom without discarding the user's choice.
    pub fn record_panel_dock(&self, window: &Window) -> RecordDock {
        if window.viewport_size().width < px(1000.) {
            RecordDock::Bottom
        } else {
            self.grid
                .record
                .as_ref()
                .map_or(RecordDock::Right, |s| s.dock)
        }
    }

    pub fn selected_record_snapshot(&self) -> Option<RecordSnapshot<'_>> {
        // Pins must be routed before live results: they never receive staged overlays.
        if let Some(result) = self.pinned_result() {
            return RecordSnapshot::retained(
                format!(
                    "Pinned database snapshot · row {} · read-only",
                    self.results.selected_row + 1
                ),
                result,
                self.results.selected_row,
            );
        }
        if self.results_read_only() {
            return None;
        }
        let (row, _) = self.selected_cell()?;
        let result = self.current_result()?;
        let mut snapshot = RecordSnapshot::retained(
            format!("Retained database record · row {}", row + 1),
            result,
            row,
        )?;
        if self.page == Page::Table && self.query_edit.batch.is_none() {
            snapshot = snapshot.with_table_changes(
                result,
                row,
                &self.changes,
                self.change_target.as_ref(),
                self.current_target().as_ref(),
            );
        } else if self.page == Page::Query
            && self.query_batch_matches()
            && self.change_target.is_some()
            && self.change_target == self.editing_target()
        {
            snapshot.staged = (0..snapshot.columns.len())
                .map(|column| {
                    self.query_effective_cell(row, column)
                        .filter(|value| *value != &snapshot.values[column])
                })
                .collect();
        }
        Some(snapshot)
    }

    pub fn render_record_toolbar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        action(
            "record-toggle",
            if self.record_panel_open() {
                "Hide record"
            } else {
                "Inspect record"
            },
            self.record_panel_open(),
            true,
            cx,
            |s, w, cx| s.toggle_record_inspector(w, cx),
        )
        .into_any_element()
    }

    pub fn render_record_panel(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.record_panel_open() {
            return div().into_any_element();
        }
        self.render_record_snapshot(self.selected_record_snapshot(), window, cx)
    }

    /// Reusable view for the selected live or pinned record. `RecordSnapshot` is also
    /// public for comparison views; callbacks re-resolve the selected source and reject
    /// a replaced row rather than retain/copy another unbudgeted result snapshot.
    pub fn render_record_snapshot(
        &self,
        snapshot: Option<RecordSnapshot<'_>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = &self.grid.record else {
            return div().into_any_element();
        };
        let dock = self.record_panel_dock(window);
        let panel = column()
            .id("record-panel")
            .debug_selector(|| "record-panel".into())
            .overflow_hidden()
            .gap_2()
            .p_2()
            .bg(cx.omarchy().surface)
            .border_1()
            .border_color(cx.omarchy().border)
            .when(dock == RecordDock::Right, |d| {
                d.w(px(390.)).h_full().flex_shrink_0()
            })
            .when(dock == RecordDock::Bottom, |d| {
                d.w_full().h(px(300.)).max_h_full().flex_shrink_1()
            })
            .child(
                row()
                    .flex_shrink_0()
                    .flex_wrap()
                    .child(label("Record · read-only", cx))
                    .child(action(
                        "record-dock",
                        if state.dock == RecordDock::Right {
                            "Dock below"
                        } else {
                            "Dock right"
                        },
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            if let Some(state) = &mut s.grid.record {
                                state.dock = if state.dock == RecordDock::Right {
                                    RecordDock::Bottom
                                } else {
                                    RecordDock::Right
                                };
                            }
                            cx.notify();
                        },
                    ))
                    .child(action(
                        "record-close",
                        "Close",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            if let Some(state) = &mut s.grid.record {
                                state.open = false;
                            }
                            cx.notify();
                        },
                    )),
            );
        // Only the compact Close/Dock row stays fixed. Search must scroll with
        // the fields: a fixed search row leaves less than a button's height in
        // a 124px query-result region, making copy actions unreachable.
        let viewport = div()
            .id("record-fields")
            .debug_selector(|| "record-fields".into())
            .min_h_0()
            .min_w_0()
            .w_full()
            .flex_1()
            .overflow_y_scroll();
        let mut content = column().w_full().flex_shrink_0().gap_2().child(
            div()
                .flex_shrink_0()
                .min_w_0()
                .w_full()
                .debug_selector(|| "record-search".into())
                .child(ui::input("record-search", &state.search, window, cx)),
        );
        let Some(snapshot) = snapshot else {
            return panel
                .child(
                    viewport
                        .child(content.child(label("Select a retained row to inspect it.", cx))),
                )
                .into_any_element();
        };
        let identity = snapshot.identity();
        let term = state.search.read(cx).value().to_string();
        let found = search_record(&snapshot, &term);
        let pages = found.columns.len().div_ceil(FIELDS_PER_PAGE).max(1);
        let page = state.field_page.min(pages - 1);
        // Keep intrinsic-height content separate from the finite scroll viewport.
        // A flex column that is both flex_1 and scrollable otherwise shrinks its
        // children to the result region, painting labels/actions over one another.
        content = content.child(label(snapshot.title.clone(), cx));
        if snapshot.deleted {
            content = content.child(label(
                "This record is staged for deletion · original retained values below",
                cx,
            ));
        }
        if found.limited {
            content = content.child(label("Search limited to 4,096 fields / 1 MiB, 64 KiB per text value / 64 binary bytes and bounded JSON nodes; additional matches may exist.", cx));
        }
        content = content.child(
            row()
                .flex_shrink_0()
                .flex_wrap()
                .child(label(
                    format!(
                        "{} fields · page {} / {pages}",
                        found.columns.len(),
                        page + 1
                    ),
                    cx,
                ))
                .child(action(
                    "record-previous",
                    "Previous",
                    false,
                    page > 0,
                    cx,
                    |s, _, cx| {
                        if let Some(state) = &mut s.grid.record {
                            state.field_page = state.field_page.saturating_sub(1);
                        }
                        cx.notify();
                    },
                ))
                .child(action(
                    "record-next",
                    "Next",
                    false,
                    page + 1 < pages,
                    cx,
                    move |s, _, cx| {
                        if let Some(state) = &mut s.grid.record {
                            state.field_page = (page + 1).min(pages - 1);
                        }
                        cx.notify();
                    },
                )),
        );
        if self.pinned_result().is_none() && self.page == Page::Table {
            content = content.child(action(
                "record-relationships",
                "Open FK relationships…",
                false,
                !self.busy && !self.has_row_draft() && self.changes.is_empty(),
                cx,
                |s, w, cx| s.show_relationships(w, cx),
            ));
        }
        let mut bytes = MAX_RENDER_BYTES;
        let mut nodes = MAX_VISIBLE_NODES;
        for &ordinal in found
            .columns
            .iter()
            .skip(page * FIELDS_PER_PAGE)
            .take(FIELDS_PER_PAGE)
        {
            let metadata = &snapshot.columns[ordinal];
            let staged = snapshot.staged.get(ordinal).is_some_and(Option::is_some);
            let mut field = column()
                .debug_selector(move || format!("record-field-{ordinal}"))
                .w_full()
                .flex_shrink_0()
                .overflow_hidden()
                .gap_1()
                .pb_2()
                .border_b_1()
                .border_color(cx.omarchy().border)
                .child(label(
                    format!(
                        "#{} · {} · {}",
                        ordinal,
                        prefix(&metadata.name, 160),
                        prefix(&metadata.data_type, 80)
                    ),
                    cx,
                ));
            for is_staged in [false, true] {
                if is_staged && !staged {
                    continue;
                }
                let Some(value) = snapshot.value(ordinal, is_staged) else {
                    continue;
                };
                let tag = if is_staged { "Staged" } else { "Original" };
                let key = format!("{ordinal}-{}", u8::from(is_staged));
                field = field.child(
                    row()
                        .min_w_0()
                        .w_full()
                        .flex_shrink_0()
                        .flex_wrap()
                        .child(label(tag, cx))
                        .child(self.record_copy_action(
                            format!("record-copy-{key}"),
                            "Copy value",
                            identity.clone(),
                            ordinal,
                            is_staged,
                            None,
                            cx,
                        ))
                        .child(action(
                            SharedString::from(format!("record-path-{key}")),
                            "Copy path",
                            false,
                            true,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(format!(
                                    "/{ordinal}"
                                )));
                            },
                        ))
                        .child({
                            let identity = identity.clone();
                            action(
                                SharedString::from(format!("record-open-{key}")),
                                "Text / hex…",
                                false,
                                true,
                                cx,
                                move |s, w, cx| {
                                    let value = s
                                        .selected_record_snapshot()
                                        .filter(|r| r.identity() == identity)
                                        .and_then(|r| {
                                            r.value(ordinal, is_staged).map(|v| {
                                                (
                                                    format!("{} · {tag}", r.columns[ordinal].name),
                                                    v.clone(),
                                                )
                                            })
                                        });
                                    if let Some((title, value)) = value {
                                        s.inspect_record_value(title, value, w, cx);
                                    }
                                },
                            )
                        }),
                );
                let tree = if let Cell::Text(raw) = value {
                    if raw.len().min(MAX_JSON_BYTES) <= bytes && nodes > 0 {
                        bytes -= raw.len().min(MAX_JSON_BYTES);
                        Some(JsonTree::parse(raw))
                    } else {
                        None
                    }
                } else {
                    None
                };
                match tree {
                    Some(Ok(tree)) => {
                        let expanded = state
                            .expanded
                            .iter()
                            .filter_map(|k| k.strip_prefix(&format!("{key}:")))
                            .map(str::to_owned)
                            .collect();
                        let visible = tree.visible_nodes(&expanded, prefix(&term, 1024), nodes);
                        if tree.truncated || visible.len() == nodes {
                            field = field.child(label("JSON tree preview limited · collapse nodes or use Text / hex / Copy value for the full value", cx));
                        }
                        nodes = nodes.saturating_sub(visible.len());
                        for index in visible {
                            let node = &tree.nodes[index];
                            let expand_key = format!("{key}:{}", node.span.start);
                            let open = state.expanded.contains(&expand_key);
                            let pointer = format!("/{ordinal}{}", node.pointer);
                            let summary = if node.container {
                                if tree.original(index).starts_with('{') {
                                    "{ object }".into()
                                } else {
                                    "[ array ]".into()
                                }
                            } else if tree.original(index) == "null" {
                                "JSON null".into()
                            } else {
                                let original = tree.original(index);
                                let short = prefix(original, 240);
                                format!(
                                    "{short}{}",
                                    if short.len() < original.len() {
                                        "… [preview truncated]"
                                    } else {
                                        ""
                                    }
                                )
                            };
                            let mut item = column()
                                .w_full()
                                .flex_shrink_0()
                                .overflow_hidden()
                                .pl(px((node.depth.min(8) * 10) as f32))
                                .gap_1()
                                .child(label(
                                    format!("{}: {summary}", prefix(&node.name, 120)),
                                    cx,
                                ));
                            let mut tools = row().min_w_0().w_full().flex_shrink_0().flex_wrap();
                            if node.container {
                                tools = tools.child(action(
                                    SharedString::from(format!("record-expand-{key}-{index}")),
                                    if open { "Collapse" } else { "Expand" },
                                    false,
                                    true,
                                    cx,
                                    move |s, _, cx| {
                                        if let Some(state) = &mut s.grid.record
                                            && !state.expanded.remove(&expand_key)
                                        {
                                            if state.expanded.len() >= MAX_JSON_NODES {
                                                state.expanded.clear();
                                            }
                                            state.expanded.insert(expand_key.clone());
                                        }
                                        cx.notify();
                                    },
                                ));
                            }
                            tools = tools
                                .child(self.record_copy_action(
                                    format!("record-node-copy-{key}-{index}"),
                                    "Copy value",
                                    identity.clone(),
                                    ordinal,
                                    is_staged,
                                    Some(node.span.clone()),
                                    cx,
                                ))
                                .child(action(
                                    SharedString::from(format!("record-node-path-{key}-{index}")),
                                    "Copy path",
                                    false,
                                    true,
                                    cx,
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            pointer.clone(),
                                        ));
                                    },
                                ));
                            item = item.child(tools);
                            field = field.child(item);
                        }
                    }
                    other => {
                        if let Some(Err(reason)) = other {
                            field = field.child(label(reason.message(), cx));
                        } else if matches!(value, Cell::Text(_)) {
                            field = field.child(label(
                                "Record tree rendering budget reached · raw preview",
                                cx,
                            ));
                        }
                        field = field.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .flex_shrink_0()
                                .overflow_hidden()
                                .text_size(px(12.))
                                .child(preview(value)),
                        );
                    }
                }
            }
            content = content.child(field);
        }
        if found.columns.is_empty() {
            content = content.child(label("No fields matched within the search limits.", cx));
        }
        content = content
            .child(label("Original = retained database value. Staged = pending Review → Apply; unsubmitted drafts are not shown.", cx))
            .child(label("Copy path: JSON Pointer /column-ordinal/nested-key (duplicate field names stay distinct).", cx));
        panel.child(viewport.child(content)).into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn record_copy_action(
        &self,
        id: String,
        caption: &'static str,
        identity: (usize, usize, String),
        ordinal: usize,
        staged: bool,
        span: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) -> ui::Button {
        action(
            SharedString::from(id),
            caption,
            false,
            true,
            cx,
            move |s, _, cx| {
                let copied = s
                    .selected_record_snapshot()
                    .filter(|r| r.identity() == identity)
                    .and_then(|r| {
                        let value = r.value(ordinal, staged)?;
                        match (&span, value) {
                            (Some(span), Cell::Text(raw)) => {
                                raw.get(span.clone()).map(str::to_owned)
                            }
                            (None, _) => Some(value.display()),
                            _ => None,
                        }
                    });
                if let Some(text) = copied {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    s.status = "Copied exact value · binary as hex; SQL NULL as NULL (see the displayed type)".into();
                    cx.notify();
                }
            },
        )
    }
}

#[cfg(test)]
#[path = "record_inspector_tests.rs"]
mod tests;
