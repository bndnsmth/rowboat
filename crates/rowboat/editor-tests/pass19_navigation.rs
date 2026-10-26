//! Retained GPUI navigation and local SQL opening; no database or native app access.
use crate::{
    app::{Page, TablePage, Workbench},
    chrome_ui::Command,
    workbench_tests::workspace,
};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{Cell, Column, QueryResult, TableRef};

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, selector: &'static str) {
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing rendered control: {selector}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
    draw(cx, window);
}

fn palette(cx: &mut TestAppContext, window: AnyWindowHandle, title: &str) {
    cx.simulate_keystrokes(window, "ctrl-shift-p");
    cx.simulate_input(window, title);
    cx.simulate_keystrokes(window, "enter");
    draw(cx, window);
}

fn result(value: &str) -> QueryResult {
    QueryResult {
        columns: vec![Column {
            name: "value".into(),
            data_type: "text".into(),
            nullable: false,
            primary_key: true,
            default_value: None,
        }],
        rows: vec![vec![Cell::Text(value.into())]],
        elapsed_ms: 12,
        ..Default::default()
    }
}

fn assert_idle(s: &Workbench) {
    assert!(!s.busy);
    assert!(s.query_stream.is_none());
    assert_eq!(
        s.query_run_invocations, 0,
        "Navigation must never invoke Run"
    );
}

#[gpui_kit::test]
fn pass19_new_and_switched_query_tabs_reveal_the_active_document(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 193, runtime.handle());
    cx.simulate_window_resize(
        window,
        gpui_kit::size(gpui_kit::px(960.), gpui_kit::px(640.)),
    );
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            for _ in 0..12 {
                s.new_query(None, w, cx);
            }
        });
    })
    .unwrap();
    draw(cx, window);
    let assert_visible = |cx: &TestAppContext| {
        cx.read(|cx| {
            let s = entity.read(cx);
            let owner = &s.tabs[s.active_tab].connection_id;
            let index = s.tabs[..s.active_tab]
                .iter()
                .filter(|tab| &tab.connection_id == owner)
                .count();
            let scroll = &s.query_tab_scroll;
            let bounds = scroll
                .bounds_for_item(index)
                .expect("active document has a tab");
            let viewport = scroll.bounds();
            assert!(bounds.left() + scroll.offset().x >= viewport.left());
            assert!(bounds.right() + scroll.offset().x <= viewport.right());
            assert_idle(s);
        });
    };
    assert_visible(cx);
    cx.update(|cx| {
        entity.update(cx, |s, cx| {
            let id = s.tabs[1].id;
            s.focus_query_tab(id, cx);
        });
    });
    draw(cx, window);
    assert_visible(cx);
}

#[gpui_kit::test]
fn pass19_restored_query_tabs_reveal_after_first_narrow_mount(cx: &mut TestAppContext) {
    use crate::session::{TabBuffer, WorkspaceBuffer};
    use gpui_kit::{point, px, size};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 194, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.startup.sql = None;
            let owner = s.owner.clone();
            s.restore_buffers(
                (0..12)
                    .map(|i| TabBuffer {
                        id: 1940 + i,
                        connection_id: owner.clone(),
                        title: if i == 11 {
                            "History query".into()
                        } else {
                            format!("Query {}", i + 1)
                        },
                        sql: format!("SELECT {i}"),
                    })
                    .collect(),
                Some(WorkspaceBuffer {
                    documents: Default::default(),
                    staged: vec![],
                    connection_id: owner,
                    active_tab: 1951,
                    split_tab: None,
                    focused_pane: 0,
                    open: true,
                }),
                w,
                cx,
            );
        });
    })
    .unwrap();
    cx.simulate_window_resize(window, size(px(960.), px(640.)));
    draw(cx, window);
    draw(cx, window);
    let mut visual = VisualTestContext::from_window(window, cx);
    let viewport = visual.debug_bounds("query-tabs-scroll").unwrap();
    let active = visual
        .debug_bounds("active-query-tab")
        .expect("restored active tab is painted");
    assert!(
        active.left() >= viewport.left() && active.right() <= viewport.right(),
        "Restored tab must actually paint inside strip: {active:?} vs {viewport:?}"
    );
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.tabs[s.active_tab].id, 1951);
        assert_eq!(
            s.tabs[s.active_tab].editor.read(cx).value().as_ref(),
            "SELECT 11"
        );
        assert_idle(s);
    });
    // A user may deliberately scroll away from the current document. Idle
    // rendering must not pull the strip back to it.
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.query_tab_scroll.set_offset(point(px(0.), px(0.)))
        })
    });
    draw(cx, window);
    draw(cx, window);
    cx.read(|cx| assert_eq!(entity.read(cx).query_tab_scroll.offset().x, px(0.)));
}

