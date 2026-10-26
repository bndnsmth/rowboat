use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext};
use rowboat_core::{Engine, QueryEvent, QueryStreamOptions, sql::quote_ident};
use std::time::Duration;

#[tokio::test]
#[ignore = "Explicit disposable MySQL13309; unique pass11_gui namespace"]
async fn pass11_live_column_names_reduce_server_statement_count() -> anyhow::Result<()> {
    let (db, _, schema) = crate::pass5_tests::fixture_prefix(Engine::MySql, "pass11_gui").await?;
    let result=async {
        let q=quote_ident(Engine::MySql,&schema);
        db.query(&format!("CREATE TABLE {q}.items(id INT PRIMARY KEY,name TEXT)"),0).await?;
        let table=crate::pass5_tests::table(&schema,"items");
        async fn count(db:&rowboat_core::Database)->anyhow::Result<u64>{Ok(db.query("SHOW SESSION STATUS LIKE 'Questions'",1).await?[0].rows[0][1].display().parse()?)}
        let before=count(&db).await?;
        let structure=db.structure(&table).await?;
        let after_structure=count(&db).await?;
        let columns=crate::completion_ui::column_names(&db,&table).await?;
        let after_names=count(&db).await?;
        assert_eq!(columns,structure.columns.into_iter().map(|c|c.name).collect::<Vec<_>>());
        let full=after_structure-before;let names=after_names-after_structure;
        assert!(full>=names+3,"Full structure={full} server statements vs names={names}");
        eprintln!("Pass11 MySQL column metadata: full structure={full}, names-only={names} server Questions deltas (each includes one measurement statement)");
        anyhow::Ok(())
    }.await;
    crate::pass5_tests::cleanup(&db, &schema).await?;
    result
}

fn settle(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<crate::app::Workbench>,
    menu: bool,
) {
    for _ in 0..1200 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let ready = cx
            .update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.busy
                        && (!menu
                            || s.tabs[s.active_tab]
                                .editor
                                .read(cx)
                                .completion_menu_state()
                                .open)
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
    panic!("Pass11 GUI operation timed out");
}

#[gpui_kit::test]
#[ignore = "Explicit disposable PG15435/MySQL13309; unique pass11_gui namespaces"]
fn pass11_live_run_stops_queued_completion_after_one_bounded_read(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture_prefix(engine, "pass11_gui"))
            .unwrap();
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let q = quote_ident(engine, &schema);
            let snapshot=runtime.handle().block_on(async{
                db.query(&format!("CREATE TABLE {q}.items(id INT PRIMARY KEY,name TEXT); CREATE TABLE {q}.second(id INT); CREATE TABLE {q}.third(id INT)"),0).await.unwrap();
                db.query(&if engine==Engine::MySql{format!("USE {q}")}else{format!("SET search_path TO {q}")},0).await.unwrap();db.namespace_catalog(None).await.unwrap()
            });
            let (w, s) = crate::workbench_tests::workspace(cx, 113, runtime.handle());
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile.clone());
                    s.endpoint = Some(profile.clone());
                    s.database = Some(db.clone());
                    s.session_ready = true;
                    s.tabs[0].connection_id = Some(profile.id.clone());
                    s.accept_namespace(snapshot, None, cx);
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT * FROM ", w, cx);
                        e.set_selected_range(14..14, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            cx.simulate_input(w, "items");
            settle(cx, w, &s, true);
            let probe = cx.read(|cx| s.read(cx).advanced.inline.metadata_probe());
            let before = probe().unwrap();
            let mut blocking=runtime.handle().block_on(db.query_stream("SELECT 1 AS v UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6",QueryStreamOptions{batch_rows:1,channel_capacity:1,..Default::default()})).unwrap();
            runtime.handle().block_on(async {
                loop {
                    if matches!(blocking.recv().await, Some(QueryEvent::Schema { .. })) {
                        break;
                    }
                }
            });
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.dismiss_completions(cx);
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT x. FROM items x, second y, third z", w, cx);
                        e.set_selected_range(9..9, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            cx.simulate_input(w, "n");
            for _ in 0..200 {
                cx.run_until_parked();
                cx.executor().advance_clock(Duration::from_millis(20));
                cx.run_until_parked();
                if probe().is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                probe().is_none(),
                "Metadata was not queued on the occupied session"
            );
            cx.read(|cx| assert!(!s.read(cx).busy, "Background completion froze UI"));
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT 1 AS pass11_result", w, cx);
                        e.focus(w, cx);
                    })
                })
            })
            .unwrap();
            cx.simulate_keystrokes(w, "ctrl-enter");
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let blocker = runtime.handle().spawn(async move {
                while blocking.recv().await.is_some() {}
                blocking.close().await.unwrap();
            });
            settle(cx, w, &s, false);
            runtime.handle().block_on(blocker).unwrap();
            for _ in 0..100 {
                if probe().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(
                probe(),
                Some(before + 1),
                "Cancelled completion continued warming other tables"
            );
            cx.read(|cx| {
                let s = s.read(cx);
                assert!(s.error.is_none(), "{:?}", s.error);
                assert_eq!(s.tabs[0].results[0].rows[0][0].display(), "1");
                assert_eq!(s.shared.borrow().state.history.len(), 1);
                assert!(!s.tabs[0].editor.read(cx).completion_menu_state().open);
            });
            eprintln!(
                "Pass11 {engine:?}: occupied-session completion cancelled at Run; one already-waiting names read drained, zero subsequent table reads, one user execution"
            );
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
            crate::pass5_tests::cleanup(&db, &schema).await.unwrap();
        });
        if let Err(e) = run {
            std::panic::resume_unwind(e);
        }
    }
}

