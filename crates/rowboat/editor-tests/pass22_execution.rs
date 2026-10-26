//! Actual scoped SQL execution through GPUI, disposable SQLite and retained tabs.
use crate::app::{RunAll, RunCurrent, RunQuery, RunSelection, Workbench};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode};
use std::time::Duration;

fn setup(
    cx: &mut TestAppContext,
    runtime: tokio::runtime::Handle,
    path: &std::path::Path,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let profile = ConnectionProfile {
        id: "pass22-disposable".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.into(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime.block_on(Database::connect(&profile, "")).unwrap();
    let (w, _, tabs) = crate::pass14_tools::workspace(cx, runtime, vec![profile.clone()]);
    let s = tabs[0].clone();
    cx.update(|cx| {
        s.update(cx, |s, _| {
            s.database = Some(db);
            s.profile = Some(profile);
            s.loaded = true;
        })
    });
    (w, s)
}
fn draw(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn set_sql(
    cx: &mut TestAppContext,
    w: AnyWindowHandle,
    s: &Entity<Workbench>,
    sql: &str,
    selection: std::ops::Range<usize>,
) {
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[s.active_tab].editor.update(cx, |e, cx| {
                e.set_value(sql, w, cx);
                e.set_selected_range(selection, cx);
                e.focus(w, cx);
            });
            cx.notify();
        })
    })
    .unwrap();
    draw(cx, w);
}
fn dispatch(cx: &mut TestAppContext, w: AnyWindowHandle, action: impl gpui_kit::Action) {
    cx.update_window(w, |_, w, cx| w.dispatch_action(Box::new(action), cx))
        .unwrap();
    cx.run_until_parked();
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
            draw(cx, w);
            return;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    panic!("Pass22 SQL did not finish");
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, id: &'static str) {
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Missing {id}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}

#[gpui_kit::test]
fn pass22_actual_current_selection_all_and_legacy_scope(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (w, s) = setup(cx, runtime.handle(), &temp.path().join("scopes.sqlite"));
    let sql = "SELECT 11 AS a;\nSELECT 'é;🙂' AS b;\nSELECT 33 AS c;";
    let second = sql.find("SELECT 'é").unwrap();
    let end = sql.find("\nSELECT 33").unwrap();
    set_sql(cx, w, &s, sql, second..second);
    cx.simulate_window_resize(w, size(px(960.), px(640.)));
    draw(cx, w);
    assert!(
        VisualTestContext::from_window(w, cx)
            .debug_bounds("execution-scope")
            .is_some()
    );
    click(cx, w, "run-current");
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        let t = &s.tabs[s.active_tab];
        assert_eq!(t.results.len(), 1);
        assert_eq!(t.results[0].rows[0][0], Cell::Text("é;🙂".into()));
        let source = s.execution_source(t.id).unwrap();
        assert_eq!(source.source_range, second..end);
        assert_eq!(source.submitted_sql, &sql[second..end]);
        assert_eq!(s.results.query_origins[&t.id].source, source.submitted_sql);
    });
    set_sql(cx, w, &s, sql, 0..0);
    dispatch(cx, w, RunSelection);
    settle(cx, w, &s);
    cx.read(|cx| {
        assert!(s.read(cx).error.as_ref().unwrap().contains("Select SQL"));
        assert_eq!(s.read(cx).tabs[0].results.len(), 1);
    });
    set_sql(cx, w, &s, sql, second..end);
    dispatch(cx, w, RunAll);
    settle(cx, w, &s);
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].results.len(), 3));
    set_sql(cx, w, &s, sql, second..end);
    dispatch(cx, w, RunQuery);
    settle(cx, w, &s);
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].results.len(), 1));
    set_sql(cx, w, &s, sql, 0..0);
    dispatch(cx, w, RunQuery);
    settle(cx, w, &s);
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].results.len(), 3));
}

