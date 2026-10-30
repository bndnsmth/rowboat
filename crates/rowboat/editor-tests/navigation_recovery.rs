//! Disposable databases only: sidebar catalogs and explicit session recovery.
use crate::app::Workbench;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_admin::objects::ObjectKind;
use rowboat_core::{ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode};
use std::time::Duration;

fn draw(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.update_window(w, |_, w, cx| {
        w.refresh();
        w.draw(cx).clear(cx);
    })
    .unwrap();
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}
fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, entity: &Entity<Workbench>) {
    for _ in 0..1000 {
        cx.run_until_parked();
        let done = cx
            .update_window(w, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.busy
                })
            })
            .unwrap();
        if done {
            draw(cx, w);
            return;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    panic!("Disposable navigation operation timed out");
}
fn attach(
    cx: &mut TestAppContext,
    runtime: tokio::runtime::Handle,
    profile: ConnectionProfile,
    db: Database,
    scope: &str,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let catalog = runtime.block_on(db.namespace_catalog(Some(scope))).unwrap();
    let (w, entity) = crate::workbench_tests::workspace(cx, 2701, runtime);
    cx.simulate_window_resize(w, gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(900.)));
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.owner = Some(profile.id.clone());
            s.tabs[0].connection_id = Some(profile.id.clone());
            s.edit_state(|state| state.connections.push(profile.clone()));
            s.profile = Some(profile);
            s.database = Some(db);
            s.accept_namespace(catalog, Some(scope.into()), cx);
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("INSERT INTO guard_items VALUES(99)", w, cx)
            });
            s.tabs[0]
                .parameters
                .update(cx, |e, cx| e.set_value("{\"keep\":\"draft\"}", w, cx));
        })
    })
    .unwrap();
    (w, entity)
}
fn sqlite(
    path: &std::path::Path,
    runtime: &tokio::runtime::Handle,
) -> (ConnectionProfile, Database) {
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.to_path_buf(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime.block_on(Database::connect(&profile, "")).unwrap();
    runtime
        .block_on(db.query("CREATE TABLE guard_items(id INTEGER PRIMARY KEY)", 0))
        .unwrap();
    (profile, db)
}
async fn retire(db: &Database) {
    let mut stream = db.query_stream("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT x FROM n", rowboat_core::QueryStreamOptions { cancellation: rowboat_core::QueryCancellation::RetireSession, batch_rows: 1, channel_capacity: 1, row_limit: None, ..Default::default() }).await.unwrap();
    assert!(stream.recv().await.is_some());
    stream.cancel();
    while stream.recv().await.is_some() {}
    assert!(db.is_retired());
}
fn assert_drafts(s: &Workbench, cx: &gpui_kit::App) {
    assert_eq!(
        s.tabs[0].editor.read(cx).value().as_ref(),
        "INSERT INTO guard_items VALUES(99)"
    );
    assert_eq!(
        s.tabs[0].parameters.read(cx).value().as_ref(),
        "{\"keep\":\"draft\"}"
    );
    assert_eq!(s.query_run_invocations, 0);
}

#[gpui_kit::test]
fn navigation_reconnect_error_recovers_retired_session_without_replaying_or_saving_profile(
    cx: &mut TestAppContext,
) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (profile, db) = sqlite(&root.path().join("reconnect.sqlite"), &runtime.handle());
    let original = serde_json::to_value(&profile).unwrap();
    let (w, entity) = attach(cx, runtime.handle(), profile, db.clone(), "main");
    runtime.handle().block_on(retire(&db));
    let (sender, receiver) = std::sync::mpsc::channel();
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.shell_tx = sender;
            s.transaction = true; // stale last-known state must not trap a retired session
            s.transaction_known = true;
            s.fail("Connection closed; reconnect required".into());
            assert!(s.reconnect_available());
            cx.notify();
        })
    })
    .unwrap();
    click(cx, w, "reconnect-error");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.form.is_some() && !s.transaction_known);
        assert!(s.connection_flow.reconnecting);
        assert_drafts(s, cx);
    });
    click(cx, w, "confirm-session-replacement");
    click(cx, w, "connect");
    let mut ack = None;
    for _ in 0..1000 {
        cx.update_window(w, |_, w, cx| entity.update(cx, |s, cx| s.poll(w, cx)))
            .unwrap();
        if let Ok(crate::workspace_shell::Command::PersistConnectionRecovery {
            ack: pending, ..
        }) = receiver.try_recv()
        {
            ack = Some(pending);
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    let ack = ack.expect("Recovery must be acknowledged before replacing a dead session");
    cx.read(|cx| assert!(entity.read(cx).database.as_ref().unwrap().is_retired()));
    ack.send(Ok(())).unwrap();
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.form.is_none() && s.transaction_known && !s.transaction);
        assert!(!s.database.as_ref().unwrap().is_retired());
        assert_eq!(
            serde_json::to_value(&s.shared.borrow().state.connections[0]).unwrap(),
            original
        );
        assert_drafts(s, cx);
        let rows = runtime
            .handle()
            .block_on(
                s.database
                    .as_ref()
                    .unwrap()
                    .query("SELECT COUNT(*) FROM guard_items", 1),
            )
            .unwrap();
        assert_eq!(rows[0].rows[0][0].display(), "0");
    });
}

