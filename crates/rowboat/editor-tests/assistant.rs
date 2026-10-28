//! Explicit disposable SQLite + fake agent. Never accesses saved profiles/models.
use crate::{
    app::{Page, Workbench},
    assistant_context::ContextOptions,
};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{
    ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode, TableRef,
};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

fn setup(
    cx: &mut TestAppContext,
    path: &Path,
    runtime: tokio::runtime::Handle,
) -> (AnyWindowHandle, Entity<Workbench>) {
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.join("assistant.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime.block_on(Database::connect(&profile, "")).unwrap();
    runtime.block_on(db.query("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO items VALUES(1, 'fixture-private-row')", 10)).unwrap();
    let catalog = runtime.block_on(db.namespace_catalog(None)).unwrap();
    let table = TableRef {
        schema: "main".into(),
        name: "items".into(),
    };
    let structure = runtime.block_on(db.structure(&table)).unwrap();
    let result = runtime
        .block_on(db.query("SELECT id,value FROM items", 10))
        .unwrap()
        .remove(0);
    let (w, entity) = crate::workbench_tests::workspace(cx, 2601, runtime);
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.owner = Some(profile.id.clone());
            s.profile = Some(profile.clone());
            s.database = Some(db);
            s.namespace.snapshot = Some(catalog);
            s.namespace.known = true;
            s.page = Page::Query;
            s.table = Some(table);
            s.structure = structure;
            s.tabs[0].connection_id = Some(profile.id.clone());
            s.tabs[0].results = vec![result];
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT id FROM items WHERE id=1; -- editor draft", w, cx)
            });
            s.capture_query_result_origin(
                s.tabs[0].id,
                "SELECT id,value FROM items; -- executed source",
            );
            s.select_grid_cell(0, 1, false, cx);
            s.edit_state(|state| state.preferences.ai.enabled = true);
            s.open_ai_assistant(w, cx);
        })
    })
    .unwrap();
    (w, entity)
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, selector: &'static str) {
    cx.update_window(w, |_, w, cx| {
        w.refresh();
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut v = VisualTestContext::from_window(w, cx);
    let bounds = v
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"));
    v.simulate_click(bounds.center(), Modifiers::default());
}
fn settle(cx: &mut TestAppContext, w: AnyWindowHandle, entity: &Entity<Workbench>) {
    for _ in 0..1000 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        let done = cx
            .update_window(w, |_, w, cx| {
                entity.update(cx, |s, cx| {
                    s.poll(w, cx);
                    !s.busy && s.assistant.request.is_none()
                })
            })
            .unwrap();
        if done {
            return;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    panic!("Fake assistant timed out");
}

#[gpui_kit::test]
fn assistant_context_tracks_provenance_and_values_require_explicit_opt_in(cx: &mut TestAppContext) {
    let fixture = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, entity) = setup(cx, fixture.path(), runtime.handle());
    cx.read(|cx| {
        let s = entity.read(cx);
        let context = s.capture_assistant_context(ContextOptions::default(), cx);
        let json = context.to_json().unwrap();
        assert!(!json.contains("fixture-private-row") && !json.contains("assistant.sqlite"));
        assert!(context.result.as_ref().unwrap().selected_cells.is_empty());
        assert!(
            context
                .query
                .as_ref()
                .unwrap()
                .sql
                .as_ref()
                .unwrap()
                .contains("editor draft")
        );
        assert!(
            context
                .result
                .as_ref()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .contains("executed source")
        );
        let options = ContextOptions {
            cells: true,
            schema: true,
            ..Default::default()
        };
        let context = s.capture_assistant_context(options, cx);
        assert_eq!(
            context.result.as_ref().unwrap().selected_cells[0].value,
            rowboat_core::Cell::Text("fixture-private-row".into())
        );
        assert!(context.schema_and_indexes.is_some());
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.state.history.is_empty());
    });
    click(cx, w, "assistant-trust");
    cx.read(|cx| {
        assert!(
            !entity
                .read(cx)
                .shared
                .borrow()
                .state
                .preferences
                .ai
                .investigation_allowed(&entity.read(cx).profile.as_ref().unwrap().id)
        )
    });
    click(cx, w, "assistant-enable-trust");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(
            s.shared
                .borrow()
                .state
                .preferences
                .ai
                .investigation_allowed(&s.profile.as_ref().unwrap().id)
        );
    });
    click(cx, w, "assistant-trust");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(
            !s.shared
                .borrow()
                .state
                .preferences
                .ai
                .investigation_allowed(&s.profile.as_ref().unwrap().id)
        );
    });
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| s.explain_query(w, cx))
    })
    .unwrap();
    settle(cx, w, &entity);
    click(cx, w, "plan-to-sidebar");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.assistant.open && s.assistant.plan.is_some());
        assert!(s.assistant.turns.is_empty() && s.assistant.request.is_none());
        assert!(
            s.capture_assistant_context(ContextOptions::default(), cx)
                .captured_plan
                .is_some()
        );
        assert_eq!(s.query_run_invocations, 0);
    });
}