#[gpui_kit::test]
fn pass19_navigation_clicks_and_palette_restore_destinations_and_retained_data(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 190, runtime.handle());
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.tabs[0].results = vec![result("query result")];
        });
    });
    let editor = cx.read(|cx| entity.read(cx).tabs[0].editor.clone());

    click(cx, window, "saved-queries-page");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::History && s.saved_queries_view);
    });
    click(cx, window, "history-page");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::History && !s.saved_queries_view);
    });
    click(cx, window, "saved-queries-page");
    palette(cx, window, "Show query history");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(!s.quick_open);
        assert!(s.page == Page::History && !s.saved_queries_view);
    });

    // Database has a useful fallback before a table has ever been selected.
    click(cx, window, "database-page");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::Query && s.database_destination);
    });
    palette(cx, window, "Show SQL editor");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::Query && !s.database_destination);
    });

    let table = TableRef {
        schema: "test".into(),
        name: "retained_rows".into(),
    };
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.table = Some(table.clone());
            s.table_result = result("table result");
            s.table_page = TablePage::Structure;
        });
    });
    click(cx, window, "saved-queries-page");
    click(cx, window, "database-page");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::Table && s.database_destination);
        assert!(s.table_page == TablePage::Structure);
        assert_eq!(s.table.as_ref(), Some(&table));
        assert_eq!(
            serde_json::to_value(&s.table_result).unwrap(),
            serde_json::to_value(result("table result")).unwrap()
        );
    });
    click(cx, window, "queries-page");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::Query && !s.database_destination);
        assert_eq!(s.tabs[0].editor, editor);
        assert_eq!(editor.read(cx).value().as_ref(), "SELECT 190");
        assert_eq!(s.table.as_ref(), Some(&table));
        assert_eq!(
            serde_json::to_value(&s.tabs[0].results).unwrap(),
            serde_json::to_value(vec![result("query result")]).unwrap()
        );
        assert_idle(s);
    });
}

#[gpui_kit::test]
fn pass19_navigation_open_saved_and_history_never_runs_or_replaces_existing_results(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 191, runtime.handle());
    let (shell_tx, commands) = std::sync::mpsc::channel();
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.shell_tx = shell_tx;
            s.tabs[0].results = vec![result("existing result")];
            s.edit_state(|state| {
                state.snippets.push(rowboat_services::Snippet {
                    id: "pass19-saved".into(),
                    name: "Saved lookup".into(),
                    sql: "SELECT 'saved';".into(),
                });
                state.history.push(rowboat_services::HistoryEntry {
                    id: "pass19-history".into(),
                    connection_id: "original-history-owner".into(),
                    sql: "SELECT 'history';".into(),
                    ..Default::default()
                });
            });
        });
    });
    let history = cx.read(|cx| serde_json::to_value(&entity.read(cx).state.history).unwrap());
    click(cx, window, "saved-queries-page");
    click(cx, window, "saved-query-open-pass19-saved");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::Query);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(
            s.tabs[s.active_tab].editor.read(cx).value().as_ref(),
            "SELECT 'saved';"
        );
        assert!(s.tabs[s.active_tab].results.is_empty());
        assert_idle(s);
    });

    click(cx, window, "history-page");
    click(cx, window, "history-query-open-pass19-history");
    // The real list handler delegates ownership-preserving OpenQuery to the shell.
    // This fixture observes that boundary; it does not substitute a fake shell.
    match commands
        .try_recv()
        .expect("History Open must reach the shell")
    {
        crate::workspace_shell::Command::OpenQuery { owner, sql, title } => {
            assert_eq!(owner.as_deref(), Some("original-history-owner"));
            assert_eq!(sql, "SELECT 'history';");
            assert_eq!(title, "History query");
        }
        _ => panic!("History Open emitted an unexpected shell command"),
    }
    assert!(commands.try_recv().is_err());
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.tabs[0].editor.read(cx).value().as_ref(), "SELECT 191");
        assert_eq!(
            serde_json::to_value(&s.tabs[0].results).unwrap(),
            serde_json::to_value(vec![result("existing result")]).unwrap()
        );
        assert_eq!(serde_json::to_value(&s.state.history).unwrap(), history);
        assert_idle(s);
    });
}

fn assert_navigation_blocked(
    entity: &Entity<Workbench>,
    window: AnyWindowHandle,
    cx: &mut TestAppContext,
) {
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            for command in [
                Command::Database,
                Command::SqlEditor,
                Command::SavedQueries,
                Command::History,
            ] {
                assert!(!s.menu_enabled(command, cx));
                s.choose_menu(command, w, cx);
                assert!(s.page == Page::Query);
                assert!(!s.saved_queries_view && !s.database_destination);
            }
            assert_idle(s);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn pass19_navigation_revalidates_row_draft_modal_and_close_guards(cx: &mut TestAppContext) {
    use rowboat_core::grid::{GridPosition, GridRange};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = workspace(cx, 192, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.grid.draft = Some(crate::grid_ui::CellDraft {
                target: crate::forms::RowTarget {
                    connection_id: "profile-192".into(),
                    session_generation: s.session_generation,
                    table: TableRef {
                        schema: "test".into(),
                        name: "rows".into(),
                    },
                },
                range: GridRange::from_size(GridPosition { row: 0, column: 0 }, 1, 1).unwrap(),
                snapshot: result("original"),
                value: crate::forms::textarea("unsaved cell", w, cx),
                kind: crate::grid_ui::ValueKind::Text,
            });
        });
    })
    .unwrap();
    click(cx, window, "saved-queries-page");
    assert_navigation_blocked(&entity, window, cx);
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            assert_eq!(
                s.grid
                    .draft
                    .as_ref()
                    .unwrap()
                    .value
                    .read(cx)
                    .value()
                    .as_ref(),
                "unsaved cell"
            );
            s.grid.draft = None;
            s.form = Some(crate::forms::ConnectionForm::new(
                Default::default(),
                String::new(),
                w,
                cx,
            ));
        });
    })
    .unwrap();
    draw(cx, window);
    // Exercise dispatch-time validation too: modal occlusion alone cannot guard
    // a stale menu callback, and closing workspaces no longer render navigation.
    assert_navigation_blocked(&entity, window, cx);
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.form = None;
            s.close_requested = true;
        })
    });
    assert_navigation_blocked(&entity, window, cx);
    cx.update(|cx| entity.update(cx, |s, _| s.close_requested = false));
    click(cx, window, "saved-queries-page");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.page == Page::History && s.saved_queries_view);
        assert_eq!(s.tabs[0].editor.read(cx).value().as_ref(), "SELECT 192");
        assert_idle(s);
    });
}
