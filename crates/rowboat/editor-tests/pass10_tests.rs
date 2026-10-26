use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Focusable, TestAppContext, base::input::EditorState,
};
use std::time::Duration;

fn settle(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(180));
    cx.run_until_parked();
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    cx.run_until_parked();
}
fn reset(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
    text: &str,
) -> Entity<EditorState> {
    let e = cx
        .update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.dismiss_completions(cx);
                let e = s.tabs[s.active_tab].editor.clone();
                e.update(cx, |e, cx| {
                    e.set_value(text, w, cx);
                    e.set_selected_range(text.len()..text.len(), cx);
                    e.focus(w, cx);
                });
                e
            })
        })
        .unwrap();
    settle(cx, w);
    e
}
fn type_text(cx: &mut TestAppContext, w: AnyWindowHandle, text: &str) {
    for c in text.chars() {
        cx.simulate_input(w, &c.to_string());
    }
}
fn text(cx: &TestAppContext, e: &Entity<EditorState>) -> String {
    cx.read(|cx| e.read(cx).value().to_string())
}
fn open(cx: &TestAppContext, e: &Entity<EditorState>) -> bool {
    cx.read(|cx| e.read(cx).completion_menu_state().open)
}

#[gpui_kit::test]
fn pass10_passive_and_pending_enter_format_without_completion(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 101, runtime.handle());
    let e = reset(cx, w, &s, "");
    type_text(cx, w, "sel");
    settle(cx, w);
    assert!(open(cx, &e));
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    assert_eq!(text(cx, &e), "sel\n");
    assert!(!open(cx, &e));
    for key in ["enter", "enter", "tab", "enter"] {
        cx.simulate_keystrokes(w, key);
        settle(cx, w);
        assert!(!open(cx, &e));
    }
    assert!(text(cx, &e).starts_with("sel\n\n\n"));
    reset(cx, w, &s, "");
    type_text(cx, w, "sel");
    // Enter before the 150 ms response, then let obsolete work complete.
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    assert_eq!(text(cx, &e), "sel\n");
    assert!(!open(cx, &e));
    reset(cx, w, &s, "");
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    assert!(open(cx, &e));
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    assert_eq!(text(cx, &e), "\n");
    assert!(!open(cx, &e));
}

#[gpui_kit::test]
fn pass10_tab_arrows_enter_shift_and_focus(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 102, runtime.handle());
    let e = reset(cx, w, &s, "");
    for key in ["tab", "down", "up"] {
        reset(cx, w, &s, "");
        type_text(cx, w, "o");
        settle(cx, w);
        assert!(open(cx, &e));
        let expected = cx.read(|cx| {
            let items = &e.read(cx).completion_menu_state().items;
            assert!(items.len() > 1);
            items[if key == "up" { items.len() - 1 } else { 0 }]
                .label
                .clone()
        });
        if key == "tab" {
            cx.simulate_keystrokes(w, "tab");
        } else {
            cx.simulate_keystrokes(w, key);
            cx.read(|cx| {
                assert!(
                    s.read(cx).advanced.completion_menus[&s.read(cx).tabs[0].id]
                        .borrow()
                        .deliberate
                )
            });
            cx.simulate_keystrokes(w, "enter");
        }
        settle(cx, w);
        assert_eq!(text(cx, &e), expected);
        assert!(!open(cx, &e));
        cx.update_window(w, |_, w, cx| {
            assert!(e.read(cx).focus_handle(cx).is_focused(w))
        })
        .unwrap();
        cx.simulate_keystrokes(w, "ctrl-z");
        settle(cx, w);
        assert_eq!(text(cx, &e), "o");
    }
    reset(cx, w, &s, "");
    type_text(cx, w, "o");
    settle(cx, w);
    cx.simulate_keystrokes(w, "down");
    cx.simulate_keystrokes(w, "shift-enter");
    settle(cx, w);
    assert_eq!(text(cx, &e), "o\n");
    reset(cx, w, &s, "");
    type_text(cx, w, "o");
    settle(cx, w);
    cx.simulate_keystrokes(w, "down");
    cx.simulate_keystrokes(w, "enter");
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    assert!(text(cx, &e).ends_with('\n'));
    assert_eq!(text(cx, &e).matches('\n').count(), 1);
}

#[gpui_kit::test]
fn pass10_refilter_caret_escape_and_context_disarm_enter(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 103, runtime.handle());
    let e = reset(cx, w, &s, "");
    for pending in [false, true] {
        reset(cx, w, &s, "");
        type_text(cx, w, "s");
        settle(cx, w);
        cx.simulate_keystrokes(w, "down");
        cx.simulate_input(w, "e");
        if !pending {
            settle(cx, w);
            assert!(open(cx, &e));
        }
        cx.simulate_keystrokes(w, "enter");
        settle(cx, w);
        assert_eq!(text(cx, &e), "se\n");
    }
    for kind in 0..4 {
        reset(cx, w, &s, "");
        type_text(cx, w, "sel");
        settle(cx, w);
        cx.simulate_keystrokes(w, "down");
        match kind {
            0 => {
                cx.simulate_keystrokes(w, "left");
                cx.simulate_keystrokes(w, "right");
            }
            1 => {
                cx.simulate_keystrokes(w, "escape");
                settle(cx, w);
                assert!(!open(cx, &e));
            }
            2 => {
                cx.update(|cx| {
                    s.update(cx, |s, cx| {
                        s.namespace.request += 1;
                        s.invalidate_completions(cx);
                    })
                });
            }
            _ => {
                cx.simulate_keystrokes(w, "ctrl-space");
                settle(cx, w);
            }
        }
        cx.simulate_keystrokes(w, "enter");
        settle(cx, w);
        assert_eq!(text(cx, &e), "sel\n", "invalidation {kind}");
    }
}