#[gpui_kit::test]
fn pass22_server_error_maps_selection_unicode_and_refuses_changed_or_parameterized_source(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (w, s) = setup(cx, runtime.handle(), &temp.path().join("error.sqlite"));
    let sql = "SELECT 0;\n-- prefix é\nSELECT 'é🙂'; SELECT 2 + FROM nowhere;";
    let start = sql.find("-- prefix").unwrap();
    let position = sql.find("FROM").unwrap();
    set_sql(cx, w, &s, sql, start..sql.len());
    dispatch(cx, w, RunSelection);
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        let tab = &s.tabs[0];
        assert!(tab.error.as_ref().unwrap().contains("syntax"));
        assert_eq!(tab.results.len(), 1);
        assert_eq!(
            s.execution_source(tab.id).unwrap().error_offset,
            Some(position)
        );
    });
    click(cx, w, "goto-query-error");
    draw(cx, w);
    cx.read(|cx| {
        assert_eq!(
            s.read(cx).tabs[0].editor.read(cx).selected_range(),
            position..position + 1
        )
    });
    set_sql(cx, w, &s, "SELECT 'changed'", 0..0);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| s.reveal_execution_error(w, cx))
    })
    .unwrap();
    cx.read(|cx| assert_eq!(s.read(cx).tabs[0].editor.read(cx).selected_range(), 0..0));
    let sql = "SELECT :value; SELECT 2 + FROM nowhere;";
    set_sql(cx, w, &s, sql, 0..0);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            s.tabs[0]
                .parameters
                .update(cx, |e, cx| e.set_value(r#"{"value":"é🙂"}"#, w, cx))
        })
    })
    .unwrap();
    dispatch(cx, w, RunAll);
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        let source = s.execution_source(s.tabs[0].id).unwrap();
        assert!(!source.position_maps_directly && source.error_offset.is_none());
    });
}

#[gpui_kit::test]
fn pass22_split_action_routes_to_focused_editor_and_local_boundary_feedback(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (w, s) = setup(cx, runtime.handle(), &temp.path().join("split.sqlite"));
    set_sql(cx, w, &s, "SELECT 11; SELECT 12;", 0..0);
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| s.toggle_editor_split(w, cx))
    })
    .unwrap();
    let right = cx.read(|cx| s.read(cx).panes.tabs[1].unwrap());
    cx.update_window(w, |_, w, cx| {
        s.update(cx, |s, cx| {
            let tab = s.tabs.iter().find(|t| t.id == right).unwrap();
            tab.editor.update(cx, |e, cx| {
                e.set_value("SELECT 21; SELECT 22;", w, cx);
                e.set_selected_range(12..12, cx);
                e.focus(w, cx);
            });
        })
    })
    .unwrap();
    draw(cx, w);
    dispatch(cx, w, RunCurrent);
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        let tab = &s.tabs[s.active_tab];
        assert_eq!(tab.id, right);
        assert_eq!(tab.results[0].rows[0][0], Cell::Text("22".into()));
        assert!(
            s.tabs
                .iter()
                .find(|t| t.id != right)
                .unwrap()
                .results
                .is_empty()
        );
    });
    set_sql(cx, w, &s, "SELECT 'unterminated", 0..0);
    dispatch(cx, w, RunCurrent);
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(s.error.as_ref().unwrap().contains("Unterminated"));
        assert_eq!(s.tabs[s.active_tab].results.len(), 1);
    });
    click(cx, w, "reveal-current-sql");
    draw(cx, w);
    cx.read(|cx| {
        let s = s.read(cx);
        assert_eq!(s.tabs[s.active_tab].editor.read(cx).cursor(), 7);
    });
}

#[gpui_kit::test]
fn pass22_budget_admission_cancels_without_claiming_complete(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (w, s) = setup(cx, runtime.handle(), &temp.path().join("budget.sqlite"));
    set_sql(cx, w, &s, "SELECT 1 AS value;", 0..0);
    cx.update(|cx| {
        s.update(cx, |s, cx| {
            s.state.preferences.query_result_limit_mib = 16;
            s.shared
                .borrow_mut()
                .state
                .preferences
                .query_result_limit_mib = 16;
            let options = s.query_stream_options(false).unwrap();
            assert!(options.byte_limit.unwrap() <= 8 * 1024 * 1024);
            s.run_query_scope(rowboat_core::execution::RunScope::All, false, cx);
            // Exhaust the captured admission reservation before any frame arrives;
            // core's logical-row cap cannot cover GUI schema/Vec allocation itself.
            s.results.stream_remaining = Some((s.tabs[0].id, 0));
        })
    });
    settle(cx, w, &s);
    cx.read(|cx| {
        let s = s.read(cx);
        assert!(
            s.tabs[0]
                .error
                .as_ref()
                .unwrap()
                .contains("Shared result memory budget")
        );
        assert!(s.tabs[0].results.is_empty());
        assert!(s.query_stream.is_none() && !s.busy);
    });
}
