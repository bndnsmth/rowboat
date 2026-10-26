//! Native pin/comparison controls plus shared-retention admission regressions.
//! Register `mod item3_result_tests;` in the dedicated editor-tests crate.
use crate::{
    app::{Page, Workbench},
    result_snapshots::ResultsView,
};
use gpui_kit::{
    AnyWindowHandle, AppContext, Context, Entity, Modifiers, Render, ScrollDelta, ScrollWheelEvent,
    TestAppContext, VisualTestContext, Window, div, point, prelude::*, px, size,
};
use rowboat_core::{Cell, Column, ConnectionProfile, QueryEvent, QueryResult};

struct ResultsHost {
    workspace: Entity<Workbench>,
}

struct ResultsLayoutHost {
    workspace: Entity<Workbench>,
}
impl Render for ResultsLayoutHost {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |s, cx| {
            let tools = s.render_results_tools(w, cx);
            let panel = s.render_results_comparison(w, cx);
            // Match the assembled workspace: explorer consumes 280px; a docked
            // inspector consumes another 320px inside the results area.
            div()
                .size_full()
                .flex()
                .child(div().w(px(280.)).flex_shrink_0())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(tools)
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .min_h_0()
                                        .flex()
                                        .flex_col()
                                        .children(panel),
                                )
                                .child(div().w(px(320.)).flex_shrink_0()),
                        ),
                )
        })
    }
}
impl Render for ResultsHost {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |s, cx| {
            let tools = s.render_results_tools(w, cx);
            let panel = s.render_results_comparison(w, cx);
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(tools)
                .children(panel)
        })
    }
}
fn result() -> QueryResult {
    QueryResult {
        columns: ["id", "amount"]
            .iter()
            .map(|name| Column {
                name: (*name).into(),
                data_type: "text".into(),
                nullable: false,
                primary_key: *name == "id",
                default_value: None,
            })
            .collect(),
        rows: vec![
            vec![
                Cell::Text("1".into()),
                Cell::Text("9007199254740993.00000000001".into()),
            ],
            vec![Cell::Text("2".into()), Cell::Null],
        ],
        ..Default::default()
    }
}
fn fixture(
    cx: &mut TestAppContext,
    id: u64,
    runtime: tokio::runtime::Handle,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let (w, entity) = crate::workbench_tests::workspace(cx, id, runtime);
    cx.simulate_window_resize(w, size(px(1200.), px(900.)));
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.page = Page::Query;
            s.profile = Some(ConnectionProfile {
                id: "pins-test".into(),
                name: "Pins test".into(),
                database: "fixture".into(),
                ..Default::default()
            });
            s.tabs[0].results = vec![result()];
            s.capture_query_result_origin(s.tabs[0].id, "SELECT original_source");
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("SELECT changed_editor", w, cx));
        });
        w.replace_root(cx, |_, cx| {
            cx.observe(&entity, |_, _, cx| cx.notify()).detach();
            ResultsHost {
                workspace: entity.clone(),
            }
        });
    })
    .unwrap();
    (w, entity)
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut v = VisualTestContext::from_window(w, cx);
    let b = v.debug_bounds(id).unwrap_or_else(|| panic!("Missing {id}"));
    v.simulate_click(b.center(), Modifiers::default());
}

#[gpui_kit::test]
fn item3_actual_pin_name_close_and_exact_copy_use_captured_sql(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = fixture(cx, 331, runtime.handle());
    click(cx, w, "results-pin");
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, cx| {
            assert_eq!(s.results.pins.len(), 1);
            assert_eq!(
                s.results.pins[0].snapshot.origin.source,
                "SELECT original_source"
            );
            assert_eq!(s.query_run_invocations, 0);
            s.results.selected_column = 1;
            w.focus(&s.grid_focus, cx);
        })
    })
    .unwrap();
    click(cx, w, "snapshot-copy-left");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "9007199254740993.00000000001"
        )
    });
    click(cx, w, "result-rename-1");
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, cx| {
            s.results
                .rename
                .as_ref()
                .unwrap()
                .1
                .update(cx, |v, cx| v.set_value("Before λ", w, cx))
        })
    })
    .unwrap();
    click(cx, w, "result-save-name");
    cx.read(|cx| assert_eq!(e.read(cx).results.pins[0].name, "Before λ"));
    click(cx, w, "result-close-1");
    cx.read(|cx| {
        let s = e.read(cx);
        assert!(s.results.pins.is_empty());
        assert_eq!(s.results.view, ResultsView::Live);
        assert_eq!(s.query_run_invocations, 0);
    });
}

