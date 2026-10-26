use gpui_kit::{AppContext, TestAppContext, VisualTestContext, px, size};
use std::time::Duration;
fn settle(cx: &mut TestAppContext, w: gpui_kit::AnyWindowHandle) {
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(180));
    cx.run_until_parked();
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    cx.run_until_parked();
}
#[gpui_kit::test]
fn pass9_typing_opens_native_caret_popup_refilters_accepts_and_undoes(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 91, runtime.handle());
    cx.simulate_window_resize(w, size(px(980.), px(640.)));
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("", w, cx);
                e.focus(w, cx);
            })
        })
    })
    .unwrap();
    cx.simulate_input(w, "s");
    settle(cx, w);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(!s.busy);
        assert!(!s.modal_open());
        assert!(s.tabs[0].editor.read(cx).completion_menu_state().open);
    });
    cx.simulate_input(w, "e");
    cx.simulate_input(w, "l");
    settle(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds("sql-completion-popup")
        .expect("typed popup");
    cx.read(|cx| {
        let e = s.read(cx).tabs[0].editor.read(cx);
        let caret = e.prepaint_caret_geometry().unwrap().cursor_bounds();
        assert!((bounds.left() - caret.left()).abs() < px(2.));
        assert!(bounds.bottom() <= px(640.));
        assert_eq!(
            e.completion_menu_state().items[0].label.to_uppercase(),
            "SELECT"
        );
    });
    cx.simulate_keystrokes(w, "tab");
    settle(cx, w);
    cx.read(|cx| {
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).value().to_uppercase(),
            "SELECT"
        )
    });
    cx.simulate_keystrokes(w, "ctrl-z");
    settle(cx, w);
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].editor.read(cx).value().as_ref(), "sel"));
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    cx.simulate_keystrokes(w, "escape");
    settle(cx, w);
    cx.read(|cx| {
        assert!(
            !s.read(cx).tabs[0]
                .editor
                .read(cx)
                .completion_menu_state()
                .open
        )
    });
    cx.simulate_input(w, "e");
    settle(cx, w);
    cx.read(|cx| {
        assert!(
            s.read(cx).tabs[0]
                .editor
                .read(cx)
                .completion_menu_state()
                .open
        )
    });
}

#[gpui_kit::test]
fn pass9_popup_tracks_scrolled_caret_and_keyboard_choice_at_small_window(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 97, runtime.handle());
    cx.simulate_window_resize(w, size(px(980.), px(640.)));
    settle(cx, w);
    // Execution/library bands consume part of the default editor panel. Give
    // the input enough room above its scrolled bottom-edge caret for the popup;
    // a shorter input correctly uses the viewport-clamped fallback instead.
    cx.update_window(w, |_, w, cx| {
        let split = s.read(cx).editor_split.clone();
        split.update(cx, |split, cx| split.resize_panel(0, px(320.), w, cx));
    })
    .unwrap();
    let prefix = format!("{}{}", "-- preceding line\n".repeat(40), " ".repeat(75));
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value(prefix.clone(), w, cx);
                e.set_selected_range(prefix.len()..prefix.len(), cx);
                e.focus(w, cx);
            });
        })
    })
    .unwrap();
    settle(cx, w);
    cx.simulate_keystrokes(w, "ctrl-end");
    cx.simulate_input(w, "o");
    settle(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds("sql-completion-popup")
        .unwrap_or_else(|| {
            cx.read(|cx| {
                let e = s.read(cx).tabs[0].editor.read(cx);
                let g = e.prepaint_caret_geometry().unwrap();
                panic!(
                    "Scrolled caret popup: open={} cursor={:?} input={:?}",
                    e.completion_menu_state().open,
                    g.cursor_bounds(),
                    g.input_bounds()
                );
            })
        });
    cx.read(|cx| {
        let e = s.read(cx).tabs[0].editor.read(cx);
        let g = e.prepaint_caret_geometry().unwrap();
        let mut caret = g.cursor_bounds();
        caret.origin += g.scroll_offset();
        assert!(g.scroll_offset().y < px(0.));
        assert!(g.input_bounds().intersects(&caret));
        assert!(bounds.left() >= px(0.) && bounds.right() <= px(980.));
        assert!(bounds.top() >= px(0.) && bounds.bottom() <= px(640.));
        assert!(
            (g.input_bounds().bottom() - caret.bottom()).abs() < caret.size.height,
            "Caret must remain at the input's bottom edge: caret={caret:?}, input={:?}",
            g.input_bounds()
        );
        assert!(
            caret.bottom() + px(3.) + bounds.size.height > g.input_bounds().bottom(),
            "Popup must not fit below the caret inside the input"
        );
        assert!(
            caret.top() - bounds.size.height - px(3.) >= g.input_bounds().top(),
            "Popup must fit above the caret inside the input: popup={bounds:?}, caret={caret:?}, input={:?}",
            g.input_bounds()
        );
        assert!(
            bounds.bottom() <= caret.top() - px(3.),
            "Popup did not flip above bottom-edge caret: popup={bounds:?}, caret={caret:?}, input={:?}, scroll={:?}",
            g.input_bounds(), g.scroll_offset()
        );
        assert!(e.completion_menu_state().items.len() > 1);
    });
    cx.simulate_keystrokes(w, "down");
    let expected = cx.read(|cx| {
        s.read(cx).tabs[0]
            .editor
            .read(cx)
            .completion_menu_state()
            .items[0]
            .label
            .clone()
    });
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    cx.read(|cx| {
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            format!("{prefix}{expected}")
        )
    });
    cx.simulate_keystrokes(w, "ctrl-z");
    settle(cx, w);
    cx.read(|cx| {
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            format!("{prefix}o")
        )
    });
}

