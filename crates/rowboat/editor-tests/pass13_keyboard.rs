//! Keyboard outcomes through the rendered Workbench and actual GPUI dispatch.
use crate::app::Workbench;
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Focusable, KeyBinding, Modifiers, TestAppContext,
    VisualTestContext, px, size,
};
use rowboat_core::{Cell, Column, QueryResult};

fn result(rows: usize, columns: usize) -> QueryResult {
    QueryResult {
        columns: (0..columns)
            .map(|i| Column {
                name: format!("column_{i}"),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            })
            .collect(),
        rows: (0..rows)
            .map(|r| {
                (0..columns)
                    .map(|c| Cell::Text(format!("{r}:{c}")))
                    .collect()
            })
            .collect(),
        ..Default::default()
    }
}
fn draw(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn workspace(
    cx: &mut TestAppContext,
    runtime: &crate::app_runtime::AppRuntime,
    rows: usize,
    columns: usize,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let (w, s) = crate::workbench_tests::workspace(cx, 130, runtime.handle());
    cx.simulate_window_resize(w, size(px(980.), px(640.)));
    cx.update_window(w, |_, w, cx| {
        // Match the result shortcuts in main.rs (the shared fixture binds the
        // remaining production keyboard and preference shortcuts).
        cx.bind_keys([
            KeyBinding::new("alt-pageup", crate::app::PreviousResult, Some("Rowboat")),
            KeyBinding::new("alt-pagedown", crate::app::NextResult, Some("Rowboat")),
        ]);
        s.update(cx, |s, cx| {
            s.tabs[0].results = vec![result(rows, columns)];
            s.tabs[0].editor.update(cx, |e, cx| e.focus(w, cx));
        });
    })
    .unwrap();
    draw(cx, w);
    (w, s)
}
fn position(cx: &TestAppContext, s: &Entity<Workbench>) -> (usize, usize) {
    cx.read(|cx| {
        let s = s.read(cx);
        let selection = s.selected.as_ref().expect("visible grid selection");
        assert_eq!(Some(&selection.origin), s.current_origin().as_ref());
        assert_eq!(selection.visible_rows, s.visible_grid_rows(cx));
        (selection.focus.row, selection.focus.column)
    })
}

#[gpui_kit::test]
fn pass13_keyboard_large_grid_edges_pages_shift_and_editor_ownership(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 10_000, 12);
    cx.simulate_keystrokes(w, "f6");
    assert_eq!(position(cx, &s), (0, 0));
    cx.simulate_keystrokes(w, "ctrl-end");
    assert_eq!(position(cx, &s), (9_999, 11));
    draw(cx, w);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.grid_x_scroll.offset().x < px(0.));
        assert!(s.grid_scroll.0.borrow().base_handle.offset().y < px(0.));
    });
    cx.simulate_keystrokes(w, "home");
    assert_eq!(position(cx, &s), (9_999, 0));
    cx.simulate_keystrokes(w, "end");
    assert_eq!(position(cx, &s), (9_999, 11));
    cx.simulate_keystrokes(w, "ctrl-home");
    assert_eq!(position(cx, &s), (0, 0));
    draw(cx, w);
    cx.read(|cx| assert_eq!(s.read(cx).grid_x_scroll.offset().x, px(0.)));
    cx.simulate_keystrokes(w, "shift-pagedown");
    let page = position(cx, &s).0;
    assert!((1..100).contains(&page), "viewport step {page}");
    cx.read(|cx| assert_eq!(s.read(cx).grid_range(cx).unwrap().rows(), page + 1));
    cx.simulate_keystrokes(w, "pageup");
    assert_eq!(position(cx, &s), (0, 0));
    cx.simulate_keystrokes(w, "ctrl-shift-end");
    cx.read(|cx| {
        let range = s.read(cx).grid_range(cx).unwrap();
        assert_eq!((range.rows(), range.columns()), (10_000, 12));
    });
    cx.simulate_keystrokes(w, "f6 ctrl-home");
    cx.update_window(w, |_, w, cx| {
        assert!(
            s.read(cx).tabs[0]
                .editor
                .read(cx)
                .focus_handle(cx)
                .is_focused(w)
        );
    })
    .unwrap();
    cx.simulate_input(w, "--");
    cx.read(|cx| assert!(s.read(cx).tabs[0].editor.read(cx).value().starts_with("--")));
    assert_eq!(
        position(cx, &s),
        (9_999, 11),
        "editor Ctrl+Home must not move grid"
    );
}

#[gpui_kit::test]
fn pass13_keyboard_filtered_rows_and_whole_row_extension_keep_display_provenance(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 6, 3);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            for r in [1, 3, 5] {
                s.tabs[0].results[0].rows[r][1] = Cell::Text("keep".into());
            }
            s.result_filter
                .update(cx, |e, cx| e.set_value("keep", w, cx));
        });
    })
    .unwrap();
    draw(cx, w);
    cx.simulate_keystrokes(w, "f6");
    assert_eq!(position(cx, &s), (1, 0));
    cx.simulate_keystrokes(w, "ctrl-shift-end");
    assert_eq!(position(cx, &s), (5, 2));
    cx.read(|cx| assert_eq!(s.read(cx).grid_range(cx).unwrap().rows(), 3));
    cx.simulate_keystrokes(w, "ctrl-home shift-space shift-pagedown");
    cx.read(|cx| {
        assert!(s.read(cx).grid.row_selection);
        let range = s.read(cx).grid_range(cx).unwrap();
        assert_eq!(range.columns(), 3);
        assert!(range.rows() > 1);
    });
    cx.simulate_keystrokes(w, "ctrl-shift-end");
    cx.read(|cx| assert_eq!(s.read(cx).grid_range(cx).unwrap().rows(), 3));
    // Changing the display invalidates the old selection; F6 initializes from
    // the newly visible row set, never reusing raw indices from the old filter.
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.result_filter.update(cx, |e, cx| {
                e.set_value("0:0", w, cx);
                e.focus(w, cx);
            });
        });
    })
    .unwrap();
    draw(cx, w);
    cx.simulate_keystrokes(w, "f6");
    assert_eq!(position(cx, &s), (0, 0));
}