#[gpui_kit::test]
fn pass10_activation_ignores_whitespace_bulk_comments_strings_and_right(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 104, runtime.handle());
    let e = reset(cx, w, &s, "");
    for input in [" ", "\n", "\t", ";", ",", "(", "'", "\"", "`", ""] {
        reset(cx, w, &s, "");
        cx.simulate_input(w, input);
        settle(cx, w);
        assert!(!open(cx, &e), "Unexpected auto popup for {input:?}");
    }
    for input in ["sel", "SELECT * FROM ", "  \n\t"] {
        reset(cx, w, &s, "");
        cx.update(|cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(input.into())));
        cx.simulate_keystrokes(w, "ctrl-v");
        settle(cx, w);
        assert_eq!(text(cx, &e), input);
        assert!(!open(cx, &e), "Paste opened completion");
    }
    for prefix in ["-- ", "/* ", "SELECT '"] {
        reset(cx, w, &s, prefix);
        type_text(cx, w, "sel");
        settle(cx, w);
        assert!(!open(cx, &e), "Popup in {prefix}");
    }
    reset(cx, w, &s, "");
    type_text(cx, w, "s");
    settle(cx, w);
    assert!(open(cx, &e));
    cx.simulate_input(w, " ");
    settle(cx, w);
    let mut visual = gpui_kit::VisualTestContext::from_window(w, cx);
    assert!(visual.debug_bounds("sql-completion-popup").is_none());
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    assert_eq!(text(cx, &e), "s \n");
    assert!(!open(cx, &e));
    reset(cx, w, &s, "");
    type_text(cx, w, "sel");
    settle(cx, w);
    cx.simulate_keystrokes(w, "right");
    settle(cx, w);
    assert_eq!(text(cx, &e), "sel");
    let runs = cx.read(|cx| s.read(cx).query_run_invocations);
    cx.simulate_keystrokes(w, "ctrl-enter");
    settle(cx, w);
    assert_eq!(text(cx, &e), "sel");
    assert!(!open(cx, &e));
    cx.read(|cx| assert_eq!(s.read(cx).query_run_invocations, runs + 1));
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("Connect to a database")
        )
    });
}

#[gpui_kit::test]
fn pass10_armed_choice_cannot_cross_tab_or_host_and_snippet_tab_wins(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (mut w, s) = crate::workbench_tests::workspace(cx, 105, runtime.handle());
    let e = reset(cx, w, &s, "");
    for moved in [false, true] {
        reset(cx, w, &s, "");
        type_text(cx, w, "sel");
        settle(cx, w);
        cx.simulate_keystrokes(w, "down");
        if moved {
            w = cx.update(|cx| {
                let new = gpui_kit::open_window(Default::default(), cx, |w, cx| {
                    s.update(cx, |s, cx| s.rehost(w, cx));
                    s.clone()
                })
                .unwrap()
                .0;
                w.update(cx, |_, w, _| w.remove_window()).unwrap();
                new
            });
        } else {
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    let original = s.tabs[s.active_tab].id;
                    s.new_query(Some("other".into()), w, cx);
                    s.focus_query_tab(original, cx);
                })
            })
            .unwrap();
        }
        cx.update_window(w, |_, w, cx| e.update(cx, |e, cx| e.focus(w, cx)))
            .unwrap();
        settle(cx, w);
        cx.simulate_keystrokes(w, "enter");
        settle(cx, w);
        assert_eq!(text(cx, &e), "sel\n");
    }
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, _| {
            s.shared
                .borrow_mut()
                .state
                .snippet_templates
                .push(rowboat_services::SnippetTemplate {
                    id: "pass10".into(),
                    name: "Pass10".into(),
                    trigger: "trig".into(),
                    body: "${one:sel} ${two:from}${0}".into(),
                });
        })
    })
    .unwrap();
    reset(cx, w, &s, "trig");
    cx.simulate_keystrokes(w, "ctrl-shift-space");
    settle(cx, w);
    cx.simulate_input(w, "s");
    settle(cx, w);
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    assert!(!open(cx, &e));
    cx.simulate_keystrokes(w, "tab");
    cx.read(|cx| assert_eq!(e.read(cx).selected_value(), "from"));
    cx.simulate_keystrokes(w, "shift-tab");
    cx.read(|cx| assert_eq!(e.read(cx).selected_value(), "s"));
    assert_eq!(text(cx, &e), "s from");
}
