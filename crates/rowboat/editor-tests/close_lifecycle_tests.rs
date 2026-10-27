//! Actual shell controls and FIFO ACKs against disposable recovery storage; no database calls.
use crate::{
    app::Workbench,
    recovery_ui::RecoveryPhase,
    session::{SessionState, SessionStore, TabBuffer},
    workspace_shell::{Command, WorkspaceShell},
    workspace_storage::{Event, Write, Writers},
};
use gpui_kit::{
    AnyWindowHandle, AppContext, Entity, Focusable, Modifiers, TestAppContext, VisualTestContext,
};
use rowboat_core::{Cell, Column, ConnectionProfile, RowChange, Structure, TableRef};

struct Harness {
    window: AnyWindowHandle,
    shell: Entity<WorkspaceShell>,
    workspaces: Vec<Entity<Workbench>>,
    events: std::sync::mpsc::Sender<Event>,
    recovery: tokio::sync::mpsc::UnboundedReceiver<Write<SessionState>>,
    state: tokio::sync::mpsc::UnboundedReceiver<Write<rowboat_services::AppState>>,
}
impl Harness {
    fn new(
        cx: &mut TestAppContext,
        runtime: tokio::runtime::Handle,
        saved: Option<SessionState>,
    ) -> Self {
        let profiles = ["a", "b"].map(|id| ConnectionProfile {
            id: id.into(),
            name: format!("Workspace {id}"),
            ..Default::default()
        });
        let (state, state_rx) = tokio::sync::mpsc::unbounded_channel();
        let (recovery, recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, events) = std::sync::mpsc::channel();
        events_tx
            .send(Event::StateLoaded(Ok(Box::new(
                rowboat_services::AppState {
                    connections: profiles.to_vec(),
                    ..Default::default()
                },
            ))))
            .unwrap();
        events_tx
            .send(Event::RecoveryLoaded(Ok(saved.unwrap_or_else(|| {
                SessionState {
                    version: crate::session::FORMAT_VERSION,
                    active_tab: 1,
                    tabs: profiles
                        .iter()
                        .enumerate()
                        .map(|(i, p)| TabBuffer {
                            id: i + 1,
                            connection_id: Some(p.id.clone()),
                            title: p.name.clone(),
                            sql: format!("SELECT '{}'", p.id),
                        })
                        .collect(),
                    ..Default::default()
                }
            }))))
            .unwrap();
        let (window, shell) = cx.update(|cx| {
            crate::test_theme::init(cx);
            crate::keyboard_ui::bind(cx);
            gpui_kit::open_window(Default::default(), cx, |w, cx| {
                cx.new(|cx| {
                    WorkspaceShell::with_writers(
                        runtime,
                        Default::default(),
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
        let workspaces = cx
            .update_window(window, |_, w, cx| {
                shell.update(cx, |s, cx| s.test_tick(w, cx))
            })
            .unwrap();
        Self {
            window,
            shell,
            workspaces,
            events: events_tx,
            recovery: recovery_rx,
            state: state_rx,
        }
    }
    fn tick(&self, cx: &mut TestAppContext) -> Vec<Entity<Workbench>> {
        cx.update_window(self.window, |_, w, cx| {
            self.shell.update(cx, |s, cx| s.test_tick(w, cx))
        })
        .unwrap()
    }
    fn request(&self, cx: &mut TestAppContext, workspace: Option<usize>) {
        let id = workspace.map(|i| cx.read(|cx| self.workspaces[i].read(cx).tx.route.workspace));
        cx.update_window(self.window, |_, w, cx| {
            self.shell.update(cx, |s, cx| match id {
                Some(id) => s.test_close_workspace(id, w, cx),
                None => s.test_close_dialog(w, cx),
            })
        })
        .unwrap();
        draw(cx, self.window);
    }
    fn checkpoint(&mut self) -> (u64, SessionState) {
        let mut latest = None;
        while let Ok(write) = self.recovery.try_recv() {
            match write {
                Write::Save(revision, state) => latest = Some((revision, state)),
                Write::Shutdown => panic!("Writer stopped before expected checkpoint"),
            }
        }
        latest.expect("Close must request a durable checkpoint")
    }
    fn ack(&self, revision: u64) {
        self.events
            .send(Event::RecoverySaved(revision, Ok(())))
            .unwrap();
    }
    fn assert_no_shutdown(&mut self) {
        assert!(
            self.recovery.try_recv().is_err(),
            "Unexpected recovery write/shutdown"
        );
        assert!(
            self.state.try_recv().is_err(),
            "Unexpected application-state write/shutdown"
        );
    }
    fn finish_shutdown(&mut self, cx: &mut TestAppContext) {
        assert!(matches!(self.recovery.try_recv().unwrap(), Write::Shutdown));
        assert!(matches!(self.state.try_recv().unwrap(), Write::Shutdown));
        self.events.send(Event::WriterStopped).unwrap();
        self.tick(cx);
        assert!(
            alive(cx, self.window),
            "First writer ACK must not remove windows"
        );
        self.events.send(Event::WriterStopped).unwrap();
        self.tick(cx);
        assert!(!alive(cx, self.window));
    }
    fn reopen(&self, cx: &mut TestAppContext, owner: &str) -> Entity<Workbench> {
        cx.update(|cx| {
            self.workspaces.last().unwrap().update(cx, |s, _| {
                s.shell_tx
                    .send(Command::OpenQuery {
                        owner: Some(owner.into()),
                        sql: "SELECT 'new tab'".into(),
                        title: "Reopened".into(),
                    })
                    .unwrap();
            })
        });
        self.tick(cx)
            .into_iter()
            .find(|w| cx.read(|cx| w.read(cx).owner.as_deref() == Some(owner)))
            .unwrap()
    }
}
fn alive(cx: &mut TestAppContext, w: AnyWindowHandle) -> bool {
    cx.update_window(w, |_, _, _| ()).is_ok()
}
fn draw(cx: &mut TestAppContext, w: AnyWindowHandle) {
    cx.update_window(w, |_, w, cx| w.draw(cx).clear(cx))
        .unwrap();
}
fn visible(cx: &mut TestAppContext, w: AnyWindowHandle, selector: &'static str) -> bool {
    draw(cx, w);
    VisualTestContext::from_window(w, cx)
        .debug_bounds(selector)
        .is_some()
}
fn click(cx: &mut TestAppContext, w: AnyWindowHandle, selector: &'static str) {
    draw(cx, w);
    let mut visual = VisualTestContext::from_window(w, cx);
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Missing {selector}"));
    visual.simulate_click(bounds.center(), Modifiers::default());
}
fn stage(cx: &mut TestAppContext, entity: &Entity<Workbench>) {
    cx.update(|cx| {
        entity.update(cx, |s, _| {
            s.table = Some(TableRef {
                schema: "public".into(),
                name: "items".into(),
            });
            s.structure = Structure {
                columns: vec![Column {
                    name: "id".into(),
                    data_type: "integer".into(),
                    primary_key: true,
                    nullable: false,
                    default_value: None,
                }],
                ..Default::default()
            };
            s.change_target = s.current_target();
            s.changes = vec![RowChange::Insert {
                values: vec![("id".into(), Cell::Text("42".into()))],
            }];
            s.capture_recovery();
            assert!(s.recovery.error.is_none());
        })
    });
}

#[gpui_kit::test]
fn close_clean_sql_is_silent_frozen_and_waits_for_latest_ack_and_both_writers(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    h.request(cx, None);
    assert!(!visible(cx, h.window, "workspace-close-guard"));
    let (first, first_sql) = h.checkpoint();
    assert_eq!(first_sql.tabs.len(), 2);
    cx.update_window(h.window, |_, w, cx| {
        h.workspaces[0].update(cx, |s, cx| {
            assert!(s.close_requested);
            let editor = s.tabs[0].editor.clone();
            editor.read(cx).focus_handle(cx).focus(w, cx);
        })
    })
    .unwrap();
    cx.simulate_input(h.window, "must not enter frozen editor");
    cx.read(|cx| {
        assert_eq!(
            h.workspaces[0].read(cx).tabs[0]
                .editor
                .read(cx)
                .value()
                .as_ref(),
            "SELECT 'a'"
        )
    });
    h.ack(first.saturating_sub(1));
    h.tick(cx);
    h.assert_no_shutdown();
    // A late programmatic SQL update must be recaptured, not covered by an older ACK.
    cx.update_window(h.window, |_, w, cx| {
        h.workspaces[0].update(cx, |s, cx| {
            s.tabs[0]
                .editor
                .update(cx, |e, cx| e.set_value("SELECT 'latest SQL'", w, cx));
            s.edit_state(|state| state.preferences.editor_font_size = 14.);
        })
    })
    .unwrap();
    let Write::Save(state_revision, _) = h.state.try_recv().unwrap() else {
        panic!()
    };
    h.ack(first);
    h.tick(cx);
    let (latest, latest_sql) = h.checkpoint();
    assert!(latest > first);
    assert!(
        latest_sql
            .tabs
            .iter()
            .any(|t| t.sql == "SELECT 'latest SQL'")
    );
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::at(directory.path()).unwrap();
    store.save(&latest_sql).unwrap();
    h.ack(latest);
    h.tick(cx);
    h.assert_no_shutdown();
    h.events
        .send(Event::StateSaved(state_revision, Ok(())))
        .unwrap();
    h.tick(cx);
    h.finish_shutdown(cx);
    assert_eq!(store.load().unwrap(), latest_sql);
}

#[gpui_kit::test]
fn close_dirty_cancel_preserves_edits_and_restores_editor_input(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    h.request(cx, Some(0));
    assert!(visible(cx, h.window, "workspace-close-guard"));
    assert!(visible(cx, h.window, "discard-workspace-close"));
    let bounds = VisualTestContext::from_window(h.window, cx)
        .debug_bounds("workspace-close-guard")
        .unwrap();
    assert!(bounds.size.width <= gpui_kit::px(480.));
    click(cx, h.window, "keep-workspaces");
    assert!(!visible(cx, h.window, "workspace-close-guard"));
    cx.update_window(h.window, |_, w, cx| {
        h.workspaces[0].update(cx, |s, cx| {
            assert!(!s.close_requested);
            assert_eq!(s.changes.len(), 1);
            assert_eq!(s.recovery_batches().len(), 1);
            s.tabs[0].editor.read(cx).focus_handle(cx).focus(w, cx);
        })
    })
    .unwrap();
    cx.simulate_input(h.window, " editable");
    cx.read(|cx| {
        assert!(
            h.workspaces[0].read(cx).tabs[0]
                .editor
                .read(cx)
                .value()
                .contains("editable")
        )
    });
    h.assert_no_shutdown();
}

#[gpui_kit::test]
fn close_keep_drafts_archives_without_apply_and_reopens_detached(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    h.request(cx, Some(0));
    click(cx, h.window, "confirm-workspace-close");
    let (revision, saved) = h.checkpoint();
    assert_eq!(saved.workspaces[0].staged.len(), 1);
    assert!(!saved.workspaces[0].open);
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::at(dir.path()).unwrap();
    store.save(&saved).unwrap();
    h.ack(revision);
    assert_eq!(h.tick(cx).len(), 1);
    h.assert_no_shutdown();
    let reopened = h.reopen(cx, "a");
    cx.read(|cx| {
        let s = reopened.read(cx);
        assert_eq!(s.recovery.detached, saved.workspaces[0].staged);
        assert!(s.changes.is_empty());
        assert_eq!(s.query_run_invocations, 0);
        assert!(
            s.tabs
                .iter()
                .any(|t| t.editor.read(cx).value().as_ref() == "SELECT 'a'")
        );
    });
    assert_eq!(store.load().unwrap(), saved);
}

#[gpui_kit::test]
fn close_discard_removes_autosaved_all_phases_and_sources_but_preserves_other_workspace(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    stage(cx, &h.workspaces[1]);
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            let batch = s.recovery_batches()[0].clone();
            for phase in [
                RecoveryPhase::Staged,
                RecoveryPhase::Stale,
                RecoveryPhase::ApplyStarted,
            ] {
                let mut saved = batch.clone();
                saved.id = uuid::Uuid::new_v4().to_string();
                saved.phase = phase;
                s.recovery.detached.push(saved);
            }
            s.prepare_apply_marker().unwrap(); // Current ApplyStarted must also be cleared.
        });
        h.workspaces[1].update(cx, |s, _| {
            s.busy = true;
            s.change_target = None;
            s.capture_recovery();
            assert!(s.recovery.error.is_some());
        });
    });
    h.request(cx, Some(0));
    cx.read(|cx| assert!(!h.workspaces[1].read(cx).close_requested));
    click(cx, h.window, "confirm-workspace-close");
    let (old_revision, old) = h.checkpoint();
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::at(dir.path()).unwrap();
    store.save(&old).unwrap(); // Represents a previously autosaved recovery file.
    assert_eq!(old.workspaces[0].staged.len(), 4);
    click(cx, h.window, "keep-workspaces");
    h.request(cx, Some(0));
    click(cx, h.window, "discard-workspace-close");
    let (revision, discarded) = h.checkpoint();
    assert!(discarded.workspaces[0].staged.is_empty());
    assert_eq!(discarded.workspaces[1], old.workspaces[1]);
    h.ack(old_revision);
    assert_eq!(h.tick(cx).len(), 2, "Stale ACK cannot authorize discard");
    cx.read(|cx| assert_eq!(h.workspaces[0].read(cx).recovery_batches().len(), 4));
    store.save(&discarded).unwrap();
    h.ack(revision);
    assert_eq!(h.tick(cx).len(), 1);
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            assert!(s.changes.is_empty());
            s.capture_recovery();
            assert!(
                s.recovery_batches().is_empty(),
                "Discarded sources must not recapture"
            );
        });
        let b = h.workspaces[1].read(cx);
        assert!(b.busy && !b.close_requested && b.recovery.error.is_some());
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.recovery_batches(), old.workspaces[1].staged);
    });
    h.assert_no_shutdown();
    let reopened = h.reopen(cx, "a");
    cx.read(|cx| assert!(reopened.read(cx).recovery_batches().is_empty()));
    let restarted = Harness::new(cx, runtime.handle(), Some(store.load().unwrap()));
    let reopened = restarted.reopen(cx, "a");
    cx.read(|cx| {
        let s = reopened.read(cx);
        assert!(s.recovery_batches().is_empty());
        assert!(
            s.tabs
                .iter()
                .any(|t| t.editor.read(cx).value().as_ref() == "SELECT 'a'")
        );
        assert_eq!(s.query_run_invocations, 0);
    });
}

