//! Real GUI events and disposable SQLite files, through production schema workers.
use crate::{
    app::{Page, TablePage, Workbench},
    management::Operation,
};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{
    ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode, TableRef,
};
use std::time::Duration;

fn profile(path: &std::path::Path) -> ConnectionProfile {
    ConnectionProfile {
        name: "Pass16 disposable file".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.into(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    }
}
fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>) {
    for _ in 0..1600 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        if cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.busy
                })
            })
            .unwrap()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("Pass16 worker timed out");
}
fn workspace(
    cx: &mut TestAppContext,
    runtime: tokio::runtime::Handle,
    p: ConnectionProfile,
    id: u64,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let (w, s) = crate::workbench_tests::workspace(cx, id, runtime);
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, cx| {
            s.owner = Some(p.id.clone());
            s.tabs[0].connection_id = Some(p.id.clone());
            s.session_ready = true;
            s.connect_profile(p, String::new(), cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.read(|cx| assert!(s.read(cx).error.is_none(), "{:?}", s.read(cx).error));
    (w, s)
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
            s.page = Page::Query;
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
fn draw(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(w, |_, w, cx| {
        w.refresh();
        w.draw(cx).clear(cx);
    })
    .unwrap();
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual.debug_bounds(id).unwrap_or_else(|| {
        panic!(
            "Missing {id}; modal={:?}; scroll={:?}",
            visual.debug_bounds("modal-dismiss"),
            visual.debug_bounds("modal-viewport-scroll")
        )
    });
    visual.simulate_click(bounds.center(), Modifiers::default());
}
fn input(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str, value: &str) {
    click(cx, w, id);
    cx.simulate_keystrokes(w, "ctrl-a");
    cx.simulate_input(w, value);
    cx.run_until_parked();
}
fn open(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>, name: &str) {
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.open_table(
                TableRef {
                    schema: "main".into(),
                    name: name.into(),
                },
                cx,
            )
        })
    });
    settle(cx, w, s);
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.table_page = TablePage::Structure;
            cx.notify();
        })
    });
}
fn show(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
    op: Operation,
    name: Option<&str>,
) {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.show_management(op, name.map(str::to_owned), w, cx)
        })
    })
    .unwrap();
    cx.read(|cx| assert!(s.read(cx).management.is_some(), "{:?}", s.read(cx).error));
}
fn review(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>) {
    click(cx, w, "management-preview");
    settle(cx, w, s);
    cx.read(|cx| {
        let d = s.read(cx).management.as_ref().unwrap();
        assert!(d.review.is_some(), "{:?}", d.message);
    });
}
fn apply(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<Workbench>) {
    click(cx, w, "management-apply");
    settle(cx, w, s);
}
fn scalar(runtime: &tokio::runtime::Handle, db: &Database, sql: &str) -> String {
    runtime.block_on(db.query(sql, 1)).unwrap()[0].rows[0][0].display()
}

