//! Real SQLite results through GPUI; fixture files never contain user data.
use gpui_kit::{
    AnyWindowHandle, AppContext, ClipboardItem, Entity, Modifiers, TestAppContext,
    VisualTestContext,
};
use rowboat_core::{
    Cell, ConnectionProfile, Engine, SqliteFileOptions, SqliteOpenMode, TableRef,
    grid::{self, ClipboardDialect, GridLimits},
};
use std::time::Duration;

fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pass13_results_")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}
fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, s: &Entity<crate::app::Workbench>) {
    for _ in 0..1600 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
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
    panic!("SQLite result operation timed out");
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
    path: &std::path::Path,
) {
    let profile = ConnectionProfile {
        name: "Pass13 result regression".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.to_owned(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    cx.update_window(w, |_, _, cx| {
        s.update(cx, |s, cx| {
            s.owner = Some(profile.id.clone());
            s.tabs[0].connection_id = Some(profile.id.clone());
            s.session_ready = true;
            s.connect_profile(profile, String::new(), cx);
        })
    })
    .unwrap();
    settle(cx, w, s);
    cx.read(|cx| assert!(s.read(cx).error.is_none(), "{:?}", s.read(cx).error));
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, selector: &'static str) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}
fn first_column(s: &crate::app::Workbench) -> Vec<String> {
    s.current_result()
        .unwrap()
        .rows
        .iter()
        .map(|r| r[0].display())
        .collect()
}
fn statement(
    s: &crate::app::Workbench,
    index: usize,
) -> crate::query_stream_ui::StatementPresentation {
    let tab = &s.tabs[0];
    crate::query_stream_ui::statement_presentation(
        tab.query_progress.as_ref(),
        index,
        &tab.results[index],
        s.query_running_for(tab.id),
        tab.metadata_incomplete.contains(&index),
    )
}

#[gpui_kit::test]
fn pass13_duplicate_headers_sort_exact_numeric_and_null_by_ordinal(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 131, runtime.handle());
    connect(cx, w, &s, &dir.path().join("sort.sqlite"));
    run(
        cx,
        w,
        &s,
        "CREATE TABLE numbers(a INTEGER PRIMARY KEY,b INTEGER); INSERT INTO numbers VALUES(1,20),(2,10),(3,NULL),(4,9007199254740993),(5,9007199254740992),(6,-2)",
    );
    run(cx, w, &s, "SELECT a AS id,b AS id FROM numbers ORDER BY a");
    click(cx, w, "column-head-1");
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(first_column(s), ["3", "6", "2", "1", "5", "4"]);
        assert_eq!(s.grid_sort_arrow(0, "id"), "");
        assert_eq!(s.grid_sort_arrow(1, "id"), " ↑");
    });
    click(cx, w, "column-head-1");
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(first_column(s), ["4", "5", "1", "2", "6", "3"]);
        assert_eq!(s.grid_sort_arrow(1, "id"), " ↓");
    });
    click(cx, w, "column-head-0");
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(first_column(s), ["1", "2", "3", "4", "5", "6"]);
        assert_eq!(s.grid_sort_arrow(0, "id"), " ↑");
        assert_eq!(s.grid_sort_arrow(1, "id"), "");
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass13_explicit_rerun_resets_sort_before_replacement_results(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 137, runtime.handle());
    connect(cx, w, &s, &dir.path().join("rerun.sqlite"));
    run(
        cx,
        w,
        &s,
        "CREATE TABLE numbers(a INTEGER PRIMARY KEY,b INTEGER); INSERT INTO numbers VALUES(1,20),(2,10),(3,30)",
    );
    run(cx, w, &s, "SELECT a,b FROM numbers ORDER BY a");
    click(cx, w, "column-head-1");
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(first_column(s), ["2", "1", "3"]);
        assert_eq!(s.query_sort, Some((1, false)));
        assert_eq!(s.grid_sort_arrow(1, "b"), " ↑");
    });
    // Replace both projection and server ordering. Reset must happen immediately
    // when Run clears the old result, before any new stream frames arrive.
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value(
                    "SELECT b AS value,a AS key FROM numbers ORDER BY a DESC",
                    w,
                    cx,
                );
                e.focus(w, cx);
            });
            s.run_query_for_editing(cx);
            assert!(s.tabs[0].results.is_empty());
            assert!(
                s.query_sort.is_none(),
                "Previous result's sort leaked into new execution"
            );
        })
    })
    .unwrap();
    settle(cx, w, &s);
    let proof = cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(first_column(s), ["30", "10", "20"]);
        assert!(s.query_sort.is_none());
        assert_eq!(s.grid_sort_arrow(0, "value"), "");
        assert_eq!(s.grid_sort_arrow(1, "key"), "");
        serde_json::to_value(s.query_plan().expect("fresh keyed capture").result()).unwrap()
    });
    click(cx, w, "column-head-1");
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(first_column(s), ["20", "10", "30"]);
        assert_eq!(
            s.query_sort,
            Some((1, false)),
            "First click must be ascending"
        );
        assert_eq!(s.grid_sort_arrow(1, "key"), " ↑");
        assert_eq!(
            serde_json::to_value(s.query_plan().unwrap().result()).unwrap(),
            proof
        );
        assert_eq!(
            s.shared.borrow().state.history.len(),
            3,
            "Only explicit runs execute SQL"
        );
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass13_sorted_filtered_staged_copy_keeps_proof_and_explicit_apply(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 132, runtime.handle());
    connect(cx, w, &s, &dir.path().join("proof.sqlite"));
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,label TEXT,note TEXT,payload BLOB,secret TEXT); INSERT INTO items VALUES(1,'keep-a','é',X'00ff','first'),(2,'omit','hidden',X'10','second'),(3,'keep-z',NULL,X'','third')",
    );
    run_for_editing(
        cx,
        w,
        &s,
        "SELECT id AS key_alias,label,note,payload FROM items ORDER BY id",
    );
    let (shell_tx, commands) = std::sync::mpsc::channel();
    let updated = "雪\t\"quoted\"\n\\N";
    let proof = cx.read(|cx| {
        serde_json::to_value(
            s.read(cx)
                .query_plan()
                .expect("editable keyed result")
                .result(),
        )
        .unwrap()
    });
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.shell_tx = shell_tx;
            s.sort_grid_column(1, cx);
            s.sort_grid_column(1, cx);
            s.result_filter
                .update(cx, |e, cx| e.set_value("keep", w, cx));
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("DELETE FROM items", w, cx));
            let visible = s.visible_grid_rows(cx);
            assert_eq!(visible.len(), 2);
            assert_eq!(
                s.current_result().unwrap().rows[visible[0]][0].display(),
                "3"
            );
            s.select_grid_cell(visible[0], 1, false, cx);
            s.begin_cell_edit(w, cx);
            s.grid
                .draft
                .as_ref()
                .unwrap()
                .value
                .update(cx, |e, cx| e.set_value(updated, w, cx));
            s.stage_cell_draft(cx);
            assert_eq!(s.changes.len(), 1, "{:?}", s.error);
            assert_eq!(
                serde_json::to_value(s.query_plan().unwrap().result()).unwrap(),
                proof
            );
            s.select_grid_cell(visible[0], 1, false, cx);
            s.select_grid_cell(visible[1], 3, true, cx);
            s.grid.lossless = true;
            s.copy_grid(false, cx);
            let text = cx.read_from_clipboard().unwrap().text().unwrap();
            let parsed =
                grid::parse_tsv(&text, ClipboardDialect::Rowboat, GridLimits::default()).unwrap();
            assert_eq!(
                parsed.rows(),
                &[
                    vec![Cell::Text(updated.into()), Cell::Null, Cell::Binary(vec![])],
                    vec![
                        Cell::Text("keep-a".into()),
                        Cell::Text("é".into()),
                        Cell::Binary(vec![0, 255])
                    ],
                ]
            );
            s.grid.lossless = false;
            s.copy_grid(false, cx);
            let plain = grid::parse_tsv(
                &cx.read_from_clipboard().unwrap().text().unwrap(),
                ClipboardDialect::Spreadsheet,
                GridLimits::default(),
            )
            .unwrap();
            assert_eq!(plain.rows()[0][0], Cell::Text(updated.into()));
            assert_eq!(plain.rows()[0][1].display(), Cell::Null.display());
            assert_eq!(
                plain.rows()[1][2].display(),
                Cell::Binary(vec![0, 255]).display()
            );
            assert_eq!(
                serde_json::to_value(s.query_plan().unwrap().result()).unwrap(),
                proof
            );
            s.review_pending(cx);
            assert!(s.query_edit.review);
            let sql = s.query_plan().unwrap().preview_changes(&s.changes).unwrap();
            assert!(sql.contains("UPDATE") && sql.contains("keep-z"));
            s.apply_changes(cx);
        })
    })
    .unwrap();
    let ack = commands
        .try_iter()
        .find_map(|c| match c {
            crate::workspace_shell::Command::PersistApply { ack, .. } => Some(ack),
            _ => None,
        })
        .expect("durable intent acknowledgment");
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    assert_eq!(
        runtime
            .handle()
            .block_on(db.query("SELECT label FROM items WHERE id=3", 1))
            .unwrap()[0]
            .rows[0][0]
            .display(),
        "keep-z"
    );
    ack.send(Ok(())).unwrap();
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert!(s.changes.is_empty());
        assert!(s.query_plan().is_none());
        assert!(s.query_edit_notice().contains("stale"));
        assert_eq!(
            s.shared.borrow().state.history.len(),
            2,
            "No editor SQL replay"
        );
    });
    let actual = runtime
        .handle()
        .block_on(db.query("SELECT id,label,secret FROM items ORDER BY id", 10))
        .unwrap();
    assert_eq!(
        actual[0].rows.len(),
        3,
        "The replacement editor DELETE was not executed"
    );
    assert_eq!(actual[0].rows[0][1].display(), "keep-a");
    assert_eq!(actual[0].rows[1][1].display(), "omit");
    assert_eq!(actual[0].rows[2][1].display(), updated);
    assert_eq!(actual[0].rows[2][2].display(), "third");
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass13_completed_statements_keep_affected_and_time_after_later_error(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 133, runtime.handle());
    connect(cx, w, &s, &dir.path().join("error.sqlite"));
    run(
        cx,
        w,
        &s,
        "CREATE TABLE effects(id INTEGER PRIMARY KEY); INSERT INTO effects VALUES(1); SELECT 42 AS empty WHERE 0; SELECT * FROM missing_pass13_table",
    );
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(s.tabs[0].results.len(), 3);
        assert!(
            s.tabs[0]
                .error
                .as_ref()
                .unwrap()
                .contains("missing_pass13_table")
        );
        for index in [0, 1] {
            let view = statement(s, index);
            assert!(view.complete);
            assert_eq!(view.icon, "✓");
            assert!(view.message.contains(if index == 1 {
                "1 rows affected"
            } else {
                "0 rows affected"
            }));
            assert_eq!(view.notice, "Result complete · query failed");
            assert_eq!(
                view.timing,
                format!("{} ms SQL/fetch", s.tabs[0].results[index].elapsed_ms)
            );
            assert!(!view.message.contains("commit") && !view.message.contains("incomplete"));
        }
        assert!(s.tabs[0].results[2].rows.is_empty());
        assert_eq!(s.tabs[0].results[2].columns[0].name, "empty");
        let timing = s.tabs[0]
            .query_progress
            .as_ref()
            .unwrap()
            .timing
            .as_ref()
            .unwrap();
        assert!(timing.total_ms.is_some() && timing.context_ms.is_some());
    });
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    assert!(
        VisualTestContext::from_window(w, cx)
            .debug_bounds("statement-result")
            .is_some()
    );
    let db = cx.read(|cx| s.read(cx).database.clone().unwrap());
    assert_eq!(
        runtime
            .handle()
            .block_on(db.query("SELECT count(*) FROM effects", 1))
            .unwrap()[0]
            .rows[0][0]
            .display(),
        "1"
    );
    // Completion does not imply COMMIT: a caller transaction can still roll back.
    run(
        cx,
        w,
        &s,
        "BEGIN; INSERT INTO effects VALUES(2); SELECT * FROM missing_pass13_table",
    );
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.transaction_known && s.transaction);
        assert!(statement(s, 1).complete);
        assert!(!statement(s, 1).message.contains("commit"));
    });
    run(cx, w, &s, "ROLLBACK");
    assert_eq!(
        runtime
            .handle()
            .block_on(db.query("SELECT count(*) FROM effects", 1))
            .unwrap()[0]
            .rows[0][0]
            .display(),
        "1"
    );
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass13_completed_write_survives_later_cancellation_and_reuse(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 134, runtime.handle());
    connect(cx, w, &s, &dir.path().join("cancel.sqlite"));
    run(cx, w, &s, "CREATE TABLE effects(id INTEGER PRIMARY KEY)");
    cx.update_window(w, |_, w, cx| s.update(cx, |s, cx| {
        s.tabs[0].editor.update(cx, |e, cx| e.set_value("INSERT INTO effects VALUES(1); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n", w, cx));
        s.run_query(cx);
    })).unwrap();
    let mut completed = false;
    for _ in 0..600 {
        completed = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.tabs[0].results.is_empty() && statement(s, 0).complete
                })
            })
            .unwrap();
        if completed {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(completed, "INSERT ResultEnd must precede cancellation");
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            let view = statement(s, 0);
            assert!(view.complete && view.message.contains("1 rows affected"));
            assert_eq!(view.notice, "Result complete · query still running");
            s.cancel(cx);
        })
    });
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        let view = statement(s, 0);
        assert!(view.complete && view.message.contains("1 rows affected"));
        assert_eq!(view.notice, "Result complete · query cancelled");
        assert!(s.transaction_known && !s.transaction);
        if s.tabs[0].results.len() > 1 {
            assert!(!statement(s, 1).complete);
            assert!(statement(s, 1).notice.contains("Incomplete"));
        }
    });
    run(cx, w, &s, "SELECT count(*) FROM effects");
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(first_column(s), ["1"]);
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass13_copy_small_rectangle_from_large_result_and_reject_payload_limit(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 135, runtime.handle());
    connect(cx, w, &s, &dir.path().join("copy.sqlite"));
    cx.update(|cx| s.update(cx, |s, _| s.state.preferences.query_row_limit = 2048));
    run(
        cx,
        w,
        &s,
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1024) SELECT x, zeroblob(32768) AS payload FROM n",
    );
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            assert!(s.error.is_none(), "{:?}", s.error);
            assert_eq!(s.current_result().unwrap().rows.len(), 1024);
            s.select_grid_cell(1023, 0, false, cx);
            s.copy_grid(false, cx);
            assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "1024");
            s.select_grid_cell(0, 1, false, cx);
            s.select_grid_cell(1023, 1, true, cx);
            s.copy_grid(false, cx);
            assert!(s.error.as_ref().unwrap().contains("limits"));
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().unwrap(),
                "1024",
                "Failed copy preserves clipboard"
            );
        })
    });
    cx.update(|cx| s.update(cx, |s, _| s.state.preferences.query_row_limit = 10_002));
    run(
        cx,
        w,
        &s,
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) SELECT x FROM n",
    );
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            assert!(s.error.is_none(), "{:?}", s.error);
            assert_eq!(s.current_result().unwrap().rows.len(), 10_001);
            s.select_grid_cell(0, 0, false, cx);
            s.select_grid_cell(10_000, 0, true, cx);
            s.copy_grid(false, cx);
            assert!(s.error.as_ref().unwrap().contains("limits"));
            assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "1024");
        })
    });
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}