#[gpui_kit::test]
fn close_failed_capture_blocks_keep_but_discard_durably_removes_old_batch(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.changes = vec![s.changes[0].clone(); crate::recovery_ui::MAX_CHANGES + 1];
        })
    });
    h.request(cx, Some(0));
    click(cx, h.window, "confirm-workspace-close");
    h.assert_no_shutdown();
    cx.read(|cx| {
        let s = h.workspaces[0].read(cx);
        assert!(s.recovery.error.is_some());
        assert_eq!(s.recovery_batches()[0].phase, RecoveryPhase::Stale);
    });
    click(cx, h.window, "discard-workspace-close");
    let (revision, saved) = h.checkpoint();
    assert!(saved.workspaces[0].staged.is_empty());
    h.ack(revision);
    assert_eq!(h.tick(cx).len(), 1);
    cx.read(|cx| assert!(h.workspaces[0].read(cx).changes.is_empty()));
}

#[gpui_kit::test]
fn close_busy_finish_rechecks_risks_and_never_confirms_for_the_user(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    cx.update(|cx| h.workspaces[0].update(cx, |s, _| s.busy = true));
    h.request(cx, None);
    click(cx, h.window, "confirm-workspace-close");
    h.assert_no_shutdown();
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.busy = false;
            s.transaction_known = false;
        })
    });
    h.tick(cx);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    h.assert_no_shutdown();
    click(cx, h.window, "keep-workspaces");
    cx.update(|cx| h.workspaces[0].update(cx, |s, _| s.transaction_known = true));
    h.request(cx, None);
    let (revision, _) = h.checkpoint();
    // A risk appearing after silent close started must revoke its permission.
    cx.update(|cx| h.workspaces[0].update(cx, |s, _| s.transaction = true));
    h.ack(revision);
    h.tick(cx);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    h.assert_no_shutdown();
    click(cx, h.window, "confirm-workspace-close");
    h.finish_shutdown(cx);
}

