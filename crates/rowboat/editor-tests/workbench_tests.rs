//! Actual retained Workbench entities rendered through GPUI TestPlatform. No native windows.
use crate::{
    app::{Startup, Workbench},
    workspace_shared::SharedState,
    workspace_shell::WorkspaceInit,
};
use gpui_kit::Focusable;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, KeyBinding, TestAppContext, WindowOptions};

pub(crate) fn workspace(
    cx: &mut TestAppContext,
    id: u64,
    runtime: tokio::runtime::Handle,
) -> (AnyWindowHandle, Entity<Workbench>) {
    cx.update(|cx| {
        crate::test_theme::init(cx);
        crate::keyboard_ui::bind(cx);
        cx.bind_keys([
            KeyBinding::new("escape", crate::app::Dismiss, Some("Rowboat")),
            KeyBinding::new("escape", crate::app::Dismiss, Some("Rowboat > Input")),
            KeyBinding::new(
                "tab",
                crate::app::NextSnippet,
                Some("RowboatSnippet > Input"),
            ),
            KeyBinding::new(
                "shift-tab",
                crate::app::PreviousSnippet,
                Some("RowboatSnippet > Input"),
            ),
            KeyBinding::new(
                "ctrl-shift-space",
                crate::app::ExpandSnippet,
                Some("RowboatSqlEditor > Input"),
            ),
        ]);
        // Match production startup order, including the broad Rowboat > Input
        // shortcuts installed after the cell-specific bindings.
        crate::preferences_ui::bind_shortcuts(&Default::default(), &Default::default(), cx)
            .unwrap();
        let (save, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let shared = std::rc::Rc::new(std::cell::RefCell::new(SharedState {
            state: Default::default(),
            writable: true,
            revision: 0,
            saved_revision: 0,
            next_tab: id as usize,
            active_workspace: id,
            shell_modal_open: false,
            save,
        }));
        let (shell_tx, _commands) = std::sync::mpsc::channel();
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| {
                Workbench::new(
                    runtime,
                    Startup {
                        sql: Some(format!("SELECT {id}")),
                        ..Default::default()
                    },
                    WorkspaceInit {
                        id,
                        owner: Some(format!("profile-{id}")),
                        shared,
                        shell_tx,
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap()
    })
}

#[gpui_kit::test]
fn pass6_actual_trigger_reverse_linked_unicode_and_undo(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 61, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.shared
                .borrow_mut()
                .state
                .snippet_templates
                .push(rowboat_services::SnippetTemplate {
                    id: "linked".into(),
                    name: "Linked".into(),
                    trigger: "trig".into(),
                    body: "${one:é} + ${one} + ${two:second}${0}".into(),
                });
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT trig; -- suffix", w, cx);
                e.set_selected_range(11..11, cx);
                e.focus(w, cx);
            });
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "ctrl-shift-space");
    cx.read(|cx| {
        assert_eq!(
            entity.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            "SELECT é + é + second; -- suffix"
        )
    });
    cx.simulate_keystrokes(window, "x tab shift-tab");
    cx.read(|cx| {
        let e = entity.read(cx).tabs[0].editor.read(cx);
        assert_eq!(e.value().as_ref(), "SELECT x + x + second; -- suffix");
        assert_eq!(&e.value()[e.selected_range()], "x");
    });
    cx.simulate_keystrokes(window, "y tab shift-tab");
    cx.read(|cx| {
        assert_eq!(
            entity.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            "SELECT y + y + second; -- suffix"
        )
    });
    cx.simulate_keystrokes(window, "escape ctrl-z ctrl-z ctrl-z");
    cx.read(|cx| {
        assert_eq!(
            entity.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            "SELECT trig; -- suffix"
        )
    });
}

#[gpui_kit::test]
fn pass6_actual_connection_modal_tab_reverse_and_escape(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 62, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.form = Some(crate::forms::ConnectionForm::new(
                Default::default(),
                String::new(),
                w,
                cx,
            ));
            w.focus(&s.modal_focus, cx);
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "tab");
    let first = cx
        .update_window(window, |_, w, cx| {
            assert!(entity.read(cx).modal_focus.contains_focused(w, cx));
            w.focused(cx)
        })
        .unwrap();
    cx.simulate_keystrokes(window, "tab shift-tab");
    cx.update_window(window, |_, w, cx| assert_eq!(w.focused(cx), first))
        .unwrap();
    for _ in 0..60 {
        cx.simulate_keystrokes(window, "tab");
        cx.update_window(window, |_, w, cx| {
            assert!(entity.read(cx).modal_focus.contains_focused(w, cx))
        })
        .unwrap();
    }
    cx.simulate_keystrokes(window, "escape");
    cx.read(|cx| assert!(entity.read(cx).form.is_none()));
}