#[gpui_kit::test]
fn pass13_table_copy_uses_staged_values_and_name_based_server_sort(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let dir = directory();
    let (w, s) = crate::workbench_tests::workspace(cx, 136, runtime.handle());
    connect(cx, w, &s, &dir.path().join("table.sqlite"));
    run(
        cx,
        w,
        &s,
        "CREATE TABLE items(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO items VALUES(1,'zulu'),(2,'alpha')",
    );
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
    cx.update(|cx| s.update(cx, |s, cx| s.sort_grid_column(1, cx)));
    settle(cx, w, &s);
    cx.read(|cx| assert_eq!(first_column(s.read(cx)), ["2", "1"]));
    cx.update(|cx| s.update(cx, |s, cx| s.sort_grid_column(1, cx)));
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(first_column(s), ["1", "2"]);
        assert_eq!(s.grid_sort_arrow(1, "label"), " ↓");
        assert_eq!(s.browse_request.sort, Some(("label".into(), false)));
    });
    cx.update(|cx| s.update(cx, |s, cx| s.sort_grid_column(1, cx)));
    settle(cx, w, &s);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            assert_eq!(first_column(s), ["2", "1"]);
            assert_eq!(s.browse_request.sort, Some(("label".into(), true)));
            assert_eq!(s.grid_sort_arrow(1, "label"), " ↑");
            s.select_grid_cell(0, 1, false, cx);
            s.begin_cell_edit(w, cx);
            s.grid
                .draft
                .as_ref()
                .unwrap()
                .value
                .update(cx, |e, cx| e.set_value("edited é", w, cx));
            s.stage_cell_draft(cx);
            assert_eq!(s.changes.len(), 1);
            s.copy_grid(true, cx);
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().unwrap(),
                "2\tedited é"
            );
            assert_eq!(
                s.table_result.rows[0][1].display(),
                "alpha",
                "Copy never rewrites raw originals"
            );
            cx.write_to_clipboard(ClipboardItem::new_string("".into()));
        })
    })
    .unwrap();
    cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
}
