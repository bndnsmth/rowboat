//! Pass 8 interaction regressions using real GPUI dispatch and explicit disposable fixtures.
use crate::workbench_tests::workspace;
use gpui_kit::{
    AppContext, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualTestContext, point,
    px, size,
};
use rowboat_core::{Cell, Column, QueryResult};

fn settle(
    cx: &mut TestAppContext,
    w: gpui_kit::AnyWindowHandle,
    e: &gpui_kit::Entity<crate::app::Workbench>,
) {
    for _ in 0..500 {
        let busy = cx
            .update_window(w, |_, w, cx| {
                e.update(cx, |s, cx| {
                    s.poll(w, cx);
                    s.busy
                })
            })
            .unwrap();
        if !busy {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("Workspace did not settle");
}

fn result() -> QueryResult {
    QueryResult {
        columns: (0..8)
            .map(|i| Column {
                name: format!("column_{i}"),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            })
            .collect(),
        rows: (0..100)
            .map(|r| (0..8).map(|c| Cell::Text(format!("{r}:{c}"))).collect())
            .collect(),
        ..Default::default()
    }
}
#[gpui_kit::test]
fn pass8_vertical_wheel_does_not_leak_into_horizontal_scroller(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = workspace(cx, 81, runtime.handle());
    cx.simulate_window_resize(w, size(px(980.), px(640.)));
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, _| s.tabs[0].results = vec![result()]);
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual.debug_bounds("data-grid").unwrap();
    // Result toolbars vary in height; dispatch inside the visible grid body.
    assert!(bounds.size.height > px(40.));
    let position = point(bounds.left() + px(100.), bounds.bottom() - px(20.));
    visual.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    cx.read(|cx| {
        let s = e.read(cx);
        assert_eq!(
            s.grid_x_scroll.offset().x,
            px(0.),
            "vertical wheel moved horizontally"
        );
        assert!(s.grid_scroll.0.borrow().base_handle.offset().y < px(0.));
    });
    let header = visual.debug_bounds("grid-header").unwrap();
    visual.simulate_event(ScrollWheelEvent {
        position: point(bounds.left() + px(100.), header.top() + px(10.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    cx.read(|cx| assert_eq!(e.read(cx).grid_x_scroll.offset().x, px(0.)));
    // Native Wayland/X11 normalize Shift-wheel to an X delta before GPUI dispatch.
    for precise in [false, true] {
        let old = cx.read(|cx| e.read(cx).grid_x_scroll.offset().x);
        let old_y = cx.read(|cx| e.read(cx).grid_scroll.0.borrow().base_handle.offset().y);
        visual.simulate_event(ScrollWheelEvent {
            position,
            delta: if precise {
                ScrollDelta::Pixels(point(px(-50.), px(0.)))
            } else {
                ScrollDelta::Lines(point(-3., 0.))
            },
            modifiers: Modifiers {
                shift: !precise,
                ..Default::default()
            },
            ..Default::default()
        });
        cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
            .unwrap();
        cx.read(|cx| assert!(e.read(cx).grid_x_scroll.offset().x < old));
        cx.read(|cx| {
            assert_eq!(
                e.read(cx).grid_scroll.0.borrow().base_handle.offset().y,
                old_y,
                "Horizontal wheel moved rows vertically"
            )
        });
    }
}

#[gpui_kit::test]
fn pass8_row_gutter_shift_range_copy_and_cell_mode_are_display_order_owned(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, e) = workspace(cx, 82, runtime.handle());
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, cx| {
            let mut rows = result();
            for (i, row) in rows.rows.iter_mut().enumerate() {
                if i % 2 == 0 {
                    row[0] = Cell::Text(format!("keep {i}:0"));
                }
            }
            s.tabs[0].results = vec![rows];
            s.result_filter
                .update(cx, |input, cx| input.set_value("keep", w, cx));
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    let first = visual.debug_bounds("row-header-0").unwrap();
    let third = visual.debug_bounds("row-header-4").unwrap();
    visual.simulate_click(first.center(), Modifiers::default());
    visual.simulate_click(
        third.center(),
        Modifiers {
            shift: true,
            ..Default::default()
        },
    );
    cx.update(|cx| {
        e.update(cx, |s, cx| {
            let range = s.grid_range(cx).unwrap();
            assert_eq!((range.rows(), range.columns()), (3, 8));
            s.copy_grid(false, cx);
        })
    });
    cx.update(|cx| {
        let text = cx.read_from_clipboard().unwrap().text().unwrap();
        assert!(text.starts_with("keep 0:0\t0:1"));
        assert!(text.contains("4:7"));
        assert!(!text.contains("1:0"));
    });
    // A normal cell selection exits row mode. Shift+Space widens only that row.
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, cx| {
            s.select_grid_cell(2, 3, false, cx);
            w.focus(&s.grid_focus, cx);
        })
    })
    .unwrap();
    cx.simulate_keystrokes(w, "shift-space");
    cx.read(|cx| {
        let s = e.read(cx);
        let range = s.grid_range(cx).unwrap();
        assert_eq!(
            (range.rows(), range.columns(), range.start().row),
            (1, 8, 1)
        );
    });
    cx.update_window(w, |_, w, cx| {
        e.update(cx, |s, cx| {
            s.result_filter.update(cx, |input, cx| input.focus(w, cx))
        })
    })
    .unwrap();
    cx.simulate_keystrokes(w, "shift-space");
    cx.read(|cx| {
        let s = e.read(cx);
        assert!(
            s.result_filter.read(cx).value().contains(' '),
            "Shift+Space did not reach the input"
        );
        assert!(
            s.selected.is_none(),
            "Changed filter must invalidate the old selection"
        );
    });
}

#[gpui_kit::test]
#[ignore = "Requires disposable PG15435/MySQL13309"]
fn pass8_live_namespace_picker_empty_scope_guards_and_failed_use_refresh(cx: &mut TestAppContext) {
    use rowboat_core::{Engine, RowChange, sql::quote_ident};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, a) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture(engine))
            .unwrap();
        let b = format!("{a}_b");
        let empty = format!("{a}_empty");
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let initial=runtime.handle().block_on(async {
                for name in [&b,&empty]{db.query(&format!("CREATE SCHEMA {}",quote_ident(engine,name)),0).await.unwrap();}
                for (name,id) in [(&a,1),(&b,2)] {db.query(&format!("CREATE TABLE {}.items(id INT PRIMARY KEY);INSERT INTO {}.items VALUES({id})",quote_ident(engine,name),quote_ident(engine,name)),0).await.unwrap();}
                db.query(&format!("CREATE VIEW {}.item_view AS SELECT * FROM {}.items",quote_ident(engine,&b),quote_ident(engine,&b)),0).await.unwrap();
                if engine==Engine::MySql {db.select_database(&a).await.unwrap();}else{db.query(&format!("SET search_path TO {}",quote_ident(engine,&a)),0).await.unwrap();}
                db.namespace_catalog(None).await.unwrap()
            });
            let (w, e) = workspace(cx, 83, runtime.handle());
            cx.update_window(w, |_, w, cx| {
                e.update(cx, |s, cx| {
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile.clone());
                    s.endpoint = Some(profile.clone());
                    s.database = Some(db.clone());
                    s.session_password = "rowboat-test".into();
                    s.session_ready = true;
                    s.tabs[0].connection_id = Some(profile.id.clone());
                    s.accept_namespace(initial, None, cx);
                    s.show_namespaces(w, cx);
                    assert!(s.namespace_choices(cx).contains(&empty));
                })
            })
            .unwrap();
            cx.simulate_input(w, &b);
            cx.simulate_keystrokes(w, "enter");
            settle(cx, w, &e);
            cx.read(|cx| {
                let s = e.read(cx);
                assert_eq!(s.namespace_scope(), Some(b.as_str()));
                assert!(s.catalog.iter().all(|t| t.table.schema == b));
                assert_eq!(s.tabs[0].editor.read(cx).value().as_ref(), "SELECT 83");
            });
            let actual = runtime
                .handle()
                .block_on(db.namespace_catalog(None))
                .unwrap();
            if engine == Engine::MySql {
                assert_eq!(actual.database, b);
            } else {
                assert_eq!(actual.current_schema, Some(a.clone()));
            }
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let mut visual = VisualTestContext::from_window(w, cx);
            let group = visual.debug_bounds("explorer-group-0").unwrap();
            let table_item = visual.debug_bounds("explorer-item-0").unwrap();
            let views = visual.debug_bounds("explorer-group-1").unwrap();
            let view_item = visual.debug_bounds("explorer-item-1").unwrap();
            assert!(
                group.bottom() <= table_item.top()
                    && table_item.bottom() <= views.top()
                    && views.bottom() <= view_item.top(),
                "Group controls must head their own children, not form a separate top toolbar"
            );
            visual.simulate_click(group.center(), Modifiers::default());
            cx.update_window(w, |_, w, cx| {
                e.update(cx, |s, cx| {
                    s.explorer_filter.update(cx, |input, cx| input.focus(w, cx))
                })
            })
            .unwrap();
            cx.simulate_keystrokes(w, "home enter");
            settle(cx, w, &e);
            cx.read(|cx| {
                let table = e.read(cx).table.as_ref().unwrap();
                assert_eq!(
                    (&table.schema, table.name.as_str()),
                    (&b, "item_view"),
                    "Collapsed tables remained in keyboard navigation"
                );
            });
            cx.update_window(w, |_, w, cx| {
                e.update(cx, |s, cx| {
                    s.explorer_page = 99;
                    s.explorer_cursor = 99;
                    s.table = Some(crate::pass5_tests::table(&b, "items"));
                    s.page = crate::app::Page::Table;
                    s.show_namespaces(w, cx);
                    s.choose_namespace(empty.clone(), false, w, cx);
                })
            })
            .unwrap();
            settle(cx, w, &e);
            cx.read(|cx| {
                let s = e.read(cx);
                assert!(s.catalog.is_empty());
                assert!(s.table.is_none());
                assert_eq!((s.explorer_page, s.explorer_cursor), (0, 0));
            });
            // A failed selector request does not relabel the session with its request.
            cx.update(|cx| {
                e.update(cx, |s, cx| {
                    s.load_namespace(Some(format!("{a}_missing")), engine == Engine::MySql, cx)
                })
            });
            settle(cx, w, &e);
            cx.read(|cx| {
                let s = e.read(cx);
                assert!(s.error.is_some());
                if engine == Engine::MySql {
                    assert_eq!(s.namespace_scope(), Some(empty.as_str()));
                }
            });
            // Re-establish a verified context, then test every UI switch gate.
            cx.update(|cx| {
                e.update(cx, |s, cx| {
                    s.load_namespace(Some(b.clone()), engine == Engine::MySql, cx)
                })
            });
            settle(cx, w, &e);
            cx.update_window(w, |_, w, cx| {
                e.update(cx, |s, cx| {
                    for gate in 0..5 {
                        match gate {
                            0 => s.busy = true,
                            1 => s.transaction = true,
                            2 => s.transaction_known = false,
                            3 => s.close_requested = true,
                            _ => s.changes.push(RowChange::Insert { values: vec![] }),
                        }
                        s.show_namespaces(w, cx);
                        assert!(s.namespace.picker.is_none());
                        s.busy = false;
                        s.transaction = false;
                        s.transaction_known = true;
                        s.close_requested = false;
                        s.changes.clear();
                    }
                })
            })
            .unwrap();
            if engine == Engine::MySql {
                // Prime a prepared unqualified operation in B, then an editor USE
                // that succeeds before the next statement fails. GUI must refresh
                // even on error and flush the old prepared database context.
                assert_eq!(
                    runtime
                        .handle()
                        .block_on(db.query("SELECT id FROM items", 1))
                        .unwrap()[0]
                        .rows[0][0]
                        .display(),
                    "2"
                );
                cx.update_window(w, |_, w, cx| {
                    e.update(cx, |s, cx| {
                        s.error = None;
                        s.tabs[0].editor.update(cx, |editor, cx| {
                            editor.set_value(
                                format!(
                                    "USE {}; SELECT * FROM no_such_pass8_table",
                                    quote_ident(engine, &a)
                                ),
                                w,
                                cx,
                            )
                        });
                        s.run_query(cx);
                        assert!(s.busy);
                    })
                })
                .unwrap();
                settle(cx, w, &e);
                cx.read(|cx| {
                    let s = e.read(cx);
                    assert!(s.error.is_some());
                    assert_eq!(s.namespace_scope(), Some(a.as_str()));
                    assert!(s.namespace.known);
                });
                assert_eq!(
                    runtime
                        .handle()
                        .block_on(db.query("SELECT id FROM items", 1))
                        .unwrap()[0]
                        .rows[0][0]
                        .display(),
                    "1"
                );
            }
            cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
        }));
        runtime.handle().block_on(async {
            let _ = db
                .query(
                    if engine == Engine::MySql {
                        "USE rowboat"
                    } else {
                        "SET search_path TO public"
                    },
                    0,
                )
                .await;
            for name in [&b, &empty, &a] {
                let _ = crate::pass5_tests::cleanup(&db, name).await;
            }
        });
        if let Err(error) = run {
            std::panic::resume_unwind(error);
        }
    }
}

