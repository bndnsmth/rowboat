//! Real retained shell/profile forms, with in-memory writers and disposable SQLite only.
use crate::{app::Workbench, workspace_shell::WorkspaceShell};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, KeyBinding, Modifiers, TestAppContext, VisualTestContext,
    px, size,
};
use rowboat_core::{ConnectionProfile, Engine, SqliteFileOptions, SqliteOpenMode};

fn file_profile(id: &str) -> ConnectionProfile {
    ConnectionProfile {
        id: id.into(),
        name: format!("Profile {id}"),
        engine: Engine::Sqlite,
        group: "Local".into(),
        file: Some(SqliteFileOptions {
            path: std::env::temp_dir().join(format!("rowboat-profile-{id}.sqlite")),
            mode: SqliteOpenMode::OpenExisting,
        }),
        ..Default::default()
    }
}
fn setup(
    cx: &mut TestAppContext,
    runtime: tokio::runtime::Handle,
) -> (AnyWindowHandle, Entity<WorkspaceShell>, Entity<Workbench>) {
    let (w, shell, workspaces) =
        crate::pass14_tools::workspace(cx, runtime, vec![file_profile("open")]);
    cx.update(|cx| {
        cx.bind_keys([KeyBinding::new(
            "escape",
            crate::app::Dismiss,
            Some("Rowboat"),
        )]);
        crate::preferences_ui::bind_shortcuts(&Default::default(), &Default::default(), cx)
            .unwrap();
        workspaces[0].update(cx, |s, _| {
            s.edit_state(|state| {
                state.connections.extend([
                    file_profile("saved-file"),
                    file_profile("other-file"),
                    ConnectionProfile {
                        id: "network".into(),
                        name: "Reporting".into(),
                        group: "Staging".into(),
                        host: "db.example".into(),
                        port: 5432,
                        database: "analytics".into(),
                        engine: Engine::Postgres,
                        ..Default::default()
                    },
                ]);
            })
        });
    });
    cx.simulate_window_resize(w, size(px(960.), px(640.)));
    (w, shell, workspaces[0].clone())
}
fn draw(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn tick(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    shell: &Entity<WorkspaceShell>,
) -> Vec<Entity<Workbench>> {
    let result = cx
        .update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
        .unwrap();
    draw(cx, w);
    result
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(cx, w);
}
fn visible(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) -> bool {
    VisualTestContext::from_window(w, cx)
        .debug_bounds(id)
        .is_some()
}
fn search(cx: &mut TestAppContext, w: AnyWindowHandle, text: &str) {
    click(cx, w, "manager-search");
    cx.simulate_keystrokes(w, "ctrl-a");
    cx.simulate_input(w, text);
    draw(cx, w);
}
fn profile_ids(s: &Workbench) -> Vec<String> {
    s.shared
        .borrow()
        .state
        .connections
        .iter()
        .map(|p| p.id.clone())
        .collect()
}

#[gpui_kit::test]
fn pass20_browser_routes_search_cancel_and_narrow_focus_have_no_workspace_side_effects(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, retained) = setup(cx, runtime.handle());
    let (editor, revision) = cx.read(|cx| {
        let s = retained.read(cx);
        (s.tabs[0].editor.clone(), s.shared.borrow().revision)
    });
    click(cx, w, "manage-connections");
    assert!(visible(cx, w, "connection-manager"));
    // Initial search focus: typing immediately filters, without an extra click.
    cx.simulate_input(w, "reporting staging postgres db.example analytics");
    draw(cx, w);
    assert!(visible(cx, w, "manager-open-network"));
    assert!(!visible(cx, w, "manager-open-saved-file"));
    search(cx, w, "no matching profile");
    assert!(visible(cx, w, "manager-empty"));
    cx.simulate_window_resize(w, size(px(800.), px(480.)));
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    for id in [
        "manager-search",
        "manager-new",
        "manager-close",
        "manager-draft",
    ] {
        let bounds = visual.debug_bounds(id).unwrap();
        assert!(
            bounds.left() >= px(0.) && bounds.right() <= px(800.) && bounds.bottom() <= px(480.),
            "{id} clipped: {bounds:?}"
        );
    }
    cx.simulate_keystrokes(w, "tab shift-tab escape");
    draw(cx, w);
    assert!(!visible(cx, w, "connection-manager"));
    assert_eq!(tick(cx, w, &shell).len(), 1);
    click(cx, w, "new-workspace");
    assert!(visible(cx, w, "connection-manager"));
    click(cx, w, "manager-close");
    click(cx, w, "workspace-menu-trigger");
    cx.simulate_keystrokes(w, "enter");
    draw(cx, w);
    assert!(visible(cx, w, "connection-manager"));
    click(cx, w, "manager-close");
    cx.simulate_keystrokes(w, "ctrl-shift-p");
    cx.simulate_input(w, "Manage saved connections");
    cx.simulate_keystrokes(w, "enter");
    tick(cx, w, &shell);
    assert!(visible(cx, w, "connection-manager"));
    click(cx, w, "manager-close");
    assert_eq!(tick(cx, w, &shell).len(), 1);
    cx.read(|cx| {
        let s = retained.read(cx);
        assert_eq!(s.tabs[0].editor, editor);
        assert_eq!(s.shared.borrow().revision, revision);
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.database.is_none() && s.query_stream.is_none() && !s.busy);
        assert!(s.form.is_none() && s.startup_connect.is_none());
        assert!(shell.read(cx).test_manager_editor().is_none());
    });
    // New Connection remains an add-form command, not the browser/draft action.
    cx.simulate_keystrokes(w, "ctrl-shift-p");
    cx.simulate_input(w, "New saved connection");
    cx.simulate_keystrokes(w, "enter");
    tick(cx, w, &shell);
    cx.read(|cx| {
        let form = shell.read(cx).test_manager_editor().unwrap();
        assert!(
            form.read(cx)
                .form
                .as_ref()
                .unwrap()
                .credential_request
                .is_none()
        );
    });
    assert_eq!(tick(cx, w, &shell).len(), 1);
}

