//! Pass 13 connection workflow regressions.
use crate::{
    app::{Event, Workbench},
    forms::ConnectionForm,
};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, RowChange, SqliteFileOptions, SqliteOpenMode,
    TableRef,
};
use std::time::Duration;

#[gpui_kit::test]
fn pass13_connection_stalled_handshake_is_dismissible_bounded_and_releases_socket(
    cx: &mut TestAppContext,
) {
    use tokio::io::AsyncReadExt;
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let listener = runtime
        .handle()
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (events, received) = std::sync::mpsc::channel();
    let server = runtime.handle().spawn(async move {
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            events.send((index, "accepted")).unwrap();
            let mut buffer = [0; 1024];
            loop {
                match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            events.send((index, "closed")).unwrap();
        }
    });
    let (w, s) = crate::workbench_tests::workspace(cx, 137, runtime.handle());
    let p = ConnectionProfile {
        name: "Task-local stalled peer".into(),
        port,
        ssl_mode: "disable".into(),
        ..Default::default()
    };
    for index in 0..2 {
        cx.update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.form = Some(ConnectionForm::new(p.clone(), String::new(), w, cx));
                s.test_connection(cx);
                assert!(!s.busy); // the rest of the form remains usable
            })
        })
        .unwrap();
        let mut accepted = false;
        for _ in 0..1000 {
            pump(cx, w, &s);
            if received.try_recv() == Ok((index, "accepted")) {
                accepted = true;
                break;
            }
        }
        assert!(accepted);
        if index == 0 {
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.form.as_ref().unwrap().fields["name"]
                        .update(cx, |v, cx| v.set_value("still editable", w, cx));
                    s.form = None; // the production close/Escape path drops this same guard
                })
            })
            .unwrap();
        }
        let started = std::time::Instant::now();
        let mut closed = false;
        for _ in 0..5000 {
            pump(cx, w, &s);
            if received.try_recv() == Ok((index, "closed")) {
                closed = true;
                break;
            }
        }
        assert!(closed, "Temporary handshake socket was not released");
        assert!(started.elapsed() < Duration::from_secs(if index == 0 { 3 } else { 23 }));
        if index == 1 {
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(
                    s.form
                        .as_ref()
                        .unwrap()
                        .test_result
                        .as_ref()
                        .unwrap()
                        .1
                        .contains("timed out")
                );
                assert_eq!(s.shared.borrow().revision, 0);
            });
        }
    }
    runtime.handle().block_on(server).unwrap();
}

