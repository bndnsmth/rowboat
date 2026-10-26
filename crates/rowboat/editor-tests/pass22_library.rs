//! Actual GPUI workspace ownership and recovery regressions for the SQL library.
use gpui_kit::{AppContext, TestAppContext};
use rowboat_services::{
    Snippet,
    query_library::{self as library, QueryMetadata, SavedQuery, SqlDocument},
};

fn saved() -> SavedQuery {
    SavedQuery {
        snippet: Snippet {
            id: "stable".into(),
            name: "Original".into(),
            sql: "SELECT 'original'".into(),
        },
        metadata: QueryMetadata {
            folder: "Reports".into(),
            connection_id: Some("different-connection".into()),
        },
    }
}

#[gpui_kit::test]
fn item2_pending_file_save_blocks_direct_tab_close_and_closed_bindings_are_removed(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 229, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let id = s.tabs[s.active_tab].id;
            s.library
                .documents
                .insert(id, SqlDocument::Saved { baseline: saved() });
            s.library.file_pending = true;
            s.close_query(w, cx);
            s.finish_close_query(id, w, cx);
            assert!(s.tabs.iter().any(|tab| tab.id == id));
            assert!(s.library.documents.contains_key(&id));
            s.library.file_pending = false;
            s.finish_close_query(id, w, cx);
            assert!(!s.tabs.iter().any(|tab| tab.id == id));
            assert!(!s.library.documents.contains_key(&id));
            assert!(!s.tabs.is_empty());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn item2_450_history_and_saved_rows_keep_intrinsic_heights_and_do_not_overlap(
    cx: &mut TestAppContext,
) {
    use gpui_kit::{VisualTestContext, px, size};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 228, runtime.handle());
    cx.simulate_window_resize(window, size(px(1600.), px(1000.)));
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| {
                for i in 0..450 {
                    let sql = format!(
                        "SELECT {i} AS id,\n       'retained text' AS description\nFROM sample;"
                    );
                    state.history.push(rowboat_services::HistoryEntry {
                        id: format!("fixture-{i}"),
                        connection_id: "fixture".into(),
                        sql: sql.clone(),
                        executed_at: "2026-10-26T12:00:00Z".into(),
                        ..Default::default()
                    });
                    state.snippets.push(Snippet {
                        id: format!("fixture-{i}"),
                        name: format!("Saved query {i}"),
                        sql,
                    });
                }
            });
            s.page = crate::app::Page::History;
            s.saved_queries_view = false;
            cx.notify();
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    {
        let mut visual = VisualTestContext::from_window(window, cx);
        let content = visual.debug_bounds("library-content").unwrap();
        let viewport = visual.debug_bounds("library-scroll").unwrap();
        assert!(
            content.size.height >= px(450. * 88.),
            "All retained rows need real scrollable space: {content:?}"
        );
        assert!(viewport.size.height < px(1000.));
        let a = visual
            .debug_bounds("history-library-row-fixture-449")
            .unwrap();
        let b = visual
            .debug_bounds("history-library-row-fixture-448")
            .unwrap();
        let c = visual
            .debug_bounds("history-library-row-fixture-447")
            .unwrap();
        assert!(a.size.height >= px(88.) && b.size.height >= px(88.));
        assert!(
            b.top() >= a.bottom() && c.top() >= b.bottom(),
            "Visible history cards must not overlap: {a:?} {b:?} {c:?}"
        );
        let preview = visual
            .debug_bounds("history-library-preview-fixture-449")
            .unwrap();
        assert!(
            preview.size.height >= px(30.)
                && preview.top() >= a.top()
                && preview.bottom() <= a.bottom(),
            "Multiline SQL needs intrinsic height inside its card: {preview:?} {a:?}"
        );
    }
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.saved_queries_view = true;
            cx.notify();
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let content = visual.debug_bounds("library-content").unwrap();
    let a = visual.debug_bounds("saved-library-row-fixture-0").unwrap();
    let b = visual.debug_bounds("saved-library-row-fixture-1").unwrap();
    assert!(content.size.height >= px(450. * 88.));
    assert!(
        a.size.height >= px(88.) && b.top() >= a.bottom(),
        "Saved-query rows must not shrink or overlap: {a:?} {b:?}"
    );
}

