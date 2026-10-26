use gpui_kit::{AppContext, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{Cell, Engine, sql::quote_ident};
fn settle(
    cx: &mut TestAppContext,
    w: gpui_kit::AnyWindowHandle,
    s: &gpui_kit::Entity<crate::app::Workbench>,
) {
    for _ in 0..800 {
        let busy = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    s.busy
                })
            })
            .unwrap();
        if !busy {
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("Query edit operation did not settle");
}
#[gpui_kit::test]
#[ignore = "Disposable PG15435/MySQL13309; injected persistence acknowledgment"]
fn pass9_live_query_alias_subset_stage_visible_update_review_apply_no_replay(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, a) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture(engine))
            .unwrap();
        let b = format!("{a}_other");
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (qa, qb) = (quote_ident(engine, &a), quote_ident(engine, &b));
            let snapshot=runtime.handle().block_on(async{
                db.query(&format!("CREATE SCHEMA {qb}"),0).await.unwrap();
                for name in [&qa,&qb]{db.query(&format!("CREATE TABLE {name}.items(id INT PRIMARY KEY,name TEXT,secret TEXT); INSERT INTO {name}.items VALUES(1,'alpha','not projected'),(2,'zulu','retain this')"),0).await.unwrap();}
                db.query(&if engine==Engine::MySql{format!("USE {qa}")}else{format!("SET search_path TO {qa}")},0).await.unwrap();db.namespace_catalog(None).await.unwrap()
            });
            let (w, s) = crate::workbench_tests::workspace(cx, 95, runtime.handle());
            let (shell_tx, commands) = std::sync::mpsc::channel();
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.shell_tx = shell_tx;
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile.clone());
                    s.endpoint = Some(profile.clone());
                    s.database = Some(db.clone());
                    s.session_ready = true;
                    s.tabs[0].connection_id = Some(profile.id.clone());
                    s.accept_namespace(snapshot, None, cx);
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(
                            "SELECT id AS key_alias,name AS label FROM items ORDER BY id",
                            w,
                            cx,
                        )
                    });
                    s.run_query_for_editing(cx);
                })
            })
            .unwrap();
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.query_plan().is_some(), "{}", s.query_edit_notice());
                assert_eq!(s.tabs[0].results[0].columns[0].name, "key_alias");
                assert_eq!(s.query_plan().unwrap().result().columns[0].name, "id");
                assert!(s.table.is_none());
            });
            // A later explorer/SQL database selection must not supply the update target.
            cx.update(|cx| {
                s.update(cx, |s, cx| {
                    s.load_namespace(Some(b.clone()), engine == Engine::MySql, cx)
                })
            });
            settle(cx, w, &s);
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.sort_column("label".into(), cx);
                    s.sort_column("label".into(), cx);
                    s.result_filter
                        .update(cx, |e, cx| e.set_value("zulu", w, cx));
                    // Mutating the editor after execution cannot alter the captured source.
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(format!("UPDATE {qb}.items SET name='MUST NOT RUN'"), w, cx)
                    });
                    s.select_grid_cell(0, 1, false, cx);
                    s.begin_cell_edit(w, cx);
                    assert!(s.grid.draft.is_some(), "{:?}", s.error);
                })
            })
            .unwrap();
            cx.simulate_keystrokes(w, "ctrl-a");
            cx.simulate_input(w, "edited");
            cx.simulate_keystrokes(w, "ctrl-enter");
            cx.read(|cx| {
                let s = s.read(cx);
                assert_eq!(s.changes.len(), 1, "{:?}", s.error);
                assert!(s.grid.draft.is_none());
                assert_eq!(s.change_target.as_ref().unwrap().table.schema, a);
                assert!(!s.namespace_switch_allowed());
                assert!(s.page == crate::app::Page::Query);
            });
            let before = runtime
                .handle()
                .block_on(db.query(&format!("SELECT name,secret FROM {qa}.items WHERE id=2"), 1))
                .unwrap();
            assert_eq!(before[0].rows[0][0].display(), "zulu");
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let mut visual = VisualTestContext::from_window(w, cx);
            let review = visual.debug_bounds("review-query-pending").unwrap();
            visual.simulate_click(review.center(), Modifiers::default());
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let mut visual = VisualTestContext::from_window(w, cx);
            assert!(
                visual.debug_bounds("query-update-preview-0").is_some(),
                "UPDATE preview hidden behind disclosure"
            );
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.query_edit.review);
                let sql = s.query_plan().unwrap().preview_changes(&s.changes).unwrap();
                assert!(
                    sql.contains("UPDATE")
                        && sql.contains(&a)
                        && sql.contains(&rowboat_core::sql::literal(
                            engine,
                            &Cell::Text("edited".into())
                        ))
                        && sql.contains(&rowboat_core::sql::literal(
                            engine,
                            &Cell::Text("zulu".into())
                        ))
                        && sql.contains("id"),
                    "{sql}"
                );
                assert!(!sql.contains("secret") && !sql.contains("key_alias"));
                assert!(s.table.is_none());
            });
            let apply = visual.debug_bounds("apply-changes").unwrap();
            visual.simulate_click(apply.center(), Modifiers::default());
            let mut acknowledgment = None;
            while let Ok(command) = commands.try_recv() {
                if let crate::workspace_shell::Command::PersistApply { marker, ack, .. } = command {
                    cx.read(|cx| assert!(s.read(cx).has_apply_marker(&marker)));
                    acknowledgment = Some(ack);
                }
            }
            assert!(
                acknowledgment.is_some(),
                "Apply did not request durable intent"
            );
            assert_eq!(
                runtime
                    .handle()
                    .block_on(db.query(&format!("SELECT name FROM {qa}.items WHERE id=2"), 1))
                    .unwrap()[0]
                    .rows[0][0]
                    .display(),
                "zulu",
                "Mutation ran before durable ack"
            );
            acknowledgment.unwrap().send(Ok(())).unwrap();
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.error.is_none(), "{:?}", s.error);
                assert!(s.changes.is_empty());
                assert!(s.query_plan().is_none());
                assert!(s.query_edit_notice().contains("stale"));
                assert_eq!(
                    s.shared.borrow().state.history.len(),
                    1,
                    "Arbitrary SQL was replayed"
                );
                assert_eq!(s.tabs[0].results[0].rows[0][1], Cell::Text("zulu".into()));
            });
            runtime.handle().block_on(async {
                let rows = db
                    .query(&format!("SELECT name,secret FROM {qa}.items WHERE id=2"), 1)
                    .await
                    .unwrap();
                assert_eq!(
                    rows[0].rows[0],
                    vec![
                        Cell::Text("edited".into()),
                        Cell::Text("retain this".into())
                    ]
                );
                assert_eq!(
                    db.query(&format!("SELECT name FROM {qb}.items WHERE id=2"), 1)
                        .await
                        .unwrap()[0]
                        .rows[0][0]
                        .display(),
                    "zulu"
                );
            });
            // A missing key produces an explanation, not an unexplained gray menu.
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.result_filter.update(cx, |e, cx| e.set_value("", w, cx));
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(format!("SELECT name FROM {qa}.items"), w, cx)
                    });
                    s.run_query_for_editing(cx);
                })
            })
            .unwrap();
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.query_plan().is_none());
                assert!(!s.grid_writable());
                assert!(
                    s.query_edit_notice().to_lowercase().contains("primary key"),
                    "{}",
                    s.query_edit_notice()
                );
            });
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
            for name in [&b, &a] {
                crate::pass5_tests::cleanup(&db, name).await.unwrap();
            }
        });
        if let Err(error) = run {
            std::panic::resume_unwind(error);
        }
    }
}

