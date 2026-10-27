//! Actual connection forms/pickers. Network cases use only explicit disposable fixtures.
use crate::{app::Workbench, workbench_tests::workspace};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use rowboat_core::{ConnectionProfile, Engine};

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(cx, window);
}
fn settle(cx: &mut TestAppContext, window: AnyWindowHandle, entity: &Entity<Workbench>) {
    for _ in 0..1000 {
        let busy = cx
            .update_window(window, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.poll(w, cx);
                    s.busy
                })
            })
            .unwrap();
        if !busy {
            draw(cx, window);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("Connection operation did not finish");
}

#[gpui_kit::test]
fn url_paste_autodetects_postgres_and_keeps_credentials_out_of_profiles(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 701, runtime.handle());
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| s.edit_connection(None, w, cx))
    })
    .unwrap();
    cx.update(|cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
        "postgresql://us%40er:private%3A%E9%9B%AA%0A@[::1]:5433/db%2Fname?sslmode=verify-full&sslrootcert=%2Fexample%2Fca.pem".into())));
    click(cx, window, "paste-connection-url");
    cx.read(|cx| {
        let s = entity.read(cx);
        let f = s.form.as_ref().unwrap();
        let p = f.profile(cx).unwrap();
        assert_eq!(p.engine, Engine::Postgres);
        assert_eq!(
            (
                p.host.as_str(),
                p.port,
                p.database.as_str(),
                p.username.as_str()
            ),
            ("::1", 5433, "db/name", "us@er")
        );
        assert_eq!(p.ssl_mode, "verify-full");
        assert_eq!(p.ssl_ca.as_deref(), Some("/example/ca.pem"));
        assert_eq!(f.password_value(cx), "private:雪\n");
        assert!(f.has_exact_password());
        assert!(f.password_dirty && !f.remember && f.url.read(cx).value().is_empty());
        assert!(f.advanced && f.url_error.is_none());
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("private") && !json.contains("password"));
        assert!(s.shared.borrow().state.connections.is_empty());
        assert!(s.database.is_none() && !s.busy);
    });
    click(cx, window, "engine-mysql");
    click(cx, window, "engine-postgres");
    cx.read(|cx| {
        let f = entity.read(cx).form.as_ref().unwrap();
        assert_eq!(f.password_value(cx), "private:雪\n");
        assert!(f.has_exact_password());
    });
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.form
                .as_ref()
                .unwrap()
                .password
                .update(cx, |i, cx| i.focus(w, cx));
        })
    })
    .unwrap();
    cx.simulate_keystrokes(window, "ctrl-a");
    // Even pasting the exact visible preview must discard the hidden line break.
    cx.simulate_input(window, "private:雪");
    cx.read(|cx| {
        let f = entity.read(cx).form.as_ref().unwrap();
        assert_eq!(f.password_value(cx), "private:雪");
        assert!(!f.has_exact_password());
    });
}

#[gpui_kit::test]
fn url_test_action_parses_before_testing_without_cancelling_its_new_trial(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 704, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_connection(None, w, cx);
            s.form.as_ref().unwrap().url.update(cx, |i, cx| {
                i.set_value(
                    "postgres://reader:private-trial@127.0.0.1:1/unused?sslmode=disable",
                    w,
                    cx,
                )
            });
        })
    })
    .unwrap();
    click(cx, window, "test-connection");
    for _ in 0..400 {
        let done = cx
            .update_window(window, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.poll(w, cx);
                    s.form.as_ref().unwrap().test_result.is_some()
                })
            })
            .unwrap();
        if done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cx.read(|cx| {
        let s = entity.read(cx);
        let f = s.form.as_ref().unwrap();
        let (_, message) = f
            .test_result
            .as_ref()
            .expect("Trial was incorrectly cancelled while clearing the parsed URL");
        assert!(message.contains("failed") && !message.contains("private-trial"));
        assert!(f.url.read(cx).value().is_empty() && f.test_request.is_none());
        assert!(s.database.is_none() && s.shared.borrow().state.connections.is_empty());
    });
}

