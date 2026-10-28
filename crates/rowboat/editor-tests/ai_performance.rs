//! Real GPUI state and an explicitly disposable SQLite file/fake agent. No
//! installed agent, provider account, user connection or paid model is contacted.
use crate::{
    app::{Page, Workbench},
    preferences_ui::PreferenceSection,
};
use gpui_kit::{AnyWindowHandle, AppContext, Entity, Modifiers, TestAppContext, VisualTestContext};
use rowboat_core::{
    ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode, TableRef,
};
use std::{os::unix::fs::PermissionsExt, time::Duration};

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, selector: &'static str) {
    cx.update_window(window, |_, w, cx| {
        w.refresh();
        w.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}
fn settle(cx: &mut TestAppContext, window: AnyWindowHandle, entity: &Entity<Workbench>) {
    for _ in 0..800 {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        let done = cx
            .update_window(window, |_, window, cx| {
                entity.update(cx, |s, cx| {
                    s.poll(window, cx);
                    !s.busy
                        && s.explain
                            .as_ref()
                            .and_then(|state| state.ai.as_ref())
                            .is_none_or(|ai| ai.request.is_none())
                })
            })
            .unwrap();
        if done {
            return;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    panic!("AI fixture worker timed out");
}
#[gpui_kit::test]
fn ai_settings_are_tabbed_draft_only_and_cancel_preserves_preferences(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 2401, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.show_preferences(window, cx);
        })
    })
    .unwrap();
    click(cx, window, "preferences-tab-ai");
    click(cx, window, "ai-enabled");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.preferences_form.as_ref().unwrap().section == PreferenceSection::Ai);
        assert!(!s.shared.borrow().state.preferences.ai.enabled);
    });
    click(cx, window, "close-preferences");
    cx.read(|cx| assert!(!entity.read(cx).shared.borrow().state.preferences.ai.enabled));
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| s.show_preferences(window, cx))
    })
    .unwrap();
    click(cx, window, "preferences-tab-ai");
    click(cx, window, "ai-enabled");
    click(cx, window, "save-preferences");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.preferences_form.is_none());
        assert!(s.shared.borrow().state.preferences.ai.enabled);
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.state.history.is_empty());
    });
}

#[gpui_kit::test]
fn ai_provider_changes_clear_overrides_only_in_the_draft_and_same_provider_keeps_them(
    cx: &mut TestAppContext,
) {
    use rowboat_services::ai::AgentProvider;
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 2404, runtime.handle());
    cx.simulate_window_resize(
        window,
        gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(900.)),
    );
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.edit_state(|state| {
                state.preferences.ai.executable = "/nonexistent-test-codex".into();
                state.preferences.ai.model = "codex-specific-model".into();
                state.preferences.ai.timeout_secs = 45;
            });
            s.show_ai_settings(window, cx);
        })
    })
    .unwrap();
    // Re-selecting the same provider must not erase user-entered overrides.
    click(cx, window, "ai-provider-codex");
    click(cx, window, "save-preferences");
    cx.read(|cx| {
        let s = entity.read(cx);
        assert!(s.preferences_form.is_none());
        let shared = s.shared.borrow();
        assert_eq!(
            shared.state.preferences.ai.executable,
            "/nonexistent-test-codex"
        );
        assert_eq!(shared.state.preferences.ai.model, "codex-specific-model");
    });
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| s.show_ai_settings(window, cx))
    })
    .unwrap();
    click(cx, window, "ai-provider-claude");
    click(cx, window, "close-preferences");
    cx.read(|cx| {
        let s = entity.read(cx);
        let shared = s.shared.borrow();
        assert_eq!(shared.state.preferences.ai.provider, AgentProvider::Codex);
        assert_eq!(
            shared.state.preferences.ai.executable,
            "/nonexistent-test-codex"
        );
        assert_eq!(shared.state.preferences.ai.model, "codex-specific-model");
    });
    for (selector, provider) in [
        ("ai-provider-claude", AgentProvider::Claude),
        ("ai-provider-opencode", AgentProvider::Opencode),
    ] {
        cx.update_window(window, |_, window, cx| {
            entity.update(cx, |s, cx| {
                s.edit_state(|state| {
                    state.preferences.ai.executable = "/nonexistent-previous-agent".into();
                    state.preferences.ai.model = "previous-agent-model".into();
                });
                s.show_ai_settings(window, cx);
            })
        })
        .unwrap();
        click(cx, window, selector);
        click(cx, window, "save-preferences");
        cx.read(|cx| {
            let s = entity.read(cx);
            assert!(s.preferences_form.is_none());
            let shared = s.shared.borrow();
            let settings = &shared.state.preferences.ai;
            assert_eq!(settings.provider, provider);
            assert!(settings.executable.is_empty() && settings.model.is_empty());
            assert_eq!(settings.timeout_secs, 45);
            assert_eq!(s.query_run_invocations, 0);
        });
    }
}

