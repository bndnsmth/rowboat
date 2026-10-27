//! Native read-only pin grid and mapped exact comparison. All actions are local.
use crate::{
    app::{Page, Workbench},
    forms,
    result_snapshots::{ResultOrigin, ResultsView},
    view::{action as workbench_action, column, label, row},
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::{
    Cell, QueryEvent, QueryResult,
    result_compare::{ColumnPair, Mapping, result_bytes},
};

const ROW_PAGE: usize = 20;
const COLUMN_PAGE: usize = 4;
const MAP_PAGE: usize = 12;
const CELL_WIDTH: f32 = 200.;
const ROW_LABEL_WIDTH: f32 = 56.;

// Result controls may contain user-provided names. Never let their text paint
// outside the control or let flexbox squeeze it into an adjacent control.
fn action(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    cx: &mut Context<Workbench>,
    handler: impl Fn(&mut Workbench, &mut Window, &mut Context<Workbench>) + 'static,
) -> ui::Button {
    workbench_action(id, text, primary, enabled, cx, handler)
        .min_w_0()
        .max_w_full()
        .flex_shrink_0()
        .overflow_hidden()
}

struct BoundedSource(String);
impl std::fmt::Write for BoundedSource {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.0.len().saturating_add(value.len()) > 1024 * 1024 {
            return Err(std::fmt::Error);
        }
        self.0.push_str(value);
        Ok(())
    }
}

/// Bounded presentation only. Exact clipboard/inspection still uses the raw cell.
fn preview(cell: &Cell) -> String {
    match cell {
        Cell::Null => "SQL NULL".into(),
        Cell::Text(s) if s.is_empty() => "Text: \"\"".into(),
        Cell::Text(s) => {
            let text: String = s.chars().take(160).collect();
            format!(
                "Text: {text}{}",
                if s.chars().take(161).count() > 160 {
                    "…"
                } else {
                    ""
                }
            )
        }
        Cell::Binary(b) => {
            use std::fmt::Write;
            let mut out = format!("Binary ({} bytes): 0x", b.len());
            for byte in b.iter().take(48) {
                let _ = write!(out, "{byte:02x}");
            }
            if b.len() > 48 {
                out.push('…');
            }
            out
        }
    }
}