#[gpui_kit::test]
fn close_discard_rejects_changed_same_count_edits_even_with_matching_checkpoint_ack(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    h.request(cx, Some(0));
    click(cx, h.window, "discard-workspace-close");
    let (revision, _) = h.checkpoint();
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.changes[0] = RowChange::Insert {
                values: vec![("id".into(), Cell::Text("different".into()))],
            };
        })
    });
    h.ack(revision);
    assert_eq!(h.tick(cx).len(), 2);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    cx.read(|cx| assert_eq!(h.workspaces[0].read(cx).changes.len(), 1));
    let (restored_revision, restored) = h.checkpoint();
    assert!(restored_revision > revision);
    assert_eq!(restored.workspaces[0].staged.len(), 1);
    assert!(restored.workspaces[0].open);
    h.ack(restored_revision);
    assert_eq!(h.tick(cx).len(), 2);
    h.assert_no_shutdown();
}

#[gpui_kit::test]
fn close_cancel_pending_discard_restores_checkpoint_in_fifo_order(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    h.request(cx, Some(0));
    click(cx, h.window, "discard-workspace-close");
    let (discard_revision, discard) = h.checkpoint();
    assert!(discard.workspaces[0].staged.is_empty());
    click(cx, h.window, "keep-workspaces");
    let (restore_revision, restored) = h.checkpoint();
    assert!(restore_revision > discard_revision);
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::at(dir.path()).unwrap();
    store.save(&discard).unwrap();
    h.ack(discard_revision);
    assert_eq!(h.tick(cx).len(), 2);
    store.save(&restored).unwrap();
    h.ack(restore_revision);
    h.tick(cx);
    assert_eq!(store.load().unwrap().workspaces[0].staged.len(), 1);
    assert!(store.load().unwrap().workspaces[0].open);
    cx.read(|cx| {
        let s = h.workspaces[0].read(cx);
        assert_eq!(s.changes.len(), 1);
        assert!(!s.close_requested);
    });
    h.assert_no_shutdown();
}