#[gpui_kit::test]
fn url_enter_populates_mysql_preserves_name_and_policy_and_rejects_stale_keyring(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 702, runtime.handle());
    let stale_credential = uuid::Uuid::new_v4();
    let stale_trial = uuid::Uuid::new_v4();
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_connection(None, w, cx);
            let f = s.form.as_mut().unwrap();
            f.fields["name"].update(cx, |i, cx| i.set_value("My custom name", w, cx));
            f.fields["group"].update(cx, |i, cx| i.set_value("Reporting", w, cx));
            f.read_only = true;
            f.ssh = true;
            for (key, value) in [
                ("ssh_host", "bastion.example"),
                ("ssh_port", "2222"),
                ("ssh_username", "operator"),
                ("ssh_identity", "/example/key"),
            ] {
                f.fields[key].update(cx, |i, cx| i.set_value(value, w, cx));
            }
            f.remember = true;
            f.credential_exists = true;
            f.credential_request = Some(stale_credential);
            f.test_request = Some(stale_trial);
            f.keyring_loading = true;
            f.password
                .update(cx, |i, cx| i.set_value("old-password", w, cx));
            f.url.update(cx, |i, cx| {
                i.set_value(
                    "mysql://reader@localhost/reports?ssl-mode=VERIFY_IDENTITY",
                    w,
                    cx,
                );
                i.focus(w, cx);
            });
        })
    })
    .unwrap();
    cx.simulate_keystrokes(window, "enter");
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let f = s.form.as_ref().unwrap();
            s.tx.send(crate::app::Event::CredentialLookup {
                request: stale_credential,
                connection_id: f.id.clone(),
                result: Ok(Some("stale-keyring-secret".into())),
            })
            .unwrap();
            s.tx.send(crate::app::Event::Connection(Box::new(
                crate::connection_ui::ConnectionEvent::Tested {
                    request: stale_trial,
                    instance: f.instance,
                    fingerprint: f.fingerprint(cx),
                    success: true,
                    message: "stale trial succeeded".into(),
                },
            )))
            .unwrap();
            s.poll(w, cx);
        })
    })
    .unwrap();
    cx.read(|cx| {
        let s = entity.read(cx);
        let f = s.form.as_ref().unwrap();
        let p = f.profile(cx).unwrap();
        assert_eq!(p.engine, Engine::MySql);
        assert_eq!(p.port, 3306);
        assert_eq!(p.name, "My custom name");
        assert_eq!(p.group, "Reporting");
        assert!(p.read_only && !f.keyring_loading && f.credential_request.is_none());
        assert!(f.credential_exists && !f.remember && f.test_result.is_none());
        let ssh = p.ssh.unwrap();
        assert_eq!(
            (ssh.host.as_str(), ssh.port, ssh.username.as_str()),
            ("bastion.example", 2222, "operator")
        );
        assert_eq!(ssh.identity_file.as_deref(), Some("/example/key"));
        assert_eq!(p.ssl_mode, "verify-full");
        assert!(f.password.read(cx).value().is_empty());
    });
}