#[gpui_kit::test]
fn ai_review_evidence_changes_preserve_redactions_until_explicit_preview_update(
    cx: &mut TestAppContext,
) {
    let fixture = tempfile::tempdir().unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: fixture.path().join("evidence.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime
        .handle()
        .block_on(Database::connect(&profile, ""))
        .unwrap();
    runtime
        .handle()
        .block_on(db.query("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)", 10))
        .unwrap();
    let table = TableRef {
        schema: "main".into(),
        name: "items".into(),
    };
    let structure = runtime.handle().block_on(db.structure(&table)).unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 2403, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.session_ready = true;
            s.owner = Some(profile.id.clone());
            s.profile = Some(profile.clone());
            s.database = Some(db);
            s.table = Some(table);
            s.structure = structure;
            s.tabs[0].connection_id = Some(profile.id.clone());
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("SELECT id FROM items", window, cx));
            s.edit_state(|state| {
                state.preferences.ai.enabled = true;
                // A broken guard must still never find a user's installed agent.
                state.preferences.ai.executable = fixture
                    .path()
                    .join("nonexistent-agent")
                    .to_string_lossy()
                    .to_string();
            });
            s.explain_query(window, cx);
        })
    })
    .unwrap();
    settle(cx, window, &entity);
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| s.prepare_ai_analysis(window, cx))
    })
    .unwrap();
    click(cx, window, "ai-context-schema");
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            let ai = s.explain.as_ref().unwrap().ai.as_ref().unwrap();
            let context: serde_json::Value =
                serde_json::from_str(&ai.context.read(cx).value()).unwrap();
            assert!(
                context["schema_and_indexes"].is_object(),
                "Untouched preview must update automatically"
            );
            ai.context.update(cx, |input, cx| {
                input.set_value("User-redacted context", window, cx)
            });
        })
    })
    .unwrap();
    click(cx, window, "ai-context-schema");
    cx.update_window(window, |_, _, cx| {
        entity.update(cx, |s, cx| {
            s.send_ai_analysis(cx);
            let ai = s.explain.as_ref().unwrap().ai.as_ref().unwrap();
            assert_eq!(
                ai.context.read(cx).value().as_ref(),
                "User-redacted context"
            );
            assert!(
                ai.request.is_none() && ai.error.is_some(),
                "Changed evidence cannot silently send an outdated preview"
            );
        })
    })
    .unwrap();
    click(cx, window, "ai-rebuild-context");
    cx.read(|cx| {
        let s = entity.read(cx);
        let ai = s.explain.as_ref().unwrap().ai.as_ref().unwrap();
        let context: serde_json::Value =
            serde_json::from_str(&ai.context.read(cx).value()).unwrap();
        assert!(context["schema_and_indexes"].is_null());
        assert!(ai.request.is_none() && ai.error.is_none());
        assert_eq!(s.query_run_invocations, 0);
        assert!(s.state.history.is_empty());
    });
}