fn directory() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.local/pass13-connection-review");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("gui-")
        .tempdir_in(root)
        .unwrap()
}
fn sqlite(path: &std::path::Path) -> ConnectionProfile {
    ConnectionProfile {
        name: "Pass13 connection".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.into(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    }
}
fn network(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        name: "Pass13 disposable".into(),
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "rowboat"
        }
        .into(),
        database: if engine == Engine::Postgres {
            "postgres"
        } else {
            "rowboat"
        }
        .into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
fn pump(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>) {
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(20));
    cx.update_window(w, |_, w, cx| s.update(cx, |s, cx| s.poll(w, cx)))
        .unwrap();
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(5));
}
fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>) {
    for _ in 0..2000 {
        pump(cx, w, s);
        if cx.read(|cx| {
            let s = s.read(cx);
            !s.busy && s.form.as_ref().is_none_or(|f| f.test_request.is_none())
        }) {
            return;
        }
    }
    panic!("Connection operation timed out");
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}
fn command(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
    commands: &std::sync::mpsc::Receiver<crate::workspace_shell::Command>,
) -> crate::workspace_shell::Command {
    for _ in 0..2000 {
        pump(cx, w, s);
        if let Ok(command) = commands.try_recv() {
            return command;
        }
    }
    panic!(
        "Coordinator command timed out: {}",
        cx.read(|cx| s.read(cx).status.clone())
    );
}
fn attach(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
    p: &ConnectionProfile,
    db: Database,
) {
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, _| {
            s.owner = Some(p.id.clone());
            s.profile = Some(p.clone());
            s.database = Some(db);
            s.tabs[0].connection_id = Some(p.id.clone());
            s.startup.autorun = false;
            s.shared.borrow_mut().state.connections.push(p.clone());
            s.state = s.shared.borrow().state.clone();
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass13_connection_engine_drafts_and_stale_keyring(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 130, runtime.handle());
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            let p = ConnectionProfile {
                port: 5678,
                database: "custom".into(),
                username: "custom_user".into(),
                ..Default::default()
            };
            s.form = Some(ConnectionForm::new(p, "draft-secret".into(), w, cx));
            let f = s.form.as_mut().unwrap();
            f.ssh = true;
            f.remember = true;
            f.credential_exists = true;
            let initial = f.fingerprint(cx);
            f.select_engine(Engine::Postgres, w, cx);
            assert_eq!(initial, f.fingerprint(cx));
            let request = uuid::Uuid::new_v4();
            f.credential_request = Some(request);
            f.keyring_loading = true;
            let id = f.id.clone();
            f.select_engine(Engine::Sqlite, w, cx);
            assert!(f.password.read(cx).value().is_empty());
            assert!(!f.ssh && !f.remember && !f.keyring_loading);
            f.fields["file_path"].update(cx, |v, cx| v.set_value("/task/draft.sqlite", w, cx));
            f.select_engine(Engine::Postgres, w, cx);
            assert_eq!(initial, f.fingerprint(cx));
            s.tx.send(Event::CredentialLookup {
                request,
                connection_id: id,
                result: Ok(Some("late-secret".into())),
            })
            .unwrap();
            s.poll(w, cx);
            assert_eq!(
                s.form.as_ref().unwrap().password.read(cx).value().as_ref(),
                "draft-secret"
            );
            let f = s.form.as_mut().unwrap();
            f.select_engine(Engine::Sqlite, w, cx);
            assert_eq!(
                f.fields["file_path"].read(cx).value().as_ref(),
                "/task/draft.sqlite"
            );
            assert!(f.password.read(cx).value().is_empty());
            // Same profile ID in a NEW form must not accept the old request either.
            let new = f.profile(cx).unwrap();
            s.form = Some(ConnectionForm::new(new, String::new(), w, cx));
            s.tx.send(Event::CredentialLookup {
                request,
                connection_id: s.form.as_ref().unwrap().id.clone(),
                result: Ok(Some("late-secret".into())),
            })
            .unwrap();
            s.poll(w, cx);
            assert!(
                s.form
                    .as_ref()
                    .unwrap()
                    .password
                    .read(cx)
                    .value()
                    .is_empty()
            );
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass13_connection_test_sqlite_no_creation_or_active_session_mutation(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let p = sqlite(&dir.path().join("existing.sqlite"));
    let db = runtime
        .handle()
        .block_on(Database::connect(&p, ""))
        .unwrap();
    runtime
        .handle()
        .block_on(db.query(
            "CREATE TABLE sentinel(id INTEGER); INSERT INTO sentinel VALUES(1); BEGIN",
            0,
        ))
        .unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 131, runtime.handle());
    attach(cx, w, &s, &p, db.clone());
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.transaction = true;
            s.form = Some(ConnectionForm::new(
                p.clone(),
                "must-not-be-used".into(),
                w,
                cx,
            ));
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("DELETE FROM sentinel", w, cx));
            s.tabs[0]
                .parameters
                .update(cx, |e, cx| e.set_value("{\"id\":1}", w, cx));
        })
    })
    .unwrap();
    click(cx, w, "test-connection");
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(
            s.form
                .as_ref()
                .unwrap()
                .test_result
                .as_ref()
                .unwrap()
                .1
                .contains("succeeded")
        );
        assert!(s.transaction && s.transaction_known);
        assert_eq!(s.session_generation, 0);
        assert_eq!(s.shared.borrow().revision, 0);
        assert!(s.passwords.is_empty());
        assert_eq!(s.query_run_invocations, 0);
        assert_eq!(s.tabs[0].parameters.read(cx).value().as_ref(), "{\"id\":1}");
    });
    let missing = dir.path().join("missing.sqlite");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.form = Some(ConnectionForm::new(sqlite(&missing), String::new(), w, cx));
            s.test_connection(cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    assert!(!missing.exists());
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .form
                .as_ref()
                .unwrap()
                .test_result
                .as_ref()
                .unwrap()
                .1
                .contains("no database was created")
        )
    });
    assert!(runtime.handle().block_on(db.in_transaction()).unwrap());
    assert_eq!(
        runtime
            .handle()
            .block_on(db.query("SELECT count(*) FROM sentinel", 1))
            .unwrap()[0]
            .rows[0][0],
        Cell::Text("1".into())
    );
    runtime.handle().block_on(db.rollback()).unwrap();
}