#[gpui_kit::test]
#[ignore = "Explicit disposable PG15435/MySQL13309; unique pass11_gui namespaces"]
fn pass11_live_queue_timing_raw_result_proof_and_relation_metadata_budget(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, schema) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture_prefix(engine, "pass11_gui"))
            .unwrap();
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let q = quote_ident(engine, &schema);
            let snapshot=runtime.handle().block_on(async {
                db.query(&format!("CREATE TABLE {q}.items(id INT PRIMARY KEY,name TEXT); INSERT INTO {q}.items VALUES(1,'one')"),0).await.unwrap();
                db.query(&if engine==Engine::MySql {format!("USE {q}")}else{format!("SET search_path TO {q}")},0).await.unwrap();
                db.namespace_catalog(None).await.unwrap()
            });
            let (w, s) = crate::workbench_tests::workspace(cx, 111, runtime.handle());
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile.clone());
                    s.endpoint = Some(profile.clone());
                    s.database = Some(db.clone());
                    s.session_ready = true;
                    s.tabs[0].connection_id = Some(profile.id.clone());
                    s.accept_namespace(snapshot, None, cx);
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT * FROM ", w, cx);
                        e.set_selected_range(14..14, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            cx.simulate_input(w, "items");
            settle(cx, w, &s, true);
            let reads = cx.read(|cx| s.read(cx).advanced.inline.metadata_reads());
            assert_eq!(
                reads,
                if engine == Engine::MySql { 0 } else { 2 },
                "FROM completion fetched unnecessary columns/structure or repeated verified MySQL namespace"
            );
            cx.simulate_input(w, " ");
            cx.simulate_keystrokes(w, "ctrl-space");
            settle(cx, w, &s, true);
            assert_eq!(
                cx.read(|cx| s.read(cx).advanced.inline.metadata_reads()),
                reads,
                "Warm relation browsing queried again"
            );
            cx.simulate_keystrokes(w, "escape");

            // First-result delivery proves the blocker owns the same physical
            // session before clicking Run. This is not a second query connection.
            let mut blocking=runtime.handle().block_on(db.query_stream("SELECT 1 AS v UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6",QueryStreamOptions{batch_rows:1,channel_capacity:1,..Default::default()})).unwrap();
            runtime.handle().block_on(async {
                loop {
                    if matches!(blocking.recv().await, Some(QueryEvent::Schema { .. })) {
                        break;
                    }
                }
            });
            let blocker = runtime.handle().spawn(async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                while blocking.recv().await.is_some() {}
                blocking.close().await.unwrap();
            });
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(format!("SELECT id AS key_alias,name FROM {q}.items"), w, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            cx.simulate_keystrokes(w, "ctrl-enter");
            cx.read(|cx| assert!(s.read(cx).busy));
            // Actual drawing remains responsive while the backend queues.
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            settle(cx, w, &s, false);
            runtime.handle().block_on(blocker).unwrap();
            cx.read(|cx|{
                let s=s.read(cx);assert!(s.error.is_none(),"{:?}",s.error);
                let timing=s.tabs[0].query_progress.as_ref().unwrap().timing.as_ref().unwrap();let core=timing.core.unwrap();
                assert!(core.queue_wait_ms>=250,"Queue wait lost: {timing:?}");
                assert!(timing.total_ms.unwrap()>=core.total_ms);assert!(timing.context_ms.is_some());
                assert_eq!(timing.result_fetch_ms,s.tabs[0].results[0].elapsed_ms);
                assert!(s.query_plan().is_none(),"Ordinary Run must not capture editing proof");
                assert!(!s.query_entry().unwrap().capture_requested);
                assert_eq!(core.source_capture_ms,0,"Ordinary Run performed editing preflight");
                assert!(s.status.contains("ms total")&&s.status.contains("SQL/fetch"));
                assert_eq!(s.shared.borrow().state.history.len(),1,"Timing replayed SQL");
                eprintln!("Pass11 {engine:?}: queue={} capture={} execution={} cleanup={} state={} context={} ready={}ms",core.queue_wait_ms,core.source_capture_ms,core.execution_ms,core.cleanup_ms,timing.state_check_ms,timing.context_ms.unwrap(),timing.total_ms.unwrap());
            });
            let mut visual = gpui_kit::VisualTestContext::from_window(w, cx);
            let bounds = visual.debug_bounds("query-timing-detail").unwrap();
            visual.simulate_event(gpui_kit::MouseMoveEvent {
                position: bounds.center(),
                pressed_button: None,
                modifiers: Default::default(),
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
                .unwrap();
            let mut visual = gpui_kit::VisualTestContext::from_window(w, cx);
            assert!(
                visual.debug_bounds("omarchy-tooltip-surface").is_some(),
                "Timing explanation is not reachable"
            );
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
            crate::pass5_tests::cleanup(&db, &schema).await.unwrap();
        });
        if let Err(e) = run {
            std::panic::resume_unwind(e);
        }
    }
}