#[gpui_kit::test]
fn pass6_actual_explorer_arrows_cross_page_without_editing_search(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 67, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.catalog = (0..205)
                .map(|i| rowboat_core::TableInfo {
                    table: rowboat_core::TableRef {
                        schema: "scope".into(),
                        name: format!("table_{i:03}"),
                    },
                    kind: "BASE TABLE".into(),
                })
                .collect();
            s.explorer_filter.update(cx, |e, cx| {
                e.set_value("table", w, cx);
                e.focus(w, cx);
            });
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "end up");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.explorer_cursor, 203);
        assert_eq!(s.explorer_page, 1);
        assert_eq!(s.explorer_filter.read(cx).value().as_ref(), "table");
    });
    cx.simulate_keystrokes(window, "home down");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.explorer_cursor, 1);
        assert_eq!(s.explorer_page, 0);
    });
}

#[gpui_kit::test]
fn pass6_actual_retained_rehost_preserves_identity_transaction_results_and_undo(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (old, entity) = workspace(cx, 63, runtime.handle());
    cx.update_window(old, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.transaction = true;
            s.transaction_known = true;
            s.tabs[0].results = vec![rowboat_core::QueryResult::default()];
            s.tabs[0].editor.update(cx, |e, cx| {
                crate::sql_editor::replace_range(e, 7..9, "99", w, cx);
                e.focus(w, cx);
            });
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let (route, editor, shared, before) = cx.read(|cx| {
        let s = entity.read(cx);
        (
            s.tx.route,
            s.tabs[0].editor.clone(),
            s.shared.clone(),
            s.session_snapshot(cx),
        )
    });
    let new = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |w, cx| {
            entity.update(cx, |s, cx| s.rehost(w, cx));
            entity.clone()
        })
        .unwrap()
        .0
    });
    cx.update_window(old, |_, w, _| w.remove_window()).unwrap();
    cx.update_window(new, |_, w, cx| {
        w.draw(cx).clear(cx);
        let s = entity.read(cx);
        assert_eq!(s.tx.route, route);
        assert!(s.transaction);
        assert_eq!(s.tabs[0].editor, editor);
        assert!(std::rc::Rc::ptr_eq(&shared, &s.shared));
        assert_eq!(s.session_snapshot(cx), before);
        assert_eq!(s.host_epoch, 1);
    })
    .unwrap();
    cx.simulate_keystrokes(new, "ctrl-z");
    cx.read(|cx| assert_eq!(editor.read(cx).value().as_ref(), "SELECT 63"));
}

#[gpui_kit::test]
fn pass6_actual_multiline_draft_native_newline_and_escape(cx: &mut TestAppContext) {
    use rowboat_core::{
        Cell, Column, QueryResult, TableRef,
        grid::{GridPosition, GridRange},
    };
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 64, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.page = crate::app::Page::Table;
            s.profile = Some(rowboat_core::ConnectionProfile {
                id: "profile-64".into(),
                ..Default::default()
            });
            let table = TableRef {
                schema: "test".into(),
                name: "rows".into(),
            };
            s.table = Some(table.clone());
            s.table_result = QueryResult {
                columns: vec![Column {
                    name: "value".into(),
                    data_type: "text".into(),
                    nullable: true,
                    primary_key: true,
                    default_value: None,
                }],
                rows: vec![vec![Cell::Text("first\nsecond".into())]],
                ..Default::default()
            };
            let value = crate::forms::textarea("first\nsecond", w, cx);
            value.update(cx, |e, cx| {
                e.set_selected_range(12..12, cx);
                e.focus(w, cx);
            });
            s.grid.draft = Some(crate::grid_ui::CellDraft {
                target: crate::forms::RowTarget {
                    connection_id: "profile-64".into(),
                    session_generation: s.session_generation,
                    table,
                },
                range: GridRange::from_size(GridPosition { row: 0, column: 0 }, 1, 1).unwrap(),
                snapshot: s.table_result.clone(),
                value,
                kind: crate::grid_ui::ValueKind::Text,
            });
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "enter x");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(
            s.grid
                .draft
                .as_ref()
                .unwrap()
                .value
                .read(cx)
                .value()
                .as_ref(),
            "first\nsecond\nx"
        );
        assert!(s.changes.is_empty());
    });
    cx.simulate_keystrokes(window, "escape");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.grid.draft.is_none());
        assert_eq!(
            s.table_result.rows[0][0],
            Cell::Text("first\nsecond".into())
        );
    });
}