fn state_failure_compensates_discard(
    cx: &mut TestAppContext,
    ack_before_failure: bool,
    fallback_close: bool,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            // Hold the independent state ACK so even an acknowledged recovery
            // discard cannot finish closing before the injected state failure.
            s.edit_state(|state| state.preferences.editor_font_size = 14.);
        })
    });
    let Write::Save(state_revision, _) = h.state.try_recv().unwrap() else {
        panic!()
    };
    h.request(cx, if fallback_close { None } else { Some(0) });
    click(cx, h.window, "discard-workspace-close");
    let (discard_revision, discarded) = h.checkpoint();
    assert!(discarded.workspaces[0].staged.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::at(dir.path()).unwrap();
    store.save(&discarded).unwrap();
    if ack_before_failure {
        h.ack(discard_revision);
        assert_eq!(h.tick(cx).len(), 2);
        h.assert_no_shutdown();
    }
    // Make periodic autosave impossible. Only immediate compensation can pass.
    cx.update(|cx| h.shell.update(cx, |s, _| s.test_defer_autosave()));
    h.events
        .send(Event::StateSaved(
            state_revision,
            Err("injected state failure during discard".into()),
        ))
        .unwrap();
    h.tick(cx);
    click(
        cx,
        h.window,
        if fallback_close {
            "confirm-workspace-close"
        } else {
            "keep-workspaces"
        },
    );
    let Write::Save(restore_revision, restored) = h
        .recovery
        .try_recv()
        .expect("Failure must enqueue compensation immediately, before shutdown")
    else {
        panic!("Shutdown overtook the compensating recovery checkpoint")
    };
    assert!(restore_revision > discard_revision);
    assert_eq!(restored.workspaces[0].staged.len(), 1);
    assert!(restored.workspaces[0].open);
    assert_eq!(restored.tabs, discarded.tabs);
    assert_eq!(
        restored.workspaces[0].documents,
        discarded.workspaces[0].documents
    );
    assert_eq!(restored.workspaces[1], discarded.workspaces[1]);
    // The real store sees the same FIFO order as production, with the retained
    // draft restored after the earlier filtered checkpoint.
    store.save(&restored).unwrap();
    if !ack_before_failure {
        h.ack(discard_revision);
    }
    h.ack(restore_revision);
    if fallback_close {
        h.finish_shutdown(cx);
    } else {
        assert_eq!(h.tick(cx).len(), 2);
        assert!(!visible(cx, h.window, "workspace-close-guard"));
        h.assert_no_shutdown();
        cx.read(|cx| assert!(!h.workspaces[0].read(cx).close_requested));
        cx.update_window(h.window, |_, w, _| w.remove_window())
            .unwrap();
    }
    cx.read(|cx| {
        let s = h.workspaces[0].read(cx);
        assert_eq!(s.changes.len(), 1);
        assert_eq!(s.recovery_batches(), restored.workspaces[0].staged);
    });
    assert_eq!(store.load().unwrap(), restored);
}