#[gpui_kit::test]
fn pass20_edit_save_without_connect_and_stale_profile_guard_keep_retained_workspace(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, retained) = setup(cx, runtime.handle());
    click(cx, w, "manage-connections");
    search(cx, w, "saved-file");
    click(cx, w, "manager-edit-saved-file");
    let editor = cx.read(|cx| shell.read(cx).test_manager_editor().unwrap());
    assert_eq!(tick(cx, w, &shell).len(), 1);
    click(cx, w, "name");
    cx.simulate_keystrokes(w, "ctrl-a");
    cx.simulate_input(w, "Renamed local profile");
    click(cx, w, "save-connection");
    tick(cx, w, &shell);
    cx.read(|cx| {
        let s = retained.read(cx);
        assert_eq!(
            s.shared
                .borrow()
                .state
                .connections
                .iter()
                .find(|p| p.id == "saved-file")
                .unwrap()
                .name,
            "Renamed local profile"
        );
        assert!(s.database.is_none() && !s.busy && s.form.is_none());
        assert_eq!(s.query_run_invocations, 0);
        let editor = editor.read(cx);
        assert!(editor.database.is_none() && !editor.busy);
        assert!(editor.form.is_none());
        assert_eq!(editor.query_run_invocations, 0);
        assert!(shell.read(cx).test_manager_editor().is_none());
    });
    assert!(visible(cx, w, "connection-manager"));
    search(cx, w, "saved-file");
    click(cx, w, "manager-edit-saved-file");
    let editor = cx.read(|cx| shell.read(cx).test_manager_editor().unwrap());
    cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.edit_state(|state| {
                state
                    .connections
                    .iter_mut()
                    .find(|p| p.id == "saved-file")
                    .unwrap()
                    .name = "Concurrent saved name".into()
            })
        })
    });
    click(cx, w, "save-connection");
    tick(cx, w, &shell);
    cx.read(|cx| {
        let e = editor.read(cx);
        assert!(e.form.is_some() && !e.busy);
        assert!(e.error.as_ref().unwrap().contains("changed"));
        assert_eq!(
            retained
                .read(cx)
                .shared
                .borrow()
                .state
                .connections
                .iter()
                .find(|p| p.id == "saved-file")
                .unwrap()
                .name,
            "Concurrent saved name"
        );
    });
    cx.simulate_keystrokes(w, "escape");
    draw(cx, w);
    assert!(visible(cx, w, "manager-search"));
    assert_eq!(tick(cx, w, &shell).len(), 1);
}