#[gpui_kit::test]
fn pass6_actual_metric_delete_atomically_removes_cards_and_stale_editor_rejects(
    cx: &mut TestAppContext,
) {
    use rowboat_services::{CustomMetricDefinition, Dashboard, DashboardCard};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 65, runtime.handle());
    let (writer, mut writes) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let mut shared = s.shared.borrow_mut();
            shared.save = writer;
            shared
                .state
                .connections
                .push(rowboat_core::ConnectionProfile {
                    id: "profile-65".into(),
                    ..Default::default()
                });
            shared.state.custom_metrics.push(CustomMetricDefinition {
                id: "scalar".into(),
                name: "Exact".into(),
                connection_id: "profile-65".into(),
                sql: "SELECT 12345678901234567890.123456789".into(),
                enabled: true,
            });
            shared.state.dashboards.push(Dashboard {
                id: "dashboard".into(),
                name: "D".into(),
                connection_id: "profile-65".into(),
                refresh_interval_secs: 5,
                columns: 1,
                cards: vec![DashboardCard {
                    id: "card".into(),
                    metric: "custom:scalar".into(),
                    title: "Exact".into(),
                    column: 0,
                    row: 0,
                    column_span: 1,
                }],
            });
            drop(shared);
            s.edit_custom_metric(Some("scalar"), w, cx);
            s.shared.borrow_mut().state.custom_metrics[0].name = "Changed elsewhere".into();
            s.save_custom_metric(true, cx);
            assert!(s.custom_metrics.editor.is_some());
            assert!(s.error.as_ref().unwrap().contains("another window"));
            s.custom_metrics.editor = None;
            s.edit_custom_metric(Some("scalar"), w, cx);
            s.save_custom_metric(true, cx);
            assert!(s.custom_metrics.editor.is_none());
        })
    })
    .unwrap();
    let crate::workspace_storage::Write::Save(_, state) = writes.try_recv().unwrap() else {
        panic!("Expected metadata write")
    };
    assert!(state.custom_metrics.is_empty());
    assert!(state.dashboards[0].cards.is_empty());
    let directory = tempfile::tempdir().unwrap();
    let store = rowboat_services::Store::open_at(directory.path()).unwrap();
    store.save(&state).unwrap();
    assert!(store.load().unwrap().custom_metrics.is_empty());
    assert!(writes.try_recv().is_err());
}