#[gpui_kit::test]
fn item3_table_pin_excludes_staged_writes_and_marks_page_partial(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = fixture(cx, 332, runtime.handle());
    cx.update_window(w, |_, _, cx| {
        e.update(cx, |s, cx| {
            s.page = Page::Table;
            s.table = Some(rowboat_core::TableRef {
                schema: "main".into(),
                name: "ledger".into(),
            });
            s.table_result = result();
            s.changes = vec![rowboat_core::RowChange::Update {
                original: vec![("id".into(), Cell::Text("1".into()))],
                values: vec![("amount".into(), Cell::Text("staged".into()))],
            }];
            s.capture_table_result_origin();
            s.pin_current_result(cx);
            let pin = &s.results.pins[0].snapshot;
            assert!(pin.partial);
            assert!(pin.origin.source.contains("ledger"));
            assert_eq!(
                pin.result.rows[0][1],
                Cell::Text("9007199254740993.00000000001".into())
            );
            assert_eq!(s.changes.len(), 1);
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn item3_actual_compare_loaded_pins_changes_one_cell_without_queries(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = fixture(cx, 333, runtime.handle());
    click(cx, w, "results-pin");
    cx.update_window(w, |_, _, cx| {
        e.update(cx, |s, cx| {
            s.tabs[0].results[0].rows.reverse();
            s.tabs[0].results[0].rows[1][1] = Cell::Text("9007199254740993.00000000002".into());
            s.pin_current_result(cx);
        })
    })
    .unwrap();
    click(cx, w, "results-map");
    click(cx, w, "result-compare");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let pending = cx
            .update_window(w, |_, _, cx| {
                e.update(cx, |s, cx| {
                    s.poll_result_comparison(cx);
                    s.results.pending()
                })
            })
            .unwrap();
        if !pending {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, _| {
            let c = s.results.compared.as_ref().unwrap();
            assert_eq!((c.diff.changed, c.diff.unchanged), (1, 1));
            assert_eq!(s.query_run_invocations, 0);
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
}

#[gpui_kit::test]
fn item3_ingress_budget_charges_schema_capacity_and_rejects_before_retention(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = fixture(cx, 334, runtime.handle());
    cx.update_window(w, |_, _, cx| {
        e.update(cx, |s, _| {
            let tab = s.tabs[0].id;
            let schema = QueryEvent::Schema {
                result_index: 0,
                columns: result().columns,
                metadata_complete: true,
            };
            s.results.stream_remaining = Some((tab, 1));
            assert!(!s.result_frame_fits(tab, &schema));
            s.results.stream_remaining = Some((tab, 4096));
            assert!(s.result_frame_fits(tab, &schema));
            let remaining = s.results.stream_remaining.unwrap().1;
            assert!(remaining < 4096);
            let rows = QueryEvent::Rows {
                result_index: 0,
                rows: vec![vec![Cell::Text("large".repeat(2000))]],
            };
            assert!(!s.result_frame_fits(tab, &rows));
            assert_eq!(s.results.stream_remaining.unwrap().1, remaining);
            assert_eq!(s.tabs[0].results[0].rows.len(), 2);
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn item3_narrow_light_dark_pin_keyboard_selection_and_copy(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = fixture(cx, 335, runtime.handle());
    click(cx, w, "results-pin");
    cx.simulate_window_resize(w, size(px(640.), px(900.)));
    for theme in [
        gpui_omarchy::Theme::flexoki_light(),
        gpui_omarchy::Theme::tokyo_night(),
    ] {
        cx.update(|cx| theme.apply(cx));
        cx.update_window(w, |_, w, cx| {
            e.update(cx, |s, cx| {
                s.results.selected_row = 0;
                s.results.selected_column = 0;
                w.focus(&s.grid_focus, cx);
            });
            w.draw(cx).clear(cx);
        })
        .unwrap();
        cx.simulate_keystrokes(w, "right");
        cx.simulate_keystrokes(w, "down");
        cx.simulate_keystrokes(w, "ctrl-c");
        cx.read(|cx| {
            let s = e.read(cx);
            assert_eq!((s.results.selected_row, s.results.selected_column), (1, 1));
        });
        cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "NULL"));
    }
    cx.simulate_keystrokes(w, "escape");
    cx.read(|cx| assert_eq!(e.read(cx).results.view, ResultsView::Live));
}

#[gpui_kit::test]
fn item3_long_json_cells_keep_nonoverlapping_bounds_with_docked_inspector(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, entity) = fixture(cx, 336, runtime.handle());
    let payload = format!(
        r#"{{"name":"Ada Lovelace","settings":{{"exact":9007199254740993,"long":"{}"}}}}"#,
        "雪".repeat(10000)
    );
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let mut loaded = result();
            loaded.columns[1].name = "name".into();
            loaded.columns.push(Column {
                name: "payload".into(),
                data_type: "json".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            });
            loaded.rows = (0..4)
                .map(|i| {
                    vec![
                        Cell::Text((i + 1).to_string()),
                        Cell::Text("Ada Lovelace".into()),
                        Cell::Text(payload.clone()),
                    ]
                })
                .collect();
            s.tabs[0].results = vec![loaded];
            s.pin_current_result(cx);
            s.results.selected_column = 2;
        });
        w.replace_root(cx, |_, cx| {
            cx.observe(&entity, |_, _, cx| cx.notify()).detach();
            ResultsLayoutHost {
                workspace: entity.clone(),
            }
        });
    })
    .unwrap();
    for (width, height) in [(1600., 1000.), (960., 640.)] {
        cx.simulate_window_resize(w, size(px(width), px(height)));
        cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
            .unwrap();
        let mut v = VisualTestContext::from_window(w, cx);
        let tools = v.debug_bounds("result-tools").unwrap();
        assert!(tools.size.height <= px(128.));
        let scroll = v.debug_bounds("snapshot-grid-scroll").unwrap();
        assert!(scroll.size.width <= px(width - 600.));
        let cells = [
            [
                "snapshot-cell-0-0",
                "snapshot-cell-0-1",
                "snapshot-cell-0-2",
            ],
            [
                "snapshot-cell-1-0",
                "snapshot-cell-1-1",
                "snapshot-cell-1-2",
            ],
            [
                "snapshot-cell-2-0",
                "snapshot-cell-2-1",
                "snapshot-cell-2-2",
            ],
            [
                "snapshot-cell-3-0",
                "snapshot-cell-3-1",
                "snapshot-cell-3-2",
            ],
        ];
        let values = [
            [
                "snapshot-value-0-0",
                "snapshot-value-0-1",
                "snapshot-value-0-2",
            ],
            [
                "snapshot-value-1-0",
                "snapshot-value-1-1",
                "snapshot-value-1-2",
            ],
            [
                "snapshot-value-2-0",
                "snapshot-value-2-1",
                "snapshot-value-2-2",
            ],
            [
                "snapshot-value-3-0",
                "snapshot-value-3-1",
                "snapshot-value-3-2",
            ],
        ];
        for row in 0..4 {
            for col in 0..3 {
                let cell = v.debug_bounds(cells[row][col]).unwrap();
                let value = v.debug_bounds(values[row][col]).unwrap();
                assert_eq!(cell.size.width, px(200.));
                assert_eq!(cell.size.height, px(60.));
                assert!(
                    value.left() >= cell.left() && value.right() <= cell.right(),
                    "text escaped its cell: {value:?} / {cell:?}"
                );
                assert!(value.top() >= cell.top() && value.bottom() <= cell.bottom());
                if col > 0 {
                    let previous = v.debug_bounds(cells[row][col - 1]).unwrap();
                    assert!(cell.left() >= previous.right(), "columns overlap");
                }
                if row > 0 {
                    let previous = v.debug_bounds(cells[row - 1][col]).unwrap();
                    assert!(cell.top() >= previous.bottom(), "rows overlap");
                }
            }
        }
    }
    click(cx, w, "snapshot-copy-left");
    cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), payload));
}

#[gpui_kit::test]
fn item3_mapping_controls_remain_above_pin_list_in_short_results_area(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, entity) = fixture(cx, 337, runtime.handle());
    cx.simulate_window_resize(w, size(px(960.), px(640.)));
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            for _ in 0..8 {
                s.pin_current_result(cx);
            }
        })
    })
    .unwrap();
    click(cx, w, "results-map");
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut v = VisualTestContext::from_window(w, cx);
    let tools = v.debug_bounds("result-tools").unwrap();
    let compare = v.debug_bounds("result-compare").unwrap();
    assert!(tools.size.height <= px(280.));
    assert!(compare.top() >= tools.top() && compare.bottom() <= tools.bottom());
    assert!(compare.size.height >= px(28.));
}