#[gpui_kit::test]
fn close_state_failure_then_cancel_compensates_before_autosave_in_both_ack_orders(
    cx: &mut TestAppContext,
) {
    for ack_before_failure in [false, true] {
        state_failure_compensates_discard(cx, ack_before_failure, false);
    }
}

#[gpui_kit::test]
fn close_state_failure_then_immediate_fallback_queues_compensation_before_shutdown(
    cx: &mut TestAppContext,
) {
    for ack_before_failure in [false, true] {
        state_failure_compensates_discard(cx, ack_before_failure, true);
    }
}

#[gpui_kit::test]
fn close_restored_only_drafts_and_unstaged_parameters_are_guarded(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            let mut batch = s.recovery_batches()[0].clone();
            batch.phase = RecoveryPhase::ApplyStarted;
            s.recovery.detached = vec![batch];
            s.changes.clear();
            s.capture_recovery();
            assert_eq!(s.close_state().recovered, 1);
        })
    });
    h.request(cx, Some(0));
    assert!(visible(cx, h.window, "workspace-close-guard"));
    click(cx, h.window, "discard-workspace-close");
    let (revision, saved) = h.checkpoint();
    assert!(saved.workspaces[0].staged.is_empty());
    h.ack(revision);
    assert_eq!(h.tick(cx).len(), 1);
    cx.update_window(h.window, |_, w, cx| {
        h.workspaces[1].update(cx, |s, cx| {
            s.tabs[0].show_parameters = true;
            s.tabs[0]
                .parameters
                .update(cx, |input, cx| input.set_value("{\"id\":42}", w, cx));
        })
    })
    .unwrap();
    h.request(cx, Some(1));
    assert!(visible(cx, h.window, "workspace-close-guard"));
    assert!(visible(cx, h.window, "discard-workspace-close"));
    assert!(
        !visible(cx, h.window, "confirm-workspace-close"),
        "Unstaged form input cannot be kept as a draft"
    );
    click(cx, h.window, "keep-workspaces");
    cx.read(|cx| {
        assert_eq!(
            h.workspaces[1].read(cx).tabs[0]
                .parameters
                .read(cx)
                .value()
                .as_ref(),
            "{\"id\":42}"
        )
    });
}

