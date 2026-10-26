//! Actual GPUI rendering/clicks for the record hooks, independent of parent layout glue.
//! Register with `mod item5_record_tests;` in editor-tests/tests.rs.
use crate::{
    app::{Page, Workbench},
    grid_ui::record_inspector::RecordDock,
};
use gpui_kit::{
    AnyWindowHandle, AppContext, Context, Entity, Modifiers, Render, ScrollDelta, ScrollWheelEvent,
    TestAppContext, VisualTestContext, Window, div, point, prelude::*, px, size,
};
use rowboat_core::{Cell, Column, QueryResult};

struct RecordHost {
    workbench: Entity<Workbench>,
    height: Option<f32>,
    show_toolbar: bool,
}
impl Render for RecordHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workbench.update(cx, |s, cx| {
            div()
                .size_full()
                .when_some(self.height, |d, height| d.h(px(height)))
                .flex()
                .flex_col()
                .track_focus(&s.grid_focus)
                .when(self.show_toolbar, |d| {
                    d.child(s.render_record_toolbar(window, cx))
                })
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_hidden()
                        .child(s.render_record_panel(window, cx)),
                )
        })
    }
}

fn fixture(
    cx: &mut TestAppContext,
    id: u64,
    runtime: tokio::runtime::Handle,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let (window, entity) = crate::workbench_tests::workspace(cx, id, runtime);
    cx.simulate_window_resize(window, size(px(1200.), px(1400.)));
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.page = Page::Query;
            s.tabs[0].results = vec![QueryResult {
                columns: vec![Column {
                    name: "雪".into(),
                    data_type: "json".into(),
                    nullable: true,
                    primary_key: false,
                    default_value: None,
                }],
                rows: vec![
                    vec![Cell::Text(
                        r#"{"a/~":[123456789012345678901234567890],"string":"\u96ea"}"#.into(),
                    )],
                    vec![Cell::Null],
                ],
                ..Default::default()
            }];
            s.select_grid_cell(0, 0, false, cx);
            w.focus(&s.grid_focus, cx);
        });
        w.replace_root(cx, |_, cx| {
            cx.observe(&entity, |_, _, cx| cx.notify()).detach();
            RecordHost {
                workbench: entity.clone(),
                height: None,
                show_toolbar: true,
            }
        });
    })
    .unwrap();
    (window, entity)
}
fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}

#[gpui_kit::test]
fn item5_record_actual_json_expand_copy_exact_number_and_pointer(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = fixture(cx, 501, runtime.handle());
    click(cx, window, "record-toggle");
    click(cx, window, "record-copy-0-0");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            r#"{"a/~":[123456789012345678901234567890],"string":"\u96ea"}"#
        )
    });
    click(cx, window, "record-expand-0-0-0");
    click(cx, window, "record-expand-0-0-1");
    click(cx, window, "record-node-copy-0-0-2");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "123456789012345678901234567890"
        )
    });
    click(cx, window, "record-node-path-0-0-2");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "/0/a~1~0/0"
        )
    });
    click(cx, window, "record-close");
    cx.read(|cx| assert!(!entity.read(cx).record_panel_open()));
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}

#[gpui_kit::test]
fn item5_record_actual_selection_preserves_focus_sql_and_cell_draft(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = fixture(cx, 502, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let original_sql = s.tabs[0].editor.read(cx).value().to_string();
            s.toggle_record_inspector(w, cx);
            assert!(s.grid_focus.is_focused(w));
            s.select_grid_cell(1, 0, false, cx);
            assert_eq!(
                s.selected_record_snapshot().unwrap().value(0, false),
                Some(&Cell::Null)
            );
            assert!(s.grid_focus.is_focused(w));
            assert_eq!(s.tabs[0].editor.read(cx).value().as_ref(), original_sql);
            let draft = crate::forms::textarea("unsubmitted 雪", w, cx);
            s.grid.draft = Some(crate::grid_ui::CellDraft {
                target: crate::forms::RowTarget {
                    connection_id: "fixture".into(),
                    session_generation: 1,
                    table: rowboat_core::TableRef {
                        schema: "main".into(),
                        name: "fixture".into(),
                    },
                },
                range: rowboat_core::grid::GridRange::from_size(
                    rowboat_core::grid::GridPosition { row: 1, column: 0 },
                    1,
                    1,
                )
                .unwrap(),
                snapshot: s.tabs[0].results[0].clone(),
                value: draft,
                kind: crate::grid_ui::ValueKind::Text,
            });
            s.toggle_record_inspector(w, cx);
            s.toggle_record_inspector(w, cx);
            s.select_grid_cell(0, 0, false, cx); // existing draft guard retains row ownership
            assert_eq!(s.selected_cell(), Some((1, 0)));
            assert_eq!(
                s.grid
                    .draft
                    .as_ref()
                    .unwrap()
                    .value
                    .read(cx)
                    .value()
                    .as_ref(),
                "unsubmitted 雪"
            );
            assert_eq!(
                s.selected_record_snapshot().unwrap().value(0, false),
                Some(&Cell::Null)
            );
        });
    })
    .unwrap();
    click(cx, window, "record-copy-0-0");
    cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "NULL"));
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}

