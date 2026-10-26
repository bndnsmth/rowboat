//! Pass14 exercises the real GPUI dialogs, service workers and disposable SQLite files.
use crate::{app::Workbench, services_ui::TransferKind};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{
    ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode, TableRef,
};
use std::{path::Path, time::Duration};

pub(crate) fn workspace(
    cx: &mut TestAppContext,
    runtime: tokio::runtime::Handle,
    profiles: Vec<ConnectionProfile>,
) -> (
    AnyWindowHandle,
    Entity<crate::workspace_shell::WorkspaceShell>,
    Vec<Entity<Workbench>>,
) {
    workspace_with_preferences(cx, runtime, profiles, Default::default())
}

pub(crate) fn workspace_with_preferences(
    cx: &mut TestAppContext,
    runtime: tokio::runtime::Handle,
    profiles: Vec<ConnectionProfile>,
    preferences: rowboat_services::Preferences,
) -> (
    AnyWindowHandle,
    Entity<crate::workspace_shell::WorkspaceShell>,
    Vec<Entity<Workbench>>,
) {
    use crate::{
        session::{SessionState, TabBuffer},
        workspace_storage::{Event, Write, Writers},
    };
    let (state, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
    let (recovery, mut recovery_rx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, events) = std::sync::mpsc::channel();
    tx.send(Event::StateLoaded(Ok(Box::new(
        rowboat_services::AppState {
            connections: profiles.clone(),
            preferences,
            ..Default::default()
        },
    ))))
    .unwrap();
    tx.send(Event::RecoveryLoaded(Ok(SessionState {
        version: 4,
        active_tab: 1,
        tabs: profiles
            .iter()
            .enumerate()
            .map(|(i, p)| TabBuffer {
                id: i + 1,
                connection_id: Some(p.id.clone()),
                title: p.name.clone(),
                sql: "SELECT 1".into(),
            })
            .collect(),
        ..Default::default()
    })))
    .unwrap();
    let state_tx = tx.clone();
    runtime.spawn(async move {
        while let Some(write) = state_rx.recv().await {
            match write {
                Write::Save(revision, _) => {
                    let _ = state_tx.send(Event::StateSaved(revision, Ok(())));
                }
                Write::Shutdown => {
                    let _ = state_tx.send(Event::WriterStopped);
                    break;
                }
            }
        }
    });
    runtime.spawn(async move {
        while let Some(write) = recovery_rx.recv().await {
            match write {
                Write::Save(revision, _) => {
                    let _ = tx.send(Event::RecoverySaved(revision, Ok(())));
                }
                Write::Shutdown => {
                    let _ = tx.send(Event::WriterStopped);
                    break;
                }
            }
        }
    });
    let (w, shell) = cx.update(|cx| {
        crate::test_theme::init(cx);
        crate::keyboard_ui::bind(cx);
        gpui_kit::open_window(Default::default(), cx, |w, cx| {
            cx.new(|cx| {
                crate::workspace_shell::WorkspaceShell::with_writers(
                    runtime,
                    Default::default(),
                    Writers {
                        state,
                        recovery,
                        events,
                    },
                    w,
                    cx,
                )
            })
        })
        .unwrap()
    });
    let mut workspaces = cx
        .update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
        .unwrap();
    workspaces.sort_by_key(|s| {
        cx.read(|cx| {
            profiles
                .iter()
                .position(|p| s.read(cx).owner.as_ref() == Some(&p.id))
        })
    });
    (w, shell, workspaces)
}

fn profile(path: &Path) -> ConnectionProfile {
    ConnectionProfile {
        name: "Pass14 file tools".into(),
        engine: Engine::Sqlite,
        database: String::new(),
        file: Some(SqliteFileOptions {
            path: path.to_owned(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    }
}

fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>) {
    for _ in 0..1600 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        let ready = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.busy
                })
            })
            .unwrap();
        if ready {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("Pass14 GUI worker timed out");
}

fn connect(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
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
    cx.read(|cx| assert!(s.read(cx).error.is_none(), "{:?}", s.read(cx).error));
}