#[gpui_kit::test]
fn close_clean_failed_recovery_ack_revokes_silent_close(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    h.request(cx, None);
    assert!(!visible(cx, h.window, "workspace-close-guard"));
    let (revision, _) = h.checkpoint();
    h.events
        .send(Event::RecoverySaved(
            revision,
            Err("injected checkpoint failure".into()),
        ))
        .unwrap();
    h.tick(cx);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    h.assert_no_shutdown();
    click(cx, h.window, "keep-workspaces");
    cx.read(|cx| assert!(!h.workspaces[0].read(cx).close_requested));
}

#[gpui_kit::test]
fn close_failed_ack_keeps_edits_and_needs_explicit_loss_acknowledgement(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    stage(cx, &h.workspaces[0]);
    h.request(cx, None);
    click(cx, h.window, "discard-workspace-close");
    let (revision, _) = h.checkpoint();
    h.events
        .send(Event::RecoverySaved(
            revision,
            Err("injected disk failure".into()),
        ))
        .unwrap();
    h.tick(cx);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    assert!(!visible(cx, h.window, "discard-workspace-close"));
    cx.read(|cx| {
        assert_eq!(h.workspaces[0].read(cx).changes.len(), 1);
        assert_eq!(h.workspaces[0].read(cx).recovery_batches().len(), 1);
    });
    h.ack(revision); // A late success cannot erase the failed-close decision.
    h.tick(cx);
    h.assert_no_shutdown();
    click(cx, h.window, "confirm-workspace-close"); // Explicit Close without saving.
    h.finish_shutdown(cx);
    cx.read(|cx| assert_eq!(h.workspaces[0].read(cx).recovery_batches().len(), 1));
}