#[gpui_kit::test]
fn item5_record_actual_unicode_search_and_responsive_docking(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = fixture(cx, 503, runtime.handle());
    click(cx, window, "record-toggle");
    click(cx, window, "record-search");
    cx.simulate_input(window, "雪");
    click(cx, window, "record-node-copy-0-0-3");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            r#""\u96ea""#
        )
    });
    click(cx, window, "record-dock");
    cx.read(|cx| {
        assert_eq!(
            entity.read(cx).grid.record.as_ref().unwrap().dock,
            RecordDock::Bottom
        )
    });
    click(cx, window, "record-dock");
    cx.simulate_window_resize(window, size(px(700.), px(600.)));
    cx.update_window(window, |_, w, cx| {
        assert_eq!(entity.read(cx).record_panel_dock(w), RecordDock::Bottom);
        assert_eq!(
            entity.read(cx).grid.record.as_ref().unwrap().dock,
            RecordDock::Right
        );
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(visual.debug_bounds("record-close").is_some());
    assert!(visual.debug_bounds("record-search").is_some());
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}

#[gpui_kit::test]
fn item5_record_actual_pin_routes_original_cells_and_blocks_live_grid_mutations(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = fixture(cx, 504, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let original = s.tabs[0].results[0].rows[0][0].clone();
            s.changes = vec![rowboat_core::RowChange::Update {
                original: vec![("雪".into(), original.clone())],
                values: vec![("雪".into(), Cell::Text("staged, never pin".into()))],
            }];
            let origin = crate::result_snapshots::ResultOrigin {
                route: s.tx.route,
                connection_id: "fixture".into(),
                connection_name: "fixture".into(),
                database: "fixture".into(),
                engine: rowboat_core::Engine::Sqlite,
                started_at: "2026-10-26T00:00:00Z".into(),
                source: "SELECT fixture".into(),
                table_page: false,
            };
            s.results
                .pin_result(
                    &s.tabs[0].results[0],
                    origin,
                    0,
                    "Retained fixture".into(),
                    false,
                    1024 * 1024,
                )
                .unwrap();
            s.tabs[0].results[0].rows[0][0] = Cell::Text("new live result".into());
            s.toggle_record_inspector(w, cx);
            let snapshot = s.selected_record_snapshot().unwrap();
            assert!(snapshot.title.contains("Pinned"));
            assert_eq!(snapshot.value(0, false), Some(&original));
            assert_eq!(snapshot.value(0, true), Some(&original));
            assert!(snapshot.staged.is_empty());
            assert!(!s.grid_writable());
            assert!(s.grid_range(cx).is_none());
            let selection = s.selected_cell();
            s.select_grid_cell(1, 0, false, cx);
            s.sort_grid_column(0, cx);
            s.clear_grid(cx);
            assert_eq!(s.selected_cell(), selection);
            assert_eq!(s.changes.len(), 1);
            assert!(s.grid.draft.is_none());
        });
    })
    .unwrap();
    click(cx, window, "record-copy-0-0");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            r#"{"a/~":[123456789012345678901234567890],"string":"\u96ea"}"#
        )
    });
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.results.selected_row = 1;
            cx.notify();
        });
    })
    .unwrap();
    click(cx, window, "record-copy-0-0");
    cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "NULL"));
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, _| {
            s.results.view = crate::result_snapshots::ResultsView::Comparison;
            assert!(s.selected_record_snapshot().is_none());
        });
    })
    .unwrap();
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}