#[gpui_kit::test]
fn invalid_urls_are_atomic_redacted_and_block_save(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 703, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| s.edit_connection(None, w, cx))
    })
    .unwrap();
    for raw in [
        "mongodb://reader:do-not-echo@host/db",
        "postgres://reader:do-not-echo@host/db?unsupported=value",
        "postgres://reader:do-not-echo%GG@host/db",
    ] {
        cx.update_window(window, |_, w, cx| {
            entity.update(cx, |s, cx| {
                let f = s.form.as_mut().unwrap();
                f.url.update(cx, |i, cx| i.set_value(raw, w, cx));
                assert!(!s.fill_connection_from_url(w, cx));
                let f = s.form.as_ref().unwrap();
                assert!(!f.url_error.as_ref().unwrap().contains("do-not-echo"));
                assert_eq!(f.fields["host"].read(cx).value().as_ref(), "127.0.0.1");
                assert!(f.password.read(cx).value().is_empty());
                s.save_connection(false, cx);
                assert!(!s.busy && s.shared.borrow().state.connections.is_empty());
                assert!(!s.error.as_ref().unwrap().contains("do-not-echo"));
            })
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn connection_manager_save_parses_pasted_url_without_connecting(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, shell, _) = crate::pass14_tools::workspace(cx, runtime.handle(), vec![]);
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    click(cx, window, "manager-new");
    let editor = cx.read(|cx| shell.read(cx).test_manager_editor().unwrap());
    cx.update_window(window, |_, w, cx| {
        editor.update(cx, |s, cx| {
            s.form.as_ref().unwrap().url.update(cx, |i, cx| {
                i.set_value(
                    "mysql://reader:memory-only-secret%0A@db.example:3307/reports",
                    w,
                    cx,
                )
            });
        })
    })
    .unwrap();
    click(cx, window, "save-connection");
    for _ in 0..100 {
        cx.update_window(window, |_, w, cx| {
            shell.update(cx, |s, cx| s.test_tick(w, cx))
        })
        .unwrap();
        if cx.read(|cx| shell.read(cx).test_manager_editor().is_none()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cx.read(|cx| {
        let s = editor.read(cx);
        assert!(s.form.is_none() && s.database.is_none() && !s.busy);
        let shared = s.shared.borrow();
        assert_eq!(shared.state.connections.len(), 1);
        let p = &shared.state.connections[0];
        assert_eq!(
            s.passwords.get(&p.id).map(String::as_str),
            Some("memory-only-secret\n")
        );
        assert_eq!(
            (p.engine, p.port, p.database.as_str()),
            (Engine::MySql, 3307, "reports")
        );
        assert!(
            !serde_json::to_string(&shared.state)
                .unwrap()
                .contains("memory-only-secret")
        );
        assert_eq!(s.query_run_invocations, 0);
    });
}

#[gpui_kit::test]
#[ignore = "Requires explicit disposable PostgreSQL on 15435; creates only uniquely named test databases"]
fn postgres_database_and_schema_pickers_preserve_sessions_and_reject_failed_or_stale_switches(
    cx: &mut TestAppContext,
) {
    use rowboat_core::{Database, sql::quote_ident};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut profile = ConnectionProfile {
        engine: Engine::Postgres,
        host: "127.0.0.1".into(),
        port: 15435,
        database: "rowboat".into(),
        username: "postgres".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    };
    let admin = runtime
        .handle()
        .block_on(Database::connect(&profile, "rowboat-test"))
        .unwrap();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let target = format!("rb_gui_db_{}_quote'雪", &id[..12]);
    let gone = format!("rb_gui_db_{}_gone", &id[..12]);
    runtime.handle().block_on(async {
        for name in [&target, &gone] {
            admin.query(&format!("CREATE DATABASE {}", quote_ident(Engine::Postgres, name)),0).await.unwrap();
        }
        let mut p = profile.clone(); p.database = target.clone();
        let setup = Database::connect(&p,"rowboat-test").await.unwrap();
        setup.query("CREATE SCHEMA analysis; CREATE TABLE analysis.witness(id INTEGER PRIMARY KEY); INSERT INTO analysis.witness VALUES(7)",0).await.unwrap();
    });
    profile.read_only = true;
    let old = runtime
        .handle()
        .block_on(Database::connect(&profile, "rowboat-test"))
        .unwrap();
    let initial = runtime
        .handle()
        .block_on(old.namespace_catalog(None))
        .unwrap();
    let original_result = runtime
        .handle()
        .block_on(old.query("SELECT current_database() AS origin", 1))
        .unwrap();
    let old_pid = runtime
        .handle()
        .block_on(old.query("SELECT pg_backend_pid()", 1))
        .unwrap()[0]
        .rows[0][0]
        .clone();
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (window, shell, workspaces) =
            crate::pass14_tools::workspace(cx, runtime.handle(), vec![profile.clone()]);
        let entity = workspaces[0].clone();
        cx.simulate_window_resize(window, size(px(960.), px(640.)));
        cx.update_window(window, |_, w, cx| {
            entity.update(cx, |s, cx| {
                s.database = Some(old.clone());
                s.profile = Some(profile.clone());
                s.endpoint = Some(profile.clone());
                s.session_password = "rowboat-test".into();
                s.session_ready = true;
                s.accept_namespace(initial, None, cx);
                s.tabs[0].results = original_result.clone();
                s.namespace.result_context.insert(
                    s.tabs[0].id,
                    "SQL context · database rowboat · schema public".into(),
                );
                s.tabs[0].editor.update(cx, |e, cx| {
                    e.set_value("SELECT 1/0; -- never replay on database switch", w, cx)
                });
            })
        })
        .unwrap();
        draw(cx, window);
        let mut visual = VisualTestContext::from_window(window, cx);
        let database = visual.debug_bounds("database-selector").unwrap();
        let schema = visual.debug_bounds("namespace-selector").unwrap();
        assert!(
            database.bottom() <= schema.top(),
            "Database/schema controls overlap"
        );
        click(cx, window, "database-selector");
        settle(cx, window, &entity);
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.namespace_choices(cx).contains(&target));
            assert!(!s.namespace_choices(cx).contains(&"analysis".into()));
        });
        cx.simulate_input(window, &target);
        cx.simulate_keystrokes(window, "enter");
        settle(cx, window, &entity);
        let (new, generation) = cx.read(|cx| {
            let s = entity.read(cx);
            assert_eq!(s.profile.as_ref().unwrap().database, target);
            assert_eq!(s.endpoint.as_ref().unwrap().database, target);
            assert!(s.profile.as_ref().unwrap().read_only);
            assert!(s.error.is_none(), "{:?}", s.error);
            assert_eq!(
                s.tabs[0].editor.read(cx).value().as_ref(),
                "SELECT 1/0; -- never replay on database switch"
            );
            assert_eq!(s.tabs[0].results[0].rows, original_result[0].rows);
            assert!(s.namespace.result_context[&s.tabs[0].id].contains("database rowboat"));
            assert_eq!(s.shared.borrow().state.connections[0].database, "rowboat");
            assert_eq!(s.query_run_invocations, 0);
            assert_eq!(s.namespace_scope(), Some("public"));
            (s.database.clone().unwrap(), s.session_generation)
        });
        runtime.handle().block_on(async {
            assert_eq!(
                old.namespace_catalog(None).await.unwrap().database,
                "rowboat"
            );
            assert_ne!(
                new.query("SELECT pg_backend_pid()", 1).await.unwrap()[0].rows[0][0],
                old_pid
            );
            assert_eq!(
                new.query("SHOW default_transaction_read_only", 1)
                    .await
                    .unwrap()[0]
                    .rows[0][0]
                    .display(),
                "on"
            );
        });
        click(cx, window, "namespace-selector");
        cx.simulate_input(window, "analysis");
        cx.simulate_keystrokes(window, "enter");
        settle(cx, window, &entity);
        cx.read(|cx| {
            let s = entity.read(cx);
            assert_eq!(s.namespace_scope(), Some("analysis"));
            assert_eq!(s.catalog[0].table.name, "witness");
            assert_eq!(s.profile.as_ref().unwrap().database, target);
        });
        assert_eq!(
            runtime
                .handle()
                .block_on(new.namespace_catalog(None))
                .unwrap()
                .current_schema
                .as_deref(),
            Some("public")
        );

        // Remove a database after discovery: failure must retain the working target.
        click(cx, window, "database-selector");
        settle(cx, window, &entity);
        runtime
            .handle()
            .block_on(admin.query(
                &format!("DROP DATABASE {}", quote_ident(Engine::Postgres, &gone)),
                0,
            ))
            .unwrap();
        cx.update_window(window, |_, w, cx| {
            entity.update(cx, |s, cx| s.choose_namespace(gone.clone(), false, w, cx))
        })
        .unwrap();
        settle(cx, window, &entity);
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.error.is_some());
            assert_eq!(s.session_generation, generation);
            assert_eq!(s.namespace_scope(), Some("analysis"));
            assert_eq!(s.profile.as_ref().unwrap().database, target);
        });
        assert_eq!(
            runtime
                .handle()
                .block_on(new.query("SELECT id FROM analysis.witness", 1))
                .unwrap()[0]
                .rows[0][0]
                .display(),
            "7"
        );

        // A server transaction started after the picker opened is rechecked.
        click(cx, window, "database-selector");
        settle(cx, window, &entity);
        runtime.handle().block_on(new.query("BEGIN", 0)).unwrap();
        cx.update_window(window, |_, w, cx| {
            entity.update(cx, |s, cx| {
                s.choose_namespace("rowboat".into(), false, w, cx)
            })
        })
        .unwrap();
        settle(cx, window, &entity);
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.transaction && s.transaction_known);
            assert_eq!(s.session_generation, generation);
        });
        runtime.handle().block_on(new.rollback()).unwrap();
        cx.update(|cx| entity.update(cx, |s, _| s.transaction = false));

        // Rehosting while a candidate connects must not install its late reply.
        click(cx, window, "database-selector");
        settle(cx, window, &entity);
        cx.update_window(window, |_, w, cx| {
            entity.update(cx, |s, cx| {
                s.choose_namespace("rowboat".into(), false, w, cx);
                s.host_epoch = s.host_epoch.wrapping_add(1);
            })
        })
        .unwrap();
        settle(cx, window, &entity);
        cx.read(|cx| {
            let s = entity.read(cx);
            assert_eq!(s.session_generation, generation);
            assert_eq!(s.profile.as_ref().unwrap().database, target);
        });

        // The other database can also open independently through the shell.
        click(cx, window, "database-selector");
        settle(cx, window, &entity);
        cx.update_window(window, |_, w, cx| {
            entity.update(cx, |s, cx| {
                s.choose_namespace("rowboat".into(), true, w, cx)
            })
        })
        .unwrap();
        let workspaces = cx
            .update_window(window, |_, w, cx| {
                shell.update(cx, |s, cx| s.test_tick(w, cx))
            })
            .unwrap();
        assert_eq!(workspaces.len(), 2);
        let other = workspaces.iter().find(|e| **e != entity).unwrap().clone();
        settle(cx, window, &other);
        cx.read(|cx| {
            assert_eq!(other.read(cx).profile.as_ref().unwrap().database, "rowboat");
            assert_eq!(entity.read(cx).profile.as_ref().unwrap().database, target);
            assert_ne!(other.read(cx).owner, entity.read(cx).owner);
            assert!(
                !serde_json::to_string(&other.read(cx).shared.borrow().state)
                    .unwrap()
                    .contains("rowboat-test")
            );
            assert_eq!(entity.read(cx).query_run_invocations, 0);
        });
        cx.update_window(window, |_, w, _| w.remove_window())
            .unwrap();
    }));
    runtime.handle().block_on(async {
        for name in [&target, &gone] {
            let _ = admin
                .query(
                    &format!(
                        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                        quote_ident(Engine::Postgres, name)
                    ),
                    0,
                )
                .await;
        }
    });
    if let Err(error) = run {
        std::panic::resume_unwind(error);
    }
}
