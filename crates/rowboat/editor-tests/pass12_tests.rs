//! Real embedded databases and actual retained GPUI workbenches; no mock provider.
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{ConnectionProfile, Engine, SqliteFileOptions, SqliteOpenMode, TableRef};
use std::time::Duration;

fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pass12_gui_")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}
fn profile(path: &std::path::Path, mode: SqliteOpenMode) -> ConnectionProfile {
    ConnectionProfile {
        name: "Pass12 SQLite".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.to_owned(),
            mode,
        }),
        ..Default::default()
    }
}
fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<crate::app::Workbench>) {
    for _ in 0..1600 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let ready = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.busy
                })
            })
            .unwrap();
        if ready {
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("SQLite GUI operation timed out");
}
fn run(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<crate::app::Workbench>, sql: &str) {
    run_with_intent(cx, w, s, sql, false);
}
fn run_for_editing(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
    sql: &str,
) {
    run_with_intent(cx, w, s, sql, true);
}
fn run_with_intent(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
    sql: &str,
    for_editing: bool,
) {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.page = crate::app::Page::Query;
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value(sql.to_owned(), w, cx);
                e.focus(w, cx);
            });
            if for_editing {
                s.run_query_for_editing(cx);
            } else {
                s.run_query(cx);
            }
        })
    })
    .unwrap();
    settle(cx, w, s);
}
fn connect(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
    p: ConnectionProfile,
) {
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, cx| {
            s.owner = Some(p.id.clone());
            s.tabs[0].connection_id = Some(p.id.clone());
            s.session_ready = true;
            s.connect_profile(p, String::new(), cx);
        })
    })
    .unwrap();
    settle(cx, w, s);
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

fn completion(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
) -> Vec<String> {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.page = crate::app::Page::Query;
            s.dismiss_completions(cx);
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT c. FROM items c", w, cx);
                e.set_selected_range(9..9, cx);
                e.focus(w, cx);
            });
        })
    })
    .unwrap();
    cx.simulate_input(w, "n");
    for _ in 0..1000 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let items = cx.read(|cx| {
            let s = s.read(cx);
            let state = s.tabs[0].editor.read(cx).completion_menu_state();
            if !state.open
                || !s
                    .advanced
                    .completion_menus
                    .get(&s.tabs[0].id)
                    .is_some_and(|menu| s.inline_offer_current(&menu.borrow()))
            {
                return Vec::new();
            }
            state
                .items
                .iter()
                .map(|i| i.label.clone())
                .collect::<Vec<_>>()
        });
        if !items.is_empty() {
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            return items;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("SQLite completion did not arrive");
}

