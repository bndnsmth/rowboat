//! Explicit connection trials and recovery. Never replay SQL or rebind staged edits.
use crate::{
    app::{Event, Workbench},
    recovery_ui::RecoveryPhase,
};
use gpui_kit::*;
use rowboat_core::{Database, SqliteOpenMode};
use rowboat_services::Tunnel;
use std::time::Duration;

#[derive(Default)]
pub struct State {
    pub replacement: Option<uuid::Uuid>,
    pub replaced_unknown: bool,
    draft: Option<(uuid::Uuid, String)>,
    checkpoint: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
}
pub enum ConnectionEvent {
    Tested {
        request: uuid::Uuid,
        instance: uuid::Uuid,
        fingerprint: String,
        success: bool,
        message: String,
    },
    Probed {
        request: uuid::Uuid,
        active: Option<bool>,
    },
}

impl Workbench {
    pub(crate) fn prepare_connection_action(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.fill_connection_from_url(window, cx) {
            return false;
        }
        let Some(form) = &mut self.form else {
            return false;
        };
        form.validation_attempted = true;
        if let Some((field, _)) = form.validation_errors(cx).first().copied() {
            form.cancel_test();
            if field.starts_with("ssl_") || field.starts_with("ssh_") {
                form.advanced = true;
            }
            form.reveal_field = Some(field);
            if field == "url" {
                form.scroll.set_offset(point(px(0.), px(0.)));
                form.url.update(cx, |i, cx| i.focus(window, cx));
            } else {
                form.fields[field].update(cx, |i, cx| i.focus(window, cx));
            }
            cx.notify();
            return false;
        }
        self.error = None;
        true
    }
    pub(crate) fn connection_settings_enabled(&self) -> bool {
        !self.busy
            && !self.close_requested
            && self.session_ready
            && !self.shared.borrow().shell_modal_open
            && !self.has_row_draft()
            && !self.has_extension_dialog()
            && self.url_input.is_none()
            && self.management.is_none()
            && self.transfer.is_none()
            && self.advanced.designer.is_none()
            && self.owner.is_some()
            && (!self.transaction_known || (!self.transaction && self.changes.is_empty()))
    }

