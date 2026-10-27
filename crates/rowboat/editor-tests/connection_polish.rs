//! Actual form validation, typed feedback and retained grid scrollbars.
use crate::{app::Workbench, workbench_tests::workspace};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Focusable, Modifiers, MouseButton, TestAppContext,
    VisualTestContext, point, px, size,
};
use rowboat_core::{
    Cell, Column, ConnectionProfile, Database, Engine, QueryResult, SqliteFileOptions,
    SqliteOpenMode,
};

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
fn trial(cx: &mut TestAppContext, window: AnyWindowHandle, entity: &Entity<Workbench>) {
    click(cx, window, "test-connection");
    for _ in 0..800 {
        let done = cx
            .update_window(window, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.poll(w, cx);
                    s.form.as_ref().unwrap().test_request.is_none()
                })
            })
            .unwrap();
        if done {
            draw(cx, window);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("Temporary test did not complete");
}

#[gpui_kit::test]
fn polish_validation_is_inline_focused_and_blocks_all_connection_actions(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 801, runtime.handle());
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_connection(None, w, cx);
            let f = s.form.as_mut().unwrap();
            for (key, value) in [("host", ""), ("port", "70000"), ("username", "")] {
                f.fields[key].update(cx, |i, cx| i.set_value(value, w, cx));
            }
        })
    })
    .unwrap();
    for action in ["test-connection", "save-connection", "connect"] {
        click(cx, window, action);
        cx.read(|cx| {
            let s = entity.read(cx);
            let f = s.form.as_ref().unwrap();
            assert!(f.validation_attempted && f.test_request.is_none());
            assert!(
                !s.busy && s.database.is_none() && s.shared.borrow().state.connections.is_empty()
            );
        });
        cx.update_window(window, |_, w, cx| {
            let f = entity.read(cx).form.as_ref().unwrap();
            assert!(f.fields["host"].read(cx).focus_handle(cx).is_focused(w));
        })
        .unwrap();
        let mut visual = VisualTestContext::from_window(window, cx);
        assert!(visual.debug_bounds("connection-error-host").is_some());
        assert!(visual.debug_bounds("connection-error-port").is_some());
    }
    // Hidden advanced fields are revealed and scrolled into view, not merely focused off-screen.
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let f = s.form.as_mut().unwrap();
            for (key, value) in [
                ("host", "localhost"),
                ("port", "5432"),
                ("username", "reader"),
                ("ssh_host", "bastion.example"),
                ("ssh_port", "0"),
                ("ssh_username", "operator"),
            ] {
                f.fields[key].update(cx, |i, cx| i.set_value(value, w, cx));
            }
            f.ssh = true;
            f.advanced = false;
        })
    })
    .unwrap();
    click(cx, window, "save-connection");
    draw(cx, window);
    cx.update_window(window, |_, w, cx| {
        let f = entity.read(cx).form.as_ref().unwrap();
        assert!(f.advanced && f.scroll.offset().y < px(0.));
        assert!(f.fields["ssh_port"].read(cx).focus_handle(cx).is_focused(w));
    })
    .unwrap();
}

#[gpui_kit::test]
fn polish_test_feedback_is_visible_real_and_invalidated_by_editing(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: directory.path().join("actual.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime
        .handle()
        .block_on(Database::connect(&profile, ""))
        .unwrap();
    let (window, entity) = workspace(cx, 802, runtime.handle());
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| s.edit_connection(Some(profile), w, cx))
    })
    .unwrap();
    trial(cx, window, &entity);
    let mut visual = VisualTestContext::from_window(window, cx);
    let frame = visual.debug_bounds("connection-form-frame").unwrap();
    let feedback = visual
        .debug_bounds("connection-test-success")
        .expect("Missing prominent success feedback");
    assert!(feedback.size.height >= px(18.) && feedback.bottom() <= frame.bottom());
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.form.as_ref().unwrap().test_feedback(cx), Some(true));
        assert!(s.database.is_none() && s.shared.borrow().state.connections.is_empty());
    });
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.form.as_ref().unwrap().fields["file_path"].update(cx, |i, cx| i.focus(w, cx));
        })
    })
    .unwrap();
    cx.simulate_keystrokes(window, "ctrl-a");
    let missing = directory.path().join("missing.sqlite");
    cx.simulate_input(window, missing.to_str().unwrap());
    draw(cx, window);
    cx.read(|cx| {
        let f = entity.read(cx).form.as_ref().unwrap();
        assert!(f.test_feedback(cx).is_none() && f.test_stale);
    });
    trial(cx, window, &entity);
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(visual.debug_bounds("connection-test-failure").is_some());
    assert!(visual.debug_bounds("connection-test-success").is_none());
    assert!(!missing.exists());
    assert!(runtime.handle().block_on(db.query("SELECT 1", 1)).is_ok());
}