#[gpui_kit::test]
fn item5_record_actual_table_staged_binary_and_original_have_independent_copy(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = fixture(cx, 505, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.profile = Some(rowboat_core::ConnectionProfile::default());
            s.table = Some(rowboat_core::TableRef {
                schema: "main".into(),
                name: "fixture".into(),
            });
            s.table_result = s.tabs[0].results[0].clone();
            s.table_result.rows[0][0] = Cell::Text("original 雪".into());
            s.page = Page::Table;
            s.select_grid_cell(0, 0, false, cx);
            s.change_target = s.current_target();
            s.changes = vec![rowboat_core::RowChange::Update {
                original: vec![("雪".into(), Cell::Text("original 雪".into()))],
                values: vec![("雪".into(), Cell::Binary(vec![0, 128, 255]))],
            }];
            s.toggle_record_inspector(w, cx);
        });
    })
    .unwrap();
    click(cx, window, "record-copy-0-1");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            Cell::Binary(vec![0, 128, 255]).display()
        )
    });
    click(cx, window, "record-copy-0-0");
    cx.update(|cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "original 雪"
        )
    });
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.table_result.rows[0][0], Cell::Text("original 雪".into()));
        assert_eq!(s.changes.len(), 1);
    });
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}

#[gpui_kit::test]
fn item5_record_124px_panel_scrolls_payload_value_and_path_with_close_dock_reachable(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = fixture(cx, 508, runtime.handle());
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    cx.update_window(window, |_, w, cx| {
        w.root::<RecordHost>()
            .unwrap()
            .unwrap()
            .update(cx, |host, cx| {
                host.height = Some(124.);
                host.show_toolbar = false;
                cx.notify();
            });
        entity.update(cx, |s, cx| {
            let result = &mut s.tabs[0].results[0];
            result.columns = ["id", "name", "payload"]
                .into_iter()
                .map(|name| Column {
                    name: name.into(),
                    data_type: "text".into(),
                    nullable: true,
                    primary_key: false,
                    default_value: None,
                })
                .collect();
            result.rows = vec![vec![
                Cell::Text("1".into()),
                Cell::Text("Ada 雪".into()),
                Cell::Text(r#"{"nested":[true,null,9007199254740993]}"#.into()),
            ]];
            s.toggle_record_inspector(w, cx);
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let panel = visual.debug_bounds("record-panel").unwrap();
    let body = visual.debug_bounds("record-fields").unwrap();
    assert_eq!(panel.size.height, px(124.));
    assert!(
        body.size.height >= px(60.),
        "inspector chrome consumed the usable viewport: {body:?}"
    );
    let close = visual.debug_bounds("record-close").unwrap();
    let dock = visual.debug_bounds("record-dock").unwrap();
    assert!(close.bottom() <= body.top());
    assert!(dock.bottom() <= body.top());
    let mut reached = false;
    for _ in 0..100 {
        cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
            .unwrap();
        let mut visual = VisualTestContext::from_window(window, cx);
        // Require whole buttons in the viewport, not just clipped centers.
        if let (Some(value), Some(path)) = (
            visual.debug_bounds("record-copy-2-0"),
            visual.debug_bounds("record-path-2-0"),
        ) && value.top() >= body.top()
            && value.bottom() <= body.bottom()
            && path.top() >= body.top()
            && path.bottom() <= body.bottom()
        {
            visual.simulate_click(value.center(), Modifiers::default());
            cx.update(|cx| {
                assert_eq!(
                    cx.read_from_clipboard().unwrap().text().unwrap(),
                    r#"{"nested":[true,null,9007199254740993]}"#
                )
            });
            click(cx, window, "record-path-2-0");
            cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "/2"));
            reached = true;
            break;
        }
        visual.simulate_event(ScrollWheelEvent {
            position: body.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(-16.))),
            ..Default::default()
        });
    }
    assert!(
        reached,
        "payload copy actions were inaccessible in a 124px inspector"
    );
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    assert_eq!(visual.debug_bounds("record-close").unwrap(), close);
    assert_eq!(visual.debug_bounds("record-dock").unwrap(), dock);
    click(cx, window, "record-dock");
    cx.read(|cx| {
        assert_eq!(
            entity.read(cx).grid.record.as_ref().unwrap().dock,
            RecordDock::Bottom
        )
    });
    click(cx, window, "record-close");
    cx.read(|cx| assert!(!entity.read(cx).record_panel_open()));
    cx.update_window(window, |_, w, _| w.remove_window())
        .unwrap();
}

#[gpui_kit::test]
fn item5_record_constrained_fields_keep_intrinsic_height_and_scroll_actions(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for (id, width, height, region) in [(506, 1200., 800., 380.), (507, 960., 640., 180.)] {
        let (window, entity) = fixture(cx, id, runtime.handle());
        cx.simulate_window_resize(window, size(px(width), px(height)));
        cx.update_window(window, |_, w, cx| {
            w.root::<RecordHost>()
                .unwrap()
                .unwrap()
                .update(cx, |host, cx| {
                    host.height = Some(region);
                    cx.notify();
                });
            entity.update(cx, |s, cx| {
                let result = &mut s.tabs[0].results[0];
                result.columns = (0..3)
                    .map(|i| Column {
                        name: format!("field_{i}"),
                        data_type: "text".into(),
                        nullable: true,
                        primary_key: false,
                        default_value: None,
                    })
                    .collect();
                result.rows = vec![vec![
                    Cell::Text("1".into()),
                    Cell::Text("雪".repeat(170)),
                    Cell::Text(r#"{"nested":[true,null]}"#.into()),
                ]];
                s.toggle_record_inspector(w, cx);
            });
            w.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = VisualTestContext::from_window(window, cx);
        let panel = visual.debug_bounds("record-panel").unwrap();
        let body = visual.debug_bounds("record-fields").unwrap();
        let search = visual.debug_bounds("record-search").unwrap();
        assert!(
            panel.bottom() <= px(region + 1.),
            "panel escaped constrained result region: {panel:?}"
        );
        assert!(search.top() >= body.top());
        assert!(search.bottom() <= body.bottom());
        assert!(body.bottom() <= panel.bottom());
        assert!(body.size.height > px(20.));
        let mut copied = false;
        for _ in 0..150 {
            cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let mut visual = VisualTestContext::from_window(window, cx);
            // Offscreen elements may be culled; whenever a field is painted its
            // actions must fit its intrinsic bounds, with no following-field overlap.
            for (field_id, button_id) in [
                ("record-field-0", "record-copy-0-0"),
                ("record-field-1", "record-copy-1-0"),
                ("record-field-2", "record-copy-2-0"),
            ] {
                if let (Some(field), Some(button)) = (
                    visual.debug_bounds(field_id),
                    visual.debug_bounds(button_id),
                ) {
                    assert!(button.top() >= field.top());
                    assert!(
                        button.bottom() <= field.bottom(),
                        "field compressed below its action row"
                    );
                    assert!(button.right() <= field.right());
                }
            }
            if let (Some(first), Some(second)) = (
                visual.debug_bounds("record-field-0"),
                visual.debug_bounds("record-field-1"),
            ) {
                assert!(second.top() >= first.bottom(), "record fields overlap");
            }
            if let (Some(second), Some(third)) = (
                visual.debug_bounds("record-field-1"),
                visual.debug_bounds("record-field-2"),
            ) {
                assert!(
                    third.top() >= second.bottom(),
                    "long text spills into next field"
                );
            }
            if let Some(button) = visual.debug_bounds("record-copy-2-0")
                && body.contains(&button.center())
            {
                visual.simulate_click(button.center(), Modifiers::default());
                copied = true;
                break;
            }
            visual.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(-24.))),
                ..Default::default()
            });
        }
        assert!(
            copied,
            "last field's actions were not reachable by scrolling"
        );
        cx.update(|cx| {
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().unwrap(),
                r#"{"nested":[true,null]}"#
            )
        });
        cx.update_window(window, |_, w, _| w.remove_window())
            .unwrap();
    }
}