#[gpui_kit::test]
#[ignore = "Requires disposable PG15435/MySQL13309; no native windows"]
fn pass6_actual_live_move_multiline_staging_keeps_physical_session(cx: &mut TestAppContext) {
    use rowboat_core::{Cell, Engine, sql::qualified_table};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture(engine))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let table = crate::pass5_tests::table(&schema, "edits");
            let q = qualified_table(engine, &table);
            let identity = if engine == Engine::Postgres {
                "SELECT pg_backend_pid()"
            } else {
                "SELECT CONNECTION_ID()"
            };
            let (pid,structure,rows,catalog)=runtime.handle().block_on(async {
                db.query(&format!("CREATE TABLE {q}(id INT PRIMARY KEY,value TEXT); INSERT INTO {q} VALUES(1,'old')"),0).await.unwrap();
                db.begin().await.unwrap();
                (db.query(identity,1).await.unwrap()[0].rows[0][0].clone(),db.structure(&table).await.unwrap(),db.query(&format!("SELECT * FROM {q}"),10).await.unwrap().remove(0),db.catalog().await.unwrap())
            });
            let (old, entity) = workspace(cx, 66, runtime.handle());
            cx.update_window(old, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile);
                    s.database = Some(db.clone());
                    s.transaction = true;
                    s.transaction_known = true;
                    s.session_ready = true;
                    s.page = crate::app::Page::Table;
                    s.table = Some(table);
                    s.structure = structure;
                    s.table_result = rows;
                    s.catalog = catalog;
                    s.select_grid_cell(0, 1, false, cx);
                    s.begin_cell_edit(w, cx);
                    s.grid.draft.as_ref().unwrap().value.update(cx, |e, cx| {
                        e.set_value("main\nstage", w, cx);
                        e.focus(w, cx);
                    });
                });
                w.draw(cx).clear(cx);
            })
            .unwrap();
            cx.simulate_keystrokes(old, "ctrl-enter");
            cx.update_window(old, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    assert!(
                        s.grid.draft.is_none(),
                        "Main-host Ctrl+Enter did not stage: {:?}",
                        s.error
                    );
                    assert_eq!(s.changes.len(), 1);
                    let rowboat_core::RowChange::Update { values, .. } = &s.changes[0] else {
                        panic!("Expected update")
                    };
                    assert!(values.contains(&("value".into(), Cell::Text("main\nstage".into()))));
                    s.begin_cell_edit(w, cx);
                    s.grid.draft.as_ref().unwrap().value.update(cx, |e, cx| {
                        e.set_value("exact\nline", w, cx);
                        e.focus(w, cx);
                    });
                });
                w.draw(cx).clear(cx);
            })
            .unwrap();
            let (route, draft_id, shared) = cx.read(|cx| {
                let s = entity.read(cx);
                (
                    s.tx.route,
                    s.grid.draft.as_ref().unwrap().value.entity_id(),
                    s.shared.clone(),
                )
            });
            let new = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |w, cx| {
                    entity.update(cx, |s, cx| s.rehost(w, cx));
                    entity.clone()
                })
                .unwrap()
                .0
            });
            cx.update_window(old, |_, w, _| w.remove_window()).unwrap();
            cx.update_window(new, |_, w, cx| {
                w.draw(cx).clear(cx);
                let s = entity.read(cx);
                assert_eq!(s.tx.route, route);
                assert!(s.transaction);
                assert_eq!(s.grid.draft.as_ref().unwrap().value.entity_id(), draft_id);
                assert!(
                    s.grid
                        .draft
                        .as_ref()
                        .unwrap()
                        .value
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(w),
                    "Draft focus missing after move: {:?}",
                    w.focused(cx)
                );
                assert!(std::rc::Rc::ptr_eq(&shared, &s.shared));
            })
            .unwrap();
            cx.simulate_keystrokes(new, "ctrl-enter");
            let live = cx.read(|cx| {
                let s = entity.read(cx);
                assert!(s.grid.draft.is_none(), "{:?}", s.error);
                assert_eq!(s.changes.len(), 1);
                let rowboat_core::RowChange::Update { values, .. } = &s.changes[0] else {
                    panic!("Expected update")
                };
                assert!(values.contains(&("value".into(), Cell::Text("exact\nline".into()))));
                s.database.clone().unwrap()
            });
            // Also cover platform delivery as native secondary Enter rather
            // than the application RunQuery action selected by default shortcuts.
            cx.update_window(new, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.begin_cell_edit(w, cx);
                    s.grid.draft.as_ref().unwrap().value.update(cx, |e, cx| {
                        e.set_value("native\naction", w, cx);
                        e.focus(w, cx);
                    });
                });
                w.draw(cx).clear(cx);
                w.dispatch_action(
                    Box::new(gpui_kit::base::input::Enter {
                        secondary: true,
                        shift: false,
                    }),
                    cx,
                );
            })
            .unwrap();
            cx.read(|cx| {
                let s = entity.read(cx);
                assert!(
                    s.grid.draft.is_none(),
                    "Native secondary Enter did not stage"
                );
                assert_eq!(s.changes.len(), 1);
                let rowboat_core::RowChange::Update { values, .. } = &s.changes[0] else {
                    panic!("Expected update")
                };
                assert!(values.contains(&("value".into(), Cell::Text("native\naction".into()))));
            });
            runtime.handle().block_on(async {
                assert!(live.in_transaction().await.unwrap());
                assert_eq!(live.query(identity, 1).await.unwrap()[0].rows[0][0], pid);
                assert_eq!(
                    live.query(&format!("SELECT value FROM {q}"), 1)
                        .await
                        .unwrap()[0]
                        .rows[0][0],
                    Cell::Text("old".into())
                );
            });
            cx.update_window(new, |_, w, _| w.remove_window()).unwrap();
        }));
        runtime
            .handle()
            .block_on(crate::pass5_tests::cleanup(&db, &schema))
            .unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