#[gpui_kit::test]
fn item2_shared_grid_guards_never_copy_or_edit_hidden_live_rows_from_snapshot_view(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 227, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.tabs[s.active_tab].results = vec![rowboat_core::QueryResult {
                columns: vec![rowboat_core::Column {
                    name: "secret_live_value".into(),
                    data_type: "text".into(),
                    nullable: false,
                    primary_key: false,
                    default_value: None,
                }],
                rows: vec![vec![rowboat_core::Cell::Text(
                    "hidden live SQL result".into(),
                )]],
                ..Default::default()
            }];
            s.select_grid_cell(0, 0, false, cx);
            assert!(s.selected_cell().is_some());
            s.results.view = crate::result_snapshots::ResultsView::Comparison;
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                "keep snapshot copy".into(),
            ));
            assert!(!s.menu_enabled(crate::chrome_ui::Command::CopyCell, cx));
            assert!(!s.menu_enabled(crate::chrome_ui::Command::EditRow, cx));
            s.copy_cell(cx);
            s.copy_row(cx);
            s.edit_row(true, w, cx);
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().unwrap(),
                "keep snapshot copy"
            );
            assert!(!s.has_row_draft());
            w.focus(&s.focus, cx);
            s.focus_grid(w, cx);
            assert!(s.grid_focus.is_focused(w));
            assert!(s.selected.is_none());
            s.new_query(None, w, cx);
            assert!(!s.results_read_only());
            assert!(s.selected.is_none());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn item2_pending_file_operation_blocks_shutdown_and_navigation_until_completion(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 226, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let tab_count = s.tabs.len();
            s.library.file_pending = true;
            assert!(s.close_state().busy);
            assert!(!s.can_leave());
            assert!(!s.menu_enabled(crate::chrome_ui::Command::Favorite, cx));
            assert!(!s.menu_enabled(crate::chrome_ui::Command::CloseQuery, cx));
            s.new_query(Some("must not open while file is pending".into()), w, cx);
            s.show_palette(w, cx);
            assert_eq!(s.tabs.len(), tab_count);
            assert!(!s.quick_open);
            s.library.file_pending = false;
            assert!(!s.close_state().busy);
            assert!(s.can_leave());
            s.new_query(Some("SELECT 'after completion'".into()), w, cx);
            assert_eq!(s.tabs.len(), tab_count + 1);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn item2_named_save_as_and_rename_use_rendered_forms_and_keep_original_identity(
    cx: &mut TestAppContext,
) {
    use gpui_kit::{Modifiers, VisualTestContext};
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 225, runtime.handle());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| library::save_query(state, saved(), None).unwrap());
            s.open_library_query("stable", w, cx);
            s.save_library_query_as(w, cx);
            let form = s.library.form.as_ref().unwrap();
            form.name
                .update(cx, |e, cx| e.set_value("Named copy", w, cx));
            form.folder
                .update(cx, |e, cx| e.set_value("Reports/Weekly", w, cx));
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let submit = visual
        .debug_bounds("library-submit")
        .expect("Named Save As must render a Save control");
    visual.simulate_click(submit.center(), Modifiers::default());
    let id = cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.library.form.is_none());
        let shared = s.shared.borrow();
        let state = &shared.state;
        assert_eq!(state.snippets.len(), 2);
        assert_eq!(library::saved_query(state, "stable"), Some(saved()));
        let query = state.snippets.iter().find(|q| q.id != "stable").unwrap();
        assert_eq!(query.name, "Named copy");
        assert_eq!(state.query_metadata[&query.id].folder, "Reports/Weekly");
        assert_eq!(s.query_run_invocations, 0);
        query.id.clone()
    });
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.page = crate::app::Page::History;
            s.saved_queries_view = true;
            cx.notify();
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let rename = visual
        .debug_bounds(Box::leak(format!("library-rename-{id}").into_boxed_str()))
        .unwrap();
    visual.simulate_click(rename.center(), Modifiers::default());
    cx.update_window(window, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.library
                .form
                .as_ref()
                .unwrap()
                .name
                .update(cx, |e, cx| e.set_value("Renamed copy", w, cx))
        });
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let submit = visual.debug_bounds("library-submit").unwrap();
    visual.simulate_click(submit.center(), Modifiers::default());
    cx.read(|cx| {
        let s = entity.read(cx);
        let shared = s.shared.borrow();
        assert_eq!(shared.state.snippets.len(), 2);
        assert_eq!(
            library::saved_query(&shared.state, &id)
                .unwrap()
                .snippet
                .name,
            "Renamed copy"
        );
        assert_eq!(library::saved_query(&shared.state, "stable"), Some(saved()));
    });
}