#[gpui_kit::test]
fn navigation_reconnect_healthy_session_works_with_readonly_preferences_and_preserves_drafts(
    cx: &mut TestAppContext,
) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (profile, db) = sqlite(
        &root.path().join("readonly-store.sqlite"),
        &runtime.handle(),
    );
    runtime
        .handle()
        .block_on(db.query("CREATE TEMP TABLE previous_session(id INTEGER)", 0))
        .unwrap();
    let (w, entity) = attach(cx, runtime.handle(), profile, db, "main");
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.shared.borrow_mut().writable = false;
            s.startup.autorun = true;
            s.startup.tab_id = Some(s.tabs[0].id);
            cx.notify();
        })
    })
    .unwrap();
    click(cx, w, "reconnect-workspace");
    click(cx, w, "connect");
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.form.is_none() && s.database.is_some());
        assert_drafts(s, cx);
        assert!(
            runtime
                .handle()
                .block_on(
                    s.database
                        .as_ref()
                        .unwrap()
                        .query("SELECT * FROM previous_session", 1)
                )
                .is_err(),
            "Reconnect must establish a new physical session"
        );
        assert_eq!(s.shared.borrow().revision, 1);
    });
}

#[gpui_kit::test]
fn navigation_reconnect_probes_a_healthy_transaction_before_replacing(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (profile, db) = sqlite(&root.path().join("transaction.sqlite"), &runtime.handle());
    let (w, entity) = attach(cx, runtime.handle(), profile, db.clone(), "main");
    // A cached UI flag is not sufficient proof: the physical session wins.
    runtime.handle().block_on(db.begin()).unwrap();
    click(cx, w, "reconnect-workspace");
    click(cx, w, "connect");
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.transaction && s.transaction_known && s.form.is_some());
        assert!(s.error.as_ref().unwrap().contains("active transaction"));
        assert_eq!(s.session_generation, 0);
        assert_drafts(s, cx);
    });
    assert!(runtime.handle().block_on(db.in_transaction()).unwrap());
    runtime.handle().block_on(db.rollback()).unwrap();
}

#[gpui_kit::test]
fn navigation_reconnect_changed_form_retains_probe_evidence_and_the_old_session(
    cx: &mut TestAppContext,
) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (profile, db) = sqlite(&root.path().join("changed.sqlite"), &runtime.handle());
    let (w, entity) = attach(cx, runtime.handle(), profile, db.clone(), "main");
    runtime.handle().block_on(db.begin()).unwrap();
    click(cx, w, "reconnect-workspace");
    click(cx, w, "connect");
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            assert!(s.busy);
            s.form.as_ref().unwrap().fields["name"].update(cx, |input, cx| {
                input.set_value("Changed during probe", w, cx)
            });
        })
    })
    .unwrap();
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.transaction && s.transaction_known && s.form.is_some());
        assert!(s.error.as_ref().unwrap().contains("draft changed"));
        assert_eq!(s.session_generation, 0);
        assert_drafts(s, cx);
    });
    assert!(runtime.handle().block_on(db.in_transaction()).unwrap());
    runtime.handle().block_on(db.rollback()).unwrap();
}