    pub(crate) fn connection_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connection_settings_enabled() {
            return;
        }
        let profile = self
            .owner
            .as_ref()
            .and_then(|id| self.state.connections.iter().find(|p| &p.id == id))
            .cloned()
            .or_else(|| self.profile.clone());
        self.edit_connection(profile, window, cx);
    }

    pub(crate) fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.close_requested {
            return;
        }
        let Some(form) = &mut self.form else {
            return;
        };
        form.validation_attempted = true;
        if form.test_request.is_some() || form.keyring_loading || form.file_request.is_some() {
            return;
        }
        let mut profile = match form.profile(cx) {
            Ok(profile) => profile,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        let fingerprint = form.fingerprint(cx);
        let instance = form.instance;
        let request = uuid::Uuid::new_v4();
        form.test_request = Some(request);
        form.test_result = None;
        form.test_succeeded = false;
        form.test_stale = false;
        let is_file = form.is_file();
        let password = if is_file {
            String::new()
        } else {
            form.password_value(cx)
        };
        // A trial never creates a database, including when the draft says Create.
        if let Some(file) = &mut profile.file {
            file.mode = SqliteOpenMode::OpenExisting;
        }
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(20), async {
                let (tunnel, endpoint) = if is_file {
                    (None, profile)
                } else {
                    let (tunnel, endpoint) = Tunnel::open(&profile).await.map_err(|e| rowboat_services::connection_issue(rowboat_services::ConnectionStage::Ssh, &e))?;
                    (Some(tunnel), endpoint)
                };
                let stage = if is_file { rowboat_services::ConnectionStage::File } else { rowboat_services::ConnectionStage::Database };
                let db = Database::connect(&endpoint, &password).await.map_err(|e| rowboat_services::connection_issue(stage, &e))?;
                drop(db);
                drop(tunnel);
                Ok::<_, anyhow::Error>(())
            }).await;
            let success = matches!(&result, Ok(Ok(())));
            let message = match result {
                Ok(Ok(())) => "Connection test succeeded · nothing saved; workspace unchanged".into(),
                Ok(Err(error)) => format!("Connection test failed: {error} · nothing saved; workspace unchanged{}", if is_file { " · Test opens existing files only; no database was created." } else { "" }),
                Err(_) => "Connection test timed out after 20 seconds · nothing saved; check reachability/TLS/SSH settings".into(),
            };
            let _ = tx.send(Event::Connection(Box::new(ConnectionEvent::Tested { request, instance, fingerprint, success, message })));
        });
        form.test_task = Some(task.abort_handle());
        cx.notify();
    }

    pub(crate) fn replace_unknown_session(&mut self, cx: &mut Context<Self>) {
        if !self.connection_settings_enabled()
            || self.transaction_known
            || !self.shared.borrow().writable
        {
            return;
        }
        let Some(form) = &self.form else {
            return;
        };
        if !form.confirm_replace || form.keyring_loading || form.file_request.is_some() {
            return;
        }
        if let Err(error) = form.profile(cx) {
            self.fail(error.to_string());
            cx.notify();
            return;
        }
        let request = uuid::Uuid::new_v4();
        self.connection_flow.draft = Some((form.instance, form.fingerprint(cx)));
        self.connection_flow.replacement = Some(request);
        self.busy = true;
        self.error = None;
        self.status = "Checking old session and preserving recovery before replacement…".into();
        let db = self.database.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let active = match db {
                Some(db) => {
                    match tokio::time::timeout(Duration::from_secs(5), db.in_transaction()).await {
                        Ok(Ok(active)) => Some(active),
                        _ => None,
                    }
                }
                None => None,
            };
            let _ = tx.send(Event::Connection(Box::new(ConnectionEvent::Probed {
                request,
                active,
            })));
        });
        cx.notify();
    }

    fn replacement_draft_current(&self, cx: &App) -> bool {
        !self.close_requested
            && self
                .connection_flow
                .draft
                .as_ref()
                .is_some_and(|(instance, fingerprint)| {
                    self.form.as_ref().is_some_and(|f| {
                        f.instance == *instance
                            && f.confirm_replace
                            && f.fingerprint(cx) == *fingerprint
                    })
                })
    }

    fn stop_replacement(&mut self, error: String, cx: &mut Context<Self>) {
        self.connection_flow.replacement = None;
        self.connection_flow.draft = None;
        self.connection_flow.checkpoint = None;
        self.busy = false;
        self.fail(format!("Session was not replaced: {error}"));
        cx.notify();
    }

    pub(crate) fn receive_connection(&mut self, event: ConnectionEvent, cx: &mut Context<Self>) {
        match event {
            ConnectionEvent::Tested {
                request,
                instance,
                fingerprint,
                success,
                message,
            } => {
                if self.close_requested {
                    return;
                }
                let Some(form) = self
                    .form
                    .as_mut()
                    .filter(|f| f.instance == instance && f.test_request == Some(request))
                else {
                    return;
                };
                form.test_request = None;
                form.test_task = None;
                if form.fingerprint(cx) == fingerprint {
                    form.test_result = Some((fingerprint, message));
                    form.test_succeeded = success;
                }
            }
            ConnectionEvent::Probed { request, active } => {
                if self.connection_flow.replacement != Some(request) {
                    return;
                }
                let result = (|| {
                    anyhow::ensure!(
                        self.replacement_draft_current(cx),
                        "Connection draft changed; review and confirm again"
                    );
                    if active == Some(true) {
                        self.transaction = true;
                        self.transaction_known = true;
                        anyhow::bail!(
                            "The old session has an active transaction. Close this form and Commit or Rollback before reconnecting."
                        );
                    }
                    // Capture against the OLD physical session and execution proof.
                    if !self.changes.is_empty() {
                        self.capture_recovery();
                        if let Some(error) = &self.recovery.error {
                            anyhow::bail!("Staged evidence cannot be preserved: {error}");
                        }
                        let old_len = self.recovery.detached.len();
                        self.finish_apply_marker(false);
                        for batch in &mut self.recovery.detached[old_len..] {
                            if batch.phase == RecoveryPhase::Staged {
                                batch.phase = RecoveryPhase::Stale;
                            }
                        }
                    }
                    let (ack, persisted) = tokio::sync::oneshot::channel();
                    self.shell_tx
                        .send(crate::workspace_shell::Command::PersistConnectionRecovery {
                            route: self.tx.route,
                            request,
                            ack,
                        })
                        .map_err(|_| anyhow::anyhow!("Recovery coordinator unavailable"))?;
                    self.connection_flow.checkpoint = Some(persisted);
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(error) = result {
                    self.stop_replacement(format!("{error:#}"), cx);
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn poll_connection_checkpoint(&mut self, cx: &mut Context<Self>) {
        let Some(checkpoint) = &mut self.connection_flow.checkpoint else {
            return;
        };
        let result = match checkpoint.try_recv() {
            Ok(result) => result,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(_) => Err("Recovery coordinator unavailable".into()),
        };
        self.connection_flow.checkpoint = None;
        if let Err(error) = result {
            self.stop_replacement(format!("recovery checkpoint failed: {error}"), cx);
        } else if !self.shared.borrow().writable {
            self.stop_replacement("Profile storage is unavailable; restore local storage before saving and reconnecting".into(), cx);
        } else if !self.replacement_draft_current(cx) {
            self.stop_replacement(
                "Connection draft changed; review and confirm again".into(),
                cx,
            );
        } else {
            self.connection_flow.replacement = None;
            self.connection_flow.draft = None;
            self.busy = false;
            self.retire_unknown_connection(cx);
            self.transaction = false;
            self.transaction_known = true;
            self.startup.autorun = false;
            self.connection_flow.replaced_unknown = true;
            self.save_connection(true, cx);
        }
        cx.notify();
    }
}