#[gpui_kit::test]
#[ignore = "Explicit disposable PG15435/MySQL13309; unique pass11_gui namespaces"]
fn pass11_live_completion_verified_context_after_sql_switch(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, profile, a) = runtime
            .handle()
            .block_on(crate::pass5_tests::fixture_prefix(engine, "pass11_gui"))
            .unwrap();
        let b = format!("{a}_b");
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (qa, qb) = (quote_ident(engine, &a), quote_ident(engine, &b));
            let snapshot=runtime.handle().block_on(async{
                db.query(&format!("CREATE SCHEMA {qb}; CREATE TABLE {qa}.shadow(id INT PRIMARY KEY,old_name TEXT); CREATE TABLE {qb}.shadow(id INT PRIMARY KEY,fresh_name TEXT)"),0).await.unwrap();
                db.query(&if engine==Engine::MySql{format!("USE {qa}")}else{format!("SET search_path TO {qa}")},0).await.unwrap();db.namespace_catalog(None).await.unwrap()
            });
            let (w, s) = crate::workbench_tests::workspace(cx, 112, runtime.handle());
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.owner = Some(profile.id.clone());
                    s.profile = Some(profile.clone());
                    s.endpoint = Some(profile.clone());
                    s.database = Some(db.clone());
                    s.session_ready = true;
                    s.tabs[0].connection_id = Some(profile.id.clone());
                    s.accept_namespace(snapshot, None, cx);
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT x. FROM shadow x", w, cx);
                        e.set_selected_range(9..9, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            cx.simulate_input(w, "o");
            settle(cx, w, &s, true);
            cx.read(|cx| {
                assert!(
                    s.read(cx).tabs[0]
                        .editor
                        .read(cx)
                        .completion_menu_state()
                        .items
                        .iter()
                        .any(|i| i.label == "old_name")
                )
            });
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(
                            if engine == Engine::MySql {
                                format!("USE {qb}")
                            } else {
                                format!("SET search_path TO {qb}")
                            },
                            w,
                            cx,
                        )
                    });
                    s.run_query(cx);
                })
            })
            .unwrap();
            settle(cx, w, &s, false);
            cx.read(|cx| assert!(s.read(cx).error.is_none(), "{:?}", s.read(cx).error));
            cx.update_window(w, |_, w, cx| {
                s.update(cx, |s, cx| {
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value("SELECT x. FROM shadow x", w, cx);
                        e.set_selected_range(9..9, cx);
                        e.focus(w, cx);
                    });
                })
            })
            .unwrap();
            cx.simulate_input(w, "f");
            settle(cx, w, &s, true);
            cx.read(|cx| {
                let s = s.read(cx);
                let items = &s.tabs[0].editor.read(cx).completion_menu_state().items;
                assert!(items.iter().any(|i| i.label == "fresh_name"), "{items:?}");
                assert!(!items.iter().any(|i| i.label == "old_name"));
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
        if let Err(e) = run {
            std::panic::resume_unwind(e);
        }
    }
}