#[gpui_kit::test]
fn navigation_reconnect_failed_attempt_retries_without_creating_a_missing_sqlite_file(
    cx: &mut TestAppContext,
) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let path = root.path().join("retry.sqlite");
    let moved = root.path().join("held.sqlite");
    let (profile, db) = sqlite(&path, &runtime.handle());
    let original = serde_json::to_value(&profile).unwrap();
    let (w, entity) = attach(cx, runtime.handle(), profile, db, "main");
    click(cx, w, "reconnect-workspace");
    std::fs::rename(&path, &moved).unwrap();
    click(cx, w, "connect");
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.error.is_some() && s.database.is_none() && s.form.is_some());
        assert!(s.connection_flow.reconnecting);
        assert_eq!(
            s.form.as_ref().unwrap().file_mode,
            SqliteOpenMode::OpenExisting
        );
        assert_drafts(s, cx);
    });
    assert!(
        !path.exists(),
        "Reconnect must not silently create a replacement database"
    );
    std::fs::rename(&moved, &path).unwrap();
    click(cx, w, "connect");
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.error.is_none() && s.database.is_some() && s.form.is_none());
        assert_eq!(
            serde_json::to_value(&s.shared.borrow().state.connections[0]).unwrap(),
            original
        );
        assert_drafts(s, cx);
    });
}

#[gpui_kit::test]
fn navigation_reconnect_session_dying_in_the_form_requires_explicit_confirmation(
    cx: &mut TestAppContext,
) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (profile, db) = sqlite(&root.path().join("late-close.sqlite"), &runtime.handle());
    let (w, entity) = attach(cx, runtime.handle(), profile, db.clone(), "main");
    click(cx, w, "reconnect-workspace");
    runtime.handle().block_on(retire(&db));
    click(cx, w, "connect");
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.transaction_known && !s.namespace.known);
        assert!(!s.form.as_ref().unwrap().confirm_replace);
        assert!(s.database.as_ref().unwrap().is_retired());
        assert_eq!(
            s.session_generation, 0,
            "An unverified session must not be replaced automatically"
        );
        assert_drafts(s, cx);
    });
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.busy = true;
            s.receive_connection(
                crate::connection_ui::ConnectionEvent::ReconnectProbed {
                    request: uuid::Uuid::new_v4(),
                    active: Some(false),
                },
                cx,
            );
            assert!(
                s.busy,
                "A stale reconnect probe must not release another operation"
            );
            s.busy = false;
            s.shared.borrow_mut().writable = false;
            s.form.as_mut().unwrap().confirm_replace = true;
            cx.notify();
        })
    })
    .unwrap();
    click(cx, w, "connect");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.busy && s.database.as_ref().unwrap().is_retired());
        assert!(
            s.connection_flow.replacement.is_none(),
            "Unknown-outcome replacement requires writable recovery storage"
        );
    });
}

