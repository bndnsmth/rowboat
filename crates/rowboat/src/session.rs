//! SQL-buffer and bounded detached staged-edit recovery. No passwords, parameters or result sets.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

// Version 5 retains saved-query identity and original file fingerprints. Older
// binaries must not silently discard document bindings while saving recovery.
// Version 4 also requires stale/ApplyStarted handling for staged changes.
pub const FORMAT_VERSION: u32 = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    pub active_tab: usize,
    pub tabs: Vec<TabBuffer>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceBuffer>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBuffer {
    #[serde(default)]
    pub documents: std::collections::BTreeMap<usize, rowboat_services::query_library::SqlDocument>,
    #[serde(default)]
    pub staged: Vec<crate::recovery_ui::RecoveryBatch>,
    pub connection_id: Option<String>,
    pub active_tab: usize,
    pub split_tab: Option<usize>,
    pub focused_pane: usize,
    pub open: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabBuffer {
    pub id: usize,
    pub connection_id: Option<String>,
    pub title: String,
    pub sql: String,
}
pub struct SessionStore {
    directory: PathBuf,
    _lock: fs::File,
}
impl SessionStore {
    pub fn load(&self) -> Result<SessionState> {
        let path = self.directory.join("session.json");
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SessionState {
                    version: 1,
                    ..Default::default()
                });
            }
            Err(e) => return Err(e).context("Open SQL recovery file"),
        };
        ensure!(
            file.metadata()?.is_file(),
            "SQL recovery file is not a regular file"
        );
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        ensure!(
            file.metadata()?.len() <= 64 * 1024 * 1024,
            "Recovery file exceeds 64 MiB limit"
        );
        let state: SessionState = serde_json::from_reader(file)
            .context("SQL recovery file is invalid; original was preserved")?;
        validate(&state)?;
        Ok(state)
    }
    pub fn save(&self, state: &SessionState) -> Result<()> {
        validate(state)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut file, state)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("session.json"))?;
        fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub(crate) fn at(path: &std::path::Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("session.lock"))?;
        ensure!(
            lock.metadata()?.is_file(),
            "SQL recovery lock is not a regular file"
        );
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        // SAFETY: this is an owned live file descriptor; flock does not dereference pointers.
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        ensure!(
            locked == 0,
            "Another Rowboat process owns SQL recovery in this data directory. Close it or use another ROWBOAT_DATA_DIR"
        );
        Ok(Self {
            directory: path.into(),
            _lock: lock,
        })
    }
}
fn validate(state: &SessionState) -> Result<()> {
    ensure!(
        serde_json::to_vec(state)?.len() <= 48 * 1024 * 1024,
        "Recovery exceeds 48 MiB serialized limit"
    );
    ensure!(
        (1..=FORMAT_VERSION).contains(&state.version),
        "Unsupported SQL recovery version {}",
        state.version
    );
    let ids: std::collections::HashSet<_> = state.tabs.iter().map(|t| t.id).collect();
    ensure!(
        ids.len() == state.tabs.len(),
        "Duplicate SQL recovery tab IDs"
    );
    ensure!(
        state.tabs.is_empty() || ids.contains(&state.active_tab),
        "SQL recovery active tab is missing"
    );
    let mut owners = std::collections::HashSet::new();
    let mut batch_ids = std::collections::HashSet::new();
    for layout in &state.workspaces {
        for (id, document) in &layout.documents {
            ensure!(
                state
                    .tabs
                    .iter()
                    .any(|tab| tab.id == *id && tab.connection_id == layout.connection_id),
                "Recovered SQL document belongs to a missing tab or another workspace"
            );
            document.validate()?;
        }
        ensure!(
            layout.staged.len() <= 16,
            "Too many detached recovery batches"
        );
        for batch in &layout.staged {
            ensure!(
                batch_ids.insert(&batch.id),
                "Duplicate recovery batch identity"
            );
            batch.validate()?;
            ensure!(
                layout.connection_id.as_ref() == Some(&batch.owner),
                "Recovery batch belongs to another workspace"
            );
        }
        ensure!(
            owners.insert(&layout.connection_id),
            "Duplicate recovery workspace"
        );
        ensure!(
            layout.focused_pane < 2 && (layout.focused_pane == 0 || layout.split_tab.is_some()),
            "Invalid focused recovery pane"
        );
        ensure!(
            layout.split_tab != Some(layout.active_tab),
            "Recovery panes must own different tabs"
        );
        for id in std::iter::once(layout.active_tab).chain(layout.split_tab) {
            ensure!(
                state
                    .tabs
                    .iter()
                    .any(|t| t.id == id && t.connection_id == layout.connection_id),
                "Recovery pane belongs to a different workspace"
            );
        }
    }
    Ok(())
}