#[gpui_kit::test]
fn pass13_connection_test_rejects_changed_reopened_and_old_generation_form(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let p = sqlite(&dir.path().join("stale.sqlite"));
    let _db = runtime
        .handle()
        .block_on(Database::connect(&p, ""))
        .unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 132, runtime.handle());
    for scenario in 0..3 {
        cx.update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.form = Some(ConnectionForm::new(p.clone(), String::new(), w, cx));
                s.test_connection(cx);
                match scenario {
                    0 => s.form.as_mut().unwrap().fields["name"]
                        .update(cx, |v, cx| v.set_value("changed", w, cx)),
                    1 => s.form = Some(ConnectionForm::new(p.clone(), String::new(), w, cx)),
                    _ => {
                        s.tx.route.generation += 1;
                        s.form = Some(ConnectionForm::new(p.clone(), String::new(), w, cx));
                    }
                }
            })
        })
        .unwrap();
        for _ in 0..40 {
            pump(cx, w, &s);
        }
        cx.read(|cx| assert!(s.read(cx).form.as_ref().unwrap().test_result.is_none()));
    }
}

#[gpui_kit::test]
#[ignore = "Explicit disposable PostgreSQL15435/MySQL13309"]
fn pass13_connection_test_network_success_failure_preserves_session_and_storage(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let p = network(engine);
        let db = runtime
            .handle()
            .block_on(Database::connect(&p, "rowboat-test"))
            .unwrap();
        runtime.handle().block_on(db.begin()).unwrap();
        let (w, s) = crate::workbench_tests::workspace(cx, 133, runtime.handle());
        attach(cx, w, &s, &p, db.clone());
        for password in ["rowboat-test", "intentionally-wrong-pass13"] {
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.transaction = true;
                    s.form = Some(ConnectionForm::new(p.clone(), password.into(), w, cx));
                    let f = s.form.as_mut().unwrap();
                    f.remember = true;
                    f.credential_exists = true;
                    s.test_connection(cx);
                })
            })
            .unwrap();
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                let message = &s.form.as_ref().unwrap().test_result.as_ref().unwrap().1;
                assert!(
                    message.contains(if password == "rowboat-test" {
                        "succeeded"
                    } else {
                        "failed"
                    }),
                    "{message}"
                );
                assert_eq!(s.shared.borrow().revision, 0);
                assert!(s.passwords.is_empty());
                assert_eq!(s.session_generation, 0);
                assert!(s.transaction && s.database.is_some());
                assert_eq!(s.query_run_invocations, 0);
            });
            assert!(runtime.handle().block_on(db.in_transaction()).unwrap());
        }
        runtime.handle().block_on(db.rollback()).unwrap();
    }
}

#[gpui_kit::test]
fn pass13_connection_replacement_rechecks_healthy_transaction_and_failed_checkpoint(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let p = sqlite(&dir.path().join("guard.sqlite"));
    let db = runtime
        .handle()
        .block_on(Database::connect(&p, ""))
        .unwrap();
    runtime.handle().block_on(db.query("CREATE TABLE guard_items(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO guard_items VALUES(1,'original')", 0)).unwrap();
    let (w, s) = crate::workbench_tests::workspace(cx, 134, runtime.handle());
    attach(cx, w, &s, &p, db.clone());
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT id,name FROM guard_items", w, cx)
            });
            s.run_query_for_editing(cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.read(|cx| assert!(s.read(cx).query_plan().is_some()));
    runtime.handle().block_on(db.begin()).unwrap();
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.transaction = true;
            assert!(!s.connection_settings_enabled());
            s.transaction_known = false;
            assert!(s.connection_settings_enabled());
            s.connection_settings(w, cx);
            s.form.as_mut().unwrap().confirm_replace = true;
            s.replace_unknown_session(cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.as_ref().unwrap().contains("active transaction"));
        assert_eq!(s.session_generation, 0);
        assert!(s.transaction && s.transaction_known && s.database.is_some());
    });
    runtime.handle().block_on(db.rollback()).unwrap();
    let (sender, commands) = std::sync::mpsc::channel();
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, cx| {
            s.shell_tx = sender;
            s.transaction_known = false;
            s.replace_unknown_session(cx);
        })
    })
    .unwrap();
    let crate::workspace_shell::Command::PersistConnectionRecovery { ack, .. } =
        command(cx, w, &s, &commands)
    else {
        panic!("Expected recovery checkpoint");
    };
    ack.send(Err("injected disk failure".into())).unwrap();
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.as_ref().unwrap().contains("checkpoint failed"));
        assert_eq!(s.session_generation, 0);
        assert!(!s.transaction_known && s.database.is_some());
    });
    // A recovered healthy session still uses the ordinary Save & Connect path.
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, cx| {
            s.form = None;
            s.transaction("ROLLBACK", cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            assert!(s.connection_settings_enabled());
            s.tabs[0]
                .parameters
                .update(cx, |e, cx| e.set_value("{\"healthy\":true}", w, cx));
            s.connection_settings(w, cx);
            assert!(!s.form.as_ref().unwrap().confirm_replace);
        })
    })
    .unwrap();
    click(cx, w, "connect");
    let crate::workspace_shell::Command::SaveProfile { save, .. } = command(cx, w, &s, &commands)
    else {
        panic!("Expected ordinary profile save");
    };
    cx.update(|cx| s.update(cx, |s, cx| s.commit_profile_save(*save, cx)));
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.form.is_none() && s.database.is_some());
        assert!(!s.transaction && s.transaction_known);
        assert_eq!(
            s.tabs[0].parameters.read(cx).value().as_ref(),
            "{\"healthy\":true}"
        );
        assert_eq!(s.query_run_invocations, 1);
        assert!(s.query_edit.results.is_empty());
    });
}