#[gpui_kit::test]
fn pass20_remove_confirmation_stale_readonly_and_open_profile_guards_preserve_data(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, retained) = setup(cx, runtime.handle());
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"database file must survive profile removal").unwrap();
    cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.edit_state(|state| {
                state.last_connection_id = Some("saved-file".into());
                state
                    .connections
                    .iter_mut()
                    .find(|p| p.id == "saved-file")
                    .unwrap()
                    .file
                    .as_mut()
                    .unwrap()
                    .path = file.path().into();
                state.history.push(rowboat_services::HistoryEntry {
                    id: "preserved-history".into(),
                    connection_id: "saved-file".into(),
                    sql: "SELECT 1".into(),
                    ..Default::default()
                });
            })
        })
    });
    click(cx, w, "manage-connections");
    search(cx, w, "Profile open");
    click(cx, w, "manager-remove-open");
    assert!(!visible(cx, w, "manager-remove-confirm"));
    search(cx, w, "Profile saved-file");
    click(cx, w, "manager-remove-saved-file");
    click(cx, w, "manager-remove-cancel");
    cx.read(|cx| assert!(profile_ids(retained.read(cx)).contains(&"saved-file".into())));
    click(cx, w, "manager-remove-saved-file");
    cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.edit_state(|state| {
                state
                    .connections
                    .iter_mut()
                    .find(|p| p.id == "saved-file")
                    .unwrap()
                    .name = "Changed before confirm".into()
            })
        })
    });
    click(cx, w, "manager-remove-confirm");
    cx.read(|cx| assert!(profile_ids(retained.read(cx)).contains(&"saved-file".into())));
    click(cx, w, "manager-remove-cancel");
    search(cx, w, "Changed before confirm");
    click(cx, w, "manager-remove-saved-file");
    cx.update(|cx| retained.update(cx, |s, _| s.shared.borrow_mut().writable = false));
    click(cx, w, "manager-remove-confirm");
    cx.read(|cx| assert!(profile_ids(retained.read(cx)).contains(&"saved-file".into())));
    cx.update(|cx| retained.update(cx, |s, _| s.shared.borrow_mut().writable = true));
    let revision = cx.read(|cx| retained.read(cx).shared.borrow().revision);
    click(cx, w, "manager-remove-confirm");
    tick(cx, w, &shell);
    cx.read(|cx| {
        let s = retained.read(cx);
        let shared = s.shared.borrow();
        assert_eq!(shared.revision, revision + 1);
        assert!(!profile_ids(s).contains(&"saved-file".into()));
        assert!(
            profile_ids(s).contains(&"other-file".into())
                && profile_ids(s).contains(&"open".into())
        );
        assert_eq!(shared.state.history.len(), 1);
        assert!(shared.state.last_connection_id.is_none());
        assert_eq!(shared.state.history[0].id, "preserved-history");
        assert_eq!(s.tabs[0].editor.read(cx).value().as_ref(), "SELECT 1");
    });
    assert_eq!(
        std::fs::read(file.path()).unwrap(),
        b"database file must survive profile removal"
    );
    assert_eq!(tick(cx, w, &shell).len(), 1);
}

#[gpui_kit::test]
fn pass20_switch_existing_workspace_preserves_physical_sqlite_session_and_sql(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut profile = file_profile("physical");
    profile.file = Some(SqliteFileOptions {
        path: directory.path().join("session.sqlite"),
        mode: SqliteOpenMode::CreateIfMissing,
    });
    let db = runtime
        .handle()
        .block_on(rowboat_core::Database::connect(&profile, ""))
        .unwrap();
    runtime.handle().block_on(db.query("CREATE TEMP TABLE session_marker(value TEXT); INSERT INTO session_marker VALUES ('same connection');", 10)).unwrap();
    let (w, shell, workspaces) =
        crate::pass14_tools::workspace(cx, runtime.handle(), vec![profile]);
    let retained = workspaces[0].clone();
    cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.database = Some(db);
            s.session_generation = 42;
        })
    });
    let sql = cx.read(|cx| retained.read(cx).tabs[0].editor.clone());
    click(cx, w, "manage-connections");
    click(cx, w, "manager-open-physical");
    let after = tick(cx, w, &shell);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0], retained);
    let db = cx.read(|cx| {
        let s = retained.read(cx);
        assert_eq!(s.session_generation, 42);
        assert_eq!(s.tabs[0].editor, sql);
        assert_eq!(s.query_run_invocations, 0);
        assert!(!s.busy && s.form.is_none());
        s.database.clone().unwrap()
    });
    let result = runtime
        .handle()
        .block_on(db.query("SELECT value FROM session_marker", 10))
        .unwrap();
    assert_eq!(
        result[0].rows[0][0],
        rowboat_core::Cell::Text("same connection".into())
    );
    // Save & Connect on an existing owner must also only activate its session.
    click(cx, w, "manage-connections");
    click(cx, w, "manager-edit-physical");
    let editor = cx.read(|cx| shell.read(cx).test_manager_editor().unwrap());
    cx.update(|cx| editor.update(cx, |s, cx| s.save_connection(true, cx)));
    tick(cx, w, &shell);
    assert!(!visible(cx, w, "connection-manager"));
    cx.read(|cx| assert_eq!(retained.read(cx).session_generation, 42));
    // Move the same entity to another host, then switch through the main browser.
    let id = cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.shell_tx
                .send(crate::workspace_shell::Command::Move {
                    route: s.tx.route,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
            s.workspace_id
        })
    });
    tick(cx, w, &shell);
    let host = cx.read(|cx| {
        shell
            .read(cx)
            .test_workspace_host(id)
            .expect("satellite opened")
    });
    click(cx, w, "manage-connections");
    click(cx, w, "manager-open-physical");
    assert_eq!(tick(cx, w, &shell).len(), 1);
    cx.read(|cx| {
        assert_eq!(shell.read(cx).test_workspace_host(id), Some(host));
        let s = retained.read(cx);
        assert_eq!(s.shared.borrow().active_workspace, id);
        assert_eq!(s.session_generation, 42);
        assert_eq!(s.tabs[0].editor, sql);
        assert_eq!(s.query_run_invocations, 0);
    });
    let result = runtime
        .handle()
        .block_on(db.query("SELECT value FROM session_marker", 10))
        .unwrap();
    assert_eq!(
        result[0].rows[0][0],
        rowboat_core::Cell::Text("same connection".into())
    );
}