use crate::{
    app::{QueryTab, Workbench},
    forms,
};
use gpui_kit::{App, Context, Window};
impl Workbench {
    pub fn session_snapshot(&self, cx: &App) -> SessionState {
        SessionState {
            version: FORMAT_VERSION,
            workspaces: vec![WorkspaceBuffer {
                documents: self.library_document_snapshot(),
                staged: self.recovery_batches(),
                connection_id: self.owner.clone(),
                active_tab: self.panes.tabs[0].unwrap_or(self.tabs[self.active_tab].id),
                split_tab: self.panes.tabs[1],
                focused_pane: self.panes.focused,
                open: true,
            }],
            active_tab: self.tabs[self.active_tab].id,
            tabs: self
                .tabs
                .iter()
                .map(|t| TabBuffer {
                    id: t.id,
                    connection_id: t.connection_id.clone(),
                    title: t.title.clone(),
                    sql: t.editor.read(cx).value().to_string(),
                })
                .collect(),
        }
    }
    pub fn queue_session_save(&mut self, cx: &mut Context<Self>) {
        // Only the shell owns the recovery lock and combines all workspaces.
        cx.notify();
    }
    pub fn restore_buffers(
        &mut self,
        buffers: Vec<TabBuffer>,
        layout: Option<WorkspaceBuffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let documents = layout
            .as_ref()
            .map(|layout| layout.documents.clone())
            .unwrap_or_default();
        self.session_ready = true;
        self.recovery.detached = layout
            .as_ref()
            .map(|l| l.staged.clone())
            .unwrap_or_default();
        if !buffers.is_empty() {
            let initial = std::mem::take(&mut self.tabs);
            self.tabs = buffers
                .iter()
                .map(|t| QueryTab {
                    id: t.id,
                    connection_id: t.connection_id.clone(),
                    title: t.title.clone(),
                    editor: crate::sql_editor::create(t.sql.clone(), window, cx),
                    parameters: forms::textarea("{}", window, cx),
                    show_parameters: false,
                    results: vec![],
                    metadata_incomplete: vec![],
                    query_progress: None,
                    result_index: 0,
                    error: None,
                })
                .collect();
            self.active_tab = layout
                .as_ref()
                .and_then(|l| self.tabs.iter().position(|t| t.id == l.active_tab))
                .unwrap_or(0);
            if self.startup.sql.is_some() {
                self.tabs.extend(initial);
                self.active_tab = self.tabs.len() - 1;
            }
        }
        self.panes.tabs = [Some(self.tabs[self.active_tab].id), None];
        self.panes.focused = 0;
        if self.startup.sql.is_none()
            && let Some(layout) = layout
        {
            self.panes.tabs = [Some(layout.active_tab), layout.split_tab];
            self.panes.focused = layout.focused_pane.min(1);
            self.reconcile_panes();
        }
        for tab in &self.tabs {
            tab.editor.update(cx, |s, cx| s.set_disabled(false, cx));
        }
        for id in self.tabs.iter().map(|t| t.id).collect::<Vec<_>>() {
            self.observe_editor_focus(id, window, cx);
        }
        self.editor_observers.clear();
        for tab in &self.tabs {
            self.editor_observers
                .insert(tab.id, cx.observe(&tab.editor, |_, _, cx| cx.notify()));
        }
        self.restore_library_documents(&documents);
        self.maybe_startup(window, cx);
        cx.notify();
    }
    pub fn select_connection_workspace(
        &mut self,
        id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let startup_tab = self.startup.tab_id.and_then(|tab_id| {
            self.tabs.iter().position(|t| {
                t.id == tab_id && t.connection_id.as_deref().is_none_or(|owner| owner == id)
            })
        });
        if let Some(i) = startup_tab {
            self.tabs[i].connection_id = Some(id.into());
            self.active_tab = i;
        } else if self.tabs.iter().all(|t| t.connection_id.is_none()) {
            for tab in &mut self.tabs {
                tab.connection_id = Some(id.into());
            }
        }
        for tab in &mut self.tabs {
            if tab.connection_id.as_deref() == Some(id) {
                tab.results.clear();
                tab.error = None;
            }
        }
        self.panes.select(self.tabs[self.active_tab].id);
        self.selected = None;
        self.widths.clear();
        self.query_sort = None;
        self.queue_session_save(cx);
    }
    pub fn connection_label(&self, id: Option<&str>) -> String {
        id.map(|id| {
            self.state
                .connections
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.into())
        })
        .unwrap_or_else(|| "Unassigned draft".into())
    }
    pub fn connect_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = self.tabs[self.active_tab].connection_id.clone();
        if let Some(id) = owner {
            if let Some(profile) = self.state.connections.iter().find(|p| p.id == id).cloned() {
                self.show_connection(Some(profile), window, cx);
            } else {
                self.fail("This workspace's saved connection no longer exists. Copy its SQL into an explicitly chosen connection workspace.".into());
            }
        } else if let Some(profile) = &self.profile {
            self.tabs[self.active_tab].connection_id = Some(profile.id.clone());
            self.status = format!("Draft attached to {}", profile.name);
            self.queue_session_save(cx);
        } else {
            self.show_connection(None, window, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionState, SessionStore, TabBuffer, WorkspaceBuffer, validate};
    #[test]
    fn recovered_document_bindings_reject_cross_workspace_and_missing_tabs() {
        use rowboat_services::{
            Snippet,
            query_library::{SavedQuery, SqlDocument},
        };
        let raw = r#"{"version":4,"active_tab":1,"tabs":[{"id":1,"connection_id":"a","title":"draft","sql":"SELECT 1"},{"id":2,"connection_id":"b","title":"other","sql":"SELECT 2"}],"workspaces":[{"connection_id":"a","active_tab":1,"split_tab":null,"focused_pane":0,"open":true}]}"#;
        let mut state: SessionState = serde_json::from_str(raw).unwrap();
        assert!(state.workspaces[0].documents.is_empty());
        let binding = SqlDocument::Saved {
            baseline: SavedQuery {
                snippet: Snippet {
                    id: "saved".into(),
                    name: "Named".into(),
                    sql: "SELECT 0".into(),
                },
                metadata: Default::default(),
            },
        };
        state.version = super::FORMAT_VERSION;
        state.workspaces[0].documents.insert(1, binding.clone());
        validate(&state).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::at(directory.path()).unwrap();
        store.save(&state).unwrap();
        let before = std::fs::read(directory.path().join("session.json")).unwrap();
        for invalid in [2, 99] {
            state.workspaces[0]
                .documents
                .insert(invalid, binding.clone());
            assert!(store.save(&state).is_err());
            assert_eq!(
                std::fs::read(directory.path().join("session.json")).unwrap(),
                before
            );
            state.workspaces[0].documents.remove(&invalid);
        }
        assert_eq!(store.load().unwrap(), state);
    }
    #[test]
    fn legacy_directory_lock_blocks_rowboat_and_recovery_versions_remain_unchanged() {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("keel");
        fs::create_dir(&old).unwrap();
        let lock_path = old.join("session.lock");
        let old_lock = fs::File::create(&lock_path).unwrap();
        assert_eq!(
            unsafe { libc::flock(old_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let inode = old_lock.metadata().unwrap().ino();
        let selected =
            rowboat_services::resolve_data_directory(None, None, Some(base.path())).unwrap();
        assert_eq!(selected, old);
        assert!(SessionStore::at(&selected).is_err());
        assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
        assert!(!base.path().join("rowboat").exists());
        drop(old_lock);
        let store = SessionStore::at(&selected).unwrap();
        for version in 1..=4 {
            let raw = format!(
                r#"{{"version":{version},"active_tab":7,"tabs":[{{"id":7,"connection_id":"keel-profile","title":"Keel draft","sql":"SELECT 'keel'"}}],"workspaces":[]}}"#
            );
            fs::write(old.join("session.json"), &raw).unwrap();
            let restored = store.load().unwrap();
            assert_eq!(restored.version, version);
            assert_eq!(
                restored.tabs[0].connection_id.as_deref(),
                Some("keel-profile")
            );
            assert_eq!(restored.tabs[0].sql, "SELECT 'keel'");
            assert_eq!(fs::read_to_string(old.join("session.json")).unwrap(), raw);
        }
        assert_eq!(fs::metadata(lock_path).unwrap().ino(), inode);
        assert_eq!(super::FORMAT_VERSION, 5);
    }
    #[test]
    fn second_gui_cannot_write_metadata_in_either_initialization_order() {
        use crate::workspace_shared::{SharedState, StorageReadiness};
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let recovery_writer = std::sync::Arc::new(SessionStore::at(dir.path()).unwrap());
        let metadata_writer = recovery_writer.clone();
        let locked = SessionStore::at(dir.path()).is_ok();
        assert!(!locked);
        for state_first in [true, false] {
            let (save, mut saved) = tokio::sync::mpsc::unbounded_channel();
            let mut state = SharedState {
                state: Default::default(),
                writable: false,
                revision: 0,
                saved_revision: 0,
                next_tab: 0,
                active_workspace: 0,
                shell_modal_open: false,
                save,
            };
            let mut gate = StorageReadiness::default();
            for step in 0..2 {
                if (step == 0) == state_first {
                    gate.state = Some(true);
                } else {
                    gate.locked_recovery = Some(locked);
                }
                state.writable = gate.writable();
                state.edit(|s| {
                    s.connections.push(rowboat_core::ConnectionProfile {
                        name: "must not be saved".into(),
                        ..Default::default()
                    })
                });
                assert_eq!(state.revision, 0);
                assert!(state.state.connections.is_empty());
                assert!(saved.try_recv().is_err());
            }
            assert!(gate.complete());
            assert!(!gate.writable());
        }
        // Metadata and SQL writers share the same guard; either one retains exclusivity.
        drop(recovery_writer);
        assert!(SessionStore::at(dir.path()).is_err());
        let independent = SessionStore::at(other.path()).unwrap();
        drop(metadata_writer);
        assert!(SessionStore::at(dir.path()).is_ok());
        drop(independent);
    }
    #[test]
    fn early_workspace_clicks_cannot_allocate_duplicate_ids_before_recovery_load() {
        use crate::workspace_model::workspace_commands_allowed;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path()).unwrap();
        let saved = SessionState {
            version: 2,
            active_tab: 0,
            tabs: vec![TabBuffer {
                id: 0,
                connection_id: Some("pg".into()),
                title: "Recovered".into(),
                sql: "SELECT 1".into(),
            }],
            workspaces: vec![],
        };
        store.save(&saved).unwrap();
        let mut next_tab = 0;
        let mut opened = vec![];
        for _ in 0..3 {
            if workspace_commands_allowed(false, false) {
                opened.push(next_tab);
                next_tab += 1;
            }
        }
        assert!(opened.is_empty());
        assert_eq!(next_tab, 0);
        let mut recovered = store.load().unwrap();
        next_tab = recovered.tabs.iter().map(|t| t.id).max().unwrap() + 1;
        if workspace_commands_allowed(true, false) {
            opened.push(next_tab);
        }
        assert_eq!(opened, vec![1]);
        recovered.tabs.push(TabBuffer {
            id: opened[0],
            connection_id: Some("my".into()),
            title: "New".into(),
            sql: "SELECT 2".into(),
        });
        store.save(&recovered).unwrap();
        assert_eq!(store.load().unwrap(), recovered);
        assert!(!workspace_commands_allowed(true, true));
    }
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn private_recovery_preserves_connection_ownership_and_does_not_touch_app_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state.json"), "existing application state").unwrap();
        let store = SessionStore::at(dir.path()).unwrap();
        let state = SessionState {
            version: 1,
            workspaces: vec![],
            active_tab: 9,
            tabs: vec![
                TabBuffer {
                    id: 9,
                    connection_id: Some("pg".into()),
                    title: "Draft".into(),
                    sql: "SELECT 'a\nb';".into(),
                },
                TabBuffer {
                    id: 10,
                    connection_id: Some("mysql".into()),
                    title: "Other".into(),
                    sql: "SELECT 2;".into(),
                },
            ],
        };
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        assert_eq!(
            fs::read_to_string(dir.path().join("state.json")).unwrap(),
            "existing application state"
        );
        assert_eq!(
            fs::metadata(dir.path().join("session.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let json = fs::read_to_string(dir.path().join("session.json")).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("parameters"));
        assert!(!json.contains("results"));
    }
    #[test]
    fn malformed_recovery_is_reported_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(&path, "broken").unwrap();
        assert!(SessionStore::at(dir.path()).unwrap().load().is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }
    #[test]
    fn a_second_process_cannot_overwrite_an_active_recovery_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = SessionStore::at(dir.path()).unwrap();
        assert!(SessionStore::at(dir.path()).is_err());
        drop(first);
        assert!(SessionStore::at(dir.path()).is_ok());
    }
    #[test]
    fn recovery_preserves_split_and_closed_workspaces_and_rejects_cross_owner_panes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path()).unwrap();
        let mut state = SessionState {
            version: 2,
            active_tab: 2,
            tabs: vec![
                TabBuffer {
                    id: 1,
                    connection_id: Some("pg".into()),
                    title: "Left".into(),
                    sql: "SELECT 1".into(),
                },
                TabBuffer {
                    id: 2,
                    connection_id: Some("pg".into()),
                    title: "Right".into(),
                    sql: "SELECT 2".into(),
                },
                TabBuffer {
                    id: 3,
                    connection_id: Some("my".into()),
                    title: "Parked".into(),
                    sql: "SELECT 3".into(),
                },
            ],
            workspaces: vec![
                WorkspaceBuffer {
                    documents: Default::default(),
                    staged: vec![],
                    connection_id: Some("pg".into()),
                    active_tab: 1,
                    split_tab: Some(2),
                    focused_pane: 1,
                    open: true,
                },
                WorkspaceBuffer {
                    documents: Default::default(),
                    staged: vec![],
                    connection_id: Some("my".into()),
                    active_tab: 3,
                    split_tab: None,
                    focused_pane: 0,
                    open: false,
                },
            ],
        };
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        state.workspaces[0].split_tab = Some(3);
        assert!(validate(&state).is_err());
        state.workspaces[0].split_tab = Some(1);
        assert!(validate(&state).is_err());
    }
}