#[gpui_kit::test]
#[ignore = "Disposable PG15435/MySQL13309"]
fn pass9_live_query_row_form_tab_freeze_recovery_and_readonly_reasons(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture(engine))
            .unwrap();
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let q = quote_ident(engine, &schema);
            let snapshot=runtime.handle().block_on(async{
                db.query(&format!("CREATE TABLE {q}.items(id INT PRIMARY KEY,name TEXT,hidden TEXT);INSERT INTO {q}.items VALUES(1,'old','must remain')"),0).await.unwrap();db.namespace_catalog(Some(&schema)).await.unwrap()
            });
            let (w, s) = crate::workbench_tests::workspace(cx, 96, runtime.handle());
            let (original, other) = cx
                .update_window(w, |_, w, cx| {
                    s.update(cx, |s, cx| {
                        s.owner = Some(profile.id.clone());
                        s.profile = Some(profile.clone());
                        s.endpoint = Some(profile.clone());
                        s.database = Some(db.clone());
                        s.session_ready = true;
                        s.tabs[0].connection_id = Some(profile.id.clone());
                        s.accept_namespace(snapshot, Some(schema.clone()), cx);
                        let first = s.tabs[0].id;
                        s.new_query(Some("SELECT 9".into()), w, cx);
                        let second = s.tabs[s.active_tab].id;
                        s.focus_query_tab(first, cx);
                        s.tabs[s.active_tab].editor.update(cx, |e, cx| {
                            e.set_value(
                                format!("SELECT id AS k,name AS label FROM {q}.items"),
                                w,
                                cx,
                            )
                        });
                        s.run_query_for_editing(cx);
                        s.focus_query_tab(second, cx);
                        (first, second)
                    })
                })
                .unwrap();
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.query_plan().is_none());
                assert!(
                    s.query_edit.results[&original].plan.is_some(),
                    "Late result was not bound to its source tab"
                );
            });
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.focus_query_tab(original, cx);
                    s.busy = true;
                    s.sort_column("label".into(), cx);
                    assert!(
                        s.query_sort.is_none(),
                        "Streaming result was locally sorted before source binding"
                    );
                    s.busy = false;
                    s.select_grid_cell(0, 1, false, cx);
                    s.edit_row(false, w, cx);
                    let form = s.row_editor.as_ref().unwrap();
                    assert_eq!(form.fields.len(), 2);
                    assert!(form.fields.iter().all(|f| f.name != "hidden"));
                    form.fields
                        .iter()
                        .find(|f| f.name == "name")
                        .unwrap()
                        .value
                        .update(cx, |e, cx| e.set_value("row draft", w, cx));
                    s.focus_query_tab(other, cx);
                    s.stage_editor(cx);
                    assert!(s.changes.is_empty());
                    assert!(s.row_editor.is_some());
                    s.focus_query_tab(original, cx);
                    s.error = None;
                    s.close_requested = true;
                    s.stage_editor(cx);
                    assert!(s.changes.is_empty());
                    s.close_requested = false;
                    s.stage_editor(cx);
                    assert_eq!(s.changes.len(), 1, "{:?}", s.error);
                    assert!(s.row_editor.is_none());
                    let history = s.shared.borrow().state.history.len();
                    s.run_query_for_editing(cx);
                    assert!(!s.busy);
                    assert_eq!(
                        s.shared.borrow().state.history.len(),
                        history,
                        "Staged batch allowed arbitrary rerun"
                    );
                    s.error = None;
                    s.focus_query_tab(other, cx);
                    s.capture_recovery();
                    assert!(s.recovery.error.is_none(), "{:?}", s.recovery.error);
                    assert_eq!(
                        s.recovery_batches()[0].phase,
                        crate::recovery_ui::RecoveryPhase::Stale,
                        "Execution proof must not degrade to replayable table recovery"
                    );
                    s.review_pending(cx);
                    assert_eq!(s.tabs[s.active_tab].id, original);
                    assert!(s.query_edit.review);
                })
            })
            .unwrap();
            assert_eq!(
                runtime
                    .handle()
                    .block_on(db.query(&format!("SELECT name FROM {q}.items"), 1))
                    .unwrap()[0]
                    .rows[0][0]
                    .display(),
                "old"
            );
            // Restoring the original through the form removes the pending update.
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.query_edit.review = false;
                    s.select_grid_cell(0, 1, false, cx);
                    s.edit_row(false, w, cx);
                    s.row_editor
                        .as_ref()
                        .unwrap()
                        .fields
                        .iter()
                        .find(|f| f.name == "name")
                        .unwrap()
                        .value
                        .update(cx, |e, cx| e.set_value("old", w, cx));
                    s.stage_editor(cx);
                    assert!(s.changes.is_empty(), "{:?}", s.error);
                    assert!(s.query_edit.batch.is_none());
                    assert!(s.row_editor.is_none());
                    s.capture_recovery();
                    s.tabs[s.active_tab].editor.update(cx, |e, cx| {
                        e.set_value(
                            format!(
                                "SELECT a.id,a.name FROM {q}.items a JOIN {q}.items b ON a.id=b.id"
                            ),
                            w,
                            cx,
                        )
                    });
                    s.run_query(cx);
                })
            })
            .unwrap();
            settle(cx, w, &s);
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.query_plan().is_none());
                assert!(s.query_edit_notice().starts_with("Read-only · "));
                assert!(
                    s.query_edit_notice().contains("Editing not requested"),
                    "Ordinary Run must not probe unsupported SQL for editing"
                );
            });
            let mut visual = VisualTestContext::from_window(w, cx);
            assert!(visual.debug_bounds("query-edit-notice").is_some());
            cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
        }));
        runtime
            .handle()
            .block_on(crate::pass5_tests::cleanup(&db, &schema))
            .unwrap();
        if let Err(e) = run {
            std::panic::resume_unwind(e);
        }
    }
}