#[gpui_kit::test]
fn close_disconnected_writer_and_state_write_failure_never_silently_close(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    h.recovery.close();
    h.request(cx, None);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    h.assert_no_shutdown();
    click(cx, h.window, "keep-workspaces");
    let mut other = Harness::new(cx, runtime.handle(), None);
    other.request(cx, None);
    let (revision, _) = other.checkpoint();
    other.ack(revision);
    other
        .events
        .send(Event::StateSaved(1, Err("injected state failure".into())))
        .unwrap();
    other.tick(cx);
    assert!(visible(cx, other.window, "workspace-close-guard"));
    other.assert_no_shutdown();
}

#[gpui_kit::test]
fn close_main_covers_satellites_and_rejects_stale_satellite_close_epoch(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), None);
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.shell_tx
                .send(Command::Move {
                    route: s.tx.route,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
        })
    });
    h.tick(cx);
    let satellite = cx.update(|cx| cx.windows().into_iter().find(|w| *w != h.window).unwrap());
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.shell_tx
                .send(Command::CloseWindowWorkspace {
                    id: s.tx.route.workspace,
                    host_epoch: s.host_epoch.wrapping_sub(1),
                })
                .unwrap();
        })
    });
    h.tick(cx);
    cx.read(|cx| assert!(!h.workspaces[0].read(cx).close_requested));
    stage(cx, &h.workspaces[0]);
    h.request(cx, None);
    cx.read(|cx| assert!(h.workspaces.iter().all(|s| s.read(cx).close_requested)));
    click(cx, h.window, "confirm-workspace-close");
    let (revision, saved) = h.checkpoint();
    assert_eq!(saved.tabs.len(), 2);
    assert_eq!(saved.workspaces[0].staged.len(), 1);
    h.ack(revision);
    h.tick(cx);
    assert!(alive(cx, satellite));
    h.finish_shutdown(cx);
    assert!(!alive(cx, satellite));
}

fn single_draft() -> SessionState {
    SessionState {
        version: crate::session::FORMAT_VERSION,
        active_tab: 9,
        tabs: vec![TabBuffer {
            id: 9,
            connection_id: None,
            title: "Only draft".into(),
            sql: "SELECT 'keep my SQL'".into(),
        }],
        ..Default::default()
    }
}

#[gpui_kit::test]
fn close_connection_home_waits_for_checkpoint_and_both_writer_acks(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), Some(SessionState::default()));
    assert!(h.workspaces.is_empty());
    assert!(visible(cx, h.window, "connection-manager"));
    h.request(cx, None);
    let (revision, snapshot) = h.checkpoint();
    assert!(snapshot.tabs.is_empty() && snapshot.workspaces.is_empty());
    assert!(!visible(cx, h.window, "connection-manager"));
    assert!(alive(cx, h.window));
    h.ack(revision);
    h.tick(cx);
    h.finish_shutdown(cx);
}

#[gpui_kit::test]
fn close_connection_home_respects_busy_forms_and_cancel_returns_to_browser(
    cx: &mut TestAppContext,
) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), Some(SessionState::default()));
    click(cx, h.window, "manager-new");
    let editor = cx.read(|cx| h.shell.read(cx).test_manager_editor().unwrap());
    cx.update(|cx| editor.update(cx, |s, _| s.busy = true));
    h.request(cx, None);
    cx.read(|cx| assert!(h.shell.read(cx).test_manager_editor().is_some()));
    h.assert_no_shutdown();
    cx.update(|cx| editor.update(cx, |s, _| s.busy = false));
    h.request(cx, None); // Idle form backs out before quitting.
    assert!(visible(cx, h.window, "connection-manager"));
    cx.read(|cx| assert!(h.shell.read(cx).test_manager_editor().is_none()));
    h.assert_no_shutdown();
    h.recovery.close();
    h.request(cx, None);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    click(cx, h.window, "keep-workspaces");
    assert!(visible(cx, h.window, "connection-manager"));
    assert!(h.tick(cx).is_empty());
    h.assert_no_shutdown();
}

