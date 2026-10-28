//! Retained-result budget preferences through the real dialog on TestPlatform.
use gpui_kit::{
    AnyWindowHandle, AppContext, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use rowboat_core::{Cell, Column, QueryResult};

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, selector: &'static str) {
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}

fn enter_budget(cx: &mut TestAppContext, window: AnyWindowHandle, value: &str) {
    click(cx, window, "preferences-tab-editor");
    click(cx, window, "query_result_limit_mib");
    cx.simulate_keystrokes(window, "ctrl-a");
    cx.simulate_input(window, value);
}

#[gpui_kit::test]
fn pass17_preferences_budget_validates_and_preserves_retained_results(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 171, runtime.handle());
    cx.simulate_window_resize(window, size(px(1100.), px(1600.)));
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.edit_state(|state| {
                state.preferences.query_row_limit = 5000;
                state.preferences.editor_font_size = 18.;
            });
            s.tabs[0].results = vec![QueryResult {
                columns: vec![Column {
                    name: "already_retained".into(),
                    data_type: "TEXT".into(),
                    nullable: false,
                    primary_key: false,
                    default_value: None,
                }],
                rows: vec![vec![Cell::Text("keep this result".into())]],
                ..Default::default()
            }];
            s.show_preferences(window, cx);
            assert!(s.preferences_form.is_some());
        });
    })
    .unwrap();
    let original = cx.read(|cx| serde_json::to_value(&entity.read(cx).state.preferences).unwrap());
    for invalid in ["15", "513", "not a number"] {
        enter_budget(cx, window, invalid);
        click(cx, window, "save-preferences");
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.preferences_form.is_some());
            assert!(s.error.is_some());
            assert_eq!(
                serde_json::to_value(&s.state.preferences).unwrap(),
                original
            );
            assert_eq!(
                s.shared.borrow().state.preferences.query_result_limit_mib,
                256
            );
        });
    }
    for value in [64, 512, 16, 256] {
        enter_budget(cx, window, &value.to_string());
        click(cx, window, "save-preferences");
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.preferences_form.is_none());
            let mut expected = original.clone();
            expected["query_result_limit_mib"] = value.into();
            assert_eq!(
                serde_json::to_value(&s.state.preferences).unwrap(),
                expected
            );
            assert_eq!(
                s.shared.borrow().state.preferences.query_result_limit_mib,
                value
            );
            assert_eq!(
                s.tabs[0].results[0].rows[0][0],
                Cell::Text("keep this result".into())
            );
            assert!(s.query_stream.is_none());
            assert!(!s.busy);
            assert!(s.state.history.is_empty());
            assert!(s.status.contains("next execution"));
        });
        cx.update_window(window, |_, window, cx| {
            entity.update(cx, |s, cx| s.show_preferences(window, cx));
        })
        .unwrap();
    }
    enter_budget(cx, window, "384");
    click(cx, window, "close-preferences");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.preferences_form.is_none());
        assert_eq!(s.state.preferences.query_result_limit_mib, 256);
        assert_eq!(
            s.tabs[0].results[0].rows[0][0],
            Cell::Text("keep this result".into())
        );
    });
}