#[gpui_kit::test]
#[ignore = "Requires disposable MySQL13309; injected writers, no user storage"]
fn pass8_live_new_database_workspace_uses_coordinator_separate_session_and_memory_password(
    cx: &mut TestAppContext,
) {
    use crate::{
        app::Startup,
        session::{SessionState, TabBuffer},
        workspace_shell::{Command, WorkspaceShell},
        workspace_storage::{Event, Write, Writers},
    };
    use rowboat_core::{Engine, sql::quote_ident};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (db, profile, a) = runtime
        .handle()
        .block_on(crate::pass5_tests::fixture(Engine::MySql))
        .unwrap();
    let b = format!("{a}_other");
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let snapshot = runtime.handle().block_on(async {
            db.query(
                &format!("CREATE SCHEMA {}", quote_ident(Engine::MySql, &b)),
                0,
            )
            .await
            .unwrap();
            db.select_database(&a).await.unwrap();
            db.namespace_catalog(None).await.unwrap()
        });
        let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
        let (recovery_tx, _recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, events) = std::sync::mpsc::channel();
        let state = rowboat_services::AppState {
            connections: vec![profile.clone()],
            ..Default::default()
        };
        event_tx
            .send(Event::StateLoaded(Ok(Box::new(state))))
            .unwrap();
        event_tx
            .send(Event::RecoveryLoaded(Ok(SessionState {
                version: 4,
                active_tab: 1,
                tabs: vec![TabBuffer {
                    id: 1,
                    connection_id: Some(profile.id.clone()),
                    title: "Original SQL".into(),
                    sql: "SELECT 'retained source'".into(),
                }],
                ..Default::default()
            })))
            .unwrap();
        let (w, shell) = cx.update(|cx| {
            crate::test_theme::init(cx);
            crate::keyboard_ui::bind(cx);
            gpui_kit::open_window(Default::default(), cx, |w, cx| {
                cx.new(|cx| {
                    WorkspaceShell::with_writers(
                        runtime.handle(),
                        Startup::default(),
                        Writers {
                            state: state_tx,
                            recovery: recovery_tx,
                            events,
                        },
                        w,
                        cx,
                    )
                })
            })
            .unwrap()
        });
        let source = cx
            .update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
            .unwrap()
            .remove(0);
        cx.update_window(w, |_, w, cx| {
            source.update(cx, |s, cx| {
                s.database = Some(db.clone());
                s.endpoint = Some(profile.clone());
                s.session_password = "rowboat-test".into();
                s.accept_namespace(snapshot, None, cx);
                let _ = s.shell_tx.send(Command::OpenDatabase {
                    route: s.tx.route,
                    host_epoch: s.host_epoch.wrapping_sub(1),
                    database: b.clone(),
                });
                w.focus(&s.focus, cx);
            })
        })
        .unwrap();
        assert_eq!(
            cx.update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
                .unwrap()
                .len(),
            1,
            "Obsolete host opened a workspace"
        );
        cx.update_window(w, |_, w, cx| {
            source.update(cx, |s, cx| {
                s.show_namespaces(w, cx);
                s.choose_namespace(b.clone(), true, w, cx);
            })
        })
        .unwrap();
        let workspaces = cx
            .update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
            .unwrap();
        assert_eq!(workspaces.len(), 2);
        let target = workspaces.iter().find(|e| **e != source).unwrap().clone();
        settle(cx, w, &target);
        let (other, shared) = cx.read(|cx| {
            let s = source.read(cx);
            let t = target.read(cx);
            assert_eq!(s.namespace_scope(), Some(a.as_str()));
            assert_eq!(
                s.tabs[0].editor.read(cx).value().as_ref(),
                "SELECT 'retained source'"
            );
            assert!(s.changes.is_empty());
            assert_ne!(s.owner, t.owner);
            assert!(std::rc::Rc::ptr_eq(&s.shared, &t.shared));
            assert_eq!(t.namespace_scope(), Some(b.as_str()));
            assert_eq!(t.session_password, "rowboat-test");
            assert!(t.form.is_none());
            let saved = t.shared.borrow().state.connections.clone();
            assert_eq!(saved.len(), 2);
            let original = saved.iter().find(|p| p.id == profile.id).unwrap();
            assert_eq!(
                serde_json::to_value(original).unwrap(),
                serde_json::to_value(&profile).unwrap()
            );
            let derived = saved.iter().find(|p| p.id != profile.id).unwrap();
            let mut expected = serde_json::to_value(&profile).unwrap();
            let actual = serde_json::to_value(derived).unwrap();
            for key in ["id", "database", "name"] {
                expected[key] = actual[key].clone();
            }
            assert_eq!(
                expected, actual,
                "Derived profile changed security/endpoint properties"
            );
            (
                t.database.clone().expect("new connection"),
                t.shared.clone(),
            )
        });
        runtime.handle().block_on(async {
            let left = db.query("SELECT CONNECTION_ID()", 1).await.unwrap();
            let right = other.query("SELECT CONNECTION_ID()", 1).await.unwrap();
            assert_ne!(left[0].rows[0][0], right[0].rows[0][0]);
            other.select_database("rowboat").await.unwrap();
            assert_eq!(db.namespace_catalog(None).await.unwrap().database, a);
        });
        let mut saw = false;
        while let Ok(write) = state_rx.try_recv() {
            if let Write::Save(_, state) = write {
                saw = true;
                assert!(
                    !serde_json::to_string(&state)
                        .unwrap()
                        .contains("rowboat-test")
                );
            }
        }
        assert!(saw);
        assert_eq!(shared.borrow().state.connections.len(), 2);
        cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
    }));
    runtime.handle().block_on(async {
        let _ = db.select_database("rowboat").await;
        for name in [&b, &a] {
            let _ = crate::pass5_tests::cleanup(&db, name).await;
        }
    });
    if let Err(error) = run {
        std::panic::resume_unwind(error);
    }
}