#[gpui_kit::test]
fn actual_workbench_satellite_escape_cannot_clear_close_guard_or_edit_frozen_buffer(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let main = workspace(cx, 1, runtime.handle());
    let satellite = workspace(cx, 2, runtime.handle());
    for (window, entity) in [&main, &satellite] {
        cx.update_window(*window, |_, window, cx| {
            entity.update(cx, |s, cx| {
                s.close_requested = true;
                for tab in &s.tabs {
                    crate::sql_editor::set_frozen(&tab.editor, true, cx);
                }
                window.focus(&s.focus, cx);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.simulate_keystrokes(*window, "escape x ctrl-z");
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.close_requested);
            assert_eq!(
                s.tabs[0].editor.read(cx).value().as_ref(),
                format!("SELECT {}", s.workspace_id)
            );
        });
    }
}

#[gpui_kit::test]
fn pass6_actual_rehost_review_keeps_visible_shortcut_route(cx: &mut TestAppContext) {
    use crate::app::{Page, TablePage};
    use rowboat_core::{Cell, Column, ConnectionProfile, RowChange, Structure, TableRef};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (mut host, entity) = workspace(cx, 68, runtime.handle());
    cx.update_window(host, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.profile = Some(ConnectionProfile {
                id: "profile-68".into(),
                ..Default::default()
            });
            s.table = Some(TableRef {
                schema: "focus_audit".into(),
                name: "not_executed".into(),
            });
            s.structure = Structure {
                columns: vec![Column {
                    name: "id".into(),
                    data_type: "integer".into(),
                    nullable: false,
                    primary_key: true,
                    default_value: None,
                }],
                ..Default::default()
            };
            s.changes = vec![RowChange::Update {
                original: vec![("id".into(), Cell::Text("1".into()))],
                values: vec![("id".into(), Cell::Text("2".into()))],
            }];
            s.change_target = s.current_target();
            s.capture_recovery();
            s.review_pending(cx);
            w.focus(&s.focus, cx);
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(host, "ctrl-shift-p");
    assert!(cx.read(|cx| entity.read(cx).quick_open));
    cx.simulate_keystrokes(host, "escape");
    let (route, target, batches) = cx.read(|cx| {
        let s = entity.read(cx);
        (s.tx.route, s.change_target.clone(), s.recovery_batches())
    });
    for page in [
        Page::Table,
        Page::Table,
        Page::History,
        Page::Tools,
        Page::Objects,
        Page::Dashboard,
        Page::Query,
    ] {
        cx.update(|cx| entity.update(cx, |s, _| s.page = page));
        let next = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, _| entity.clone())
                .unwrap()
                .0
        });
        cx.update_window(next, |_, w, cx| {
            entity.update(cx, |s, cx| s.rehost(w, cx));
            w.draw(cx).clear(cx);
        })
        .unwrap();
        cx.update_window(host, |_, w, _| w.remove_window()).unwrap();
        host = next;
        cx.simulate_keystrokes(host, "ctrl-shift-p");
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(
                s.quick_open,
                "Visible page lost palette shortcut after rehost"
            );
            assert_eq!(s.tx.route, route);
            assert_eq!(s.change_target, target);
            assert_eq!(s.recovery_batches(), batches);
            assert!(s.page != Page::Table || s.table_page == TablePage::Changes);
        });
        cx.simulate_keystrokes(host, "escape");
    }
}

