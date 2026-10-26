//! Real retained UI, keyboard menus and resize state; no native compositor control.
use crate::{
    app::{Page, TablePage},
    workbench_tests::workspace,
};
use gpui_kit::{
    AnyWindowHandle, AppContext, Modifiers, TestAppContext, VisualTestContext, WindowOptions, px,
    size,
};

fn draw(window: AnyWindowHandle, cx: &mut TestAppContext) {
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}

#[gpui_kit::test]
fn pass7_resizable_editor_results_preserve_buffer_selection_and_host_layout(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (old, entity) = workspace(cx, 71, runtime.handle());
    cx.simulate_window_resize(old, size(px(980.), px(640.)));
    draw(old, cx);
    let mut visual = VisualTestContext::from_window(old, cx);
    let editor_bounds = visual.debug_bounds("query-editor-region").unwrap();
    let result_bounds = visual.debug_bounds("query-results-region").unwrap();
    assert!(editor_bounds.size.height >= px(120.));
    assert!(result_bounds.size.height >= px(140.));
    assert!(result_bounds.bottom() <= px(640.));
    let (layout, editor) = cx.read(|cx| {
        let s = entity.read(cx);
        (s.editor_split.clone(), s.tabs[0].editor.clone())
    });
    cx.update_window(old, |_, w, cx| {
        editor.update(cx, |e, cx| {
            e.set_selected_range(0..6, cx);
            e.focus(w, cx);
        });
        layout.update(cx, |state, cx| state.resize_panel(0, px(230.), w, cx));
    })
    .unwrap();
    draw(old, cx);
    let sizes = cx.read(|cx| layout.read(cx).sizes().clone());
    assert!((f32::from(sizes[0]) - 230.).abs() < 2.);
    // A second render must not reset the user's divider position to its initial ratio.
    draw(old, cx);
    cx.read(|cx| assert_eq!(layout.read(cx).sizes(), &sizes));
    let new = cx.update(|cx| {
        let new = gpui_kit::open_window(WindowOptions::default(), cx, |w, cx| {
            entity.update(cx, |s, cx| s.rehost(w, cx));
            entity.clone()
        })
        .unwrap()
        .0;
        // Match the coordinator: one rendered host per workspace. Leaving two
        // root windows alive makes their shared ResizableState fight over bounds.
        old.update(cx, |_, w, _| w.remove_window()).unwrap();
        new
    });
    cx.simulate_window_resize(new, size(px(980.), px(640.)));
    draw(new, cx);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.editor_split, layout);
        assert_eq!(s.tabs[0].editor, editor);
        assert_eq!(editor.read(cx).selected_range(), 0..6);
        assert_eq!(editor.read(cx).value().as_ref(), "SELECT 71");
    });
    cx.simulate_window_resize(new, size(px(1440.), px(940.)));
    draw(new, cx);
    let mut visual = VisualTestContext::from_window(new, cx);
    assert!(
        visual
            .debug_bounds("query-editor-region")
            .unwrap()
            .size
            .height
            >= px(120.)
    );
    assert!(
        visual
            .debug_bounds("query-results-region")
            .unwrap()
            .bottom()
            <= px(940.)
    );
}

#[gpui_kit::test]
fn pass7_query_menu_keyboard_skips_disabled_opens_dialog_and_restores_commands(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 72, runtime.handle());
    draw(window, cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual.debug_bounds("query-menu").unwrap();
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(window, cx);
    // Offline completion is now available. Move past Parameters, skipping the
    // disconnected Explain action, then back past it to Parameters.
    cx.simulate_keystrokes(window, "down down down down up enter");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.tabs[0].show_parameters);
        assert_eq!(s.tabs[0].editor.read(cx).value().as_ref(), "SELECT 72");
    });
    cx.simulate_keystrokes(window, "escape ctrl-shift-p");
    assert!(cx.read(|cx| entity.read(cx).quick_open));
    cx.simulate_keystrokes(window, "escape");
    cx.update(|cx| entity.update(cx, |s, _| s.tabs[0].show_parameters = false));
    draw(window, cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual.debug_bounds("query-menu").unwrap();
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(window, cx);
    cx.simulate_keystrokes(window, "escape enter down down down enter");
    assert!(
        cx.read(|cx| entity.read(cx).tabs[0].show_parameters),
        "Enter must reopen the focused menu trigger"
    );
    cx.simulate_keystrokes(window, "escape ctrl-shift-p");
    assert!(cx.read(|cx| entity.read(cx).quick_open));
}

#[gpui_kit::test]
fn pass7_satellite_minimum_keeps_query_menu_reachable_after_large_resize(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (old, entity) = workspace(cx, 76, runtime.handle());
    cx.simulate_window_resize(old, size(px(1440.), px(940.)));
    draw(old, cx);
    cx.update_window(old, |_, w, cx| {
        let layout = entity.read(cx).editor_split.clone();
        layout.update(cx, |state, cx| state.resize_panel(0, px(620.), w, cx));
    })
    .unwrap();
    draw(old, cx);
    let minimum = crate::workspace_shell::satellite_min_size();
    let new = cx.update(|cx| {
        let new = gpui_kit::open_window(
            WindowOptions {
                window_min_size: Some(minimum),
                ..Default::default()
            },
            cx,
            |w, cx| {
                entity.update(cx, |s, cx| s.rehost(w, cx));
                entity.clone()
            },
        )
        .unwrap()
        .0;
        old.update(cx, |_, w, _| w.remove_window()).unwrap();
        new
    });
    cx.simulate_window_resize(new, minimum);
    draw(new, cx);
    draw(new, cx);
    let mut visual = VisualTestContext::from_window(new, cx);
    for selector in ["query-editor-region", "query-results-region", "query-menu"] {
        let bounds = visual.debug_bounds(selector).unwrap();
        assert!(
            bounds.left() >= px(0.) && bounds.right() <= minimum.width,
            "{selector} clipped horizontally: {bounds:?}, viewport {minimum:?}"
        );
        assert!(
            bounds.top() >= px(0.) && bounds.bottom() <= minimum.height,
            "{selector} clipped vertically: {bounds:?}, viewport {minimum:?}"
        );
    }
    let menu = visual.debug_bounds("query-menu").unwrap();
    visual.simulate_click(menu.center(), Modifiers::default());
    draw(new, cx);
    let popup = visual.debug_bounds("omarchy-menu-content").unwrap();
    assert!(
        popup.left() >= px(0.)
            && popup.right() <= minimum.width
            && popup.bottom() <= minimum.height
    );
    cx.simulate_keystrokes(new, "down down down enter");
    cx.read(|cx| assert!(entity.read(cx).tabs[0].show_parameters));
}