#[gpui_kit::test]
fn item2_open_update_keeps_identity_and_workspace_without_connecting_or_running(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 221, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| library::save_query(state, saved(), None).unwrap());
            let owner = s.owner.clone();
            s.open_library_query("stable", window, cx);
            assert_eq!(s.owner, owner);
            assert_eq!(s.tabs[s.active_tab].connection_id, owner);
            assert!(s.database.is_none());
            assert!(!s.busy);
            assert_eq!(s.query_run_invocations, 0);
            s.tabs[s.active_tab]
                .editor
                .update(cx, |e, cx| e.set_value("SELECT 'updated λ'", window, cx));
            s.save_library_query(window, cx);
            s.save_library_query(window, cx);
            let state = &s.shared.borrow().state;
            assert_eq!(state.snippets.len(), 1);
            assert_eq!(state.snippets[0].id, "stable");
            assert_eq!(state.snippets[0].sql, "SELECT 'updated λ'");
            assert_eq!(
                state.query_metadata["stable"].connection_id.as_deref(),
                Some("different-connection")
            );
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn item2_stale_window_save_preserves_new_shared_sql_and_local_draft(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 222, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| library::save_query(state, saved(), None).unwrap());
            s.open_library_query("stable", window, cx);
            let mut elsewhere = saved();
            elsewhere.snippet.sql = "SELECT 'other window'".into();
            s.shared
                .borrow_mut()
                .edit(|state| library::save_query(state, elsewhere, Some(&saved())).unwrap());
            s.tabs[s.active_tab].editor.update(cx, |e, cx| {
                e.set_value("SELECT 'local unsaved'", window, cx)
            });
            s.save_library_query(window, cx);
            assert!(s.error.as_deref().unwrap().contains("another window"));
            assert_eq!(
                s.shared.borrow().state.snippets[0].sql,
                "SELECT 'other window'"
            );
            assert_eq!(
                s.tabs[s.active_tab].editor.read(cx).value().as_ref(),
                "SELECT 'local unsaved'"
            );
            assert!(s.library.documents[&s.tabs[s.active_tab].id].dirty("SELECT 'local unsaved'"));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn item2_palette_finds_latest_name_sql_and_folder_from_shared_state(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 223, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| library::save_query(state, saved(), None).unwrap());
            let mut moved = saved();
            moved.snippet.name = "New title".into();
            moved.metadata.folder = "Archive".into();
            s.shared
                .borrow_mut()
                .edit(|state| library::save_query(state, moved, Some(&saved())).unwrap());
            // The local AppState cache is deliberately stale until the regular poll.
            assert_eq!(s.state.snippets[0].name, "Original");
            s.quick_filter
                .update(cx, |e, cx| e.set_value("new archive original", window, cx));
            let items = s.palette_items(cx);
            assert!(items.iter().any(|item| item.target
                == crate::palette::Target::Favorite("stable".into())
                && item.title == "New title"));
            assert_eq!(s.query_run_invocations, 0);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn item2_session_restores_file_identity_and_unsaved_sql_without_reading_changed_disk(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 224, runtime.handle());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("query.sql");
    let document = library::create_sql_document(&path, "SELECT 'disk'").unwrap();
    let recovery = crate::session::SessionStore::at(directory.path()).unwrap();
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.startup.sql = None;
            let tab = &s.tabs[s.active_tab];
            s.library.documents.insert(tab.id, document.clone());
            tab.editor.update(cx, |e, cx| {
                e.set_value("SELECT 'recovered draft'", window, cx)
            });
            let snapshot = s.session_snapshot(cx);
            recovery.save(&snapshot).unwrap();
            std::fs::write(&path, "SELECT 'external'").unwrap();
            let recovered = recovery.load().unwrap();
            s.library.documents.clear();
            s.restore_buffers(
                recovered.tabs,
                recovered.workspaces.into_iter().next(),
                window,
                cx,
            );
            let tab = &s.tabs[s.active_tab];
            assert_eq!(
                tab.editor.read(cx).value().as_ref(),
                "SELECT 'recovered draft'"
            );
            let binding = &s.library.documents[&tab.id];
            assert_eq!(binding, &document);
            assert!(matches!(binding, SqlDocument::File { .. }));
            assert!(binding.dirty(&tab.editor.read(cx).value()));
            assert!(library::save_sql_document(binding, &tab.editor.read(cx).value()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 'external'");
        });
    })
    .unwrap();
}