#[gpui_kit::test]
fn pass12_sqlite_main_temp_attached_completion_and_enter_intent(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 124, runtime.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("catalog.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    let attached = rowboat_core::sql::literal(
        Engine::Sqlite,
        &rowboat_core::Cell::Text(dir.path().join("attached.sqlite").to_str().unwrap().into()),
    );
    run(
        cx,
        w,
        &s,
        &format!(
            "CREATE TABLE main.items(id INTEGER PRIMARY KEY,name TEXT); CREATE TEMP TABLE items(id INTEGER PRIMARY KEY,nickname TEXT); ATTACH DATABASE {attached} AS aux; CREATE TABLE aux.items(id INTEGER PRIMARY KEY,notes TEXT)"
        ),
    );
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        let snapshot = s.namespace.snapshot.as_ref().unwrap();
        for n in ["main", "temp", "aux"] {
            assert!(snapshot.namespaces.iter().any(|s| s == n));
        }
    });
    cx.update(|cx| s.update(cx, |s, cx| s.load_namespace(Some("aux".into()), false, cx)));
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(s.namespace_scope(), Some("aux"));
        assert!(s.catalog.iter().all(|t| t.table.schema == "aux"));
    });
    let items = completion(cx, w, &s);
    assert!(items.iter().any(|s| s == "nickname"), "{items:?}");
    assert!(
        !items.iter().any(|s| s == "name" || s == "notes"),
        "Explorer scope altered SQL lookup: {items:?}"
    );
    cx.simulate_keystrokes(w, "enter");
    cx.read(|cx| {
        assert!(
            s.read(cx).tabs[0].editor.read(cx).value().contains("c.n\n"),
            "Passive Enter accepted completion"
        )
    });
    completion(cx, w, &s);
    cx.simulate_keystrokes(w, "down enter");
    cx.read(|cx| {
        assert!(
            s.read(cx).tabs[0]
                .editor
                .read(cx)
                .value()
                .contains("nickname")
        )
    });
    run(cx, w, &s, "DROP TABLE temp.items");
    let items = completion(cx, w, &s);
    assert!(items.iter().any(|s| s == "name"), "{items:?}");
    assert!(
        !items.iter().any(|s| s == "nickname"),
        "Stale temp shadow survived terminal verification"
    );
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass12_sqlite_file_form_existing_create_readonly_and_no_credentials(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let path = dir.path().join("form.sqlite");
    let (w, s) = crate::workbench_tests::workspace(cx, 121, runtime.handle());
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.loaded = true;
            s.edit_connection(None, w, cx);
            let f = s.form.as_mut().unwrap();
            f.fields["name"].update(cx, |e, cx| e.set_value("File", w, cx));
        })
    })
    .unwrap();
    click(cx, w, "engine-sqlite");
    click(cx, w, "sqlite-choose-file");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && !options.directories && !options.multiple);
        Some(vec![path.clone()])
    });
    cx.run_until_parked();
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            let f = s.form.as_mut().unwrap();
            assert_eq!(
                f.fields["file_path"].read(cx).value().as_ref(),
                path.to_str().unwrap()
            );
            // Empty network fields must not be validated for a file connection.
            for key in ["host", "port", "username", "ssl_mode"] {
                f.fields[key].update(cx, |e, cx| e.set_value("", w, cx));
            }
            // A retained network default must never become a file target label.
            f.fields["database"].update(cx, |e, cx| e.set_value("postgres", w, cx));
            assert_eq!(f.file_mode, SqliteOpenMode::OpenExisting);
            assert!(!f.keyring_loading);
            let p = f.profile(cx).unwrap();
            s.owner = Some(p.id.clone());
            s.tabs[0].connection_id = Some(p.id.clone());
            s.connect_profile(p, String::new(), cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_some());
        let p = s.profile.as_ref().unwrap();
        assert_eq!(p.database, "postgres");
        let target = crate::profiles_ui::connection_target(p);
        assert_eq!(target.compact, "form.sqlite");
        assert_eq!(target.file_path.as_deref(), path.to_str());
    });
    assert!(!path.exists(), "Default open silently created a file");
    click(cx, w, "sqlite-create");
    click(cx, w, "sqlite-choose-file");
    cx.simulate_new_path_selection(|_| Some(path.clone()));
    cx.run_until_parked();
    assert!(
        !path.exists(),
        "File picker created the database before Connect"
    );
    let p = cx.read(|cx| s.read(cx).form.as_ref().unwrap().profile(cx).unwrap());
    let (shell_tx, commands) = std::sync::mpsc::channel();
    cx.update(|cx| s.update(cx, |s, _| s.shell_tx = shell_tx));
    click(cx, w, "connect");
    let save = commands
        .try_iter()
        .find_map(|command| match command {
            crate::workspace_shell::Command::SaveProfile { save, .. } => Some(*save),
            _ => None,
        })
        .expect("File form did not submit SaveProfile");
    assert!(save.password.is_empty() && !save.remember && !save.exists);
    assert_eq!(save.profile.file.as_ref().unwrap().path, path);
    cx.update(|cx| s.update(cx, |s, cx| s.commit_profile_save(save, cx)));
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.database.is_some());
        let p = s.profile.as_ref().unwrap();
        let target = crate::profiles_ui::connection_target(p);
        assert_eq!(target.compact, "form.sqlite");
        assert_eq!(target.file_path.as_deref(), path.to_str());
        let item = s.palette_items(cx).into_iter().find(|item| {
            matches!(&item.target, crate::palette::Target::Connection(id) if id == &p.id)
        }).expect("Saved file profile missing from command palette");
        assert_eq!(item.detail, "Connection · Sqlite · form.sqlite");

        // Network presentation still uses the database, even with stray file options.
        for engine in [Engine::Postgres, Engine::MySql] {
            let mut network = p.clone();
            network.engine = engine;
            network.database = "postgres".into();
            let target = crate::profiles_ui::connection_target(&network);
            assert_eq!(target.compact, "postgres");
            assert!(target.file_path.is_none());
        }
        let mut missing_file = p.clone();
        missing_file.file = None;
        assert_eq!(
            crate::profiles_ui::connection_target(&missing_file).compact,
            "No database file"
        );
    });
    assert!(path.exists());
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'saved')",
    );
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            let mut p = p.clone();
            p.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
            p.read_only = true;
            s.state.connections.push(p.clone());
            s.edit_connection(Some(p), w, cx);
            assert!(
                !s.form.as_ref().unwrap().keyring_loading,
                "File profile requested keyring"
            );
            let id = s.form.as_ref().unwrap().id.clone();
            s.receive_credential(id, Ok(Some("late-network-secret".into())), w, cx);
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
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    assert!(visual.debug_bounds("file_path").is_some());
    assert!(visual.debug_bounds("connection-password").is_none());
    assert!(visual.debug_bounds("host").is_none());
    let p = cx.read(|cx| s.read(cx).form.as_ref().unwrap().profile(cx).unwrap());
    connect(cx, w, &s, p);
    run(cx, w, &s, "SELECT id,name FROM items");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(s.tabs[0].results[0].rows[0][1].display(), "saved");
        assert!(!s.grid_writable());
    });
    run(cx, w, &s, "INSERT INTO items VALUES(2,'blocked')");
    cx.read(|cx| assert!(s.read(cx).error.is_some()));
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass12_sqlite_query_edit_review_apply_provenance_and_browse(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 122, runtime.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("edit.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT,secret TEXT); INSERT INTO items VALUES(1,'alpha','a'),(2,'zulu','b')",
    );
    let (shell_tx, commands) = std::sync::mpsc::channel();
    cx.update(|cx| s.update(cx, |s, _| s.shell_tx = shell_tx));
    run_for_editing(
        cx,
        w,
        &s,
        "SELECT id AS key_alias,name AS label FROM items ORDER BY id",
    );
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            assert!(s.error.is_none(), "{:?}", s.error);
            assert!(s.query_plan().is_some(), "{}", s.query_edit_notice());
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("DELETE FROM items", w, cx));
            s.select_grid_cell(1, 1, false, cx);
            s.begin_cell_edit(w, cx);
            assert!(s.grid.draft.is_some());
        })
    })
    .unwrap();
    cx.simulate_keystrokes(w, "ctrl-a");
    cx.simulate_input(w, "edited");
    cx.simulate_keystrokes(w, "ctrl-enter");
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    click(cx, w, "review-query-pending");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.query_edit.review);
        let sql = s.query_plan().unwrap().preview_changes(&s.changes).unwrap();
        assert!(
            sql.contains("UPDATE")
                && sql.contains(&rowboat_core::sql::literal(
                    Engine::Sqlite,
                    &rowboat_core::Cell::Text("edited".into())
                ))
                && sql.contains(&rowboat_core::sql::literal(
                    Engine::Sqlite,
                    &rowboat_core::Cell::Text("zulu".into())
                )),
            "{sql}"
        );
        assert!(!sql.contains("key_alias"));
    });
    click(cx, w, "apply-changes");
    let ack = commands
        .try_iter()
        .find_map(|c| match c {
            crate::workspace_shell::Command::PersistApply { ack, .. } => Some(ack),
            _ => None,
        })
        .expect("Missing durable apply intent");
    assert_eq!(
        runtime
            .handle()
            .block_on(db.query("SELECT name FROM items WHERE id=2", 1))
            .unwrap()[0]
            .rows[0][0]
            .display(),
        "zulu"
    );
    ack.send(Ok(())).unwrap();
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.query_plan().is_none());
        assert!(s.query_edit_notice().contains("stale"));
        assert!(s.changes.is_empty());
        assert_eq!(s.shared.borrow().state.history.len(), 2);
    });
    let rows = runtime
        .handle()
        .block_on(db.query("SELECT name,secret FROM items ORDER BY id", 10))
        .unwrap();
    assert_eq!(rows[0].rows[0][0].display(), "alpha");
    assert_eq!(rows[0].rows[1][0].display(), "edited");
    assert_eq!(rows[0].rows[1][1].display(), "b");
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.open_table(
                TableRef {
                    schema: "main".into(),
                    name: "items".into(),
                },
                cx,
            )
        })
    });
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(s.table_result.rows.len(), 2);
        assert!(s.structure.columns.iter().any(|c| c.primary_key));
        assert!(s.grid_writable());
    });
    run_for_editing(cx, w, &s, "SELECT name FROM items");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.query_plan().is_none());
        assert!(!s.grid_writable());
        assert!(!s.query_edit_notice().is_empty());
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass17_plain_run_is_lazy_and_explicit_editing_refreshes_once_without_sticking(
    cx: &mut TestAppContext,
) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 170, rt.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("lazy.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'old')",
    );
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT id,name FROM items", w, cx);
                e.focus(w, cx);
            });
        })
    })
    .unwrap();
    cx.simulate_keystrokes(w, "ctrl-enter");
    settle(cx, w, &s);
    let db = cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.query_plan().is_none());
        assert!(!s.query_entry().unwrap().capture_requested);
        assert_eq!(s.query_edit_notice(), "Read-only · Editing not requested");
        assert_eq!(
            s.tabs[0]
                .query_progress
                .as_ref()
                .unwrap()
                .timing
                .as_ref()
                .unwrap()
                .core
                .unwrap()
                .source_capture_ms,
            0
        );
        assert_eq!(s.shared.borrow().state.history.len(), 2);
        s.database.clone().unwrap()
    });
    // A double-click/edit request must not fetch metadata or replay SQL.
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.select_grid_cell(0, 1, false, cx);
            s.begin_cell_edit(w, cx);
            assert!(!s.has_row_draft());
            assert!(!s.busy);
            assert_eq!(s.query_run_invocations, 2);
        })
    })
    .unwrap();
    rt.handle()
        .block_on(db.query("UPDATE items SET name='fresh'", 0))
        .unwrap();
    click(cx, w, "run-for-editing");
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(
            s.query_plan().is_some(),
            "{:?} {}",
            s.error,
            s.query_edit_notice()
        );
        assert_eq!(s.tabs[0].results[0].rows[0][1].display(), "fresh");
        assert_eq!(s.shared.borrow().state.history.len(), 3);
        assert_eq!(
            s.tabs[0].editor.read(cx).value().as_str(),
            "SELECT id,name FROM items"
        );
    });
    cx.update(|cx| s.update(cx, |s, cx| s.run_query(cx)));
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.query_plan().is_none());
        assert!(!s.query_entry().unwrap().capture_requested);
        assert_eq!(s.shared.borrow().state.history.len(), 4);
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass17_editing_action_never_replays_dml_or_unsupported_batches(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 171, rt.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("dml.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'original')",
    );
    run(cx, w, &s, "UPDATE items SET name=name || '!'");
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    for sql in [
        "UPDATE items SET name=name || '!'",
        "DELETE FROM items",
        "SELECT id,name FROM items; DELETE FROM items",
        "WITH gone AS (SELECT * FROM items) SELECT * FROM gone",
    ] {
        run_for_editing(cx, w, &s, sql);
        cx.read(|cx| {
            let s = s.read(cx);
            assert!(
                s.error.is_some(),
                "Unsupported editing run must fail: {sql}"
            );
            assert!(s.query_plan().is_none());
            assert_eq!(s.tabs[0].editor.read(cx).value().as_str(), sql);
        });
        let rows = rt
            .handle()
            .block_on(db.query("SELECT id,name FROM items", 10))
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
        assert_eq!(rows[0].rows[0][1].display(), "original!");
    }
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass17_editing_uses_current_selection_and_revalidates_workspace_guards(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 172, rt.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("selection.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'keep'),(2,'other'),(3,'third')",
    );
    let select = "SELECT id,name FROM items WHERE id=1";
    let buffer = format!("{select}; DELETE FROM items");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value(buffer.clone(), w, cx);
                e.set_selected_range(0..select.len(), cx);
            });
            s.choose_menu(crate::chrome_ui::Command::RunForEditing, w, cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            assert!(s.query_plan().is_some(), "{:?}", s.error);
            assert_eq!(s.tabs[0].results[0].rows.len(), 1);
            let runs = s.query_run_invocations;
            let assert_blocked =
                |s: &mut crate::app::Workbench,
                 cx: &mut gpui_kit::Context<crate::app::Workbench>| {
                    assert!(!s.run_for_editing_enabled());
                    s.run_query_for_editing(cx);
                    assert_eq!(s.query_run_invocations, runs);
                    assert!(s.query_plan().is_some());
                    assert_eq!(s.tabs[0].editor.read(cx).value().as_str(), buffer);
                };
            let db = s.database.take();
            assert_blocked(s, cx);
            s.database = db;
            s.profile.as_mut().unwrap().read_only = true;
            assert_blocked(s, cx);
            s.profile.as_mut().unwrap().read_only = false;
            s.transaction = true;
            assert_blocked(s, cx);
            s.transaction = false;
            s.transaction_known = false;
            assert_blocked(s, cx);
            s.transaction_known = true;
            s.busy = true;
            assert_blocked(s, cx);
            s.busy = false;
            s.close_requested = true;
            assert_blocked(s, cx);
            s.close_requested = false;
            let owner = s.tabs[0].connection_id.replace("another-workspace".into());
            assert_blocked(s, cx);
            s.tabs[0].connection_id = owner;
            s.select_grid_cell(0, 1, false, cx);
            s.begin_cell_edit(w, cx);
            assert!(s.has_row_draft());
            assert_blocked(s, cx);
            s.grid.draft = None;
            s.query_edit.draft = None;
            s.changes.push(rowboat_core::RowChange::Update {
                original: vec![],
                values: vec![],
            });
            assert_blocked(s, cx);
            s.changes.clear();
            assert!(s.run_for_editing_enabled());
        })
    })
    .unwrap();
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    assert_eq!(
        rt.handle()
            .block_on(db.query("SELECT count(*) FROM items", 1))
            .unwrap()[0]
            .rows[0][0]
            .display(),
        "3"
    );
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass17_truncated_plain_result_reports_retained_and_read_counts(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 173, rt.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("counts.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    cx.update(|cx| s.update(cx, |s, _| s.state.preferences.query_row_limit = 2));
    run(
        cx,
        w,
        &s,
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<9) SELECT x FROM n",
    );
    cx.read(|cx| {
        let s = s.read(cx);
        let tab = &s.tabs[0];
        assert_eq!(tab.results[0].rows.len(), 2);
        assert!(tab.results[0].truncated);
        assert_eq!(
            crate::query_stream_ui::result_row_count(
                tab.query_progress.as_ref(),
                0,
                &tab.results[0]
            ),
            "2 retained of 9 rows read"
        );
        assert!(
            s.status.contains("2 retained of 9 rows read"),
            "{}",
            s.status
        );
        assert!(s.menu_enabled(crate::chrome_ui::Command::StreamCsv, cx));
        assert!(!s.query_entry().unwrap().capture_requested);
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass17_gui_budget_above_64_mib_and_editing_clamp_keep_exact_proof(cx: &mut TestAppContext) {
    const MIB: usize = 1024 * 1024;
    const PAYLOAD_BYTES: usize = 512 * 1024;
    // Capture before launch: launching replaces this tab's results and refreshes
    // its origin metadata. Other retained results/pins still consume the shared
    // workspace budget; half the remainder is reserved as allocation headroom.
    let ordinary_allowance = |s: &crate::app::Workbench| {
        let occupied = s.results.retained_bytes()
            + s.active_results_bytes(Some(s.tabs[s.active_tab].id), false);
        (s.state.preferences.query_result_limit_bytes().unwrap() - occupied) / 2
    };
    let payload_bytes = |result: &rowboat_core::QueryResult| {
        result
            .rows
            .iter()
            .map(|row| match &row[1] {
                rowboat_core::Cell::Binary(value) => {
                    assert_eq!(value.len(), PAYLOAD_BYTES);
                    value.len()
                }
                _ => panic!("Retained payload lost its binary type"),
            })
            .sum::<usize>()
    };
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 174, rt.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("budget.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    rt.handle().block_on(db.query("CREATE TABLE payloads(id INTEGER PRIMARY KEY,payload BLOB); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<140) INSERT INTO payloads SELECT x,zeroblob(524288) FROM n", 0)).unwrap();
    let default_allowance = cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(
            s.state.preferences.query_result_limit_bytes().unwrap(),
            256 * MIB
        );
        let allowance = ordinary_allowance(s);
        assert!(allowance > 64 * MIB && allowance <= 128 * MIB);
        allowance
    });
    run(cx, w, &s, "SELECT id,payload FROM payloads ORDER BY id");
    cx.read(|cx| {
        let s = s.read(cx);
        let tab = &s.tabs[0];
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(
            tab.query_progress.as_ref().unwrap().byte_limit(),
            Some(default_allowance)
        );
        assert_eq!(
            tab.results[0].rows.len(),
            140,
            "Ordinary GUI must retain more than 64 MiB"
        );
        assert!(!tab.results[0].truncated);
        assert_eq!(payload_bytes(&tab.results[0]), 70 * MIB);
        assert!(payload_bytes(&tab.results[0]) <= default_allowance);
        assert!(s.query_plan().is_none());
    });
    run_for_editing(cx, w, &s, "SELECT id,payload FROM payloads ORDER BY id");
    cx.read(|cx| {
        let s = s.read(cx);
        let tab = &s.tabs[0];
        assert!(
            s.query_plan().is_some(),
            "{:?} {}",
            s.error,
            s.query_edit_notice()
        );
        assert_eq!(
            tab.query_progress.as_ref().unwrap().byte_limit(),
            Some(64 * 1024 * 1024)
        );
        assert_eq!(tab.results[0].rows.len(), 127);
        assert!(tab.results[0].truncated);
        let proof = s.query_plan().unwrap().result();
        assert!(
            proof.rows == tab.results[0].rows,
            "Editing proof must preserve every retained original cell exactly"
        );
        assert_eq!(payload_bytes(proof), 127 * PAYLOAD_BYTES);
        assert!(
            s.query_edit_notice()
                .contains("64 MiB editing snapshot limit")
        );
        let (_, notice) = crate::query_stream_ui::result_presentation(
            tab.query_progress.as_ref(),
            0,
            &tab.results[0],
            false,
        );
        assert!(
            notice.contains("64 MiB per-query retention limit"),
            "{notice}"
        );
    });
    let low_allowance = cx.update(|cx| {
        s.update(cx, |s, _| {
            s.state.preferences.query_result_limit_mib = 16;
            let allowance = ordinary_allowance(s);
            // Existing provenance/default table storage makes the actual limit just
            // under 8 MiB. Fifteen complete 512 KiB payloads fit; sixteen do not.
            assert!(allowance > 15 * PAYLOAD_BYTES && allowance < 16 * PAYLOAD_BYTES);
            allowance
        })
    });
    run(cx, w, &s, "SELECT id,payload FROM payloads ORDER BY id");
    cx.read(|cx| {
        let s = s.read(cx);
        let tab = &s.tabs[0];
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(tab.error.is_none(), "{:?}", tab.error);
        assert_eq!(
            tab.query_progress.as_ref().unwrap().byte_limit(),
            Some(low_allowance)
        );
        assert_eq!(tab.results[0].rows.len(), 15);
        assert_eq!(payload_bytes(&tab.results[0]), 15 * PAYLOAD_BYTES);
        assert!(payload_bytes(&tab.results[0]) <= low_allowance);
        assert!(s.results.retained_bytes() + s.active_results_bytes(None, false) <= 16 * MIB);
        assert!(tab.results[0].truncated);
        assert!(s.query_plan().is_none());
        assert_eq!(
            crate::query_stream_ui::result_row_count(
                tab.query_progress.as_ref(),
                0,
                &tab.results[0]
            ),
            "15 retained of 140 rows read"
        );
        let (_, notice) = crate::query_stream_ui::result_presentation(
            tab.query_progress.as_ref(),
            0,
            &tab.results[0],
            false,
        );
        assert!(
            notice.contains(&format!("{low_allowance} bytes per-query retention limit")),
            "{notice}"
        );
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass12_sqlite_unsupported_tools_and_cancellation(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 123, runtime.handle());
    connect(
        cx,
        w,
        &s,
        profile(
            &dir.path().join("tools.sqlite"),
            SqliteOpenMode::CreateIfMissing,
        ),
    );
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            for operation in [
                rowboat_admin::providers::Operation::AccessManagement,
                rowboat_admin::providers::Operation::ServerMetrics,
                rowboat_admin::providers::Operation::SchemaComparison,
                rowboat_admin::providers::Operation::Analyze,
            ] {
                assert!(!s.admin_supported(operation));
            }
            for command in [
                crate::chrome_ui::Command::Dashboard,
                crate::chrome_ui::Command::Objects,
            ] {
                assert!(!s.menu_enabled(command, cx));
            }
            s.show_access_matrix(w, cx);
            assert!(s.access_matrix.is_none());
            assert!(!s.busy);
            s.show_designer(w, cx);
            assert!(s.advanced.designer.is_some());
            s.advanced.designer = None;
            s.show_transfer(crate::services_ui::TransferKind::Backup, w, cx);
            assert!(s.transfer.is_some());
            s.transfer = None;
            s.sample_metrics(cx);
            assert!(!s.busy);
            assert!(s.admin_supported(rowboat_admin::providers::Operation::Explain));
            assert!(s.admin_supported(rowboat_admin::providers::Operation::SchemaDdl));
            s.show_management(crate::management::Operation::CreateDatabase, None, w, cx);
            assert!(s.management.is_none());
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("SELECT 'unchanged'", w, cx));
            s.format_query(w, cx);
            assert!(s.tabs[0].editor.read(cx).value().contains("'unchanged'"));
            assert_eq!(s.status, "SQL formatted");
            assert!(s.shared.borrow().state.history.is_empty());
            s.page = crate::app::Page::Tools;
        })
    })
    .unwrap();
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    cx.update_window(w,|_,w,cx|s.update(cx,|s,cx|{
        s.page=crate::app::Page::Query;
        s.tabs[0].editor.update(cx,|e,cx|e.set_value("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n",w,cx));s.run_query(cx);
    })).unwrap();
    std::thread::sleep(Duration::from_millis(40));
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| s.cancel(cx));
        w.draw(cx).clear(cx);
    })
    .unwrap();
    settle(cx, w, &s);
    run(cx, w, &s, "SELECT 42 AS reusable");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(s.tabs[0].results[0].rows[0][0].display(), "42");
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass12_sqlite_table_edit_explicit_apply_and_external_conflict(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let p = profile(
        &dir.path().join("table.sqlite"),
        SqliteOpenMode::CreateIfMissing,
    );
    let (w, s) = crate::workbench_tests::workspace(cx, 125, runtime.handle());
    connect(cx, w, &s, p.clone());
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'original'),(2,'untouched')",
    );
    let (shell_tx, commands) = std::sync::mpsc::channel();
    cx.update(|cx| s.update(cx, |s, _| s.shell_tx = shell_tx));
    let other = runtime
        .handle()
        .block_on(rowboat_core::Database::connect(&p, ""))
        .unwrap();
    for conflict in [false, true] {
        cx.update(|cx| {
            s.update(cx, |s, cx| {
                s.open_table(
                    TableRef {
                        schema: "main".into(),
                        name: "items".into(),
                    },
                    cx,
                )
            })
        });
        settle(cx, w, &s);
        cx.update_window(w, |_, w, cx| {
            s.update(cx, |s, cx| {
                s.select_grid_cell(0, 1, false, cx);
                s.begin_cell_edit(w, cx);
                assert!(s.grid.draft.is_some(), "{:?}", s.error);
            })
        })
        .unwrap();
        cx.simulate_keystrokes(w, "ctrl-a");
        cx.simulate_input(
            w,
            if conflict {
                "must not overwrite"
            } else {
                "table edited"
            },
        );
        cx.simulate_keystrokes(w, "ctrl-enter");
        cx.update(|cx| {
            s.update(cx, |s, cx| {
                s.review_pending(cx);
                assert!(s.table_page == crate::app::TablePage::Changes);
                s.review_details.insert(0);
            })
        });
        if conflict {
            runtime
                .handle()
                .block_on(other.query("UPDATE items SET name='external' WHERE id=1", 0))
                .unwrap();
        }
        click(cx, w, "apply-changes");
        let ack = commands
            .try_iter()
            .find_map(|c| match c {
                crate::workspace_shell::Command::PersistApply { ack, .. } => Some(ack),
                _ => None,
            })
            .expect("Missing durable intent");
        ack.send(Ok(())).unwrap();
        settle(cx, w, &s);
        let rows = runtime
            .handle()
            .block_on(other.query("SELECT name FROM items ORDER BY id", 10))
            .unwrap();
        assert_eq!(
            rows[0].rows[0][0].display(),
            if conflict { "external" } else { "table edited" }
        );
        assert_eq!(rows[0].rows[1][0].display(), "untouched");
        cx.read(|cx| {
            let s = s.read(cx);
            if conflict {
                assert!(s.error.is_some());
                assert!(s.changes.is_empty(), "Failed Apply must not be replayable");
                assert_eq!(s.recovery.detached.len(), 1);
                assert!(
                    s.recovery
                        .error
                        .as_ref()
                        .is_some_and(|e| e.contains("reconciliation"))
                );
            } else {
                assert!(s.error.is_none(), "{:?}", s.error);
                assert!(s.changes.is_empty());
            }
        });
    }
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass12_sqlite_file_picker_cancel_and_obsolete_reply(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let path = dir.path().join("must-not-create.sqlite");
    let (w, s) = crate::workbench_tests::workspace(cx, 126, runtime.handle());
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.edit_connection(Some(profile(&path, SqliteOpenMode::OpenExisting)), w, cx);
            s.choose_database_file(w, cx);
        })
    })
    .unwrap();
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    cx.read(|cx| assert!(s.read(cx).form.as_ref().unwrap().file_request.is_none()));
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.choose_database_file(w, cx);
            s.form
                .as_mut()
                .unwrap()
                .select_engine(Engine::Postgres, w, cx);
        })
    })
    .unwrap();
    let obsolete = dir.path().join("obsolete.sqlite");
    cx.simulate_path_prompt_response(|_| Some(vec![obsolete.clone()]));
    cx.run_until_parked();
    cx.read(|cx| {
        let s = s.read(cx);
        let f = s.form.as_ref().unwrap();
        assert_eq!(f.engine, Engine::Postgres);
        assert_eq!(
            f.fields["file_path"].read(cx).value().as_ref(),
            path.to_str().unwrap()
        );
        assert!(f.file_request.is_none());
        assert!(s.database.is_none());
    });
    assert!(!path.exists() && !obsolete.exists());
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}