impl Workbench {
    pub fn pinned_result(&self) -> Option<&QueryResult> {
        self.results.selected_snapshot().map(|s| &s.result)
    }
    pub fn results_read_only(&self) -> bool {
        self.results.view != ResultsView::Live
    }
    fn make_result_origin(&self, source: String, table_page: bool) -> Option<ResultOrigin> {
        let p = self.profile.as_ref()?;
        Some(ResultOrigin {
            route: self.tx.route,
            connection_id: p.id.clone(),
            connection_name: p.name.clone(),
            database: p.database.clone(),
            engine: p.engine,
            started_at: chrono::Utc::now().to_rfc3339(),
            source,
            table_page,
        })
    }
    pub fn capture_query_result_origin(&mut self, tab: usize, execution_sql: &str) {
        self.results.forget_query(tab);
        // Source capture must itself fit the shared allowance. Do not silently
        // truncate SQL and later imply it is the exact execution source.
        let available = self.available_results_bytes(Some(tab)).unwrap_or(0);
        if execution_sql.len().saturating_add(4096) <= available
            && execution_sql.len() <= 1024 * 1024
            && let Some(origin) = self.make_result_origin(execution_sql.into(), false)
        {
            self.results.query_origins.insert(tab, origin);
        }
        self.results.stream_remaining =
            Some((tab, self.available_results_bytes(Some(tab)).unwrap_or(0)));
        self.results.view = ResultsView::Live;
    }
    pub fn capture_table_result_origin(&mut self) {
        use std::fmt::Write;
        self.results.table_origin = None;
        if let Some(table) = &self.table {
            let mut source = BoundedSource(String::new());
            if write!(&mut source, "Table {:?}.{:?}\nBrowse request: {:?}\nFilter: {:?}\nGenerated SQL is not exposed by this adapter.", table.schema, table.name, self.browse_request, self.filter_expression()).is_ok()
                && self.available_results_bytes(None).is_ok_and(|available| source.0.capacity().saturating_add(4096) <= available)
            {
                self.results.table_origin = self.make_result_origin(source.0, true);
            }
        }
        self.results.view = ResultsView::Live;
    }
    pub fn active_results_bytes(
        &self,
        replacing_tab: Option<usize>,
        replacing_table: bool,
    ) -> usize {
        self.tabs
            .iter()
            .filter(|t| Some(t.id) != replacing_tab)
            .flat_map(|t| &t.results)
            .fold(
                if replacing_table {
                    0
                } else {
                    result_bytes(&self.table_result)
                },
                |n, r| n.saturating_add(result_bytes(r)),
            )
    }
    /// Same configurable 16–512 MiB workspace allowance for active results, pins,
    /// and comparison allocations. QueryStreamOptions::byte_limit must be at most
    /// this / 2 (Vec capacity headroom), with the existing editing cap retained.
    pub fn available_results_bytes(&self, replacing_tab: Option<usize>) -> anyhow::Result<usize> {
        Ok(self
            .state
            .preferences
            .query_result_limit_bytes()?
            .saturating_sub(self.results.retained_bytes())
            .saturating_sub(self.active_results_bytes(replacing_tab, false)))
    }
    /// Mandatory ingress check: stream byte_limit accounts logical rows but not
    /// schemas or Vec spare capacity. On false: cancel, skip frame, mark truncated
    /// and report shared-budget exhaustion; never silently claim complete output.
    pub fn result_frame_fits(&mut self, tab: usize, event: &QueryEvent) -> bool {
        let extra = match event {
            QueryEvent::Schema { columns, .. } => {
                4 * std::mem::size_of::<QueryResult>()
                    + columns.capacity() * std::mem::size_of::<rowboat_core::Column>()
                    + columns
                        .iter()
                        .map(|c| {
                            c.name.capacity()
                                + c.data_type.capacity()
                                + c.default_value.as_ref().map_or(0, String::capacity)
                        })
                        .sum::<usize>()
            }
            QueryEvent::Rows { rows, result_index } => {
                let current = self
                    .tabs
                    .iter()
                    .find(|t| t.id == tab)
                    .and_then(|t| t.results.get(*result_index));
                let (len, capacity) = current.map_or((0, 0), |r| (r.rows.len(), r.rows.capacity()));
                // Account the new outer allocation at peak, before the old buffer
                // is freed, rather than assuming growth is only this batch size.
                let allocation = if len.saturating_add(rows.len()) > capacity {
                    capacity
                        .saturating_mul(2)
                        .max(len.saturating_add(rows.len()))
                        .max(4)
                        .saturating_mul(std::mem::size_of::<Vec<Cell>>())
                } else {
                    0
                };
                rows.iter().fold(allocation, |n, r| {
                    n.saturating_add(
                        r.capacity() * std::mem::size_of::<Cell>()
                            + r.iter()
                                .map(|c| match c {
                                    Cell::Null => 0,
                                    Cell::Text(s) => s.capacity(),
                                    Cell::Binary(b) => b.capacity(),
                                })
                                .sum::<usize>(),
                    )
                })
            }
            _ => return true,
        };
        let available = self
            .results
            .stream_remaining
            .filter(|(owner, _)| *owner == tab)
            .map_or_else(
                || self.available_results_bytes(None).unwrap_or(0),
                |(_, bytes)| bytes,
            );
        if extra > available {
            return false;
        }
        self.results.stream_remaining = Some((tab, available - extra));
        true
    }
    pub fn table_result_fits(&self, result: &QueryResult) -> bool {
        self.state
            .preferences
            .query_result_limit_bytes()
            .is_ok_and(|limit| {
                self.results
                    .retained_bytes()
                    .saturating_add(self.active_results_bytes(None, true))
                    .saturating_add(result_bytes(result))
                    <= limit
            })
    }
    pub fn poll_result_comparison(&mut self, cx: &mut Context<Self>) {
        if self.results.poll(self.tx.route) {
            cx.notify();
        }
    }
    pub fn pin_current_result(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.close_requested || !matches!(self.page, Page::Query | Page::Table) {
            return;
        }
        let outcome = (|| {
            let (origin, index, partial, completeness) = if self.page == Page::Table {
                (
                    self.results.table_origin.clone(),
                    0,
                    true,
                    "Table page only · retained database values; staged changes excluded"
                        .to_owned(),
                )
            } else {
                let t = &self.tabs[self.active_tab];
                let r = t.results.get(t.result_index).ok_or("No retained result")?;
                let p = crate::query_stream_ui::statement_presentation(
                    t.query_progress.as_ref(),
                    t.result_index,
                    r,
                    false,
                    t.metadata_incomplete.contains(&t.result_index),
                );
                (
                    self.results.query_origins.get(&t.id).cloned(),
                    t.result_index,
                    !p.complete || r.truncated,
                    if p.notice.is_empty() {
                        "Completed SQL result · retained rows only".into()
                    } else {
                        p.notice
                    },
                )
            };
            let origin = origin.ok_or(
                "Exact execution provenance is unavailable (source exceeds 1 MiB, shared budget exhausted, or result predates capture); free space and run/load again before pinning",
            )?;
            if origin.route != self.tx.route {
                return Err("Result belongs to a previous connection session".into());
            }
            let available = self
                .available_results_bytes(None)
                .map_err(|e| e.to_string())?;
            // Borrow disjoint fields: avoid a temporary duplicate before admission.
            let result = if self.page == Page::Table {
                &self.table_result
            } else {
                let t = &self.tabs[self.active_tab];
                t.results.get(t.result_index).ok_or("No retained result")?
            };
            if result.columns.is_empty() {
                return Err("This statement has no retained column schema to pin".into());
            }
            self.results.pin_result(
                result,
                origin,
                index,
                completeness,
                partial || result.truncated,
                available,
            )?;
            self.selected = None;
            Ok::<(), String>(())
        })();
        if let Err(e) = outcome {
            self.results.error = Some(e);
        }
        cx.notify();
    }
    fn choose_comparison_pin(&mut self, id: u64, left: bool) {
        self.results.cancel();
        self.results.compared = None;
        if left {
            self.results.left = Some(id);
        } else {
            self.results.right = Some(id);
        }
        self.results.mapping = Mapping::default();
        self.results.selected_column = 0;
        self.results.show_mapping = true;
        let (Some(l), Some(r)) = (
            self.results.left.and_then(|id| self.results.pin(id)),
            self.results.right.and_then(|id| self.results.pin(id)),
        ) else {
            return;
        };
        let mut mapping = Mapping::default();
        for (i, c) in l.snapshot.result.columns.iter().enumerate() {
            let matching: Vec<_> = r
                .snapshot
                .result
                .columns
                .iter()
                .enumerate()
                .filter(|(_, rc)| rc.name == c.name)
                .collect();
            if matching.len() == 1
                && l.snapshot
                    .result
                    .columns
                    .iter()
                    .filter(|lc| lc.name == c.name)
                    .count()
                    == 1
            {
                let pair = ColumnPair {
                    left: i,
                    right: matching[0].0,
                };
                mapping.columns.push(pair);
                if c.primary_key && matching[0].1.primary_key {
                    mapping.keys.push(pair);
                }
            }
        }
        self.results.mapping = mapping;
    }
    fn start_result_comparison(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.close_requested {
            return;
        }
        self.results.compared = None;
        match self
            .available_results_bytes(None)
            .map_err(|e| e.to_string())
            .and_then(|available| self.results.start(self.tx.route, available, &self.runtime))
        {
            Ok(()) => self.selected = None,
            Err(e) => self.results.error = Some(e),
        }
        cx.notify();
    }
    pub fn render_results_tools(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut tools = column()
            .id("result-tools")
            .debug_selector(|| "result-tools".into())
            .max_h(px(if self.results.show_mapping {
                280.
            } else {
                128.
            }))
            .flex_shrink_0()
            .overflow_hidden()
            .overflow_y_scroll()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(cx.omarchy().border)
            .child(
                row()
                    .flex_shrink_0()
                    .flex_wrap()
                    .child(action(
                        "results-live",
                        "Live result",
                        self.results.view == ResultsView::Live,
                        true,
                        cx,
                        |s, _, cx| {
                            s.results.view = ResultsView::Live;
                            s.selected = None;
                            cx.notify();
                        },
                    ))
                    .child(
                        action(
                            "results-pin",
                            "Pin database result",
                            false,
                            !self.busy && !self.close_requested && self.current_result().is_some(),
                            cx,
                            |s, _, cx| s.pin_current_result(cx),
                        )
                        .debug_selector(|| "results-pin".into()),
                    )
                    .child(
                        action(
                            "results-map",
                            "Map / compare pins",
                            self.results.show_mapping,
                            self.results.pins.len() >= 2,
                            cx,
                            |s, _, cx| {
                                s.results.show_mapping = !s.results.show_mapping;
                                if s.results.mapping.columns.is_empty()
                                    && let Some(id) = s.results.left
                                {
                                    s.choose_comparison_pin(id, true);
                                }
                                cx.notify();
                            },
                        )
                        .debug_selector(|| "results-map".into()),
                    )
                    .when(self.results.pending(), |tools| {
                        tools.child(action(
                            "results-cancel-active",
                            "Cancel comparison",
                            false,
                            true,
                            cx,
                            |s, _, cx| {
                                s.results.cancel();
                                s.results.error = Some("Comparison cancelled".into());
                                cx.notify();
                            },
                        ))
                    })
                    .when(self.results.compared.is_some(), |tools| {
                        tools.child(action(
                            "results-close-comparison",
                            "Close comparison",
                            false,
                            true,
                            cx,
                            |s, _, cx| {
                                s.results.close_comparison();
                                cx.notify();
                            },
                        ))
                    })
                    .child(label(
                        format!(
                            "{} pins · {} MiB pinned/comparison · memory only",
                            self.results.pins.len(),
                            self.results.retained_bytes() / (1024 * 1024)
                        ),
                        cx,
                    )),
            );
        // Mapping controls precede the potentially long pin list, so Compare
        // remains reachable in short windows without scrolling past every pin.
        if self.results.show_mapping {
            tools = tools.child(self.render_result_mapping(cx));
        }
        let pins: Vec<_> = self
            .results
            .pins
            .iter()
            .map(|p| (p.snapshot.id, p.name.clone(), p.snapshot.partial))
            .collect();
        for (id, name, partial) in pins {
            tools = tools.child(
                row()
                    .flex_shrink_0()
                    .flex_wrap()
                    .child(action(
                        ("result-pin", id as usize),
                        format!("{name}{}", if partial { " · partial" } else { "" }),
                        self.results.view == ResultsView::Pin(id),
                        true,
                        cx,
                        move |s, w, cx| {
                            s.results.view = ResultsView::Pin(id);
                            s.results.selected_row = 0;
                            s.results.selected_column = 0;
                            s.selected = None;
                            w.focus(&s.grid_focus, cx);
                            cx.notify();
                        },
                    ))
                    .child(action(
                        ("result-left", id as usize),
                        "Left",
                        self.results.left == Some(id),
                        true,
                        cx,
                        move |s, _, cx| {
                            s.choose_comparison_pin(id, true);
                            cx.notify();
                        },
                    ))
                    .child(action(
                        ("result-right", id as usize),
                        "Right",
                        self.results.right == Some(id),
                        true,
                        cx,
                        move |s, _, cx| {
                            s.choose_comparison_pin(id, false);
                            cx.notify();
                        },
                    ))
                    .child(
                        action(
                            ("result-rename", id as usize),
                            "Name",
                            false,
                            true,
                            cx,
                            move |s, w, cx| {
                                if let Some(p) = s.results.pin(id) {
                                    s.results.rename = Some((
                                        id,
                                        forms::input(p.name.clone(), "Snapshot name", w, cx),
                                    ));
                                }
                                cx.notify();
                            },
                        )
                        .debug_selector(move || format!("result-rename-{id}")),
                    )
                    .child(
                        action(
                            ("result-close", id as usize),
                            "Close pin",
                            false,
                            true,
                            cx,
                            move |s, _, cx| {
                                s.results.close_pin(id);
                                cx.notify();
                            },
                        )
                        .debug_selector(move || format!("result-close-{id}")),
                    ),
            );
        }
        if let Some((id, input)) = self.results.rename.clone() {
            tools = tools.child(
                row()
                    .flex_wrap()
                    .child(div().w(px(240.)).child(ui::input(
                        "result-pin-name",
                        &input,
                        window,
                        cx,
                    )))
                    .child(
                        action(
                            "result-save-name",
                            "Save name",
                            true,
                            true,
                            cx,
                            move |s, _, cx| {
                                if let Some((_, value)) = &s.results.rename {
                                    let name = value.read(cx).value().to_string();
                                    if let Err(e) = s.results.rename_pin(id, &name) {
                                        s.results.error = Some(e);
                                    }
                                }
                                cx.notify();
                            },
                        )
                        .debug_selector(|| "result-save-name".into()),
                    )
                    .child(action(
                        "result-cancel-name",
                        "Cancel",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            s.results.rename = None;
                            cx.notify();
                        },
                    )),
            );
        }
        if let Some(error) = &self.results.error {
            tools = tools.child(div().text_color(cx.omarchy().warning).child(error.clone()));
        }
        tools.into_any_element()
    }
    fn render_result_mapping(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (Some(left), Some(right)) = (
            self.results.left.and_then(|id| self.results.pin(id)),
            self.results.right.and_then(|id| self.results.pin(id)),
        ) else {
            return label("Choose Left and Right pins above", cx).into_any_element();
        };
        let (ln, rn) = (left.name.clone(), right.name.clone());
        let lcols = left.snapshot.result.columns.clone();
        let rcols = right.snapshot.result.columns.clone();
        let offset = (self.results.selected_column / MAP_PAGE) * MAP_PAGE;
        let body = column().flex_shrink_0().gap_1().child(
            label(
                format!("{ln} → {rn} · choose keys; click targets to cycle (Off excludes)"),
                cx,
            )
            .min_w_0()
            .truncate(),
        );
        let mut mappings = column()
            .id("result-mapping-columns")
            .max_h(px(100.))
            .flex_shrink_0()
            .overflow_y_scroll()
            .gap_1();
        for (i, c) in lcols.iter().enumerate().skip(offset).take(MAP_PAGE) {
            let pair = self
                .results
                .mapping
                .columns
                .iter()
                .find(|p| p.left == i)
                .copied();
            let key = self.results.mapping.keys.iter().any(|p| p.left == i);
            let target = pair
                .and_then(|p| {
                    rcols
                        .get(p.right)
                        .map(|c| format!("{}: {} ({})", p.right + 1, c.name, c.data_type))
                })
                .unwrap_or("Off".into());
            let count = rcols.len();
            mappings = mappings.child(
                row()
                    .flex_shrink_0()
                    .flex_wrap()
                    .child(
                        label(format!("{}: {} ({})", i + 1, c.name, c.data_type), cx)
                            .w(px(200.))
                            .min_w_0()
                            .truncate(),
                    )
                    .child(
                        action(
                            ("result-map-target", i),
                            format!("→ {target}"),
                            false,
                            count > 0 && !self.results.pending(),
                            cx,
                            move |s, _, cx| {
                                s.results.compared = None;
                                let next = pair.map_or(Some(0), |p| {
                                    (p.right + 1 < count).then_some(p.right + 1)
                                });
                                s.results.mapping.columns.retain(|p| p.left != i);
                                s.results.mapping.keys.retain(|p| p.left != i);
                                if let Some(right) = next {
                                    let p = ColumnPair { left: i, right };
                                    s.results.mapping.columns.push(p);
                                    if key {
                                        s.results.mapping.keys.push(p);
                                    }
                                }
                                cx.notify();
                            },
                        )
                        .w(px(240.))
                        .min_w_0(),
                    )
                    .child(action(
                        ("result-map-key", i),
                        if key { "☑ Key" } else { "☐ Key" },
                        key,
                        pair.is_some() && !self.results.pending(),
                        cx,
                        move |s, _, cx| {
                            s.results.compared = None;
                            s.results.mapping.keys.retain(|p| p.left != i);
                            if !key && let Some(p) = pair {
                                s.results.mapping.keys.push(p);
                            }
                            cx.notify();
                        },
                    )),
            );
        }
        body.child(
            row()
                .flex_wrap()
                .child(action(
                    "map-prev-columns",
                    "Previous mappings",
                    false,
                    offset > 0,
                    cx,
                    |s, _, cx| {
                        s.results.selected_column =
                            s.results.selected_column.saturating_sub(MAP_PAGE);
                        cx.notify();
                    },
                ))
                .child(action(
                    "map-next-columns",
                    "Next mappings",
                    false,
                    offset + MAP_PAGE < lcols.len(),
                    cx,
                    |s, _, cx| {
                        s.results.selected_column += MAP_PAGE;
                        cx.notify();
                    },
                ))
                .child(
                    action(
                        "result-compare",
                        "Compare retained rows",
                        true,
                        !self.busy && !self.results.pending(),
                        cx,
                        |s, _, cx| s.start_result_comparison(cx),
                    )
                    .debug_selector(|| "result-compare".into()),
                )
                .child(action(
                    "result-cancel-compare",
                    "Cancel comparison",
                    false,
                    self.results.pending(),
                    cx,
                    |s, _, cx| {
                        s.results.cancel();
                        s.results.error = Some("Comparison cancelled".into());
                        cx.notify();
                    },
                )),
        )
        .child(mappings)
        .into_any_element()
    }
    fn results_move(&mut self, rows: isize, columns: isize) {
        let (nr, nc) = match self.results.view {
            ResultsView::Pin(_) => self
                .pinned_result()
                .map_or((0, 0), |r| (r.rows.len(), r.columns.len())),
            ResultsView::Comparison => self
                .results
                .compared
                .as_ref()
                .map_or((0, 0), |c| (c.diff.rows.len(), c.mapping.columns.len())),
            ResultsView::Live => (0, 0),
        };
        self.results.selected_row = self
            .results
            .selected_row
            .saturating_add_signed(rows)
            .min(nr.saturating_sub(1));
        self.results.selected_column = self
            .results
            .selected_column
            .saturating_add_signed(columns)
            .min(nc.saturating_sub(1));
    }
    fn copy_result_snapshot_cell(&self, right: bool, cx: &mut Context<Self>) {
        let value = match self.results.view {
            ResultsView::Pin(_) => self
                .pinned_result()
                .and_then(|r| r.rows.get(self.results.selected_row))
                .and_then(|r| r.get(self.results.selected_column)),
            ResultsView::Comparison => self.results.compared.as_ref().and_then(|c| {
                let d = c.diff.rows.get(self.results.selected_row)?;
                let p = c.mapping.columns.get(self.results.selected_column)?;
                if right {
                    c.right.result.rows.get(d.right?)?.get(p.right)
                } else {
                    c.left.result.rows.get(d.left?)?.get(p.left)
                }
            }),
            ResultsView::Live => None,
        };
        if let Some(value) = value {
            cx.write_to_clipboard(ClipboardItem::new_string(value.display()));
        }
    }
    pub fn render_results_comparison(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.results.view == ResultsView::Live {
            return None;
        }
        let theme = cx.omarchy().clone();
        // The workbench places this inside a clipped, non-flex results wrapper.
        // flex_1 alone sizes to content there and leaves nothing to scroll.
        // Explicit percentage sizing makes this the bounded scrolling viewport.
        let mut body = column()
            .id("result-snapshot-panel")
            .debug_selector(|| "result-snapshot-panel".into())
            .size_full()
            .min_h_0()
            .flex_1()
            .overflow_hidden()
            .overflow_y_scroll()
            .track_scroll(&self.results.viewport_scroll)
            .gap_2()
            .p_2()
            .bg(theme.background)
            .track_focus(&self.grid_focus)
            .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                match e.keystroke.key.as_str() {
                    "up" => s.results_move(-1, 0),
                    "down" => s.results_move(1, 0),
                    "left" => s.results_move(0, -1),
                    "right" => s.results_move(0, 1),
                    "pageup" => s.results_move(-(ROW_PAGE as isize), 0),
                    "pagedown" => s.results_move(ROW_PAGE as isize, 0),
                    "escape" => s.results.view = ResultsView::Live,
                    "c" if e.keystroke.modifiers.control => s.copy_result_snapshot_cell(false, cx),
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(
                label(
                    "READ ONLY · arrows select · Page Up/Down · Ctrl+C exact original · Esc live",
                    cx,
                )
                .flex_shrink_0()
                .min_w_0()
                .truncate(),
            )
            .child(
                row()
                    .flex_shrink_0()
                    .flex_wrap()
                    .child(action(
                        "snapshot-prev-row",
                        "Previous row",
                        false,
                        self.results.selected_row > 0,
                        cx,
                        |s, _, cx| {
                            s.results_move(-1, 0);
                            cx.notify();
                        },
                    ))
                    .child(action(
                        "snapshot-next-row",
                        "Next row",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            s.results_move(1, 0);
                            cx.notify();
                        },
                    ))
                    .child(action(
                        "snapshot-prev-field",
                        "Previous column",
                        false,
                        self.results.selected_column > 0,
                        cx,
                        |s, _, cx| {
                            s.results_move(0, -1);
                            cx.notify();
                        },
                    ))
                    .child(action(
                        "snapshot-next-field",
                        "Next column",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            s.results_move(0, 1);
                            cx.notify();
                        },
                    ))
                    .child(
                        action(
                            "snapshot-copy-left",
                            "Copy exact / left",
                            false,
                            true,
                            cx,
                            |s, _, cx| s.copy_result_snapshot_cell(false, cx),
                        )
                        .debug_selector(|| "snapshot-copy-left".into()),
                    )
                    .child(action(
                        "snapshot-copy-right",
                        "Copy exact right",
                        false,
                        self.results.view == ResultsView::Comparison,
                        cx,
                        |s, _, cx| s.copy_result_snapshot_cell(true, cx),
                    )),
            );
        if let Some(snapshot) = self.results.selected_snapshot() {
            let origin = &snapshot.origin;
            let text = format!(
                "{:?} · {} [{}] · database {} · result {} · execution {} · pinned {} · {} ms\n{} · {} retained rows",
                origin.engine,
                origin.connection_name,
                origin.connection_id,
                origin.database,
                snapshot.result_index + 1,
                origin.started_at,
                snapshot.captured_at,
                snapshot.result.elapsed_ms,
                snapshot.completeness,
                snapshot.result.rows.len()
            );
            body = body
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex_shrink_0()
                        .overflow_hidden()
                        .text_color(if snapshot.partial {
                            theme.warning
                        } else {
                            theme.secondary
                        })
                        .child(text),
                )
                .child(action(
                    "snapshot-source",
                    if self.results.show_source {
                        "Hide execution source"
                    } else {
                        "Show execution source"
                    },
                    false,
                    true,
                    cx,
                    |s, _, cx| {
                        s.results.show_source = !s.results.show_source;
                        cx.notify();
                    },
                ));
            if self.results.show_source {
                body = body.child(div().whitespace_normal().child(origin.source.chars().take(12000).collect::<String>()))
                    .child(label("Source display capped at 12,000 characters; full captured source remains in memory", cx))
                    .child(action("snapshot-copy-source", "Copy exact execution source", false, true, cx, |s, _, cx| {
                        if let Some(snapshot) = s.results.selected_snapshot() {
                            cx.write_to_clipboard(ClipboardItem::new_string(snapshot.origin.source.clone()));
                        }
                    }));
            }
            let r = &snapshot.result;
            let row_start = self.results.selected_row / ROW_PAGE * ROW_PAGE;
            let col_start = self.results.selected_column / COLUMN_PAGE * COLUMN_PAGE;
            body = body.child(
                label(
                    format!(
                        "Rows {}–{} / {} · columns {}–{} / {}",
                        if r.rows.is_empty() { 0 } else { row_start + 1 },
                        (row_start + ROW_PAGE).min(r.rows.len()),
                        r.rows.len(),
                        col_start + 1,
                        (col_start + COLUMN_PAGE).min(r.columns.len()),
                        r.columns.len()
                    ),
                    cx,
                )
                .flex_shrink_0(),
            );
            let shown_columns = r.columns.len().saturating_sub(col_start).min(COLUMN_PAGE);
            let grid_width = ROW_LABEL_WIDTH + shown_columns as f32 * (CELL_WIDTH + 8.);
            let mut grid = column()
                .w(px(grid_width))
                .min_w(px(grid_width))
                .flex_shrink_0()
                .gap_2();
            for (ri, cells) in r.rows.iter().enumerate().skip(row_start).take(ROW_PAGE) {
                let mut line = row()
                    .w(px(grid_width))
                    .min_w(px(grid_width))
                    .h(px(60.))
                    .flex_shrink_0()
                    .items_start()
                    .child(
                        label(format!("Row {}", ri + 1), cx)
                            .w(px(ROW_LABEL_WIDTH))
                            .min_w(px(ROW_LABEL_WIDTH))
                            .flex_shrink_0()
                            .truncate(),
                    );
                for (ci, c) in r
                    .columns
                    .iter()
                    .enumerate()
                    .skip(col_start)
                    .take(COLUMN_PAGE)
                {
                    let value = cells.get(ci).map(preview).unwrap_or("Missing value".into());
                    line = line.child(
                        action(
                            ("snapshot-cell", ri * r.columns.len() + ci),
                            "",
                            self.results.selected_row == ri && self.results.selected_column == ci,
                            true,
                            cx,
                            move |s, w, cx| {
                                s.results.selected_row = ri;
                                s.results.selected_column = ci;
                                w.focus(&s.grid_focus, cx);
                                cx.notify();
                            },
                        )
                        .accessibility_label(format!(
                            "Row {}, column {}: {} · {}",
                            ri + 1,
                            ci + 1,
                            c.name,
                            value
                        ))
                        .debug_selector(move || format!("snapshot-cell-{ri}-{ci}"))
                        .w(px(CELL_WIDTH))
                        .min_w(px(CELL_WIDTH))
                        .max_w(px(CELL_WIDTH))
                        .h(px(60.))
                        .justify_start()
                        .child(
                            column()
                                .w_full()
                                .min_w_0()
                                .overflow_hidden()
                                .gap_1()
                                .child(div().w_full().min_w_0().truncate().child(format!(
                                    "{} · {}",
                                    ci + 1,
                                    c.name
                                )))
                                .child(
                                    div()
                                        .id(("snapshot-value", ri * r.columns.len() + ci))
                                        .debug_selector(move || format!("snapshot-value-{ri}-{ci}"))
                                        .w_full()
                                        .min_w_0()
                                        .truncate()
                                        .child(value),
                                ),
                        ),
                    );
                }
                grid = grid.child(line);
            }
            body = body.child(
                div()
                    .relative()
                    .w_full()
                    .flex_shrink_0()
                    .pb(px(12.))
                    .child(
                        div()
                            .id("snapshot-grid-scroll")
                            .debug_selector(|| "snapshot-grid-scroll".into())
                            .w_full()
                            .min_w_0()
                            .flex_shrink_0()
                            .overflow_x_scroll()
                            .track_scroll(&self.results.horizontal_scroll)
                            .child(grid),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(12.))
                            .debug_selector(|| "snapshot-scrollbar-x".into())
                            .child(
                                ui::scrollbar(
                                    "snapshot-scroll-x",
                                    gpui_kit::base::ScrollbarAxis::Horizontal,
                                    &self.results.horizontal_scroll,
                                    window,
                                    cx,
                                )
                                .viewport_from_layout(),
                            ),
                    ),
            );
        } else if let Some(c) = &self.results.compared {
            body = body.child(div().flex_shrink_0().text_color(if c.diff.partial { theme.warning } else { theme.secondary }).child(format!("{} added · {} removed · {} changed · {} unchanged — mapped columns of retained rows ONLY{}", c.diff.added, c.diff.removed, c.diff.changed, c.diff.unchanged, if c.diff.partial { " · PARTIAL inputs (page, interrupted or retention-limited)" } else { "" })))
                .child(label(format!("Left [{} / {} / {}] → Right [{} / {} / {}]", c.left.origin.connection_name, c.left.origin.database, c.left.origin.started_at, c.right.origin.connection_name, c.right.origin.database, c.right.origin.started_at), cx).flex_shrink_0());
            if let Some(d) = c.diff.rows.get(self.results.selected_row) {
                body = body.child(
                    label(
                        format!(
                            "Difference {} / {} · {:?} · left row {:?} / right row {:?}",
                            self.results.selected_row + 1,
                            c.diff.rows.len(),
                            d.kind,
                            d.left.map(|i| i + 1),
                            d.right.map(|i| i + 1)
                        ),
                        cx,
                    )
                    .flex_shrink_0(),
                );
                let start = self.results.selected_column / MAP_PAGE * MAP_PAGE;
                for (i, p) in c
                    .mapping
                    .columns
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(MAP_PAGE)
                {
                    let lv = d
                        .left
                        .and_then(|r| c.left.result.rows.get(r))
                        .and_then(|r| r.get(p.left));
                    let rv = d
                        .right
                        .and_then(|r| c.right.result.rows.get(r))
                        .and_then(|r| r.get(p.right));
                    let changed = lv != rv;
                    body = body.child(
                        column()
                            .w_full()
                            .flex_shrink_0()
                            .overflow_hidden()
                            .gap_1()
                            .p_2()
                            .border_1()
                            .border_color(if changed { theme.warning } else { theme.border })
                            .child(
                                action(
                                    ("difference-column", i),
                                    format!(
                                        "{} ↔ {}{}",
                                        c.left.result.columns[p.left].name,
                                        c.right.result.columns[p.right].name,
                                        if changed {
                                            " · DIFFERENT"
                                        } else {
                                            " · equal"
                                        }
                                    ),
                                    self.results.selected_column == i,
                                    true,
                                    cx,
                                    move |s, w, cx| {
                                        s.results.selected_column = i;
                                        w.focus(&s.grid_focus, cx);
                                        cx.notify();
                                    },
                                )
                                .debug_selector(move || format!("difference-column-{i}")),
                            )
                            .child(div().w_full().min_w_0().truncate().child(format!(
                                "Left: {}",
                                lv.map(preview).unwrap_or("ROW ABSENT".into())
                            )))
                            .child(div().w_full().min_w_0().truncate().child(format!(
                                "Right: {}",
                                rv.map(preview).unwrap_or("ROW ABSENT".into())
                            ))),
                    );
                }
            } else {
                body = body.child("No differences in mapped columns of the retained rows. This is not a whole-database equality claim.");
            }
        } else {
            body = body.child(if self.results.pending() {
                "Comparing retained rows on CPU… Use Cancel comparison above."
            } else {
                "Choose two pins and key/column mappings, then Compare retained rows."
            });
        }
        Some(
            div()
                .relative()
                .size_full()
                .pr(px(12.))
                .child(body)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(px(12.))
                        .debug_selector(|| "snapshot-scrollbar-y".into())
                        .child(
                            ui::scrollbar(
                                "snapshot-scroll-y",
                                gpui_kit::base::ScrollbarAxis::Vertical,
                                &self.results.viewport_scroll,
                                window,
                                cx,
                            )
                            .viewport_from_layout(),
                        ),
                )
                .into_any_element(),
        )
    }
}