#[gpui_kit::test]
fn polish_connection_context_distinguishes_sql_schema_explorer_and_original_host(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: directory.path().join("context.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime
        .handle()
        .block_on(Database::connect(&profile, ""))
        .unwrap();
    let mut snapshot = runtime
        .handle()
        .block_on(db.namespace_catalog(None))
        .unwrap();
    snapshot.database = "billing".into();
    snapshot.current_schema = Some("public".into());
    let (window, entity) = workspace(cx, 803, runtime.handle());
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.database = Some(db);
            s.profile = Some(ConnectionProfile {
                host: "db.production.example".into(),
                database: "saved-default".into(),
                read_only: true,
                ..Default::default()
            });
            s.endpoint = Some(ConnectionProfile {
                host: "127.0.0.1".into(),
                ..Default::default()
            });
            s.namespace.snapshot = Some(snapshot);
            s.namespace.selected = Some("analysis".into());
            s.namespace.known = true;
        })
    });
    draw(cx, window);
    cx.read(|cx| {
        let target = entity.read(cx).connection_context_label();
        assert!(target.contains("db.production.example:5432 → billing"));
        assert!(target.contains("SQL schema: public") && target.contains("explorer: analysis"));
        assert!(!target.contains("127.0.0.1") && !target.contains("saved-default"));
    });
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(visual.debug_bounds("active-connection-context").is_some());
    assert!(visual.debug_bounds("connection-read-only").is_some());
}

#[gpui_kit::test]
fn polish_results_scrollbars_click_drag_both_axes_and_remain_at_visible_edge(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 804, runtime.handle());
    cx.simulate_window_resize(window, size(px(960.), px(740.)));
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.tabs[0].results = vec![QueryResult {
                columns: (0..12)
                    .map(|i| Column {
                        name: format!("field_{i}"),
                        data_type: "text".into(),
                        nullable: true,
                        primary_key: false,
                        default_value: None,
                    })
                    .collect(),
                rows: (0..400)
                    .map(|r| (0..12).map(|c| Cell::Text(format!("{r}:{c}"))).collect())
                    .collect(),
                ..Default::default()
            }];
        })
    });
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let vertical = visual.debug_bounds("results-scrollbar-y").unwrap();
    let horizontal = visual.debug_bounds("results-scrollbar-x").unwrap();
    assert!(vertical.size.height > px(30.) && horizontal.size.width > px(200.));
    assert!(vertical.right() <= px(960.) && horizontal.right() <= vertical.left());
    // Grab the first visible thumb, not an invisible off-screen list edge.
    let start = point(vertical.center().x, vertical.top() + px(8.));
    let end = point(start.x, vertical.bottom() - px(8.));
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    visual.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    draw(cx, window);
    cx.read(|cx| {
        assert!(
            entity
                .read(cx)
                .grid_scroll
                .0
                .borrow()
                .base_handle
                .offset()
                .y
                < px(0.)
        )
    });
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_click(
        point(horizontal.right() - px(8.), horizontal.center().y),
        Modifiers::default(),
    );
    draw(cx, window);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.grid_x_scroll.offset().x < px(0.));
        assert_eq!(s.tabs[0].results[0].rows.len(), 400);
        assert_eq!(s.query_run_invocations, 0);
    });
    let mut visual = VisualTestContext::from_window(window, cx);
    assert_eq!(
        visual.debug_bounds("results-scrollbar-y").unwrap(),
        vertical
    );
    // Pinned results use their own scroll handles, with the same visible gutters.
    cx.update(|cx| {
        entity.update(cx, |s, cx| {
            s.profile = Some(ConnectionProfile {
                database: "fixture".into(),
                ..Default::default()
            });
            s.capture_query_result_origin(s.tabs[0].id, "SELECT test fixture");
            s.pin_current_result(cx);
        })
    });
    draw(cx, window);
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            let scroll = &s.results.viewport_scroll;
            assert!(scroll.max_offset().y > px(0.));
            scroll.set_offset(point(px(0.), -scroll.max_offset().y));
        })
    });
    draw(cx, window);
    click(cx, window, "snapshot-scrollbar-x");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.results.horizontal_scroll.offset().x < px(0.));
        assert_eq!(s.query_run_invocations, 0);
    });
}

#[gpui_kit::test]
fn polish_results_that_fit_do_not_scroll_or_invent_overflow(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 805, runtime.handle());
    cx.simulate_window_resize(window, size(px(1100.), px(900.)));
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.tabs[0].results = vec![QueryResult {
                columns: vec![Column {
                    name: "only_column".into(),
                    data_type: "text".into(),
                    nullable: true,
                    primary_key: false,
                    default_value: None,
                }],
                rows: vec![vec![Cell::Text("only row".into())]],
                ..Default::default()
            }];
        })
    });
    draw(cx, window);
    click(cx, window, "results-scrollbar-y");
    click(cx, window, "results-scrollbar-x");
    cx.read(|cx| {
        let s = entity.read(cx);
        let vertical = &s.grid_scroll.0.borrow().base_handle;
        assert_eq!(vertical.max_offset().y, px(0.));
        assert_eq!(vertical.offset().y, px(0.));
        assert_eq!(s.grid_x_scroll.max_offset().x, px(0.));
        assert_eq!(s.grid_x_scroll.offset().x, px(0.));
    });
}
