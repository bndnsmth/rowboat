//! Portable palettes, actual preference widgets and isolated system observations.
use crate::theme_controller::{self as themes, PALETTES};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance, px, size,
};
use gpui_omarchy::{ActiveTheme, Theme};

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn click(cx: &mut TestAppContext, window: AnyWindowHandle, selector: &'static str) {
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(cx, window);
}
fn open_preferences(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
) {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| s.show_preferences(w, cx))
    })
    .unwrap();
    draw(cx, w);
}
fn white_draft(cx: &mut TestAppContext, w: AnyWindowHandle) {
    click(cx, w, "preferences-theme");
    cx.simulate_keystrokes(w, "end");
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let popup = visual.debug_bounds("omarchy-choice-popup").unwrap();
    let last = visual.debug_bounds("omarchy-choice-option-22").unwrap();
    let viewport = cx.update_window(w, |_, w, _| w.viewport_size()).unwrap();
    assert!(
        popup.left() >= px(0.)
            && popup.top() >= px(0.)
            && popup.right() <= viewport.width
            && popup.bottom() <= viewport.height,
        "Dropdown must fit the viewport: {popup:?}"
    );
    assert!(
        last.top() >= popup.top() && last.bottom() <= popup.bottom(),
        "End must scroll the last palette into view"
    );
    cx.simulate_keystrokes(w, "enter");
    draw(cx, w);
}

#[test]
fn pass21_all_embedded_palettes_parse_and_ids_cover_assets() {
    assert_eq!(PALETTES.len(), 22);
    for excluded in ["aether", "theme-tokyo-liftoff", "wallhaven-6dpzmq"] {
        assert!(themes::embedded(excluded).is_none());
    }
    let mut ids = std::collections::BTreeSet::new();
    for palette in PALETTES {
        assert!(ids.insert(palette.id));
        let parsed = Theme::from_colors_toml(palette.name, palette.colors).unwrap();
        assert_eq!(parsed.name.as_ref(), palette.name);
        assert_eq!(themes::embedded(palette.id), Some(parsed));
    }
    let files: std::collections::BTreeSet<_> =
        std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/themes"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.extension().is_some_and(|s| s == "toml"))
            .map(|p| p.file_stem().unwrap().to_str().unwrap().to_owned())
            .collect();
    assert_eq!(files, ids.into_iter().map(str::to_owned).collect());
}

#[test]
fn pass21_system_reader_recovers_from_missing_invalid_and_replaced_symlinks() {
    let home = tempfile::tempdir().unwrap();
    assert!(themes::load_omarchy(home.path()).is_none());
    let legacy = home.path().join(".config/omarchy/current");
    std::fs::create_dir_all(legacy.join("theme")).unwrap();
    std::fs::write(legacy.join("theme/colors.toml"), PALETTES[0].colors).unwrap();
    assert!(themes::load_omarchy(home.path()).is_some());
    let current = home.path().join(".local/state/omarchy/current");
    std::fs::create_dir_all(&current).unwrap();
    assert!(
        themes::load_omarchy(home.path()).is_none(),
        "broken modern current must not resurrect stale legacy colors"
    );
    let a = home.path().join("palette-a");
    let b = home.path().join("palette-b");
    for (path, id) in [(&a, "tokyo-night"), (&b, "white")] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("colors.toml"),
            PALETTES.iter().find(|p| p.id == id).unwrap().colors,
        )
        .unwrap();
    }
    std::os::unix::fs::symlink(&a, current.join("theme")).unwrap();
    let dark = themes::load_omarchy(home.path()).unwrap();
    std::os::unix::fs::symlink(&b, current.join("theme-next")).unwrap();
    std::fs::rename(current.join("theme-next"), current.join("theme")).unwrap();
    let light = themes::load_omarchy(home.path()).unwrap();
    assert_ne!(dark.background, light.background);
    std::fs::write(b.join("colors.toml"), "background = 'broken'").unwrap();
    assert!(themes::load_omarchy(home.path()).is_none());
    std::fs::write(b.join("colors.toml"), PALETTES[0].colors).unwrap();
    assert!(themes::load_omarchy(home.path()).is_some());
    std::fs::write(b.join("colors.toml"), " ".repeat(65_537)).unwrap();
    assert!(themes::load_omarchy(home.path()).is_none());
}