#[gpui_kit::test]
#[ignore = "Explicit disposable PostgreSQL15435/MySQL13309; terminates only own backend"]
fn pass13_connection_dead_session_replacement_durable_evidence_isolation_no_replay(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let p = network(engine);
        let db = runtime
            .handle()
            .block_on(Database::connect(&p, "rowboat-test"))
            .unwrap();
        let other = runtime
            .handle()
            .block_on(Database::connect(&p, "rowboat-test"))
            .unwrap();
        let controller = runtime
            .handle()
            .block_on(Database::connect(&p, "rowboat-test"))
            .unwrap();
        let fixture = format!("pass13_connection_{}", uuid::Uuid::new_v4().simple());
        let schema = if engine == Engine::Postgres {
            fixture.clone()
        } else {
            "rowboat".into()
        };
        let name = if engine == Engine::Postgres {
            "items".into()
        } else {
            fixture
        };
        let q = rowboat_core::sql::quote_ident(engine, &schema);
        let qt = rowboat_core::sql::quote_ident(engine, &name);
        let table = TableRef {
            schema: schema.clone(),
            name,
        };
        let create_schema = if engine == Engine::Postgres {
            format!("CREATE SCHEMA {q}; ")
        } else {
            String::new()
        };
        runtime.handle().block_on(db.query(&format!("{create_schema}CREATE TABLE {q}.{qt}(id INT PRIMARY KEY, name TEXT); INSERT INTO {q}.{qt} VALUES(1,'original')"), 0)).unwrap();
        let structure = runtime.handle().block_on(db.structure(&table)).unwrap();
        let rows = runtime
            .handle()
            .block_on(db.query(&format!("SELECT id,name FROM {q}.{qt}"), 10))
            .unwrap()
            .remove(0);
        let backend = runtime
            .handle()
            .block_on(db.query(
                if engine == Engine::Postgres {
                    "SELECT pg_backend_pid()"
                } else {
                    "SELECT CONNECTION_ID()"
                },
                1,
            ))
            .unwrap()[0]
            .rows[0][0]
            .display();
        runtime.handle().block_on(other.begin()).unwrap();
        let (w, s) = crate::workbench_tests::workspace(cx, 135, runtime.handle());
        let (ow, os) = crate::workbench_tests::workspace(cx, 136, runtime.handle());
        attach(cx, w, &s, &p, db.clone());
        attach(cx, ow, &os, &p, other.clone());
        cx.update(|cx| os.update(cx, |s, _| s.transaction = true));
        let (sender, commands) = std::sync::mpsc::channel();
        let old_sender = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.shell_tx = sender;
                    s.table = Some(table.clone());
                    s.structure = structure;
                    s.table_result = rows;
                    s.page = crate::app::Page::Table;
                    s.change_target = s.current_target();
                    s.changes.push(RowChange::Update {
                        original: vec![
                            ("id".into(), Cell::Text("1".into())),
                            ("name".into(), Cell::Text("original".into())),
                        ],
                        values: vec![("name".into(), Cell::Text("draft".into()))],
                    });
                    s.capture_recovery();
                    assert!(s.recovery.error.is_none());
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(format!("UPDATE {q}.{qt} SET name='REPLAYED'"), w, cx)
                    });
                    s.tabs[0]
                        .parameters
                        .update(cx, |e, cx| e.set_value("{\"keep\":7}", w, cx));
                    s.tx.clone()
                })
            })
            .unwrap();
        runtime
            .handle()
            .block_on(controller.query(
                &if engine == Engine::Postgres {
                    format!("SELECT pg_terminate_backend({backend})")
                } else {
                    format!("KILL CONNECTION {backend}")
                },
                1,
            ))
            .unwrap();
        let lost = runtime
            .handle()
            .block_on(db.in_transaction())
            .map_err(|e| e.to_string());
        assert!(lost.is_err());
        cx.update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.tx.send(Event::TransactionState(lost)).unwrap();
                s.poll(w, cx);
                assert!(s.connection_settings_enabled());
                // Direct form construction avoids ALL user keyring access in this harness.
                s.form = Some(ConnectionForm::new(p.clone(), "rowboat-test".into(), w, cx));
                s.replace_unknown_session(cx);
                assert!(!s.busy);
                assert_eq!(s.changes.len(), 1); // confirmation is required
                s.form.as_mut().unwrap().confirm_replace = true;
                s.replace_unknown_session(cx);
            })
        })
        .unwrap();
        let crate::workspace_shell::Command::PersistConnectionRecovery { route, ack, .. } =
            command(cx, w, &s, &commands)
        else {
            panic!("Expected recovery checkpoint");
        };
        let dir = directory();
        let store = crate::session::SessionStore::at(dir.path()).unwrap();
        let snapshot = cx.read(|cx| {
            let s = s.read(cx);
            assert_eq!(s.tx.route, route);
            assert!(s.database.is_some()); // retirement waits for real durable acknowledgment
            assert!(s.changes.is_empty());
            assert_eq!(s.recovery.detached.len(), 1);
            assert_eq!(
                s.recovery.detached[0].phase,
                crate::recovery_ui::RecoveryPhase::Stale
            );
            s.session_snapshot(cx)
        });
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), snapshot);
        ack.send(Ok(())).unwrap();
        let crate::workspace_shell::Command::SaveProfile { save, .. } =
            command(cx, w, &s, &commands)
        else {
            panic!("Expected profile save");
        };
        cx.update(|cx| s.update(cx, |s, cx| s.commit_profile_save(*save, cx)));
        settle(cx, w, &s);
        old_sender.send(Event::TransactionState(Ok(true))).unwrap();
        old_sender
            .send(Event::Notice(Err("OLD CALLBACK".into())))
            .unwrap();
        pump(cx, w, &s);
        let replacement = cx.read(|cx| {
            let s = s.read(cx);
            assert!(s.error.is_none(), "{:?}", s.error);
            assert!(s.status.contains("outcome unconfirmed"));
            assert!(s.session_generation > route.generation);
            assert!(!s.transaction && s.transaction_known);
            assert_eq!(
                s.tabs[0].parameters.read(cx).value().as_ref(),
                "{\"keep\":7}"
            );
            assert!(s.tabs[0].editor.read(cx).value().contains("REPLAYED"));
            assert_eq!(s.query_run_invocations, 0);
            assert!(s.query_edit.results.is_empty());
            assert_eq!(s.recovery.detached.len(), 1);
            assert!(os.read(cx).transaction);
            s.database.clone().unwrap()
        });
        assert!(runtime.handle().block_on(other.in_transaction()).unwrap());
        assert_eq!(
            runtime
                .handle()
                .block_on(replacement.query(&format!("SELECT name FROM {q}.{qt}"), 1))
                .unwrap()[0]
                .rows[0][0],
            Cell::Text("original".into())
        );
        runtime.handle().block_on(other.rollback()).unwrap();
        runtime
            .handle()
            .block_on(controller.query(
                &if engine == Engine::Postgres {
                    format!("DROP SCHEMA {q} CASCADE")
                } else {
                    format!("DROP TABLE {q}.{qt}")
                },
                0,
            ))
            .unwrap();
    }
}
