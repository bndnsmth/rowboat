//! Mandatory terminal context synchronization across the real coordinator close flow.
use crate::{
    app::Startup,
    session::{SessionState, TabBuffer},
    workspace_shell::WorkspaceShell,
    workspace_storage::{Event, Writers},
};
use gpui_kit::{AppContext, TestAppContext};
use rowboat_core::{Engine, sql::quote_ident};

fn keep_working(cx: &mut TestAppContext, w: gpui_kit::AnyWindowHandle) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
    let mut visual = gpui_kit::VisualTestContext::from_window(w, cx);
    let button = visual
        .debug_bounds("keep-workspaces")
        .expect("real close modal Keep working button");
    visual.simulate_click(button.center(), gpui_kit::Modifiers::default());
}

#[gpui_kit::test]
#[ignore = "Explicit disposable MySQL13309; injected writers, no user storage"]
fn pass8_terminal_namespace_sync_survives_close_and_keep_working(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (db, profile, a) = runtime
        .handle()
        .block_on(crate::pass5_tests::fixture(Engine::MySql))
        .unwrap();
    let b = format!("{a}_close");
    runtime
        .handle()
        .block_on(db.query(
            &format!("CREATE DATABASE {}", quote_ident(Engine::MySql, &b)),
            0,
        ))
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (state, _state_rx) = tokio::sync::mpsc::unbounded_channel();
        let (recovery, _recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, events) = std::sync::mpsc::channel();
        tx.send(Event::StateLoaded(Ok(Box::new(
            rowboat_services::AppState {
                connections: vec![profile.clone()],
                ..Default::default()
            },
        ))))
        .unwrap();
        tx.send(Event::RecoveryLoaded(Ok(SessionState {
            version: 4,
            active_tab: 1,
            tabs: vec![TabBuffer {
                id: 1,
                connection_id: Some(profile.id.clone()),
                title: "Close synchronization".into(),
                sql: "SELECT 1".into(),
            }],
            ..Default::default()
        })))
        .unwrap();
        let (w, shell) = cx.update(|cx| {
            crate::test_theme::init(cx);
            crate::keyboard_ui::bind(cx);
            gpui_kit::open_window(Default::default(), cx, |w, cx| {
                cx.new(|cx| {
                    WorkspaceShell::with_writers(
                        runtime.handle(),
                        Startup::default(),
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
        let source = cx
            .update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
            .unwrap()
            .remove(0);
        for keep_early in [false, true] {
            let snapshot = runtime.handle().block_on(async {
                db.select_database(&a).await.unwrap();
                db.namespace_catalog(None).await.unwrap()
            });
            cx.update_window(w, |_, w, cx| {
                source.update(cx, |s, cx| {
                    s.database = Some(db.clone());
                    s.endpoint = Some(profile.clone());
                    s.accept_namespace(snapshot, None, cx);
                    s.table = Some(crate::pass5_tests::table(&a, "never_executed"));
                    s.tabs[0].editor.update(cx, |e, cx| {
                        e.set_value(
                            format!(
                                "USE {}; SELECT * FROM pass8_close_missing_table",
                                quote_ident(Engine::MySql, &b)
                            ),
                            w,
                            cx,
                        )
                    });
                    s.run_query(cx);
                    assert!(s.busy);
                });
                shell.update(cx, |s, cx| s.test_close_dialog(w, cx));
            })
            .unwrap();
            let namespace_request = cx.read(|cx| {
                let s = source.read(cx);
                assert!(s.query_stream.is_some());
                s.namespace.request
            });
            // A streamed QueryEvent::Error precedes cleanup and may arrive in a
            // separate poll. Stop on Delivery::Finished instead: it removes the
            // stream and queues Event::Query, which this poll consumes to start
            // namespace synchronization. The metadata reply cannot enter that
            // already-collected event batch, even if its worker finishes at once.
            let mut terminal = false;
            for _ in 0..500 {
                terminal = cx
                    .update_window(w, |_, w, cx| {
                        source.update(cx, |s, cx| {
                            s.poll(w, cx);
                            s.query_stream.is_none()
                        })
                    })
                    .unwrap();
                if terminal {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                terminal,
                "Query worker did not acknowledge terminal cleanup"
            );
            cx.read(|cx| {
                let s = source.read(cx);
                assert!(s.tabs[0].error.is_some());
                assert_eq!(
                    s.namespace.request,
                    namespace_request.wrapping_add(1),
                    "Terminal cleanup must start exactly one context refresh"
                );
                assert!(s.close_requested);
                assert!(
                    s.busy,
                    "Terminal SQL released busy before required frozen-context refresh"
                );
                assert!(
                    !s.namespace.known,
                    "Old context stayed verified during terminal synchronization"
                );
            });
            if keep_early {
                keep_working(cx, w);
                cx.read(|cx| {
                    let s = source.read(cx);
                    assert!(!s.close_requested);
                    assert!(s.busy);
                    assert!(!s.namespace_switch_allowed());
                });
            }
            let mut settled = false;
            for _ in 0..500 {
                settled = cx
                    .update_window(w, |_, w, cx| {
                        source.update(cx, |s, cx| {
                            s.poll(w, cx);
                            !s.busy
                        })
                    })
                    .unwrap();
                if settled {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(settled, "Closing must not deadlock on its own freeze guard");
            cx.read(|cx| {
                assert!(
                    !source.read(cx).close_state().busy,
                    "Completed metadata must release the close gate"
                )
            });
            if !keep_early {
                keep_working(cx, w);
            }
            cx.update_window(w, |_, w, cx| shell.update(cx, |s, cx| s.test_tick(w, cx)))
                .unwrap();
            cx.read(|cx| {
                let s = source.read(cx);
                assert_eq!(s.namespace_scope(), Some(b.as_str()));
                assert!(s.namespace.known);
                assert!(s.table.is_none());
                assert!(!s.close_requested);
                assert!(s.namespace_switch_allowed());
                assert!(s.namespace.result_context[&1].contains(&a));
            });
            assert_eq!(
                runtime
                    .handle()
                    .block_on(db.query("SELECT DATABASE()", 1))
                    .unwrap()[0]
                    .rows[0][0]
                    .display(),
                b
            );
        }
        cx.update_window(w, |_, w, _| w.remove_window()).unwrap();
    }));
    runtime.handle().block_on(async {
        let _ = db.select_database("rowboat").await;
        for name in [&b, &a] {
            crate::pass5_tests::cleanup(&db, name).await.unwrap();
        }
    });
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