#[gpui_kit::test]
fn navigation_sidebar_shared_paging_collapse_search_and_group_order(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (profile, db) = sqlite(&root.path().join("paging.sqlite"), &runtime.handle());
    let (w, entity) = attach(cx, runtime.handle(), profile, db, "main");
    // Presentation-only catalog fixture. No mocked identity is inspected or run.
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.profile.as_mut().unwrap().engine = Engine::Postgres;
            s.ensure_sidebar_objects();
            for group in &mut s.objects.groups {
                let count = match group.kind {
                    ObjectKind::Procedure => 205,
                    ObjectKind::Function => 1,
                    _ => 0,
                };
                group.result = Some(Ok((0..count)
                    .map(|i| rowboat_admin::object_editor::ObjectIdentity {
                        engine: Engine::Postgres,
                        schema: "main".into(),
                        kind: group.kind,
                        name: format!("object_{i:03}"),
                        signature: Some("integer".into()),
                        parent: None,
                    })
                    .collect()));
                group.sidebar_expanded = true;
            }
            cx.notify();
        })
    })
    .unwrap();
    draw(cx, w);
    {
        let mut visual = VisualTestContext::from_window(w, cx);
        assert!(visual.debug_bounds("explorer-item-199").is_some());
        assert!(
            visual.debug_bounds("explorer-item-200").is_none(),
            "Only 200 object children may render per page"
        );
        assert!(
            visual
                .debug_bounds("explorer-group-procedures")
                .unwrap()
                .top()
                < visual
                    .debug_bounds("explorer-group-functions")
                    .unwrap()
                    .top(),
            "Visual groups must match keyboard order"
        );
    }
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.explorer_filter.update(cx, |input, cx| input.focus(w, cx));
        })
    })
    .unwrap();
    cx.simulate_keystrokes(w, "end");
    draw(cx, w);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!((s.explorer_page, s.explorer_cursor), (1, 206));
        assert!(matches!(s.explorer_items(cx)[206], crate::namespace_ui::ExplorerItem::Object(id) if id.kind == ObjectKind::Function));
    });
    {
        let mut visual = VisualTestContext::from_window(w, cx);
        assert!(visual.debug_bounds("explorer-item-199").is_none());
        assert!(visual.debug_bounds("explorer-item-206").is_some());
        assert!(visual.debug_bounds("explorer-group-procedures").is_some());
    }
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            let index = s
                .sidebar_groups()
                .find(|(_, group)| group.kind == ObjectKind::Procedure)
                .unwrap()
                .0;
            s.toggle_sidebar_object_group(index, cx);
            assert_eq!((s.explorer_page, s.explorer_cursor), (0, 0));
            assert_eq!(
                s.explorer_items(cx).len(),
                2,
                "Collapsed procedures must not remain keyboard targets"
            );
        })
    })
    .unwrap();
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.explorer_filter.update(cx, |input, cx| input.focus(w, cx));
        })
    })
    .unwrap();
    cx.simulate_input(w, "object_204(integer)");
    cx.run_until_parked();
    cx.read(|cx| {
        let s = entity.read(cx);
        let items = s.explorer_items(cx);
        assert_eq!(items.len(), 1, "Search must include loaded groups even after collapse");
        assert!(matches!(items[0], crate::namespace_ui::ExplorerItem::Object(id) if id.kind == ObjectKind::Procedure));
        assert_drafts(s, cx);
    });
}