#[gpui_kit::test]
fn pass16_designer_review_explicit_apply_catalog_and_proof_invalidation(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = profile(&dir.path().join("designer.sqlite"));
    let (w, s) = workspace(cx, rt.handle(), p, 160);
    run(
        cx,
        w,
        &s,
        "CREATE TABLE source(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO source VALUES(1,'keep'); CREATE TABLE sentinel(id INTEGER)",
    );
    run_for_editing(cx, w, &s, "SELECT id,name FROM source");
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    let runs = cx.read(|cx| {
        assert!(s.read(cx).query_plan().is_some());
        s.read(cx).query_run_invocations
    });
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("INSERT INTO sentinel VALUES(99)", w, cx)
            });
            s.show_designer(w, cx);
        })
    })
    .unwrap();
    input(cx, w, "design-table", "new table");
    click(cx, w, "design-preview");
    cx.simulate_keystrokes(w, "escape");
    cx.read(|cx| {
        assert!(
            s.read(cx).advanced.designer.is_some(),
            "Escape dismissed an in-flight schema review"
        )
    });
    settle(cx, w, &s);
    cx.read(|cx| {
        let review = s
            .read(cx)
            .advanced
            .designer
            .as_ref()
            .unwrap()
            .review
            .as_ref()
            .unwrap();
        assert!(review.display.sql().contains("new table"));
        assert!(review.target.database.ends_with("designer.sqlite"));
        assert_eq!(s.read(cx).query_run_invocations, runs);
    });
    assert_eq!(
        scalar(
            &rt.handle(),
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name='new table'"
        ),
        "0"
    );
    click(cx, w, "design-back");
    input(cx, w, "design-table", "created table");
    click(cx, w, "design-preview");
    settle(cx, w, &s);
    click(cx, w, "design-apply");
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.advanced.designer.is_none());
        assert!(s.catalog.iter().any(|t| t.table.name == "created table"));
        assert_eq!(s.table.as_ref().unwrap().name, "created table");
        assert_eq!(s.structure.columns[0].name, "id");
        assert!(s.tabs[0].results.is_empty());
        assert!(s.query_edit.results.is_empty());
        assert_eq!(
            s.tabs[0].editor.read(cx).value().as_ref(),
            "INSERT INTO sentinel VALUES(99)"
        );
        assert_eq!(s.query_run_invocations, runs);
    });
    assert_eq!(
        scalar(&rt.handle(), &db, "SELECT count(*) FROM sentinel"),
        "0"
    );
    assert_eq!(
        scalar(
            &rt.handle(),
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name='new table'"
        ),
        "0"
    );
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass16_structure_column_index_table_forms_and_native_inspection(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (w, s) = workspace(
        cx,
        rt.handle(),
        profile(&dir.path().join("structure.sqlite")),
        161,
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE items(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parent(id),name TEXT); INSERT INTO items VALUES(1,NULL,'a')",
    );
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    open(cx, w, &s, "items");
    show(cx, w, &s, Operation::AddColumn, None);
    input(cx, w, "management-Name", "new field");
    review(cx, w, &s);
    assert_eq!(
        scalar(
            &rt.handle(),
            &db,
            "SELECT count(*) FROM pragma_table_info('items') WHERE name='new field'"
        ),
        "0"
    );
    apply(cx, w, &s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .structure
                .columns
                .iter()
                .any(|c| c.name == "new field")
        )
    });
    show(cx, w, &s, Operation::RenameColumn, Some("new field"));
    input(cx, w, "management-NewName", "renamed field");
    review(cx, w, &s);
    apply(cx, w, &s);
    show(cx, w, &s, Operation::CreateIndex, None);
    input(cx, w, "management-Name", "index with space");
    click(cx, w, "pick-column-name");
    review(cx, w, &s);
    apply(cx, w, &s);
    cx.read(|cx| {
        assert!(
            s.read(cx)
                .structure
                .indexes
                .iter()
                .any(|i| i.name == "index with space")
        )
    });
    show(cx, w, &s, Operation::InspectConstraints, None);
    click(cx, w, "management-preview");
    settle(cx, w, &s);
    cx.read(|cx| {
        let d = s.read(cx).management.as_ref().unwrap();
        assert!(d.inspection.len() >= 4, "{:?}", d.message);
        let json = serde_json::to_string(&d.inspection).unwrap();
        assert!(json.contains("CREATE TABLE"));
        assert!(json.contains("index with space"));
        assert!(json.contains("parent"));
    });
    click(cx, w, "management-close");
    show(cx, w, &s, Operation::DropIndex, Some("index with space"));
    review(cx, w, &s);
    apply(cx, w, &s);
    show(cx, w, &s, Operation::DropColumn, Some("renamed field"));
    review(cx, w, &s);
    apply(cx, w, &s);
    show(cx, w, &s, Operation::RenameTable, None);
    input(cx, w, "management-NewName", "renamed table");
    review(cx, w, &s);
    apply(cx, w, &s);
    cx.read(|cx| {
        assert_eq!(s.read(cx).table.as_ref().unwrap().name, "renamed table");
        assert!(s.read(cx).management.is_none());
    });
    show(cx, w, &s, Operation::DropTable, None);
    review(cx, w, &s);
    apply(cx, w, &s);
    cx.read(|cx| {
        assert!(s.read(cx).table.is_none());
        assert!(
            !s.read(cx)
                .catalog
                .iter()
                .any(|t| t.table.name == "renamed table")
        );
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass16_stale_external_ddl_rejects_then_fresh_review_and_session_guard(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = profile(&dir.path().join("fresh.sqlite"));
    let (w, s) = workspace(cx, rt.handle(), p.clone(), 162);
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT)",
    );
    open(cx, w, &s, "items");
    let other = rt.handle().block_on(Database::connect(&p, "")).unwrap();
    show(cx, w, &s, Operation::RenameColumn, Some("name"));
    input(cx, w, "management-NewName", "title");
    review(cx, w, &s);
    rt.handle()
        .block_on(other.query("ALTER TABLE items ADD COLUMN external TEXT", 0))
        .unwrap();
    apply(cx, w, &s);
    cx.read(|cx| {
        let d = s.read(cx).management.as_ref().unwrap();
        assert!(d.review.is_none());
        assert!(d.message.is_some());
    });
    assert_eq!(
        scalar(
            &rt.handle(),
            &other,
            "SELECT count(*) FROM pragma_table_info('items') WHERE name='name'"
        ),
        "1"
    );
    review(cx, w, &s);
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.session_generation += 1;
            s.apply_management(cx);
            assert!(!s.busy);
            assert!(s.error.as_ref().unwrap().contains("changed"));
            s.session_generation -= 1;
            s.error = None;
        })
    });
    apply(cx, w, &s);
    assert_eq!(
        scalar(
            &rt.handle(),
            &other,
            "SELECT count(*) FROM pragma_table_info('items') WHERE name='title'"
        ),
        "1"
    );
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass16_failed_unique_index_rolls_back_without_sql_replay(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (w, s) = workspace(
        cx,
        rt.handle(),
        profile(&dir.path().join("rollback.sqlite")),
        163,
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'duplicate'),(2,'duplicate')",
    );
    open(cx, w, &s, "items");
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    show(cx, w, &s, Operation::CreateUniqueIndex, None);
    input(cx, w, "management-Name", "unique_names");
    click(cx, w, "pick-column-name");
    review(cx, w, &s);
    apply(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        let d = s.management.as_ref().unwrap();
        assert!(d.review.is_none());
        assert!(
            d.message.as_ref().unwrap().contains("RolledBack"),
            "{:?}",
            d.message
        );
        assert!(s.transaction_known);
        assert!(!s.transaction);
    });
    assert_eq!(
        scalar(
            &rt.handle(),
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name='unique_names'"
        ),
        "0"
    );
    assert_eq!(scalar(&rt.handle(), &db, "SELECT count(*) FROM items"), "2");
    assert!(!rt.handle().block_on(db.in_transaction()).unwrap());
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass16_guards_and_operation_level_filtering(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (w, s) = workspace(
        cx,
        rt.handle(),
        profile(&dir.path().join("guards.sqlite")),
        164,
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); CREATE VIEW item_view AS SELECT * FROM items",
    );
    open(cx, w, &s, "items");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            for op in [
                Operation::AlterColumn,
                Operation::SetType,
                Operation::SetNullable,
                Operation::SetDefault,
                Operation::AddCheck,
                Operation::DropForeignKey,
                Operation::RenameIndex,
            ] {
                assert!(!s.structure_action_enabled(op));
                s.show_management(op, None, w, cx);
                assert!(s.management.is_none());
            }
            for guard in 0..5 {
                s.error = None;
                match guard {
                    0 => s.busy = true,
                    1 => s.transaction = true,
                    2 => s.transaction_known = false,
                    3 => s.profile.as_mut().unwrap().read_only = true,
                    _ => s
                        .changes
                        .push(rowboat_core::RowChange::Insert { values: vec![] }),
                }
                s.show_designer(w, cx);
                assert!(s.advanced.designer.is_none());
                s.show_management(Operation::AddColumn, None, w, cx);
                assert!(s.management.is_none());
                s.busy = false;
                s.transaction = false;
                s.transaction_known = true;
                s.profile.as_mut().unwrap().read_only = false;
                s.changes.clear();
            }
            s.error = None;
        })
    })
    .unwrap();
    show(cx, w, &s, Operation::AddColumn, None);
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    assert!(visual.debug_bounds("Set column type / USING").is_none());
    assert!(visual.debug_bounds("Rename index").is_none());
    click(cx, w, "management-close");
    open(cx, w, &s, "item_view");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            assert!(!s.structure_action_enabled(Operation::DropTable));
            s.show_management(Operation::DropTable, None, w, cx);
            assert!(s.management.is_none());
            s.profile.as_mut().unwrap().read_only = true;
            s.error = None;
            s.show_management(Operation::InspectConstraints, None, w, cx);
            assert!(s.management.is_some());
        })
    })
    .unwrap();
    click(cx, w, "management-preview");
    settle(cx, w, &s);
    cx.read(|cx| {
        assert!(
            !s.read(cx)
                .management
                .as_ref()
                .unwrap()
                .inspection
                .is_empty()
        )
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass16_explain_native_tree_copy_analyze_absent_and_no_dml(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (w, s) = workspace(
        cx,
        rt.handle(),
        profile(&dir.path().join("plans.sqlite")),
        165,
    );
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'keep'); CREATE INDEX names ON items(name)",
    );
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    let runs = cx.read(|cx| s.read(cx).query_run_invocations);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value(
                    "SELECT * FROM items WHERE name IN (SELECT name FROM items WHERE id=1)",
                    w,
                    cx,
                )
            });
            s.explain_query(w, cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    let raw = cx.read(|cx| {
        let e = s.read(cx).explain.as_ref().unwrap();
        assert!(e.error.is_none(), "{:?}", e.error);
        let report = e.report.as_ref().unwrap();
        assert!(report.timing.is_empty());
        assert!(
            report
                .roots
                .iter()
                .all(|n| n.estimates.is_empty() && n.actual.is_empty())
        );
        assert!(report.roots.iter().any(|n| !n.children.is_empty()));
        report.raw_report.clone()
    });
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    assert!(visual.debug_bounds("execute-analyze").is_none());
    click(cx, w, "copy-plan-json");
    cx.read(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), raw));
    click(cx, w, "plan-node-1");
    draw(cx, w);
    assert!(
        VisualTestContext::from_window(w, cx)
            .debug_bounds("plan-node-1-0")
            .is_none()
    );
    click(cx, w, "plan-node-1");
    draw(cx, w);
    assert!(
        VisualTestContext::from_window(w, cx)
            .debug_bounds("plan-node-1-0")
            .is_some()
    );
    click(cx, w, "plan-copy-0");
    cx.read(|cx| {
        let json: serde_json::Value =
            serde_json::from_str(&cx.read_from_clipboard().unwrap().text().unwrap()).unwrap();
        assert!(json.get("id").is_some());
        assert!(json.get("detail").is_some());
    });
    click(cx, w, "close-visual-explain");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("UPDATE items SET name='must not execute'", w, cx)
            });
            s.explain_query(w, cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    assert_eq!(scalar(&rt.handle(), &db, "SELECT name FROM items"), "keep");
    cx.read(|cx| {
        assert_eq!(s.read(cx).query_run_invocations, runs);
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            "UPDATE items SET name='must not execute'"
        );
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass16_independent_workspaces_keep_buffers_and_reject_old_schema_proof(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = profile(&dir.path().join("shared.sqlite"));
    let (a, first) = workspace(cx, rt.handle(), p.clone(), 166);
    run(
        cx,
        a,
        &first,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'original')",
    );
    let mut alias = p.clone();
    alias.id = uuid::Uuid::new_v4().to_string();
    let (b, second) = workspace(cx, rt.handle(), alias, 167);
    let (c, unrelated) = workspace(
        cx,
        rt.handle(),
        profile(&dir.path().join("independent.sqlite")),
        168,
    );
    run_for_editing(cx, b, &second, "SELECT id,name FROM items");
    run(
        cx,
        c,
        &unrelated,
        "CREATE TABLE own(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO own VALUES(1,'independent')",
    );
    run_for_editing(cx, c, &unrelated, "SELECT id,name FROM own");
    let (shell_tx, commands) = std::sync::mpsc::channel();
    cx.update_window(b, |_, w, cx| {
        second.update(cx, |s, cx| {
            s.shell_tx = shell_tx;
            assert!(s.query_plan().is_some());
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("DELETE FROM items", w, cx));
            s.select_grid_cell(0, 1, false, cx);
            s.begin_cell_edit(w, cx);
        })
    })
    .unwrap();
    cx.simulate_keystrokes(b, "ctrl-a");
    cx.simulate_input(b, "must not write");
    cx.simulate_keystrokes(b, "ctrl-enter");
    click(cx, b, "review-query-pending");
    let before = cx.read(|cx| {
        (
            second.read(cx).query_run_invocations,
            unrelated.read(cx).query_run_invocations,
        )
    });
    open(cx, a, &first, "items");
    show(cx, a, &first, Operation::AddColumn, None);
    input(cx, a, "management-Name", "schema_changed");
    review(cx, a, &first);
    apply(cx, a, &first);
    cx.read(|cx| {
        assert!(first.read(cx).query_edit.results.is_empty());
        assert!(unrelated.read(cx).query_plan().is_some());
        assert_eq!(
            unrelated.read(cx).tabs[0].results[0].rows[0][1].display(),
            "independent"
        );
        assert_eq!(
            second.read(cx).tabs[0].editor.read(cx).value().as_ref(),
            "DELETE FROM items"
        );
    });
    click(cx, b, "apply-changes");
    let ack = commands
        .try_iter()
        .find_map(|c| match c {
            crate::workspace_shell::Command::PersistApply { ack, .. } => Some(ack),
            _ => None,
        })
        .expect("Missing durable apply intent");
    ack.send(Ok(())).unwrap();
    settle(cx, b, &second);
    cx.read(|cx| {
        let s = second.read(cx);
        assert!(s.error.is_some(), "Old schema proof was reused");
        assert_eq!(
            s.recovery.detached.len(),
            1,
            "Conflicted edits must remain recoverable"
        );
        assert!(s.query_plan().is_none(), "Rejected proof must be revoked");
        assert_eq!(s.query_run_invocations, before.0);
        assert_eq!(unrelated.read(cx).query_run_invocations, before.1);
    });
    let db = cx.read(|cx| first.read(cx).database.clone().unwrap());
    assert_eq!(
        scalar(&rt.handle(), &db, "SELECT name FROM items"),
        "original"
    );
    for w in [a, b, c] {
        cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
    }
}