#[gpui_kit::test]
fn pass9_ime_utf8_click_and_empty_enter_preserve_native_editing(cx: &mut TestAppContext) {
    use gpui_kit::{ElementInputHandler, InputHandler as _};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 93, runtime.handle());
    let prefix = "-- 🦆 café\n";
    let editor = cx
        .update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.tabs[0].editor.update(cx, |e, cx| {
                    e.set_value(prefix, w, cx);
                    e.set_selected_range(prefix.len()..prefix.len(), cx);
                    e.focus(w, cx);
                });
                s.tabs[0].editor.clone()
            })
        })
        .unwrap();
    settle(cx, w);
    cx.update_window(w, |_, w, cx| {
        let mut handler = ElementInputHandler::new(editor.read(cx).input_bounds(), editor.clone());
        handler.replace_and_mark_text_in_range(None, "sel", Some(3..3), w, cx);
    })
    .unwrap();
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    cx.read(|cx| {
        assert!(
            !editor.read(cx).completion_menu_state().open,
            "IME preedit opened completion"
        )
    });
    cx.update_window(w, |_, w, cx| {
        let mut handler = ElementInputHandler::new(editor.read(cx).input_bounds(), editor.clone());
        handler.replace_text_in_range(None, "sel", w, cx);
    })
    .unwrap();
    // Multi-character IME commits do not auto-open; explicitly request afterward.
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let choice = visual.debug_bounds("sql-candidate-0").unwrap();
    visual.simulate_click(choice.center(), Default::default());
    settle(cx, w);
    cx.read(|cx| assert_eq!(editor.read(cx).value().as_ref(), format!("{prefix}SELECT")));
    cx.simulate_keystrokes(w, "ctrl-z");
    settle(cx, w);
    cx.read(|cx| assert_eq!(editor.read(cx).value().as_ref(), format!("{prefix}sel")));
    // A new edit invalidates the old offered item before its debounce completes.
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    cx.simulate_input(w, "zz");
    cx.simulate_keystrokes(w, "enter");
    settle(cx, w);
    cx.read(|cx| {
        let sql = editor.read(cx).value();
        assert!(
            sql.contains("selzz\n"),
            "Enter swallowed by stale/empty completion: {sql}"
        );
    });
}

#[gpui_kit::test]
fn pass9_pending_completion_is_invalidated_by_caret_close_context_and_host(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (mut w, s) = crate::workbench_tests::workspace(cx, 94, runtime.handle());
    for invalidation in 0..4 {
        cx.update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.close_requested = false;
                s.tabs[0].editor.update(cx, |e, cx| {
                    e.set_value("", w, cx);
                    e.focus(w, cx);
                });
            })
        })
        .unwrap();
        for ch in ["s", "e", "l"] {
            cx.simulate_input(w, ch);
        }
        cx.run_until_parked();
        match invalidation {
            0 => cx.simulate_keystrokes(w, "left"),
            1 => cx.update(|cx| s.update(cx, |s, _| s.close_requested = true)),
            2 => cx.update(|cx| {
                s.update(cx, |s, cx| {
                    s.tx.route.generation += 1;
                    s.invalidate_completions(cx);
                })
            }),
            _ => {
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
            }
        }
        settle(cx, w);
        cx.read(|cx| {
            assert!(
                !s.read(cx).tabs[0]
                    .editor
                    .read(cx)
                    .completion_menu_state()
                    .open,
                "Obsolete request survived invalidation {invalidation}"
            )
        });
    }
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| e.focus(w, cx))
        })
    })
    .unwrap();
    cx.simulate_keystrokes(w, "ctrl-space");
    settle(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    assert!(
        visual.debug_bounds("sql-completion-popup").is_some(),
        "New host cannot open completion"
    );
}