#[gpui_kit::test]
#[ignore = "Explicit disposable MySQL13309/PostgreSQL15435 only; creates unique test schemas"]
fn navigation_reconnect_live_active_target_and_memory_only_credentials(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::MySql, Engine::Postgres] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture_prefix(
                engine,
                "reconnect_target",
            ))
            .unwrap();
        let original = serde_json::to_value(&profile).unwrap();
        let ns = rowboat_core::sql::quote_ident(engine, &schema);
        let session_sql = if engine == Engine::MySql {
            "SELECT CONNECTION_ID()"
        } else {
            "SELECT pg_backend_pid()"
        };
        let previous =
            runtime.handle().block_on(db.query(session_sql, 1)).unwrap()[0].rows[0][0].display();
        runtime
            .handle()
            .block_on(db.query(
                &format!("CREATE TABLE {ns}.guard_items(id INTEGER PRIMARY KEY)"),
                0,
            ))
            .unwrap();
        if engine == Engine::MySql {
            runtime
                .handle()
                .block_on(db.select_database(&schema))
                .unwrap();
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (w, entity) = attach(cx, runtime.handle(), profile, db.clone(), &schema);
            cx.update_window(w, |_, _, cx| {
                entity.update(cx, |s, cx| {
                    s.session_password = "rowboat-test".into();
                    cx.notify();
                })
            })
            .unwrap();
            click(cx, w, "reconnect-workspace");
            cx.read(|cx| {
                let s = entity.read(cx);
                let form = s.form.as_ref().unwrap();
                assert_eq!(form.password_value(cx), "rowboat-test");
                assert!(!form.keyring_loading);
                assert_eq!(
                    form.profile(cx).unwrap().database,
                    if engine == Engine::MySql {
                        &schema
                    } else {
                        "rowboat"
                    }
                );
            });
            click(cx, w, "connect");
            settle(cx, w, &entity);
            cx.read(|cx| {
                let s = entity.read(cx);
                assert!(s.error.is_none() && s.form.is_none(), "{:?}", s.error);
                let current = s.database.as_ref().unwrap();
                let next = runtime
                    .handle()
                    .block_on(current.query(session_sql, 1))
                    .unwrap()[0]
                    .rows[0][0]
                    .display();
                assert_ne!(
                    previous, next,
                    "Reconnect must establish a fresh server session"
                );
                assert_eq!(
                    s.namespace.snapshot.as_ref().unwrap().database,
                    if engine == Engine::MySql {
                        &schema
                    } else {
                        "rowboat"
                    }
                );
                assert_eq!(
                    serde_json::to_value(&s.shared.borrow().state.connections[0]).unwrap(),
                    original
                );
                assert_drafts(s, cx);
                let guard = runtime
                    .handle()
                    .block_on(current.query(&format!("SELECT COUNT(*) FROM {ns}.guard_items"), 1))
                    .unwrap();
                assert_eq!(guard[0].rows[0][0].display(), "0");
            });
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
#[ignore = "Explicit disposable MySQL13309/PostgreSQL15435 only; creates unique test schemas"]
fn navigation_sidebar_live_routines_search_inspection_and_scope_isolation(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::MySql, Engine::Postgres] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture_prefix(
                engine,
                "sidebar_objects",
            ))
            .unwrap();
        let ns = rowboat_core::sql::quote_ident(engine, &schema);
        runtime.handle().block_on(async {
            db.query(&format!("CREATE TABLE {ns}.guard_items(id INTEGER PRIMARY KEY); CREATE TABLE {ns}.inspection_guard(calls INTEGER); INSERT INTO {ns}.inspection_guard VALUES(0); CREATE VIEW {ns}.item_view AS SELECT id FROM {ns}.guard_items"), 0).await.unwrap();
            let procedure = if engine == Engine::MySql { format!("CREATE PROCEDURE {ns}.recalculate() UPDATE {ns}.inspection_guard SET calls=calls+1") } else { format!("CREATE PROCEDURE {ns}.recalculate() LANGUAGE SQL AS 'UPDATE {ns}.inspection_guard SET calls=calls+1'") };
            db.query(&procedure, 0).await.unwrap();
            let function = if engine == Engine::MySql { format!("CREATE FUNCTION {ns}.answer() RETURNS INTEGER DETERMINISTIC RETURN 7") } else { format!("CREATE FUNCTION {ns}.answer() RETURNS INTEGER LANGUAGE SQL AS 'SELECT 7'") };
            db.query(&function, 0).await.unwrap();
            if engine == Engine::MySql {
                db.query(&format!("CREATE TRIGGER {ns}.insert_guard BEFORE INSERT ON {ns}.guard_items FOR EACH ROW UPDATE {ns}.inspection_guard SET calls=calls+1"), 0).await.unwrap();
            } else {
                db.query(&format!("CREATE FUNCTION {ns}.insert_guard_body() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE {ns}.inspection_guard SET calls=calls+1; RETURN NEW; END $$; CREATE TRIGGER insert_guard BEFORE INSERT ON {ns}.guard_items FOR EACH ROW EXECUTE FUNCTION {ns}.insert_guard_body(); CREATE SEQUENCE {ns}.manual_sequence"), 0).await.unwrap();
            }
            if engine == Engine::MySql { db.select_database(&schema).await.unwrap(); }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (w, entity) = attach(cx, runtime.handle(), profile, db.clone(), &schema);
            draw(cx, w);
            cx.read(|cx| {
                assert!(
                    entity
                        .read(cx)
                        .objects
                        .groups
                        .iter()
                        .all(|g| g.result.is_none()),
                    "Rendering must not issue extra catalog SQL"
                )
            });
            click(cx, w, "explorer-group-procedures");
            settle(cx, w, &entity);
            let (ordinal, identity) = cx.read(|cx| {
                let s = entity.read(cx);
                s.explorer_items(cx)
                    .iter()
                    .enumerate()
                    .find_map(|(i, item)| match item {
                        crate::namespace_ui::ExplorerItem::Object(id)
                            if id.kind == ObjectKind::Procedure && id.name == "recalculate" =>
                        {
                            Some((i, (*id).clone()))
                        }
                        _ => None,
                    })
                    .unwrap()
            });
            assert_eq!(ordinal, 3, "Procedures follow the two tables and view");
            click(cx, w, "explorer-item-3");
            settle(cx, w, &entity);
            cx.read(|cx| {
                let s = entity.read(cx);
                assert!(s.selected_sidebar_object(&identity));
                assert_drafts(s, cx);
            });
            click(cx, w, "copy-full-definition");
            cx.update(|cx| {
                assert!(
                    cx.read_from_clipboard()
                        .unwrap()
                        .text()
                        .unwrap()
                        .contains("recalculate")
                )
            });
            cx.simulate_keystrokes(w, "escape");
            click(cx, w, "explorer-group-functions");
            settle(cx, w, &entity);
            click(cx, w, "explorer-group-triggers");
            settle(cx, w, &entity);
            if engine == Engine::Postgres {
                click(cx, w, "explorer-group-sequences");
                settle(cx, w, &entity);
            }
            cx.read(|cx| {
                let s = entity.read(cx);
                let items = s.explorer_items(cx);
                assert!(items.iter().any(|item| matches!(item, crate::namespace_ui::ExplorerItem::Object(id) if id.kind == ObjectKind::Function && id.name == "answer")));
                assert!(items.iter().any(|item| matches!(item, crate::namespace_ui::ExplorerItem::Object(id) if id.kind == ObjectKind::Trigger && id.name == "insert_guard" && id.parent.as_deref() == Some("guard_items"))));
                if engine == Engine::Postgres {
                    assert!(items.iter().any(|item| matches!(item, crate::namespace_ui::ExplorerItem::Object(id) if id.kind == ObjectKind::Sequence && id.name == "manual_sequence")));
                }
            });
            cx.update_window(w, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.explorer_filter.update(cx, |input, cx| {
                        input.set_value("recalculate", w, cx);
                        input.focus(w, cx);
                    })
                })
            })
            .unwrap();
            cx.simulate_keystrokes(w, "end enter");
            settle(cx, w, &entity);
            cx.read(|cx| assert!(entity.read(cx).selected_sidebar_object(&identity)));
            cx.simulate_keystrokes(w, "escape");
            let next_scope = if engine == Engine::MySql {
                "rowboat"
            } else {
                "public"
            };
            if engine == Engine::MySql {
                runtime
                    .handle()
                    .block_on(db.select_database(next_scope))
                    .unwrap();
            }
            let snapshot = runtime
                .handle()
                .block_on(db.namespace_catalog(Some(next_scope)))
                .unwrap();
            cx.update_window(w, |_, _, cx| entity.update(cx, |s, cx| {
                s.accept_namespace(snapshot, Some(next_scope.into()), cx);
                assert!(s.objects.groups.is_empty());
                s.busy = true;
                s.receive_object(crate::objects_ui::ObjectEvent::Group { request: uuid::Uuid::new_v4(), schema: schema.clone(), kind: ObjectKind::Procedure, result: Ok(vec![identity.clone()]) }, cx);
                assert!(s.busy && s.objects.groups.is_empty(), "A stale catalog reply must not replace the new scope or release another operation");
                s.busy = false;
                assert_drafts(s, cx);
            })).unwrap();
            let guard = runtime
                .handle()
                .block_on(db.query(&format!("SELECT calls FROM {ns}.inspection_guard"), 1))
                .unwrap();
            assert_eq!(
                guard[0].rows[0][0].display(),
                "0",
                "Selecting a procedure must inspect it, never CALL it"
            );
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