#[gpui_kit::test]
fn pass21_system_precedence_live_appearance_and_late_observation_cannot_override_choice(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        crate::test_theme::init(cx);
        themes::ensure(cx);
        themes::appearance_changed(WindowAppearance::Light, cx);
        assert_eq!(cx.omarchy(), &Theme::flexoki_light());
        themes::appearance_changed(WindowAppearance::Dark, cx);
        assert_eq!(cx.omarchy(), &Theme::tokyo_night());
        let omarchy = themes::embedded("gruvbox").unwrap();
        themes::system_observed(Some(omarchy.clone()), cx);
        themes::appearance_changed(WindowAppearance::Light, cx);
        assert_eq!(cx.omarchy(), &omarchy);
        themes::set_choice("nord", cx);
        let explicit = cx.omarchy().clone();
        themes::system_observed(Some(themes::embedded("white").unwrap()), cx);
        themes::appearance_changed(WindowAppearance::VibrantDark, cx);
        assert_eq!(cx.omarchy(), &explicit);
        themes::set_choice("system", cx);
        assert_eq!(cx.omarchy(), &themes::embedded("white").unwrap());
        themes::system_observed(None, cx);
        assert_eq!(cx.omarchy(), &Theme::tokyo_night());
        themes::appearance_changed(WindowAppearance::VibrantLight, cx);
        assert_eq!(cx.omarchy(), &Theme::flexoki_light());
        themes::set_choice("palette-from-a-future-release", cx);
        assert_eq!(cx.omarchy(), &Theme::flexoki_light());
        assert!(themes::description(cx).contains("unavailable"));
    });
}

#[gpui_kit::test]
fn pass21_preferences_dropdown_cancel_save_stale_readonly_and_narrow_keyboard(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, workspaces) = crate::pass14_tools::workspace(
        cx,
        runtime.handle(),
        vec![rowboat_core::ConnectionProfile::default()],
    );
    let s = &workspaces[0];
    cx.simulate_window_resize(w, size(px(800.), px(480.)));
    open_preferences(cx, w, s);
    let before = cx.read(|cx| {
        (
            cx.omarchy().clone(),
            s.read(cx).shared.borrow().revision,
            s.read(cx).tabs[0].editor.clone(),
        )
    });
    white_draft(cx, w);
    cx.read(|cx| assert_eq!(cx.omarchy(), &before.0));
    let mut visual = VisualTestContext::from_window(w, cx);
    let save = visual.debug_bounds("save-preferences").unwrap();
    assert!(save.bottom() <= px(480.) && save.right() <= px(800.));
    click(cx, w, "close-preferences");
    cx.read(|cx| {
        assert_eq!(s.read(cx).shared.borrow().revision, before.1);
        assert_eq!(s.read(cx).shared.borrow().state.preferences.theme, "system");
        assert_eq!(cx.omarchy(), &before.0);
    });
    open_preferences(cx, w, s);
    // Escape closes only the open dropdown; the second Escape cancels the form.
    click(cx, w, "preferences-theme");
    cx.simulate_keystrokes(w, "escape");
    draw(cx, w);
    cx.read(|cx| {
        assert!(s.read(cx).preferences_form.is_some() && !s.read(cx).preferences_theme_open(cx))
    });
    cx.simulate_keystrokes(w, "escape");
    draw(cx, w);
    cx.read(|cx| assert!(s.read(cx).preferences_form.is_none()));
    // Exercise the application's Escape binding as well as the component default.
    cx.update(|cx| {
        cx.bind_keys([gpui_kit::KeyBinding::new(
            "escape",
            crate::app::Dismiss,
            Some("Rowboat"),
        )])
    });
    open_preferences(cx, w, s);
    click(cx, w, "preferences-theme");
    cx.simulate_keystrokes(w, "escape");
    draw(cx, w);
    cx.read(|cx| {
        assert!(s.read(cx).preferences_form.is_some() && !s.read(cx).preferences_theme_open(cx))
    });
    click(cx, w, "preferences-theme");
    cx.simulate_keystrokes(w, "tab shift-tab");
    draw(cx, w);
    cx.update_window(w, |_, w, cx| {
        assert!(s.read(cx).modal_focus.contains_focused(w, cx))
    })
    .unwrap();
    cx.simulate_keystrokes(w, "escape");
    draw(cx, w);
    cx.read(|cx| assert!(s.read(cx).preferences_form.is_none()));
    open_preferences(cx, w, s);
    white_draft(cx, w);
    click(cx, w, "save-preferences");
    cx.read(|cx| {
        assert_eq!(cx.omarchy(), &themes::embedded("white").unwrap());
        let s = s.read(cx);
        assert_eq!(s.shared.borrow().state.preferences.theme, "white");
        assert_eq!(s.tabs[0].editor, before.2);
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.database.is_none() && s.preferences_form.is_none());
    });
    open_preferences(cx, w, s);
    click(cx, w, "preferences-theme");
    cx.simulate_keystrokes(w, "home enter");
    draw(cx, w);
    cx.update(|cx| {
        s.update(cx, |s, _| {
            s.edit_state(|state| state.preferences.theme = "nord".into())
        })
    });
    click(cx, w, "save-preferences");
    cx.read(|cx| {
        assert!(s.read(cx).error.as_ref().unwrap().contains("changed"));
        assert_eq!(s.read(cx).shared.borrow().state.preferences.theme, "nord");
    });
    click(cx, w, "close-preferences");
    cx.update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
        .unwrap();
    open_preferences(cx, w, s);
    white_draft(cx, w);
    cx.update(|cx| s.update(cx, |s, _| s.shared.borrow_mut().writable = false));
    click(cx, w, "save-preferences");
    cx.read(|cx| {
        assert_eq!(s.read(cx).shared.borrow().state.preferences.theme, "nord");
        assert_eq!(cx.omarchy(), &themes::embedded("nord").unwrap());
        assert!(s.read(cx).preferences_form.is_some());
    });
}