#[gpui_kit::test]
fn pass13_keyboard_multiple_results_cross_empty_and_affected_rows_without_mouse(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 2, 2);
    cx.update(|cx| {
        s.update(cx, |s, _| {
            s.tabs[0].results.extend([
                QueryResult {
                    rows_affected: 1,
                    ..Default::default()
                },
                result(0, 2),
                result(2, 2),
            ]);
        })
    });
    draw(cx, w);
    cx.simulate_keystrokes(w, "f6 alt-pagedown");
    draw(cx, w);
    cx.simulate_keystrokes(w, "alt-pagedown");
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].result_index, 2));
    cx.simulate_keystrokes(w, "f6 alt-pagedown ctrl-shift-p");
    cx.read(|cx| {
        assert_eq!(s.read(cx).tabs[0].result_index, 3);
        assert!(s.read(cx).quick_open);
    });
    cx.simulate_keystrokes(w, "escape alt-pageup alt-pageup alt-pageup");
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].result_index, 0));
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| e.focus(w, cx))
        });
    })
    .unwrap();
    draw(cx, w);
    cx.simulate_keystrokes(w, "alt-pagedown");
    draw(cx, w);
    cx.update_window(w, |_, w, cx| {
        assert!(
            s.read(cx).tabs[0]
                .editor
                .read(cx)
                .focus_handle(cx)
                .is_focused(w)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass13_keyboard_review_f6_and_palette_never_focus_hidden_grid(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 2, 2);
    cx.simulate_keystrokes(w, "f6");
    cx.update(|cx| s.update(cx, |s, _| s.query_edit.review = true));
    draw(cx, w);
    cx.simulate_keystrokes(w, "f6 ctrl-shift-p");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.quick_open);
        let item = s
            .palette_items(cx)
            .into_iter()
            .find(|i| {
                i.target == crate::palette::Target::Command(crate::palette::Command::FocusGrid)
            })
            .unwrap();
        assert!(!item.enabled);
    });
    cx.simulate_keystrokes(w, "escape tab");
    cx.update_window(w, |_, w, cx| {
        let s = s.read(cx);
        assert!(!s.grid_focus.is_focused(w));
        assert!(!s.tabs[0].editor.read(cx).focus_handle(cx).is_focused(w));
        assert!(s.focus.contains_focused(w, cx));
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass13_keyboard_inspector_escape_resumes_same_grid_selection(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 3, 2);
    cx.simulate_keystrokes(w, "f6 right enter");
    cx.read(|cx| assert!(s.read(cx).grid.inspector.is_some()));
    cx.simulate_keystrokes(w, "escape down");
    assert_eq!(position(cx, &s), (1, 1));
    cx.update_window(w, |_, w, cx| assert!(s.read(cx).grid_focus.is_focused(w)))
        .unwrap();
}

#[gpui_kit::test]
fn pass13_keyboard_palette_cancel_restores_caret_grid_and_command_destination(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 3, 2);
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_selected_range(6..6, cx))
        });
    })
    .unwrap();
    cx.simulate_keystrokes(w, "ctrl-shift-p escape");
    cx.simulate_input(w, "x");
    cx.read(|cx| {
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            "SELECTx 130"
        )
    });
    cx.simulate_keystrokes(w, "f6 ctrl-shift-p escape down");
    assert_eq!(position(cx, &s), (1, 0));
    cx.simulate_keystrokes(w, "ctrl-shift-p");
    cx.simulate_input(w, "Focus table explorer");
    cx.simulate_keystrokes(w, "enter");
    cx.update_window(w, |_, w, cx| {
        assert!(
            s.read(cx)
                .explorer_filter
                .read(cx)
                .focus_handle(cx)
                .is_focused(w)
        );
    })
    .unwrap();
    // Mouse close must use the same return path as Escape.
    cx.simulate_keystrokes(w, "ctrl-shift-p");
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let close = visual.debug_bounds("close-palette").unwrap();
    visual.simulate_click(close.center(), Modifiers::default());
    cx.update_window(w, |_, w, cx| {
        assert!(
            s.read(cx)
                .explorer_filter
                .read(cx)
                .focus_handle(cx)
                .is_focused(w)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass13_keyboard_palette_rejects_stale_host_return_target(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = workspace(cx, &runtime, 3, 2);
    cx.simulate_keystrokes(w, "ctrl-shift-p");
    // A retained palette response from an earlier host must not restore its
    // editor handle, even if the handle itself remains alive.
    cx.update(|cx| s.update(cx, |s, _| s.host_epoch += 1));
    cx.simulate_keystrokes(w, "escape");
    cx.update_window(w, |_, w, cx| assert!(s.read(cx).focus.is_focused(w)))
        .unwrap();
    cx.simulate_keystrokes(w, "ctrl-shift-p");
    cx.read(|cx| assert!(s.read(cx).quick_open));
}