#[gpui_kit::test]
fn actual_workbench_capture_failure_exports_explicit_stale_checkpoint(cx: &mut TestAppContext) {
    use rowboat_core::{Cell, Column, ConnectionProfile, RowChange, TableRef};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 3, runtime.handle());
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, cx| {
            let profile = ConnectionProfile {
                id: "profile-3".into(),
                ..Default::default()
            };
            let table = TableRef {
                schema: "test".into(),
                name: "rows".into(),
            };
            s.profile = Some(profile);
            s.table = Some(table.clone());
            s.structure.columns = vec![Column {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
                default_value: None,
            }];
            s.change_target = Some(crate::forms::RowTarget {
                connection_id: "profile-3".into(),
                session_generation: s.session_generation,
                table,
            });
            s.changes = (0..200)
                .map(|id| RowChange::Delete {
                    original: vec![("id".into(), Cell::Text(id.to_string()))],
                })
                .collect();
            s.capture_recovery();
            assert!(s.recovery.error.is_none());
            assert_eq!(
                s.recovery_batches()[0].phase,
                crate::recovery_ui::RecoveryPhase::Staged
            );
            s.changes.push(RowChange::Delete {
                original: vec![("id".into(), Cell::Text("201".into()))],
            });
            s.capture_recovery();
            assert!(s.recovery.error.is_some());
            assert_eq!(
                s.session_snapshot(cx).workspaces[0].staged[0].phase,
                crate::recovery_ui::RecoveryPhase::Stale
            );
            s.changes = vec![RowChange::Insert { values: vec![] }];
            s.capture_recovery();
            assert!(s.recovery.error.as_ref().unwrap().contains("primary-key"));
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn actual_access_dialog_escape_dismisses_and_restores_workbench_focus(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 4, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.profile = Some(rowboat_core::ConnectionProfile::default());
            s.show_access_matrix(window, cx);
            assert!(s.access_matrix.is_some());
        });
        window.draw(cx).clear(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(window, "escape");
    cx.update_window(window, |_, window, cx| {
        let s = entity.read(cx);
        assert!(s.access_matrix.is_none());
        assert!(s.focus.is_focused(window));
    })
    .unwrap();
}

#[gpui_kit::test]
fn actual_profile_save_defers_owner_change_until_coordinator_and_checkpoint_stays_unique(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 5, runtime.handle());
    let (sender, commands) = std::sync::mpsc::channel();
    let mut registry = crate::workspace_model::WindowOwnership::default();
    registry.claim(None, 5);
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.owner = None;
            s.tabs[0].connection_id = None;
            s.shell_tx = sender;
            let profile = rowboat_core::ConnectionProfile {
                id: "assigned".into(),
                name: "Assigned".into(),
                ..Default::default()
            };
            let mut form = crate::forms::ConnectionForm::new(profile, String::new(), window, cx);
            form.remember = false;
            form.credential_exists = false;
            s.form = Some(form);
            s.save_connection(false, cx);
            assert!(s.owner.is_none());
            assert!(s.busy);
        })
    })
    .unwrap();
    let crate::workspace_shell::Command::SaveProfile { route, save } = commands.try_recv().unwrap()
    else {
        panic!("Expected coordinator profile request")
    };
    assert_eq!(route.workspace, 5);
    let owner = Some(save.profile.id.clone());
    registry.reassign(5, &None, owner.clone()).unwrap();
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, cx| s.commit_profile_save(*save, cx))
    })
    .unwrap();
    assert_eq!(registry.claim(owner.clone(), 6), 5); // OpenWindow resolves existing host.
    assert_eq!(registry.claim(None, 6), 6); // New draft gets a distinct workspace.
    let (_, draft) = workspace(cx, 6, runtime.handle());
    cx.update(|cx| {
        draft.update(cx, |s, _| {
            s.owner = None;
            s.tabs[0].connection_id = None;
        })
    });
    let snapshot = cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.owner, owner);
        assert!(!s.busy);
        let mut snapshot = s.session_snapshot(cx);
        assert_eq!(snapshot.version, 5); // Old binaries reject document bindings they cannot enforce.
        let other = draft.read(cx).session_snapshot(cx);
        snapshot.tabs.extend(other.tabs);
        snapshot.workspaces.extend(other.workspaces);
        snapshot
    });
    let directory = tempfile::tempdir().unwrap();
    let store = crate::session::SessionStore::at(directory.path()).unwrap();
    store.save(&snapshot).unwrap();
    assert_eq!(store.load().unwrap(), snapshot);
}