#[gpui_kit::test]
fn pass21_persisted_startup_and_shared_satellite_theme(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, workspaces) = crate::pass14_tools::workspace_with_preferences(
        cx,
        runtime.handle(),
        vec![rowboat_core::ConnectionProfile::default()],
        rowboat_services::Preferences {
            theme: "nord".into(),
            ..Default::default()
        },
    );
    cx.read(|cx| assert_eq!(cx.omarchy(), &themes::embedded("nord").unwrap()));
    let s = &workspaces[0];
    cx.read(|cx| assert_eq!(s.read(cx).shared.borrow().revision, 0));
    let id = cx.update(|cx| {
        s.update(cx, |s, _| {
            s.shell_tx
                .send(crate::workspace_shell::Command::Move {
                    route: s.tx.route,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
            s.workspace_id
        })
    });
    cx.update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
        .unwrap();
    let satellite = cx.read(|cx| shell.read(cx).test_workspace_host(id).unwrap());
    open_preferences(cx, satellite, s);
    white_draft(cx, satellite);
    click(cx, satellite, "save-preferences");
    for window in [w, satellite] {
        cx.update_window(window, |_, w, cx| {
            w.draw(cx).clear(cx);
            assert_eq!(cx.omarchy(), &themes::embedded("white").unwrap());
        })
        .unwrap();
    }
    // Returning to System resumes native following across both hosts.
    open_preferences(cx, satellite, s);
    click(cx, satellite, "preferences-theme");
    cx.simulate_keystrokes(satellite, "home enter");
    draw(cx, satellite);
    click(cx, satellite, "save-preferences");
    cx.update(|cx| themes::appearance_changed(WindowAppearance::Light, cx));
    cx.read(|cx| assert_eq!(cx.omarchy(), &Theme::flexoki_light()));
    cx.update(|cx| themes::appearance_changed(WindowAppearance::Dark, cx));
    cx.read(|cx| assert_eq!(cx.omarchy(), &Theme::tokyo_night()));
}