#[gpui_kit::test]
fn pass7_grid_navigation_respects_filtered_display_order_and_input_ownership(
    cx: &mut TestAppContext,
) {
    use rowboat_core::{Cell, Column, QueryResult};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 73, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let col = |name: &str| Column {
                name: name.into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            };
            s.tabs[0].results = vec![QueryResult {
                columns: vec![col("a"), col("b")],
                rows: vec![
                    vec![Cell::Text("keep one".into()), Cell::Null],
                    vec![Cell::Text("omit".into()), Cell::Null],
                    vec![Cell::Text("keep two".into()), Cell::Null],
                ],
                ..Default::default()
            }];
            s.result_filter
                .update(cx, |e, cx| e.set_value("keep", w, cx));
            w.focus(&s.grid_focus, cx);
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "right down");
    cx.read(|cx| {
        let s = entity.read(cx);
        let selected = s.selected.as_ref().unwrap();
        assert_eq!(selected.focus.row, 2);
        assert_eq!(selected.focus.column, 1);
    });
    cx.simulate_keystrokes(window, "enter");
    assert!(cx.read(|cx| entity.read(cx).grid.inspector.is_some()));
    cx.simulate_keystrokes(window, "escape");
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.result_filter.update(cx, |e, cx| e.focus(w, cx))
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let before = cx.read(|cx| entity.read(cx).selected.as_ref().unwrap().focus);
    cx.simulate_keystrokes(window, "left home end");
    cx.read(|cx| assert_eq!(entity.read(cx).selected.as_ref().unwrap().focus, before));
    cx.simulate_keystrokes(window, "f6");
    cx.update_window(window, |_, w, cx| {
        assert!(entity.read(cx).grid_focus.is_focused(w))
    })
    .unwrap();
    cx.simulate_keystrokes(window, "f6");
    cx.update_window(window, |_, w, cx| {
        use gpui_kit::Focusable;
        assert!(
            entity.read(cx).tabs[0]
                .editor
                .read(cx)
                .focus_handle(cx)
                .is_focused(w)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass7_review_disclosure_preserves_exact_batch_and_reachable_actions(cx: &mut TestAppContext) {
    use rowboat_core::{Cell, ConnectionProfile, RowChange, TableRef};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 75, runtime.handle());
    let change = RowChange::Update {
        original: vec![
            ("id".into(), Cell::Text("1".into())),
            ("value".into(), Cell::Text("original".into())),
        ],
        values: vec![(
            "value".into(),
            Cell::Text(format!("exact\n\t{}", "é".repeat(300))),
        )],
    };
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.profile = Some(ConnectionProfile {
                id: "profile-75".into(),
                ..Default::default()
            });
            s.table = Some(TableRef {
                schema: "review_only".into(),
                name: "not_executed".into(),
            });
            s.changes = vec![change.clone(); 20];
            s.change_target = s.current_target();
            s.review_pending(cx);
            w.focus(&s.focus, cx);
        })
    })
    .unwrap();
    cx.simulate_window_resize(window, size(px(980.), px(640.)));
    draw(window, cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(visual.debug_bounds("copy-change-evidence-0").is_none());
    let actions = visual.debug_bounds("review-actions").unwrap();
    let details = visual.debug_bounds("change-details-0").unwrap();
    visual.simulate_click(details.center(), Modifiers::default());
    draw(window, cx);
    let copy = visual.debug_bounds("copy-change-evidence-0").unwrap();
    visual.simulate_click(copy.center(), Modifiers::default());
    cx.update(|cx| {
        let json = cx.read_from_clipboard().unwrap().text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::to_value(&change).unwrap()
        );
    });
    assert_eq!(visual.debug_bounds("review-actions").unwrap(), actions);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.changes.len(), 20);
        assert_eq!(
            serde_json::to_value(&s.changes[0]).unwrap(),
            serde_json::to_value(&change).unwrap()
        );
        assert!(!s.busy);
    });
}

#[gpui_kit::test]
fn pass7_stale_menu_choice_cannot_modify_frozen_or_busy_workspace(cx: &mut TestAppContext) {
    use crate::chrome_ui::Command;
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 74, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            assert!(s.menu_enabled(Command::Parameters, cx));
            s.close_requested = true;
            s.choose_menu(Command::Parameters, w, cx);
            assert!(!s.tabs[0].show_parameters);
            s.close_requested = false;
            s.busy = true;
            s.choose_menu(Command::Parameters, w, cx);
            assert!(!s.tabs[0].show_parameters);
            s.busy = false;
            s.page = Page::Table;
            s.table_page = TablePage::Changes;
            s.choose_menu(Command::Parameters, w, cx);
            assert!(!s.tabs[0].show_parameters);
        })
    })
    .unwrap();
}
