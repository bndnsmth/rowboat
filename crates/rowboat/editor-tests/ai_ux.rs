//! Native element geometry and pointer interactions, with disposable data only.
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Modifiers, MouseButton, Pixels, TestAppContext,
    VisualTestContext, point, px, size,
};
use rowboat_core::{ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode};

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .unwrap();
}
fn bounds(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    selector: &'static str,
) -> Bounds<Pixels> {
    draw(cx, window);
    VisualTestContext::from_window(window, cx)
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"))
}
fn click(cx: &mut TestAppContext, window: AnyWindowHandle, selector: &'static str) {
    let at = bounds(cx, window, selector).center();
    VisualTestContext::from_window(window, cx).simulate_click(at, Modifiers::default());
    draw(cx, window);
}
fn drag(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    selector: &'static str,
    dx: f32,
    dy: f32,
) {
    let from = bounds(cx, window, selector).center();
    let to = point(from.x + px(dx), from.y + px(dy));
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    visual.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    draw(cx, window);
}

#[gpui_kit::test]
fn ai_ux_preferences_resize_maximize_restore_and_keep_drafts(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 2501, runtime.handle());
    cx.simulate_window_resize(window, size(px(1200.), px(900.)));
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.show_preferences(window, cx);
        })
    })
    .unwrap();
    let before = bounds(cx, window, "dialog-frame-preferences");
    drag(cx, window, "dialog-resize-preferences", 60., 35.);
    let resized = bounds(cx, window, "dialog-frame-preferences");
    assert!(
        resized.size.width > before.size.width + px(80.),
        "{before:?} → {resized:?}"
    );
    assert!(resized.size.height > before.size.height + px(40.));
    click(cx, window, "dialog-maximize-preferences");
    let maximized = bounds(cx, window, "dialog-frame-preferences");
    assert!(maximized.size.width > resized.size.width);
    click(cx, window, "dialog-maximize-preferences");
    assert_eq!(
        bounds(cx, window, "dialog-frame-preferences").size,
        resized.size
    );
    click(cx, window, "dialog-resize-preferences");
    cx.simulate_keystrokes(window, "left");
    let keyboard_resized = bounds(cx, window, "dialog-frame-preferences");
    assert_eq!(keyboard_resized.size.width, resized.size.width - px(32.));
    cx.simulate_keystrokes(window, "shift-right");
    assert_eq!(
        bounds(cx, window, "dialog-frame-preferences").size.width,
        keyboard_resized.size.width + px(8.)
    );
    click(cx, window, "preferences-tab-ai");
    click(cx, window, "ai-enabled");
    cx.simulate_window_resize(window, size(px(800.), px(480.)));
    let constrained = bounds(cx, window, "dialog-frame-preferences");
    assert!(
        constrained.left() >= px(0.)
            && constrained.right() <= px(800.)
            && constrained.bottom() <= px(480.)
    );
    cx.simulate_window_resize(window, size(px(1200.), px(900.)));
    click(cx, window, "save-preferences");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.shared.borrow().state.preferences.ai.enabled);
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.state.history.is_empty());
    });
}

#[gpui_kit::test]
fn ai_ux_global_entry_is_top_right_and_does_not_run_sql_or_send_context(cx: &mut TestAppContext) {
    let fixture = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: fixture.path().join("fixture.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime
        .handle()
        .block_on(Database::connect(&profile, ""))
        .unwrap();
    let (window, _shell, workspaces) =
        crate::pass14_tools::workspace(cx, runtime.handle(), vec![profile.clone()]);
    let entity = &workspaces[0];
    cx.simulate_window_resize(window, size(px(1200.), px(900.)));
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, _cx| {
            s.database = Some(db.clone());
            s.profile = Some(profile);
            s.session_ready = true;
            s.edit_state(|state| state.preferences.ai.enabled = true);
        })
    })
    .unwrap();
    let entry = bounds(cx, window, "global-ai");
    assert!(
        entry.left() > px(1000.) && entry.bottom() <= px(40.),
        "{entry:?}"
    );
    click(cx, window, "global-ai");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.assistant.open && s.assistant.turns.is_empty());
        assert!(s.explain.is_none());
        assert_eq!(s.query_run_invocations, 0);
        assert!(!s.busy);
        assert!(s.state.history.is_empty());
    });
    let frame = bounds(cx, window, "assistant-panel");
    assert!(frame.size.width <= px(442.) && frame.right() <= px(1200.));
    drag(cx, window, "assistant-resize", -50., 0.);
    assert!(bounds(cx, window, "assistant-panel").size.width > frame.size.width);
    assert!(bounds(cx, window, "assistant-panel").right() <= px(1200.));
    assert!(bounds(cx, window, "assistant-close").right() <= px(1200.));
    cx.simulate_window_resize(window, size(px(900.), px(600.)));
    assert!(bounds(cx, window, "assistant-panel").right() <= px(900.));
    assert!(bounds(cx, window, "assistant-send").bottom() <= px(600.));
    click(cx, window, "assistant-close");
    cx.simulate_window_resize(window, size(px(1200.), px(900.)));
    click(cx, window, "manage-connections");
    let connections = bounds(cx, window, "dialog-frame-connections");
    drag(cx, window, "dialog-resize-connections", 40., 30.);
    assert!(bounds(cx, window, "dialog-frame-connections").size.width > connections.size.width);
    cx.read(|cx| assert_eq!(entity.read(cx).query_run_invocations, 0));
}