#[gpui_kit::test]
fn item3_integrated_narrow_workbench_scrolls_past_two_pin_provenance_to_rows_and_copy(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    // Keep the real Workbench root: its split editor/results panels and the
    // non-flex clipped results wrapper are essential to reproducing this bug.
    let (window, entity) = crate::workbench_tests::workspace(cx, 338, runtime.handle());
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    let expected = "last retained row · 雪 · 9007199254740993.00000000001";
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.page = Page::Query;
            s.profile = Some(ConnectionProfile {
                id: "profile-338".into(),
                name: "Rowboat sandbox / project sample".into(),
                database: "/example/workspaces/rowboat/demo-fixtures/database with a long display path/project sample.sqlite".into(),
                ..Default::default()
            });
            s.tabs[0].connection_id = s.profile.as_ref().map(|p| p.id.clone());
            let mut loaded = result();
            loaded.rows = (1..=4).map(|id| vec![Cell::Text(id.to_string()), Cell::Text(if id == 4 { expected.into() } else { format!("retained row {id}") })]).collect();
            s.tabs[0].results = vec![loaded];
            s.capture_query_result_origin(s.tabs[0].id, "SELECT id,amount FROM retained_rows ORDER BY id");
            s.pin_current_result(cx);
            s.tabs[0].results[0].rows[0][1] = Cell::Text("changed retained row 1".into());
            s.pin_current_result(cx);
            assert_eq!(s.results.pins.len(), 2);
        });
        w.draw(cx).clear(cx);
    }).unwrap();

    let panel = VisualTestContext::from_window(window, cx)
        .debug_bounds("result-snapshot-panel")
        .unwrap();
    assert!(
        panel.size.height > px(30.) && panel.size.height <= px(180.),
        "Expected the short integrated results viewport, got {panel:?}"
    );
    assert!(
        panel.bottom() <= px(640.),
        "Snapshot scroll viewport escaped its clipped Workbench parent: {panel:?}"
    );
    let mut selected = false;
    for _ in 0..60 {
        cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
            .unwrap();
        let mut visual = VisualTestContext::from_window(window, cx);
        if let Some(cell) = visual.debug_bounds("snapshot-cell-3-1")
            && cell.top() >= panel.top()
            && cell.bottom() <= panel.bottom()
        {
            visual.simulate_click(cell.center(), Modifiers::default());
            selected = true;
            break;
        }
        visual.simulate_event(ScrollWheelEvent {
            position: panel.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-24.))),
            ..Default::default()
        });
    }
    assert!(
        selected,
        "Native wheel input never exposed the last snapshot row"
    );
    cx.read(|cx| {
        assert_eq!(
            (
                entity.read(cx).results.selected_row,
                entity.read(cx).results.selected_column
            ),
            (3, 1)
        )
    });
    let mut copied = false;
    for _ in 0..60 {
        cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
            .unwrap();
        let mut visual = VisualTestContext::from_window(window, cx);
        if let Some(copy) = visual.debug_bounds("snapshot-copy-left")
            && copy.top() >= panel.top()
            && copy.bottom() <= panel.bottom()
        {
            visual.simulate_click(copy.center(), Modifiers::default());
            copied = true;
            break;
        }
        visual.simulate_event(ScrollWheelEvent {
            position: panel.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(24.))),
            ..Default::default()
        });
    }
    assert!(
        copied,
        "Copy action was not reachable by scrolling back through provenance"
    );
    cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), expected));

    // The same bounded viewport must also contain comparison provenance/cards.
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, _| {
            use rowboat_core::result_compare::{ColumnPair, Mapping};
            s.results.mapping = Mapping {
                keys: vec![ColumnPair { left: 0, right: 0 }],
                columns: vec![
                    ColumnPair { left: 0, right: 0 },
                    ColumnPair { left: 1, right: 1 },
                ],
            };
            let available = s.available_results_bytes(None).unwrap();
            s.results.start(s.tx.route, available, &s.runtime).unwrap();
        })
    })
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while cx
        .update_window(window, |_, _, cx| {
            entity.update(cx, |s, cx| {
                s.poll_result_comparison(cx);
                s.results.pending()
            })
        })
        .unwrap()
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    for (target, delta) in [("difference-column-1", -24.), ("snapshot-copy-left", 24.)] {
        let mut reached = false;
        for _ in 0..60 {
            cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let mut visual = VisualTestContext::from_window(window, cx);
            let viewport = visual.debug_bounds("result-snapshot-panel").unwrap();
            assert!(viewport.size.height > px(30.) && viewport.size.height <= px(180.));
            assert!(viewport.bottom() <= px(640.));
            if let Some(button) = visual.debug_bounds(target)
                && button.top() >= viewport.top()
                && button.bottom() <= viewport.bottom()
            {
                visual.simulate_click(button.center(), Modifiers::default());
                reached = true;
                break;
            }
            visual.simulate_event(ScrollWheelEvent {
                position: viewport.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                ..Default::default()
            });
        }
        assert!(
            reached,
            "Comparison action {target} not reachable by native wheel scrolling"
        );
    }
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "retained row 1"
        )
    });
    cx.read(|cx| assert_eq!(entity.read(cx).query_run_invocations, 0));
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}