#[gpui_kit::test]
fn ai_review_delegates_only_on_send_and_opens_sql_without_execution(cx: &mut TestAppContext) {
    let fixture = tempfile::tempdir().unwrap();
    let agent = fixture.path().join("fake-codex");
    let capture = fixture.path().join("reviewed-input.txt");
    let args = fixture.path().join("args.txt");
    std::fs::write(&agent, format!(r##"#!/bin/sh
case " $* " in
  *" --help "*) printf '%s\n' '--ignore-user-config --ignore-rules --ephemeral --sandbox --json'; exit 0 ;;
esac
printf '%s\n' "$@" > '{}'
cat > '{}'
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"An index is only a candidate.\n```sql\nCREATE INDEX suggested_idx ON items(value);\n```"}}}}'
"##, args.display(), capture.display())).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: fixture.path().join("fixture.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = runtime
        .handle()
        .block_on(Database::connect(&profile, ""))
        .unwrap();
    runtime.handle().block_on(db.query("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO items VALUES (1, 'do-not-share-result-rows')", 10)).unwrap();
    let (window, entity) = crate::workbench_tests::workspace(cx, 2402, runtime.handle());
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            s.loaded = true;
            s.session_ready = true;
            s.owner = Some(profile.id.clone());
            s.profile = Some(profile.clone());
            s.database = Some(db.clone());
            s.tabs[0].connection_id = Some(profile.id.clone());
            s.page = Page::Query;
            s.tabs[0].editor.update(cx, |e, cx| {
                e.set_value("SELECT id FROM items WHERE value = 'example'", window, cx)
            });
            s.edit_state(|state| {
                state.preferences.ai.enabled = true;
                state.preferences.ai.executable = agent.to_string_lossy().to_string();
            });
            s.explain_query(window, cx);
        })
    })
    .unwrap();
    settle(cx, window, &entity);
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            assert!(
                s.explain.as_ref().unwrap().report.is_some(),
                "{:?}",
                s.explain.as_ref().unwrap().error
            );
            s.prepare_ai_analysis(window, cx);
            let ai = s.explain.as_ref().unwrap().ai.as_ref().unwrap();
            let context = ai.context.read(cx).value();
            assert!(context.contains("captured_plan"));
            assert!(!context.contains("do-not-share-result-rows"));
            assert!(!context.contains("fixture.sqlite"));
            assert!(!capture.exists());
            s.send_ai_analysis(cx);
            s.receive_ai(
                crate::ai_ui::AiEvent::Text {
                    request: uuid::Uuid::new_v4(),
                    text: "wrong request".into(),
                },
                cx,
            );
            assert!(
                s.explain
                    .as_ref()
                    .unwrap()
                    .ai
                    .as_ref()
                    .unwrap()
                    .response
                    .is_empty()
            );
        })
    })
    .unwrap();
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            let request = s
                .explain
                .as_ref()
                .unwrap()
                .ai
                .as_ref()
                .unwrap()
                .request
                .unwrap();
            s.receive_ai(
                crate::ai_ui::AiEvent::Text {
                    request,
                    text: "Partial advice".into(),
                },
                cx,
            );
        });
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    let mut visual = VisualTestContext::from_window(window, cx);
    assert!(visual.debug_bounds("ai-analysis-working").is_some());
    assert!(visual.debug_bounds("loading-status").is_some());
    settle(cx, window, &entity);
    let sent = std::fs::read_to_string(&capture).unwrap();
    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .unwrap();
    assert!(
        VisualTestContext::from_window(window, cx)
            .debug_bounds("ai-analysis-working")
            .is_none()
    );
    assert!(sent.contains("SELECT id FROM items"));
    assert!(!sent.contains("do-not-share-result-rows"));
    assert!(!std::fs::read_to_string(args).unwrap().contains("SELECT"));
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            let ai = s.explain.as_ref().unwrap().ai.as_ref().unwrap();
            ai.follow_up.update(cx, |input, cx| {
                input.set_value("What are the index write costs?", window, cx)
            });
            s.review_ai_follow_up(window, cx);
            let context = s
                .explain
                .as_ref()
                .unwrap()
                .ai
                .as_ref()
                .unwrap()
                .context
                .read(cx)
                .value();
            assert!(context.contains("PREVIOUS UNTRUSTED AGENT ADVICE"));
            assert!(context.contains("What are the index write costs?"));
            assert_eq!(
                std::fs::read_to_string(&capture).unwrap(),
                sent,
                "Follow-up review must not submit a prompt"
            );
            s.send_ai_analysis(cx);
            let request = s
                .explain
                .as_ref()
                .unwrap()
                .ai
                .as_ref()
                .unwrap()
                .request
                .unwrap();
            s.cancel_ai_analysis(cx);
            let tabs = s.tabs.len();
            s.open_ai_suggestion(0, window, cx);
            assert_eq!(
                s.tabs.len(),
                tabs,
                "Incomplete/cancelled advice is not actionable"
            );
            s.receive_ai(
                crate::ai_ui::AiEvent::Finished {
                    request,
                    result: Ok("late cancelled response".into()),
                },
                cx,
            );
            assert!(
                !s.explain
                    .as_ref()
                    .unwrap()
                    .ai
                    .as_ref()
                    .unwrap()
                    .response
                    .contains("late cancelled")
            );
            s.send_ai_analysis(cx);
        })
    })
    .unwrap();
    settle(cx, window, &entity);
    assert!(
        std::fs::read_to_string(&capture)
            .unwrap()
            .contains("What are the index write costs?")
    );
    cx.update_window(window, |_, window, cx| {
        entity.update(cx, |s, cx| {
            let ai = s.explain.as_ref().unwrap().ai.as_ref().unwrap();
            assert!(ai.error.is_none(), "{:?}", ai.error);
            assert!(ai.response.contains("suggested_idx"));
            assert!(s.state.history.is_empty());
            let tabs = s.tabs.len();
            s.open_ai_suggestion(0, window, cx);
            assert_eq!(s.tabs.len(), tabs + 1);
            assert!(
                s.tabs[s.active_tab]
                    .editor
                    .read(cx)
                    .value()
                    .contains("CREATE INDEX")
            );
            assert_eq!(s.query_run_invocations, 0);
        })
    })
    .unwrap();
    let indexes = runtime
        .handle()
        .block_on(db.query(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='suggested_idx'",
            10,
        ))
        .unwrap();
    assert!(
        indexes[0].rows.is_empty(),
        "AI SQL must never execute automatically"
    );
}