fn run(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>, sql: &str) {
    run_with_intent(cx, w, s, sql, false);
}
fn run_for_editing(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>, sql: &str) {
    run_with_intent(cx, w, s, sql, true);
}
fn run_with_intent(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
    sql: &str,
    for_editing: bool,
) {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.page = crate::app::Page::Query;
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value(sql.to_owned(), w, cx));
            if for_editing {
                s.run_query_for_editing(cx);
            } else {
                s.run_query(cx);
            }
        })
    })
    .unwrap();
    settle(cx, w, s);
    cx.read(|cx| assert!(s.read(cx).error.is_none(), "{:?}", s.read(cx).error));
}

fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    cx.run_until_parked();
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}

fn show(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>, kind: TransferKind) {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| s.show_transfer(kind, w, cx))
    })
    .unwrap();
    cx.read(|cx| assert!(s.read(cx).transfer.is_some(), "{:?}", s.read(cx).error));
}

fn reviewed_file(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
    runtime: &tokio::runtime::Handle,
    path: &Path,
) {
    let file = runtime
        .block_on(crate::services_ui::review_transfer_file(
            path.to_owned(),
            true,
        ))
        .unwrap();
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            let d = s.transfer.as_mut().unwrap();
            d.file = Some(file);
            d.confirmation
                .update(cx, |e, cx| e.set_value(d.database.clone(), w, cx));
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass14_sqlite_backup_restore_binary_wal_live_handles_no_replay(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target.sqlite");
    let backup = dir.path().join("snapshot.sqlite");
    let p = profile(&path);
    let (w, _shell, workspaces) = workspace(cx, runtime.handle(), vec![p.clone()]);
    let s = workspaces[0].clone();
    connect(cx, w, &s, p.clone());
    run(
        cx,
        w,
        &s,
        "PRAGMA journal_mode=WAL; CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'snapshot'); CREATE TABLE sentinel(id INTEGER)",
    );
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    let mut ro = p.clone();
    ro.read_only = true;
    ro.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    let reader = runtime
        .handle()
        .block_on(Database::connect(&ro, ""))
        .unwrap();
    runtime
        .handle()
        .block_on(reader.query("SELECT * FROM items", 10))
        .unwrap();
    show(cx, w, &s, TransferKind::Backup);
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    assert!(visual.debug_bounds("transfer-image").is_none());
    assert!(visual.debug_bounds("transfer-target").is_some());
    cx.read(|cx| {
        let d = s.read(cx).transfer.as_ref().unwrap();
        assert!(d.file_provider);
        assert_eq!(d.database, path.to_str().unwrap());
    });
    // Real save chooser and worker; no shell client/container is involved.
    click(cx, w, "choose-transfer-file");
    cx.simulate_new_path_selection(|_| Some(backup.clone()));
    for _ in 0..20 {
        cx.run_until_parked();
        cx.update_window(w, |_, w, cx| s.update(cx, |s, cx| s.poll(w, cx)))
            .unwrap();
    }
    click(cx, w, "run-transfer");
    settle(cx, w, &s);
    assert!(
        std::fs::read(&backup)
            .unwrap()
            .starts_with(b"SQLite format 3\0")
    );
    runtime
        .handle()
        .block_on(db.query("UPDATE items SET name='changed'", 10))
        .unwrap();
    run_for_editing(cx, w, &s, "SELECT id,name FROM items");
    let invocations = cx.read(|cx| {
        assert!(s.read(cx).query_plan().is_some());
        s.read(cx).query_run_invocations
    });
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("INSERT INTO sentinel VALUES(99)", w, cx)
            });
        })
    })
    .unwrap();
    show(cx, w, &s, TransferKind::Restore);
    reviewed_file(cx, w, &s, &runtime.handle(), &backup);
    cx.read(|cx| {
        let d = s.read(cx).transfer.as_ref().unwrap();
        assert!(d.file.as_ref().unwrap().preview.contains("binary snapshot"));
        assert!(!d.database.is_empty());
    });
    click(cx, w, "run-transfer");
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.transfer.is_none());
        assert!(s.tabs[0].results.is_empty());
        assert!(s.query_edit.results.is_empty());
        assert!(s.query_plan().is_none());
        assert_eq!(s.query_run_invocations, invocations);
        assert_eq!(
            s.tabs[0].editor.read(cx).value().as_ref(),
            "INSERT INTO sentinel VALUES(99)"
        );
    });
    for handle in [&db, &reader] {
        let rows = runtime
            .handle()
            .block_on(handle.query("SELECT name FROM items", 10))
            .unwrap();
        assert_eq!(rows[0].rows[0][0].display(), "snapshot");
        let rows = runtime
            .handle()
            .block_on(handle.query("SELECT count(*) FROM sentinel", 1))
            .unwrap();
        assert_eq!(rows[0].rows[0][0].display(), "0");
    }
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass14_sqlite_restore_exact_path_guards_and_cancel(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guard.sqlite");
    let p = profile(&path);
    let (w, _shell, workspaces) = workspace(cx, runtime.handle(), vec![p.clone()]);
    let s = workspaces[0].clone();
    connect(cx, w, &s, p);
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'keep')",
    );
    show(cx, w, &s, TransferKind::Restore);
    // Empty legacy profile.database is never sufficient confirmation.
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.start_transfer(cx);
            assert!(!s.busy);
            assert!(s.error.as_ref().unwrap().contains("confirmation"));
            s.error = None;
        })
    });
    click(cx, w, "choose-transfer-file");
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    settle(cx, w, &s);
    cx.read(|cx| assert!(s.read(cx).transfer.as_ref().unwrap().file.is_none()));
    click(cx, w, "close-transfer");
    run(cx, w, &s, "BEGIN; SELECT * FROM items");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.show_transfer(TransferKind::Restore, w, cx);
            assert!(s.transfer.is_none());
            assert!(s.error.as_ref().unwrap().contains("transaction"));
        })
    })
    .unwrap();
    run(cx, w, &s, "ROLLBACK");
    show(cx, w, &s, TransferKind::Restore);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            let d = s.transfer.as_ref().unwrap();
            d.confirmation
                .update(cx, |e, cx| e.set_value("guard.sqlite", w, cx));
            s.start_transfer(cx);
            assert!(!s.busy);
            assert!(s.error.as_ref().unwrap().contains("confirmation"));
            s.transfer = None;
            s.error = None;
            s.profile.as_mut().unwrap().read_only = true;
            s.show_transfer(TransferKind::Restore, w, cx);
            assert!(s.transfer.is_none());
            assert!(s.error.as_ref().unwrap().contains("read-only"));
        })
    })
    .unwrap();
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass14_sqlite_stream_preview_requires_explicit_apply(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("rows.json");
    std::fs::write(
        &input,
        r#"[{"id":1,"name":"first"},{"id":2,"name":"second"}]"#,
    )
    .unwrap();
    let p = profile(&dir.path().join("import.sqlite"));
    let (w, _shell, workspaces) = workspace(cx, runtime.handle(), vec![p.clone()]);
    let s = workspaces[0].clone();
    connect(cx, w, &s, p);
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT)",
    );
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
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
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.show_data_transfer(true, rowboat_services::TransferFormat::JsonArray, cx)
        })
    });
    click(cx, w, "choose-stream-file");
    cx.simulate_path_prompt_response(|_| Some(vec![input.clone()]));
    cx.run_until_parked();
    settle(cx, w, &s);
    click(cx, w, "start-stream-transfer");
    settle(cx, w, &s);
    let count = || {
        runtime
            .handle()
            .block_on(db.query("SELECT count(*) FROM items", 1))
            .unwrap()[0]
            .rows[0][0]
            .display()
    };
    assert_eq!(count(), "0", "Preview must not import");
    click(cx, w, "apply-stream-import");
    settle(cx, w, &s);
    assert_eq!(count(), "2");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(!s.transaction);
        assert!(s.transaction_known);
        assert!(!s.transfer_running);
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass14_sqlite_formatter_availability_undo_and_no_execution(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = profile(&dir.path().join("format.sqlite"));
    let (w, _shell, workspaces) = workspace(cx, runtime.handle(), vec![p.clone()]);
    let s = workspaces[0].clone();
    connect(cx, w, &s, p);
    let original = "select '[literal;--]' as [MiX], X'00ff' as [blob], 7/2 as [half]";
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            assert!(s.service_supported(rowboat_services::ServiceOperation::Backup));
            assert!(s.service_supported(rowboat_services::ServiceOperation::Restore));
            assert!(s.menu_enabled(crate::chrome_ui::Command::Format, cx));
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value(original, w, cx);
                e.focus(w, cx);
            });
            s.format_query(w, cx);
            assert_eq!(s.status, "SQL formatted");
            assert_eq!(s.query_run_invocations, 0);
            assert!(s.shared.borrow().state.history.is_empty());
        })
    })
    .unwrap();
    let formatted = cx.read(|cx| s.read(cx).tabs[0].editor.read(cx).value().to_string());
    assert_ne!(formatted, original);
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    let expected = runtime.handle().block_on(db.query(original, 1)).unwrap();
    let actual = runtime.handle().block_on(db.query(&formatted, 1)).unwrap();
    assert_eq!(actual[0].rows, expected[0].rows);
    cx.simulate_keystrokes(w, "ctrl-z");
    cx.read(|cx| {
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            original
        )
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass14_sqlite_restore_rechecks_transaction_and_reviewed_file(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("reviewed.sqlite");
    let p = profile(&dir.path().join("guarded.sqlite"));
    let (w, _shell, workspaces) = workspace(cx, runtime.handle(), vec![p.clone()]);
    let s = workspaces[0].clone();
    connect(cx, w, &s, p.clone());
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'snapshot')",
    );
    runtime
        .handle()
        .block_on(rowboat_services::backup(
            &p,
            "",
            &backup,
            &rowboat_services::ClientRuntime::Native,
        ))
        .unwrap();
    run(cx, w, &s, "UPDATE items SET name='keep'");
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    show(cx, w, &s, TransferKind::Restore);
    reviewed_file(cx, w, &s, &runtime.handle(), &backup);
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.changes.push(rowboat_core::RowChange::Insert {
                values: vec![("id".into(), rowboat_core::Cell::Text("2".into()))],
            });
            s.start_transfer(cx);
            assert!(!s.busy);
            assert!(s.error.as_ref().unwrap().contains("staged"));
            assert_eq!(s.changes.len(), 1);
            s.changes.clear();
            s.transaction_known = false;
            s.start_transfer(cx);
            assert!(!s.busy);
            s.transaction_known = true;
            s.error = None;
        })
    });
    // Simulate a transaction started after the UI's last state sample.
    runtime
        .handle()
        .block_on(db.query("BEGIN; SELECT * FROM items", 10))
        .unwrap();
    click(cx, w, "run-transfer");
    settle(cx, w, &s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("active transaction")
        )
    });
    let rows = runtime
        .handle()
        .block_on(db.query("SELECT name FROM items", 1))
        .unwrap();
    assert_eq!(rows[0].rows[0][0].display(), "keep");
    runtime.handle().block_on(db.rollback()).unwrap();
    // A reviewed file's metadata is rechecked before the provider is entered.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&backup)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    cx.update(|cx| s.update(cx, |s, cx| s.start_transfer(cx)));
    settle(cx, w, &s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("changed after review")
        )
    });
    let rows = runtime
        .handle()
        .block_on(db.query("SELECT name FROM items", 1))
        .unwrap();
    assert_eq!(rows[0].rows[0][0].display(), "keep");
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass14_sqlite_same_file_workspaces_guard_freeze_and_invalidate(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("shared.sqlite");
    let backup = dir.path().join("snapshot.sqlite");
    let p = profile(&target);
    let external = runtime
        .handle()
        .block_on(Database::connect(&p, ""))
        .unwrap();
    runtime.handle().block_on(external.query("PRAGMA journal_mode=WAL; CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'snapshot'); CREATE TABLE sentinel(id INTEGER)", 10)).unwrap();
    runtime
        .handle()
        .block_on(rowboat_services::backup(
            &p,
            "",
            &backup,
            &rowboat_services::ClientRuntime::Native,
        ))
        .unwrap();
    runtime
        .handle()
        .block_on(external.query("UPDATE items SET name='before restore'", 10))
        .unwrap();
    let alias = dir.path().join("alias.sqlite");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let mut b = profile(&alias);
    b.name = "same file alias".into();
    let mut r = profile(&alias);
    r.name = "read-only WAL reader".into();
    r.read_only = true;
    r.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    let other = profile(&dir.path().join("unrelated.sqlite"));
    let profiles = vec![p.clone(), b.clone(), r.clone(), other.clone()];
    let (w, shell, workspaces) = workspace(cx, runtime.handle(), profiles.clone());
    let s = &workspaces[0];
    let alias = &workspaces[1];
    let reader = &workspaces[2];
    let unrelated = &workspaces[3];
    for (entity, profile) in workspaces.iter().zip(profiles) {
        connect(cx, w, entity, profile);
    }
    run_for_editing(cx, w, s, "SELECT id,name FROM items");
    run_for_editing(cx, w, alias, "SELECT id,name FROM items");
    run(cx, w, reader, "BEGIN; SELECT * FROM items");
    run(
        cx,
        w,
        unrelated,
        "CREATE TABLE own(id INTEGER); BEGIN; INSERT INTO own VALUES(7)",
    );
    show(cx, w, s, TransferKind::Restore);
    reviewed_file(cx, w, s, &runtime.handle(), &backup);
    cx.update(|cx| s.update(cx, |s, cx| s.start_transfer(cx)));
    settle(cx, w, s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("same-file workspace")
        );
        assert!(s.read(cx).query_plan().is_some());
        assert!(alias.read(cx).query_plan().is_some());
        assert!(reader.read(cx).transaction);
    });
    run(cx, w, reader, "ROLLBACK");
    // An actively streaming query in an alias is also a restore blocker.
    cx.update_window(w, |_,w,cx| alias.update(cx, |s,cx| {
        s.tabs[0].editor.update(cx, |e,cx| e.set_value("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n",w,cx));
        s.run_query(cx);
        assert!(s.busy);
    })).unwrap();
    cx.update(|cx| s.update(cx, |s, cx| s.start_transfer(cx)));
    settle(cx, w, s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("same-file workspace")
        )
    });
    cx.update(|cx| alias.update(cx, |s, cx| s.cancel(cx)));
    settle(cx, w, alias);
    run_for_editing(cx, w, alias, "SELECT id,name FROM items");
    // Real edit draft remains intact after rejection.
    cx.update_window(w, |_, w, cx| {
        alias.update(cx, |s, cx| {
            s.select_grid_cell(0, 1, false, cx);
            s.begin_cell_edit(w, cx);
            assert!(s.has_row_draft());
        })
    })
    .unwrap();
    cx.update(|cx| s.update(cx, |s, cx| s.start_transfer(cx)));
    settle(cx, w, s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .error
                .as_ref()
                .unwrap()
                .contains("same-file workspace")
        );
        assert!(alias.read(cx).has_row_draft());
    });
    cx.update(|cx| {
        alias.update(cx, |s, _| {
            s.grid.draft = None;
            s.query_edit.draft = None;
        })
    });
    let mut invocations = cx.read(|cx| {
        workspaces
            .iter()
            .map(|s| s.read(cx).query_run_invocations)
            .collect::<Vec<_>>()
    });
    for entity in [s, alias] {
        cx.update_window(w, |_, w, cx| {
            entity.update(cx, |s, cx| {
                s.tabs[0].editor.update(cx, |e, cx| {
                    e.set_value("INSERT INTO sentinel VALUES(99)", w, cx)
                })
            })
        })
        .unwrap();
    }
    // External lock holds the actual backend in progress while we test the lease.
    runtime
        .handle()
        .block_on(external.query("BEGIN IMMEDIATE", 10))
        .unwrap();
    cx.update(|cx| s.update(cx, |s, cx| s.start_transfer(cx)));
    cx.update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
        .unwrap();
    cx.read(|cx| {
        for workspace in &workspaces {
            assert!(workspace.read(cx).close_requested);
        }
        assert!(unrelated.read(cx).transaction);
    });
    let source_route = cx.read(|cx| s.read(cx).tx.route);
    cx.update_window(w, |_, w, cx| {
        alias.update(cx, |s, cx| {
            let before = s.session_generation;
            s.connect_profile(b.clone(), String::new(), cx);
            assert_eq!(s.session_generation, before);
            s.run_query(cx);
        });
        s.update(cx, |s, _| {
            s.shell_tx
                .send(crate::workspace_shell::Command::Open {
                    profile: Some(Box::new(profile(&target))),
                    password: None,
                })
                .unwrap();
            s.shell_tx
                .send(crate::workspace_shell::Command::Move {
                    route: source_route,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
            // A late callback from another operation cannot release this lease.
            s.shell_tx
                .send(crate::workspace_shell::Command::RestoreCompleted {
                    id: uuid::Uuid::new_v4(),
                    attempted: false,
                    result: Ok("stale".into()),
                })
                .unwrap();
        });
        shell.update(cx, |s, cx| {
            s.test_close_dialog(w, cx);
            assert_eq!(s.test_tick(w, cx).len(), 4);
        });
    })
    .unwrap();
    cx.read(|cx| assert!(s.read(cx).close_requested));
    invocations[1] += 1; // The explicitly attempted (blocked) Run above.
    runtime.handle().block_on(external.rollback()).unwrap();
    settle(cx, w, s);
    cx.read(|cx| {
        assert!(s.read(cx).error.is_none(), "{:?}", s.read(cx).error);
        for (i, workspace) in workspaces.iter().enumerate() {
            let state = workspace.read(cx);
            assert!(!state.close_requested);
            assert_eq!(state.query_run_invocations, invocations[i]);
            if i < 3 {
                assert!(state.tabs[0].results.is_empty());
                assert!(state.query_edit.results.is_empty());
            }
        }
        assert!(unrelated.read(cx).transaction);
        assert!(unrelated.read(cx).transaction_known);
    });
    let rows = runtime
        .handle()
        .block_on(external.query("SELECT name,(SELECT count(*) FROM sentinel) FROM items", 1))
        .unwrap();
    assert_eq!(rows[0].rows[0][0].display(), "snapshot");
    assert_eq!(rows[0].rows[0][1].display(), "0");
    let other_db = cx.read(|cx| unrelated.read(cx).database.clone().unwrap());
    assert!(
        runtime
            .handle()
            .block_on(other_db.in_transaction())
            .unwrap()
    );
    runtime.handle().block_on(other_db.rollback()).unwrap();
    // A provider-entered failure releases the same lease and conservatively
    // revokes every alias's proof; the native provider preserves target contents.
    run(cx, w, s, "SELECT id,name FROM items");
    run(cx, w, alias, "SELECT id,name FROM items");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&backup)
        .unwrap()
        .set_len(100)
        .unwrap();
    show(cx, w, s, TransferKind::Restore);
    reviewed_file(cx, w, s, &runtime.handle(), &backup);
    cx.update(|cx| s.update(cx, |s, cx| s.start_transfer(cx)));
    settle(cx, w, s);
    cx.read(|cx| {
        assert!(s.read(cx).error.is_some());
        for state in [s.read(cx), alias.read(cx)] {
            assert!(!state.close_requested);
            assert!(state.query_edit.results.is_empty());
            assert!(state.tabs[0].results.is_empty());
        }
    });
    let rows = runtime
        .handle()
        .block_on(external.query("SELECT name FROM items", 1))
        .unwrap();
    assert_eq!(rows[0].rows[0][0].display(), "snapshot");
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}