#[gpui_kit::test]
fn pass16_actual_readonly_schema_inspection_and_dml_query_plan(cx: &mut TestAppContext) {
    let rt = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut p = profile(&dir.path().join("readonly.sqlite"));
    let writer = rt.handle().block_on(Database::connect(&p, "")).unwrap();
    rt.handle().block_on(writer.query("CREATE TABLE items(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO items VALUES(1,'keep')",0)).unwrap();
    p.read_only = true;
    p.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    let (w, s) = workspace(cx, rt.handle(), p, 169);
    open(cx, w, &s, "items");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.show_management(Operation::DropTable, None, w, cx);
            assert!(s.management.is_none());
            s.show_designer(w, cx);
            assert!(s.advanced.designer.is_none());
            s.error = None;
        })
    })
    .unwrap();
    show(cx, w, &s, Operation::InspectConstraints, None);
    click(cx, w, "management-preview");
    settle(cx, w, &s);
    cx.read(|cx| {
        assert!(
            !s.read(cx)
                .management
                .as_ref()
                .unwrap()
                .inspection
                .is_empty()
        )
    });
    click(cx, w, "management-close");
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.page = Page::Query;
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("UPDATE items SET name='not executed'", w, cx)
            });
            s.explain_query(w, cx);
        })
    })
    .unwrap();
    settle(cx, w, &s);
    cx.read(|cx| {
        let e = s.read(cx).explain.as_ref().unwrap();
        assert!(e.error.is_none(), "{:?}", e.error);
        assert!(e.report.is_some());
    });
    assert_eq!(
        scalar(&rt.handle(), &writer, "SELECT name FROM items"),
        "keep"
    );
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}