#[gpui_kit::test]
fn close_last_main_workspace_opens_connections_and_keeps_reopenable_sql(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), Some(single_draft()));
    h.request(cx, Some(0));
    let (revision, snapshot) = h.checkpoint();
    assert!(!snapshot.workspaces[0].open);
    assert_eq!(snapshot.tabs[0].sql, "SELECT 'keep my SQL'");
    h.ack(revision);
    assert!(h.tick(cx).is_empty());
    assert!(visible(cx, h.window, "connection-manager"));
    assert!(!visible(cx, h.window, "manager-close"));
    click(cx, h.window, "manager-draft");
    let reopened = h.tick(cx);
    assert_eq!(reopened.len(), 1);
    cx.read(|cx| {
        let s = reopened[0].read(cx);
        assert_eq!(s.tabs[0].id, 9);
        assert_eq!(
            s.tabs[0].editor.read(cx).value().as_ref(),
            "SELECT 'keep my SQL'"
        );
        assert_eq!(s.query_run_invocations, 0);
    });
    assert!(!visible(cx, h.window, "connection-manager"));
}

#[gpui_kit::test]
fn close_satellite_from_connection_home_keeps_browser_and_archives_sql(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut h = Harness::new(cx, runtime.handle(), Some(single_draft()));
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.shell_tx
                .send(Command::Move {
                    route: s.tx.route,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
        })
    });
    h.tick(cx);
    let satellite = cx.read(|cx| {
        h.shell
            .read(cx)
            .test_workspace_host(h.workspaces[0].read(cx).workspace_id)
            .unwrap()
    });
    assert!(visible(cx, h.window, "connection-manager"));
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.shell_tx
                .send(Command::CloseWindowWorkspace {
                    id: s.workspace_id,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
        })
    });
    h.tick(cx);
    let (revision, snapshot) = h.checkpoint();
    assert!(!snapshot.workspaces[0].open);
    h.ack(revision);
    assert!(h.tick(cx).is_empty());
    assert!(!alive(cx, satellite));
    assert!(visible(cx, h.window, "connection-manager"));
    assert_eq!(snapshot.tabs[0].sql, "SELECT 'keep my SQL'");
}

#[gpui_kit::test]
fn close_main_from_connection_home_still_guards_satellite_edits(cx: &mut TestAppContext) {
    let runtime = crate::app_runtime::AppRuntime::new().unwrap();
    let mut session = single_draft();
    session.tabs[0].connection_id = Some("a".into());
    let mut h = Harness::new(cx, runtime.handle(), Some(session));
    cx.update(|cx| {
        h.workspaces[0].update(cx, |s, _| {
            s.shell_tx
                .send(Command::Move {
                    route: s.tx.route,
                    host_epoch: s.host_epoch,
                })
                .unwrap();
        })
    });
    h.tick(cx);
    assert!(visible(cx, h.window, "connection-manager"));
    stage(cx, &h.workspaces[0]);
    h.request(cx, None);
    assert!(visible(cx, h.window, "workspace-close-guard"));
    cx.read(|cx| assert!(h.workspaces[0].read(cx).close_requested));
    click(cx, h.window, "keep-workspaces");
    assert!(visible(cx, h.window, "connection-manager"));
    cx.read(|cx| {
        let s = h.workspaces[0].read(cx);
        assert!(!s.close_requested && !s.shared.borrow().shell_modal_open);
    });
    h.request(cx, None);
    click(cx, h.window, "confirm-workspace-close");
    let (revision, snapshot) = h.checkpoint();
    assert_eq!(snapshot.workspaces[0].staged.len(), 1);
    h.ack(revision);
    h.tick(cx);
    h.finish_shutdown(cx);
}