fn wait(
    cx: &mut TestAppContext,
    w: gpui_kit::AnyWindowHandle,
    s: &gpui_kit::Entity<crate::app::Workbench>,
    menu: bool,
) {
    settle(cx, w);
    for _ in 0..500 {
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let ready = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    if menu {
                        s.tabs[s.active_tab]
                            .editor
                            .read(cx)
                            .completion_menu_state()
                            .open
                    } else {
                        !s.busy
                    }
                })
            })
            .unwrap();
        if ready {
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cx.read(|cx| {
        let s = s.read(cx);
        panic!(
            "Completion/query did not settle: SQL={} note={} busy={} error={:?}",
            s.tabs[s.active_tab].editor.read(cx).value(),
            s.advanced.inline.note,
            s.busy,
            s.error
        );
    });
}
#[gpui_kit::test]
#[ignore = "Explicit disposable PG15435/MySQL13309"]
fn pass9_live_typed_relations_alias_columns_cached_refilter_and_ctrl_enter(
    cx: &mut TestAppContext,
) {
    use rowboat_core::{Engine, sql::quote_ident};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture(engine))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let snapshot=runtime.handle().block_on(async{
                let q=quote_ident(engine,&schema);
                db.query(&format!("CREATE TABLE {q}.customers(id INT PRIMARY KEY,name TEXT,nickname TEXT); INSERT INTO {q}.customers VALUES(1,'one','first')"),0).await.unwrap();
                db.query(&if engine==Engine::Postgres{format!("SET search_path TO {q}")}else{format!("USE {q}")},0).await.unwrap();
                db.namespace_catalog(None).await.unwrap()
            });
            let (w, s) = crate::workbench_tests::workspace(cx, 92, runtime.handle());
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile.clone());
                    s.endpoint = Some(profile.clone());
                    s.database = Some(db.clone());
                    s.session_ready = true;
                    s.tabs[0].connection_id = Some(profile.id.clone());
                    s.accept_namespace(snapshot, None, cx);
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT * FROM ", w, cx);
                        e.set_selected_range(14..14, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            for ch in ["c", "u", "s"] {
                cx.simulate_input(w, ch);
            }
            wait(cx, w, &s, true);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(!s.busy);
                assert!(!s.modal_open());
                let items = &s.tabs[0].editor.read(cx).completion_menu_state().items;
                assert!(
                    items.iter().any(|i| i.label.contains("customers")),
                    "{items:?}"
                );
            });
            cx.simulate_keystrokes(w, "tab");
            settle(cx, w);
            cx.read(|cx| {
                assert!(
                    s.read(cx).tabs[0]
                        .editor
                        .read(cx)
                        .value()
                        .contains("customers")
                )
            });
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT c. FROM customers c", w, cx);
                        e.set_selected_range(9..9, cx);
                        e.focus(w, cx);
                    })
                })
            })
            .unwrap();
            cx.simulate_input(w, "n");
            wait(cx, w, &s, true);
            cx.read(|cx| {
                let items = &s.read(cx).tabs[0]
                    .editor
                    .read(cx)
                    .completion_menu_state()
                    .items;
                assert!(items.iter().any(|i| i.label == "name"));
                assert!(items.iter().any(|i| i.label == "nickname"));
            });
            let reads = cx.read(|cx| s.read(cx).advanced.inline.metadata_reads());
            assert!(reads < 10);
            cx.simulate_input(w, "a");
            wait(cx, w, &s, true);
            cx.read(|cx| {
                assert_eq!(
                    s.read(cx).advanced.inline.metadata_reads(),
                    reads,
                    "Refilter queried metadata again"
                );
            });
            cx.simulate_keystrokes(w, "tab");
            settle(cx, w);
            cx.read(|cx| {
                assert!(
                    s.read(cx).tabs[0]
                        .editor
                        .read(cx)
                        .value()
                        .contains(&format!("c.{}", quote_ident(engine, "name")))
                )
            });
            // Ctrl+Enter runs exactly once and closes the same inline surface.
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT 1 AS ", w, cx);
                        e.set_selected_range(12..12, cx);
                        e.focus(w, cx);
                    })
                })
            })
            .unwrap();
            for ch in ["s", "e", "l"] {
                cx.simulate_input(w, ch);
            }
            wait(cx, w, &s, true);
            let history = cx.read(|cx| s.read(cx).shared.borrow().state.history.len());
            cx.simulate_keystrokes(w, "ctrl-enter");
            wait(cx, w, &s, false);
            cx.read(|cx| {
                let s = s.read(cx);
                assert_eq!(s.shared.borrow().state.history.len(), history + 1);
                assert_eq!(s.tabs[0].results[0].rows[0][0].display(), "1");
                assert!(!s.tabs[0].editor.read(cx).completion_menu_state().open);
            });
            cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
        }));
        runtime.handle().block_on(async {
            let _ = db
                .query(
                    if engine == Engine::Postgres {
                        "SET search_path TO public"
                    } else {
                        "USE rowboat"
                    },
                    0,
                )
                .await;
            crate::pass5_tests::cleanup(&db, &schema).await.unwrap();
        });
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