#[gpui_kit::test]
fn assistant_sends_frozen_review_keeps_history_and_ctrl_enter_never_runs_sql(
    cx: &mut TestAppContext,
) {
    let fixture = tempfile::tempdir().unwrap();
    let capture = fixture.path().join("input.txt");
    let agent = fixture.path().join("fake-codex");
    std::fs::write(&agent, format!("#!/bin/sh\ncase \" $* \" in *\" --help \"*) echo '--ignore-user-config --ignore-rules --ephemeral --sandbox --json'; exit 0 ;; esac\ncat > '{}'\nprintf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"## Suggestion\\n```sql\\nSELECT id FROM items;\\n```\"}}}}'\n", capture.display())).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, entity) = setup(cx, fixture.path(), runtime.handle());
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| {
                state.preferences.ai.executable = agent.to_string_lossy().to_string()
            });
            s.assistant.prompt.update(cx, |input, cx| {
                input.set_value("Help author SQL", w, cx);
                input.focus(w, cx);
            });
        })
    })
    .unwrap();
    assert!(!capture.exists());
    cx.update_window(w, |_, w, cx| {
        w.refresh();
        w.draw(cx).clear(cx);
        let s = entity.read(cx);
        assert!(
            s.assistant.focus.contains_focused(w, cx),
            "Assistant focus lost: {:?}",
            w.context_stack()
        );
        assert!(!s.modal_open() && s.assistant.open);
    })
    .unwrap();
    cx.simulate_keystrokes(w, "ctrl-enter");
    settle(cx, w, &entity);
    cx.read(|cx| {
        let s = entity.read(cx);
        assert_eq!(s.assistant.turns.len(), 1);
        assert!(
            s.assistant.turns[0].complete,
            "{:?}",
            s.assistant.turns[0].error
        );
        assert_eq!(s.query_run_invocations, 0);
        assert!(!s.busy);
    });
    let sent = std::fs::read_to_string(&capture).unwrap();
    assert!(sent.contains("editor draft") && !sent.contains("fixture-private-row"));
    click(cx, w, "assistant-context-toggle");
    click(cx, w, "assistant-review");
    click(cx, w, "assistant-context-preview");
    cx.simulate_keystrokes(w, "ctrl-a");
    cx.simulate_input(w, "Redacted pinned context");
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT 'new-editor-must-not-leak'", w, cx)
            });
            s.assistant.prompt.update(cx, |input, cx| {
                input.set_value("What about ordering?", w, cx)
            });
        })
    })
    .unwrap();
    // Re-reviewing is not an explicit discard/rebuild. Preserve redactions even
    // after the live editor changes and the sidebar is closed and reopened.
    click(cx, w, "assistant-review");
    click(cx, w, "assistant-close");
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.toggle_assistant(w, cx);
            s.send_assistant_message(w, cx);
        })
    })
    .unwrap();
    settle(cx, w, &entity);
    let sent = std::fs::read_to_string(&capture).unwrap();
    assert!(
        sent.contains("Redacted pinned context")
            && sent.contains("Help author SQL")
            && sent.contains("What about ordering?")
    );
    assert!(!sent.contains("new-editor-must-not-leak"));
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let turn = s.assistant.turns.last().unwrap().id;
            let tabs = s.tabs.len();
            s.open_assistant_sql(turn, 0, w, cx);
            assert_eq!(s.tabs.len(), tabs + 1);
            assert!(
                s.tabs[s.active_tab]
                    .editor
                    .read(cx)
                    .value()
                    .contains("SELECT id FROM items")
            );
            assert_eq!(s.query_run_invocations, 0);
            s.assistant
                .prompt
                .update(cx, |input, cx| input.set_value("Keep unsent draft", w, cx));
            s.send_assistant_message(w, cx);
            let stale = s.assistant.request.unwrap();
            s.session_generation += 1;
            s.sync_assistant_scope();
            s.receive_assistant(
                crate::assistant_ui::AssistantEvent::Finished {
                    request: stale,
                    result: Ok("wrong database".into()),
                },
                cx,
            );
            assert!(s.assistant.turns.is_empty() && s.assistant.request.is_none());
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn assistant_sharing_defaults_apply_once_and_reopening_preserves_exclusions(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, entity) = crate::workbench_tests::workspace(cx, 2602, runtime.handle());
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            s.edit_state(|state| {
                state.preferences.ai.include_schema = true;
                state.preferences.ai.include_metrics = true;
                state.preferences.ai.executable = "/nonexistent-test-agent".into();
            });
            s.toggle_assistant(w, cx);
        })
    })
    .unwrap();
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.assistant.options.schema && s.assistant.options.metrics);
    });
    click(cx, w, "assistant-share-schema");
    click(cx, w, "assistant-share-metrics");
    click(cx, w, "assistant-share-sql");
    click(cx, w, "assistant-context-toggle");
    click(cx, w, "assistant-close");
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| s.toggle_assistant(w, cx))
    })
    .unwrap();
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(
            !s.assistant.options.schema && !s.assistant.options.metrics && !s.assistant.options.sql
        );
        assert!(!s.assistant.context_expanded);
        assert!(s.assistant.request.is_none());
        assert_eq!(s.query_run_invocations, 0);
    });
}

