//! A short-lived shell-owned restore lease. No process-global connection registry.
use super::*;
use crate::workspace_model::Route;
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone)]
struct FileIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl FileIdentity {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let metadata = std::fs::metadata(&path)?;
        anyhow::ensure!(metadata.is_file(), "Restore target must be a regular file");
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    fn matches(&self, profile: &ConnectionProfile) -> bool {
        profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File
            && profile.file.as_ref().is_some_and(|file| {
                Self::read(&file.path).is_ok_and(|other| {
                    self.path == other.path
                        || (self.device, self.inode) == (other.device, other.inode)
                })
            })
    }
}

pub(super) struct RestoreLease {
    id: uuid::Uuid,
    owner: Route,
    matching: Vec<u64>,
}

/// Dropping/aborting/panicking a worker must release the UI lease, too.
struct Completion {
    tx: mpsc::Sender<Command>,
    id: uuid::Uuid,
    attempted: bool,
    result: Option<Result<String, String>>,
}
impl Drop for Completion {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::RestoreCompleted {
            id: self.id,
            attempted: self.attempted,
            result: self.result.take().unwrap_or_else(|| {
                Err("Restore worker stopped; inspect the database before retrying".into())
            }),
        });
    }
}

impl WorkspaceShell {
    pub(super) fn reject_during_restore(&self, command: &Command, _: &App) -> bool {
        self.restore.is_some() && !matches!(command, Command::RestoreCompleted { .. })
    }

    pub(super) fn reject_restore(&self, route: Route, error: String, cx: &App) {
        if let Some(workspace) = self
            .workspaces
            .iter()
            .find(|w| w.entity.read(cx).tx.route == route)
        {
            // No provider was entered: retain the existing result/proof on rejection.
            let _ = workspace
                .entity
                .read(cx)
                .tx
                .send(crate::app::Event::TransferFinished {
                    restore: false,
                    result: Err(error),
                });
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin_file_restore(
        &mut self,
        route: Route,
        endpoint: ConnectionProfile,
        path: PathBuf,
        size: u64,
        modified: Option<SystemTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prepared = (|| -> anyhow::Result<_> {
            anyhow::ensure!(
                self.initialized
                    && self.closing.is_none()
                    && self.live_diff.is_none()
                    && self.connection_manager.is_none()
                    && self.restore.is_none(),
                "Finish the current workspace operation before restoring"
            );
            let source = self
                .workspaces
                .iter()
                .find(|w| w.entity.read(cx).tx.route == route)
                .ok_or_else(|| anyhow::anyhow!("Restore owner changed"))?;
            let state = source.entity.read(cx);
            anyhow::ensure!(
                state.busy && state.transfer_running && !state.close_requested,
                "Restore request is no longer active"
            );
            anyhow::ensure!(!endpoint.read_only, "Restore requires a writable profile");
            let target = FileIdentity::read(
                &endpoint
                    .file
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing file target"))?
                    .path,
            )?;
            anyhow::ensure!(
                state
                    .endpoint
                    .as_ref()
                    .is_some_and(|p| p.id == endpoint.id && target.matches(p)),
                "Restore target changed"
            );
            let mut matching = Vec::new();
            let mut handles = Vec::new();
            for workspace in &self.workspaces {
                let state = workspace.entity.read(cx);
                if !state.profile.as_ref().is_some_and(|p| target.matches(p))
                    && !state.endpoint.as_ref().is_some_and(|p| target.matches(p))
                {
                    continue;
                }
                let own = state.tx.route == route;
                let guard = state.close_state();
                anyhow::ensure!(
                    (own || !guard.busy)
                        && !guard.transaction
                        && !guard.unknown_transaction
                        && guard.staged == 0
                        && !state.has_row_draft()
                        && !state.has_extension_dialog()
                        && state.form.is_none()
                        && state.management.is_none()
                        && state.query_edit.batch.is_none()
                        && state.query_edit.draft.is_none()
                        && (own || !guard.drafts),
                    "Finish running work, transactions and staged edits/drafts in same-file workspace '{}' before restoring",
                    state
                        .profile
                        .as_ref()
                        .map_or("unnamed", |p| p.name.as_str())
                );
                matching.push(workspace.id);
                if let Some(db) = &state.database {
                    handles.push(db.clone());
                }
            }
            Ok((matching, handles))
        })();
        let (matching, handles) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.reject_restore(route, format!("{error:#}"), cx);
                return;
            }
        };
        let id = uuid::Uuid::new_v4();
        self.restore = Some(RestoreLease {
            id,
            owner: route,
            matching,
        });
        self.shared.borrow_mut().shell_modal_open = true;
        // Freeze every host's entry points: a different/draft profile cannot be
        // repointed at the target while the backend check and restore are pending.
        // Existing unrelated transactions and operations are not changed.
        for workspace in &self.workspaces {
            workspace.update_in(window, cx, |s, w, cx| {
                s.close_requested = true;
                s.invalidate_completions(cx);
                for tab in &s.tabs {
                    crate::sql_editor::set_frozen(&tab.editor, true, cx);
                }
                w.focus(&s.focus, cx);
                cx.notify();
            });
        }
        let completion = Completion {
            tx: self.command_tx.clone(),
            id,
            attempted: false,
            result: None,
        };
        self.runtime.spawn(async move {
            let mut completion = completion;
            completion.result = Some(async {
                for db in handles {
                    let active = tokio::time::timeout(Duration::from_secs(5), db.in_transaction()).await
                        .map_err(|_| anyhow::anyhow!("Timed out checking same-file transaction state"))??;
                    anyhow::ensure!(!active, "A same-file database reports an active transaction; finish it before restoring");
                }
                let metadata = tokio::fs::metadata(&path).await?;
                anyhow::ensure!(metadata.len() == size && metadata.modified().ok() == modified, "The backup file changed after review; choose it again");
                // From here, failures are treated conservatively: revoke proofs.
                completion.attempted = true;
                rowboat_services::restore(&endpoint, "", &path, &rowboat_services::ClientRuntime::Native).await?;
                Ok::<_, anyhow::Error>("Database restore completed · prior same-file results invalidated".to_owned())
            }.await.map_err(|e| format!("{e:#}")));
            drop(completion);
        });
        cx.notify();
    }

    pub(super) fn finish_file_restore(
        &mut self,
        id: uuid::Uuid,
        attempted: bool,
        result: Result<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.restore.as_ref().is_some_and(|lease| lease.id == id) {
            return;
        }
        let lease = self.restore.take().unwrap();
        for workspace in &self.workspaces {
            workspace.update_in(window, cx, |s, _, cx| {
                if attempted && lease.matching.contains(&workspace.id) {
                    s.invalidate_restore_results();
                    s.status =
                        "Database restore finished · rerun explicitly for fresh results".into();
                }
                s.close_requested = false;
                for tab in &s.tabs {
                    crate::sql_editor::set_frozen(&tab.editor, false, cx);
                }
                cx.notify();
            });
        }
        self.shared.borrow_mut().shell_modal_open = false;
        if let Some(owner) = self
            .workspaces
            .iter()
            .find(|w| w.entity.read(cx).tx.route == lease.owner)
        {
            let _ = owner
                .entity
                .read(cx)
                .tx
                .send(crate::app::Event::TransferFinished {
                    restore: attempted,
                    result,
                });
        }
        cx.notify();
    }
}