#[gpui_kit::test]
fn pass20_long_list_pinned_controls_and_explicit_draft(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, retained) = setup(cx, runtime.handle());
    cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.edit_state(|state| {
                state
                    .connections
                    .extend((0..60).map(|i| file_profile(&format!("long-list-{i:03}"))));
            })
        })
    });
    cx.simulate_window_resize(w, size(px(800.), px(480.)));
    click(cx, w, "new-workspace");
    assert_eq!(tick(cx, w, &shell).len(), 1);
    let mut visual = VisualTestContext::from_window(w, cx);
    let list = visual.debug_bounds("manager-list").unwrap();
    let search = visual.debug_bounds("manager-search").unwrap();
    let target = visual.debug_bounds("profile-target-long-list-000").unwrap();
    assert!(
        target.size.height >= px(12.),
        "Profile target collapsed: {target:?}"
    );
    let draft = visual.debug_bounds("manager-draft").unwrap();
    assert!(search.bottom() <= list.top() && list.bottom() <= draft.top());
    assert!(draft.bottom() <= px(480.));
    click(cx, w, "manager-draft");
    let workspaces = tick(cx, w, &shell);
    assert_eq!(workspaces.len(), 2);
    cx.read(|cx| {
        let draft = workspaces
            .iter()
            .find(|s| s.read(cx).owner.is_none())
            .unwrap()
            .read(cx);
        assert!(draft.form.is_none() && draft.database.is_none());
        assert_eq!(draft.query_run_invocations, 0);
        assert_eq!(draft.shared.borrow().active_workspace, draft.workspace_id);
    });
    click(cx, w, "new-workspace");
    click(cx, w, "manager-draft");
    assert_eq!(tick(cx, w, &shell).len(), 2);
}

#[gpui_kit::test]
fn pass20_async_removal_failure_stale_callback_and_form_owner_guard(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, shell, retained) = setup(cx, runtime.handle());
    click(cx, w, "manage-connections");
    search(cx, w, "network");
    // Search uses displayed target/name/engine, not opaque IDs.
    search(cx, w, "reporting");
    click(cx, w, "manager-remove-network");
    let request = cx.update(|cx| shell.update(cx, |s, _| s.test_manager_pending_removal()));
    cx.simulate_keystrokes(w, "escape");
    draw(cx, w);
    assert!(visible(cx, w, "manager-remove-confirm"));
    cx.update(|cx| {
        retained.update(cx, |s, _| {
            s.shell_tx
                .send(crate::workspace_shell::Command::ProfileRemoved {
                    request: uuid::Uuid::new_v4(),
                    result: Ok(()),
                })
                .unwrap();
            s.shell_tx
                .send(crate::workspace_shell::Command::ProfileRemoved {
                    request,
                    result: Err("Injected keyring failure; profile kept".into()),
                })
                .unwrap();
        })
    });
    tick(cx, w, &shell);
    cx.read(|cx| assert!(profile_ids(retained.read(cx)).contains(&"network".into())));
    assert!(visible(cx, w, "manager-remove-confirm"));
    click(cx, w, "manager-remove-cancel");
    // A late form owner also prevents removal at confirmation time.
    search(cx, w, "saved-file");
    click(cx, w, "manager-remove-saved-file");
    cx.update_window(w, |_, w, cx| {
        retained.update(cx, |s, cx| {
            s.form = Some(crate::forms::ConnectionForm::new(
                file_profile("saved-file"),
                String::new(),
                w,
                cx,
            ));
        })
    })
    .unwrap();
    click(cx, w, "manager-remove-confirm");
    cx.read(|cx| assert!(profile_ids(retained.read(cx)).contains(&"saved-file".into())));
}
