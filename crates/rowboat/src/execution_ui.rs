//! Captured execution provenance and editor-linked positions. No database I/O.
use crate::app::Workbench;
use gpui_kit::{prelude::*, *};
use rowboat_core::{
    Engine,
    execution::{BoundaryError, ErrorPosition, RunScope},
};
use std::{cell::RefCell, collections::HashMap, ops::Range};

pub struct RunSource {
    pub editor_snapshot: SharedString,
    pub source_range: Range<usize>,
    pub submitted_sql: String,
    pub connection_id: String,
    pub engine: Engine,
    pub session_generation: u64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub scope: RunScope,
    /// None if parameters/rewrite changed the submitted SQL coordinates.
    pub error_offset: Option<usize>,
    pub position_maps_directly: bool,
}
struct Preview {
    sql: SharedString,
    engine: Engine,
    ranges: Result<Vec<Range<usize>>, BoundaryError>,
}
#[derive(Default)]
pub struct State {
    pub launches: HashMap<usize, RunSource>,
    preview: RefCell<Option<Preview>>,
}
fn coordinates(text: &str, byte: usize) -> (usize, usize) {
    let before = &text[..byte];
    (
        before.bytes().filter(|b| *b == b'\n').count() + 1,
        before.rsplit('\n').next().unwrap_or("").chars().count() + 1,
    )
}
fn range_label(text: &str, range: &Range<usize>) -> String {
    let (line, col) = coordinates(text, range.start);
    let (end_line, end_col) = coordinates(text, range.end);
    format!("L{line}:{col}–L{end_line}:{end_col}")
}
impl Workbench {
    pub fn execution_source(&self, tab: usize) -> Option<&RunSource> {
        self.execution.launches.get(&tab)
    }
    fn execution_engine(&self) -> Engine {
        self.profile
            .as_ref()
            .or_else(|| {
                self.state
                    .connections
                    .iter()
                    .find(|p| Some(&p.id) == self.owner.as_ref())
            })
            .map_or(Engine::Postgres, |p| p.engine)
    }
    pub(crate) fn current_execution_range(&self, cx: &App) -> Result<Range<usize>, BoundaryError> {
        let editor = self.tabs[self.active_tab].editor.read(cx);
        let sql = editor.value();
        let engine = self.execution_engine();
        let mut cache = self.execution.preview.borrow_mut();
        if cache
            .as_ref()
            .is_none_or(|p| p.sql != sql || p.engine != engine)
        {
            *cache = Some(Preview {
                ranges: rowboat_core::execution::statement_ranges(engine, &sql),
                sql,
                engine,
            });
        }
        let ranges = cache
            .as_ref()
            .unwrap()
            .ranges
            .as_ref()
            .map_err(Clone::clone)?;
        ranges
            .iter()
            .find(|r| editor.cursor() < r.end)
            .or_else(|| ranges.last())
            .cloned()
            .ok_or_else(|| BoundaryError {
                offset: editor.cursor(),
                message: "No SQL statement at the cursor".into(),
            })
    }
    pub(crate) fn capture_execution_source(
        &mut self,
        range: Range<usize>,
        submitted_sql: &str,
        scope: RunScope,
        cx: &App,
    ) {
        self.execution
            .launches
            .retain(|id, _| self.tabs.iter().any(|t| t.id == *id));
        let tab = &self.tabs[self.active_tab];
        let snapshot = tab.editor.read(cx).value();
        if snapshot.len() > 1024 * 1024 || submitted_sql.len() > 1024 * 1024 {
            self.execution.launches.remove(&tab.id);
            return;
        }
        let position_maps_directly = &snapshot[range.clone()] == submitted_sql;
        let scope = if scope == RunScope::SelectionOrAll {
            if tab.editor.read(cx).selected_range().is_empty() {
                RunScope::All
            } else {
                RunScope::Selection
            }
        } else {
            scope
        };
        self.execution.launches.insert(
            tab.id,
            RunSource {
                editor_snapshot: snapshot,
                source_range: range,
                submitted_sql: submitted_sql.to_owned(),
                connection_id: self.profile.as_ref().unwrap().id.clone(),
                engine: self.execution_engine(),
                session_generation: self.session_generation,
                started_at: chrono::Utc::now(),
                scope,
                error_offset: None,
                position_maps_directly,
            },
        );
    }
    pub(crate) fn receive_execution_position(
        &mut self,
        tab: usize,
        position: Option<ErrorPosition>,
    ) {
        if let Some(source) = self.execution.launches.get_mut(&tab) {
            source.error_offset = position
                .filter(|p| {
                    source.position_maps_directly
                        && source.session_generation == self.session_generation
                        && p.byte_offset <= source.submitted_sql.len()
                        && source.submitted_sql.is_char_boundary(p.byte_offset)
                })
                .map(|p| source.source_range.start + p.byte_offset);
        }
    }
    pub(crate) fn reveal_execution_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &self.tabs[self.active_tab];
        let Some(source) = self.execution_source(tab.id) else {
            return;
        };
        let Some(offset) = source.error_offset else {
            return;
        };
        if source.editor_snapshot != tab.editor.read(cx).value()
            || source.session_generation != self.session_generation
            || self
                .profile
                .as_ref()
                .is_none_or(|p| p.id != source.connection_id || p.engine != source.engine)
        {
            self.fail("SQL changed since this execution; error navigation is unavailable.".into());
            cx.notify();
            return;
        }
        let end = source.editor_snapshot[offset..]
            .chars()
            .next()
            .map_or(offset, |c| offset + c.len_utf8());
        tab.editor.update(cx, |e, cx| {
            e.set_selected_range(offset..end, cx);
            e.focus(window, cx);
        });
        cx.notify();
    }
    pub(crate) fn reveal_current_execution(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let range = match self.current_execution_range(cx) {
            Ok(range) => range,
            Err(error) => error.offset..error.offset,
        };
        if range.end <= self.tabs[self.active_tab].editor.read(cx).value().len() {
            self.tabs[self.active_tab].editor.update(cx, |e, cx| {
                e.set_selected_range(range, cx);
                e.focus(window, cx);
            });
            cx.notify();
        }
    }
    pub(crate) fn render_execution_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[self.active_tab];
        let editor = tab.editor.read(cx);
        let current = self.current_execution_range(cx);
        let text = match &current {
            Ok(range) => format!("Current: {}", range_label(&editor.value(), range)),
            Err(error) => format!(
                "Current unavailable: {} · use an explicit Selection or All",
                error.message
            ),
        };
        let source = self.execution_source(tab.id);
        let last = source.map(|s| {
            format!(
                "Last {:?}: {} · {}",
                s.scope,
                range_label(&s.editor_snapshot, &s.source_range),
                s.started_at.format("%H:%M:%S")
            )
        });
        let error_available = source.is_some_and(|s| {
            s.error_offset.is_some()
                && s.editor_snapshot == editor.value()
                && s.session_generation == self.session_generation
        });
        crate::view::row()
            .id("execution-scope")
            .debug_selector(|| "execution-scope".into())
            .flex_wrap()
            .px_3()
            .py_1()
            .gap_2()
            .child(crate::view::label(text, cx))
            .child(
                crate::view::action(
                    "reveal-current-sql",
                    if current.is_ok() {
                        "Reveal current"
                    } else {
                        "Locate issue"
                    },
                    false,
                    !self.modal_open(),
                    cx,
                    |s, w, cx| s.reveal_current_execution(w, cx),
                )
                .debug_selector(|| "reveal-current-sql".into()),
            )
            .when_some(last, |d, last| d.child(crate::view::label(last, cx)))
            .when(source.is_some_and(|s| s.error_offset.is_some()), |d| {
                d.child(
                    crate::view::action(
                        "goto-query-error",
                        "Go to error",
                        false,
                        error_available && !self.modal_open(),
                        cx,
                        |s, w, cx| s.reveal_execution_error(w, cx),
                    )
                    .debug_selector(|| "goto-query-error".into()),
                )
            })
            .into_any_element()
    }
}