#[gpui_kit::test]
fn assistant_context_collapses_to_one_row_and_pending_activity_stays_visible(
    cx: &mut TestAppContext,
) {
    fn present(cx: &mut TestAppContext, w: AnyWindowHandle, selector: &'static str) -> bool {
        cx.update_window(w, |_, w, cx| {
            w.refresh();
            w.draw(cx).clear(cx);
        })
        .unwrap();
        VisualTestContext::from_window(w, cx)
            .debug_bounds(selector)
            .is_some()
    }
    let fixture = tempfile::tempdir().unwrap();
    let agent = fixture.path().join("fake-waiting-agent");
    std::fs::write(&agent, "#!/bin/sh\ncase \" $* \" in *\" --help \"*) echo '--ignore-user-config --ignore-rules --ephemeral --sandbox --json'; exit 0 ;; esac\ncat >/dev/null\nsleep 30\n").unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (w, entity) = setup(cx, fixture.path(), runtime.handle());
    cx.simulate_window_resize(w, gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(900.)));
    cx.update_window(w, |_, w, cx| {
        entity.update(cx, |s, cx| {
            let connection = s.profile.as_ref().unwrap().id.clone();
            s.edit_state(|state| {
                state.preferences.ai.executable = agent.to_string_lossy().to_string();
                state
                    .preferences
                    .ai
                    .investigation_connections
                    .insert(connection);
            });
            s.assistant.options.schema = true;
            s.assistant.options.metrics = true;
            s.assistant.options.cells = true;
            s.assistant.prompt.update(cx, |input, cx| {
                input.set_value("Keep my question and context", w, cx)
            });
        })
    })
    .unwrap();
    assert!(present(cx, w, "assistant-context-details"));
    click(cx, w, "assistant-context-toggle");
    assert!(!present(cx, w, "assistant-context-details"));
    let mut visual = VisualTestContext::from_window(w, cx);
    let toolbar = visual.debug_bounds("assistant-context-toolbar").unwrap();
    assert_eq!(toolbar.size.height, gpui_kit::px(40.));
    assert!(visual.debug_bounds("assistant-trusted-badge").is_some());
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(
            s.assistant.options.schema && s.assistant.options.metrics && s.assistant.options.cells
        );
        assert_eq!(
            s.assistant.prompt.read(cx).value().as_ref(),
            "Keep my question and context"
        );
        assert!(s.assistant.request.is_none() && s.assistant.turns.is_empty());
        assert_eq!(s.query_run_invocations, 0);
    });
    // The same focused control must expand with the keyboard, not only a mouse.
    let mut visual = VisualTestContext::from_window(w, cx);
    let keystroke = gpui_kit::Keystroke::parse("enter").unwrap();
    visual.simulate_event(gpui_kit::KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(gpui_kit::KeyUpEvent { keystroke });
    assert!(present(cx, w, "assistant-context-details"));
    click(cx, w, "assistant-review");
    assert!(present(cx, w, "assistant-context-preview"));
    click(cx, w, "assistant-context-toggle");
    assert!(present(cx, w, "assistant-context-preview"));
    assert!(!present(cx, w, "assistant-context-details"));
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.assistant.width = 320.;
            cx.notify();
        });
    })
    .unwrap();
    assert!(present(cx, w, "assistant-trusted-badge"));
    let mut visual = VisualTestContext::from_window(w, cx);
    let toolbar = visual.debug_bounds("assistant-context-toolbar").unwrap();
    let trusted = visual.debug_bounds("assistant-trusted-badge").unwrap();
    assert_eq!(toolbar.size.height, gpui_kit::px(40.));
    assert!(
        trusted.right() <= toolbar.right() && trusted.bottom() <= toolbar.bottom(),
        "Pinned/value/trusted indicators must fit the narrow toolbar"
    );
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.assistant.width = 440.;
            cx.notify();
        });
    })
    .unwrap();
    click(cx, w, "assistant-context-toggle");
    // Revoke tools before sending: this is an advice-only fake agent test.
    click(cx, w, "assistant-trust");
    click(cx, w, "assistant-send");
    let request = cx.read(|cx| entity.read(cx).assistant.request.unwrap());
    assert!(
        !present(cx, w, "assistant-context-details"),
        "Send should reclaim conversation space"
    );
    assert!(present(cx, w, "assistant-working"));
    assert!(present(cx, w, "loading-status"));
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.receive_assistant(
                crate::assistant_ui::AssistantEvent::Update {
                    request,
                    update: rowboat_services::ai::AgentUpdate::Text("Partial response".into()),
                },
                cx,
            );
        })
    })
    .unwrap();
    assert!(
        present(cx, w, "assistant-working"),
        "Activity continues while the response streams"
    );
    click(cx, w, "assistant-context-toggle");
    assert!(present(cx, w, "assistant-context-details"));
    cx.read(|cx| assert_eq!(entity.read(cx).assistant.request, Some(request)));
    click(cx, w, "assistant-cancel");
    assert!(!present(cx, w, "assistant-working"));
    assert!(!present(cx, w, "loading-status"));
    cx.update_window(w, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.receive_assistant(
                crate::assistant_ui::AssistantEvent::Finished {
                    request,
                    result: Ok("Late completion".into()),
                },
                cx,
            );
            assert!(s.assistant.request.is_none() && !s.assistant.turns[0].complete);
            assert_eq!(s.query_run_invocations, 0);
        })
    })
    .unwrap();
}
