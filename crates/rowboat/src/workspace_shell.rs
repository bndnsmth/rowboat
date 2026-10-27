//! Window shell: retained workspace entities, one shared state/recovery writer.
#[path = "connection_manager_ui.rs"]
mod connection_manager_ui;
#[path = "live_diff_ui.rs"]
mod live_diff_ui;
#[path = "restore_coordinator.rs"]
mod restore_coordinator;
use crate::{
    app::{Startup, Workbench},
    session::{SessionState, TabBuffer, WorkspaceBuffer},
    workspace_model::CloseState,
    workspace_shared::{Shared, SharedState},
    workspace_storage::{Event, Write, Writers},
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::ConnectionProfile;
use rowboat_services::AppState;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

pub struct WorkspaceInit {
    pub id: u64,
    pub owner: Option<String>,
    pub shared: Shared,
    pub shell_tx: mpsc::Sender<Command>,
}
pub(crate) fn satellite_min_size() -> Size<Pixels> {
    // The workspace split requires 190px of sidebar and 600px of content.
    // Keep the host wide enough that right-aligned toolbar menus remain reachable.
    size(px(800.), px(480.))
}
pub enum Command {
    Connections {
        new_connection: bool,
    },
    ProfileRemoved {
        request: uuid::Uuid,
        result: Result<(), String>,
    },
    RestoreFile {
        route: crate::workspace_model::Route,
        endpoint: Box<ConnectionProfile>,
        path: std::path::PathBuf,
        size: u64,
        modified: Option<std::time::SystemTime>,
    },
    RestoreCompleted {
        id: uuid::Uuid,
        attempted: bool,
        result: Result<String, String>,
    },
    PersistConnectionRecovery {
        route: crate::workspace_model::Route,
        request: uuid::Uuid,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    OpenDatabase {
        route: crate::workspace_model::Route,
        host_epoch: u64,
        database: String,
    },
    PersistApply {
        route: crate::workspace_model::Route,
        marker: String,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    SaveProfile {
        route: crate::workspace_model::Route,
        save: Box<crate::app::ProfileSave>,
    },
    OpenWindow {
        profile: Box<ConnectionProfile>,
    },
    CloseWindowWorkspace {
        id: u64,
        host_epoch: u64,
    },
    Move {
        route: crate::workspace_model::Route,
        host_epoch: u64,
    },
    Open {
        profile: Option<Box<ConnectionProfile>>,
        password: Option<String>,
    },
    OpenQuery {
        owner: Option<String>,
        sql: String,
        title: String,
    },
}
struct Workspace {
    id: u64,
    entity: Entity<Workbench>,
    _observer: Subscription,
    host: Option<AnyWindowHandle>,
}
impl Workspace {
    fn update_in<R>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Workbench, &mut Window, &mut Context<Workbench>) -> R,
    ) -> Option<R> {
        if let Some(host) = self.host {
            host.update(cx, |_, w, cx| self.entity.update(cx, |s, cx| f(s, w, cx)))
                .ok()
        } else {
            Some(self.entity.update(cx, |s, cx| f(s, window, cx)))
        }
    }
}
struct Archived {
    owner: Option<String>,
    tabs: Vec<TabBuffer>,
    layout: WorkspaceBuffer,
}
struct PendingApply {
    revision: u64,
    route: crate::workspace_model::Route,
    marker: Checkpoint,
    ack: tokio::sync::oneshot::Sender<Result<(), String>>,
}
enum Checkpoint {
    Apply(String),
    Connection(uuid::Uuid),
}
impl Checkpoint {
    fn matches(&self, workspace: &Workbench) -> bool {
        match self {
            Self::Apply(marker) => workspace.has_apply_marker(marker),
            Self::Connection(request) => workspace.connection_flow.replacement == Some(*request),
        }
    }
}
#[derive(Clone, Copy)]
enum Closing {
    Window,
    Workspace(u64),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseChoice {
    Keep,
    Discard,
    WithoutRecovery,
}
pub struct WorkspaceShell {
    connection_manager: Option<connection_manager_ui::Manager>,
    restore: Option<restore_coordinator::RestoreLease>,
    live_diff: Option<live_diff_ui::LiveDiffDialog>,
    runtime: tokio::runtime::Handle,
    shared: Shared,
    shortcut_bindings: BTreeMap<String, String>,
    workspaces: Vec<Workspace>,
    ownership: crate::workspace_model::WindowOwnership,
    archived: Vec<Archived>,
    active: u64,
    next_workspace: u64,
    commands: mpsc::Receiver<Command>,
    command_tx: mpsc::Sender<Command>,
    events: mpsc::Receiver<Event>,
    recovery_tx: tokio::sync::mpsc::UnboundedSender<Write<SessionState>>,
    shutting_down: bool,
    writers_stopped: usize,
    recovery_writable: bool,
    recovery_revision: u64,
    recovery_saved: u64,
    pending_applies: Vec<PendingApply>,
    last_snapshot: Option<SessionState>,
    shutdown_snapshot: Option<SessionState>,
    last_save: Instant,
    storage_access: crate::workspace_shared::StorageReadiness,
    recovery_loaded: Option<SessionState>,
    initialized: bool,
    startup: Option<Startup>,
    closing: Option<Closing>,
    closing_after_save: bool,
    closing_silent: bool,
    close_choice: Option<CloseChoice>,
    close_authorized: Option<CloseState>,
    close_discard_evidence: Option<Vec<(u64, String)>>,
    pending_close_checkpoint: bool,
    error: Option<String>,
    focus: FocusHandle,
    modal_focus: FocusHandle,
    _poll: Task<()>,
}
impl WorkspaceShell {
    fn open_window_workspace(
        &mut self,
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !crate::workspace_model::workspace_commands_allowed(
            self.initialized,
            self.closing.is_some()
                || self.restore.is_some()
                || self.connection_manager_blocks_workspaces(),
        ) {
            return;
        }
        // A profile already hosted anywhere is activated, never connected a second time.
        if let Some(existing) = self.ownership.find(&Some(profile.id.clone())) {
            self.activate(existing, window, cx);
            return;
        }
        let id = self.next_workspace;
        self.next_workspace += 1;
        let owner = Some(profile.id.clone());
        let archive = self.archived.iter().find(|a| a.owner == owner);
        let tabs = archive.map(|a| a.tabs.clone()).unwrap_or_default();
        let layout = archive.map(|a| a.layout.clone());
        let init = WorkspaceInit {
            id,
            owner: owner.clone(),
            shared: self.shared.clone(),
            shell_tx: self.command_tx.clone(),
        };
        let runtime = self.runtime.clone();
        let close_tx = self.command_tx.clone();
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(format!("Rowboat — {}", profile.name).into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1280.), px(860.)),
                cx,
            ))),
            window_min_size: Some(size(px(960.), px(640.))),
            ..Default::default()
        };
        match gpui_kit::open_window(options, cx, move |w, cx| {
            let entity = cx.new(|cx| Workbench::new(runtime, Startup::default(), init, w, cx));
            entity.update(cx, |s, cx| {
                s.profile = Some(profile.clone());
                s.restore_buffers(tabs, layout, w, cx);
                s.edit_connection(Some(profile), w, cx);
            });
            w.on_window_should_close(cx, move |_, _| {
                let _ = close_tx.send(Command::CloseWindowWorkspace { id, host_epoch: 0 });
                false
            });
            entity
        }) {
            Ok((host, entity)) => {
                self.ownership.claim(owner.clone(), id);
                self.archived.retain(|a| a.owner != owner);
                let observer = cx.observe(&entity, |_, _, cx| cx.notify());
                self.workspaces.push(Workspace {
                    id,
                    entity,
                    host: Some(host),
                    _observer: observer,
                });
                self.shared.borrow_mut().active_workspace = id;
            }
            Err(error) => self.error = Some(format!("Could not open workspace window: {error:#}")),
        }
        cx.notify();
    }
    pub fn new(
        runtime: tokio::runtime::Handle,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let writers = Writers::spawn(&runtime);
        Self::with_writers(runtime, startup, writers, window, cx)
    }
    pub(crate) fn with_writers(
        runtime: tokio::runtime::Handle,
        startup: Startup,
        writers: Writers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (command_tx, commands) = mpsc::channel();
        crate::theme_controller::ensure(cx);
        cx.observe_window_appearance(window, |_, window, cx| {
            crate::theme_controller::appearance_changed(window.appearance(), cx);
        })
        .detach();
        let Writers {
            state: save,
            recovery: recovery_tx,
            events,
        } = writers;
        let shared = Rc::new(RefCell::new(SharedState {
            state: AppState::default(),
            writable: false,
            revision: 0,
            saved_revision: 0,
            next_tab: 0,
            active_workspace: 0,
            shell_modal_open: false,
            save,
        }));
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let Some(this) = this.upgrade() else {
                    break;
                };
                let _ = this.update_in(cx, |this, w, cx| this.poll(w, cx));
            }
        });
        let focus = cx.focus_handle();
        let entity = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = entity.update(cx, |s, cx| s.request_close(Closing::Window, window, cx));
            false
        });
        Self {
            connection_manager: None,
            restore: None,
            live_diff: None,
            runtime,
            shared,
            shortcut_bindings: rowboat_services::default_shortcuts(),
            workspaces: vec![],
            ownership: Default::default(),
            archived: vec![],
            active: 0,
            next_workspace: 1,
            commands,
            command_tx,
            events,
            recovery_tx,
            shutting_down: false,
            writers_stopped: 0,
            recovery_writable: false,
            recovery_revision: 0,
            recovery_saved: 0,
            pending_applies: Vec::new(),
            last_snapshot: None,
            shutdown_snapshot: None,
            last_save: Instant::now(),
            storage_access: Default::default(),
            recovery_loaded: None,
            initialized: false,
            startup: Some(startup),
            closing: None,
            closing_after_save: false,
            closing_silent: false,
            close_choice: None,
            close_authorized: None,
            close_discard_evidence: None,
            pending_close_checkpoint: false,
            error: None,
            focus,
            modal_focus: cx.focus_handle(),
            _poll: poll,
        }
    }
    fn create_workspace(
        &mut self,
        owner: Option<String>,
        tabs: Vec<TabBuffer>,
        layout: Option<WorkspaceBuffer>,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        if let Some(existing) = self.ownership.find(&owner) {
            return existing;
        }
        if !self.connection_manager_blocks_workspaces() {
            self.connection_manager = None;
        }
        let id = self.next_workspace;
        self.next_workspace += 1;
        let init = WorkspaceInit {
            id,
            owner: owner.clone(),
            shared: self.shared.clone(),
            shell_tx: self.command_tx.clone(),
        };
        let entity = cx.new(|cx| Workbench::new(self.runtime.clone(), startup, init, window, cx));
        let profile = owner.as_ref().and_then(|id| {
            self.shared
                .borrow()
                .state
                .connections
                .iter()
                .find(|p| &p.id == id)
                .cloned()
        });
        entity.update(cx, |s, cx| {
            s.profile = profile;
            s.restore_buffers(tabs, layout, window, cx);
        });
        let observer = cx.observe(&entity, |_, _, cx| cx.notify());
        self.workspaces.push(Workspace {
            id,
            entity,
            _observer: observer,
            host: None,
        });
        self.ownership.claim(owner, id);
        id
    }
    #[cfg(test)]
    #[allow(dead_code)] // Called by the isolated actual-GPUI harness.
    pub(crate) fn test_tick(
        &mut self,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Entity<Workbench>> {
        self.poll(w, cx);
        self.workspaces.iter().map(|w| w.entity.clone()).collect()
    }
    #[cfg(test)]
    #[allow(dead_code)] // Real close request, exposed to the isolated GPUI harness.
    pub(crate) fn test_close_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.request_close(Closing::Window, w, cx);
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_close_workspace(&mut self, id: u64, w: &mut Window, cx: &mut Context<Self>) {
        self.request_close(Closing::Workspace(id), w, cx);
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_defer_autosave(&mut self) {
        self.last_save = Instant::now() + Duration::from_secs(3600);
    }
    fn initialize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.initialized || !self.storage_access.complete() || self.recovery_loaded.is_none() {
            return;
        }
        self.initialized = true;
        let recovery = self.recovery_loaded.take().unwrap();
        self.recovery_writable = self.storage_access.writable();
        self.shared.borrow_mut().writable = self.storage_access.writable();
        self.shared.borrow_mut().next_tab = recovery
            .tabs
            .iter()
            .map(|t| t.id)
            .max()
            .map_or(0, |id| id + 1);
        let mut startup = self.startup.take().unwrap_or_default();
        let startup_owner = startup.connection.as_ref().and_then(|name| {
            self.shared
                .borrow()
                .state
                .connections
                .iter()
                .find(|p| &p.id == name || &p.name == name)
                .map(|p| p.id.clone())
        });
        let has_startup = startup.sql.is_some() || startup.connection.is_some();
        let active_owner = recovery
            .tabs
            .iter()
            .find(|t| t.id == recovery.active_tab)
            .and_then(|t| t.connection_id.clone());
        let mut groups: BTreeMap<Option<String>, Vec<TabBuffer>> = BTreeMap::new();
        for tab in recovery.tabs {
            groups
                .entry(tab.connection_id.clone())
                .or_default()
                .push(tab);
        }
        if has_startup {
            groups.entry(startup_owner.clone()).or_default();
        }
        for (owner, tabs) in groups {
            let layout = recovery
                .workspaces
                .iter()
                .find(|w| w.connection_id == owner)
                .cloned();
            let selected_startup = has_startup && owner == startup_owner;
            if layout.as_ref().is_some_and(|l| !l.open) && !selected_startup {
                self.archived.push(Archived {
                    owner,
                    tabs,
                    layout: layout.unwrap(),
                });
                continue;
            }
            let config = if selected_startup {
                std::mem::take(&mut startup)
            } else {
                Startup::default()
            };
            let id = self.create_workspace(owner.clone(), tabs, layout, config, window, cx);
            if (has_startup && owner == startup_owner) || (!has_startup && owner == active_owner) {
                self.active = id;
            }
        }
        if self.workspaces.is_empty() {
            self.active = 0;
            self.ensure_connection_manager(window, cx);
            cx.notify();
            return;
        }
        if !self.workspaces.iter().any(|w| w.id == self.active) {
            self.active = self.workspaces[0].id;
        }
        self.activate(self.active, window, cx);
    }
    fn activate(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing.is_some() || self.connection_manager_blocks_workspaces() {
            return;
        }
        if let Some(workspace) = self.workspaces.iter().find(|w| w.id == id) {
            if let Some(host) = workspace.host {
                let entity = workspace.entity.clone();
                self.shared.borrow_mut().active_workspace = id;
                let _ = host.update(cx, |_, w, cx| {
                    w.activate_window();
                    let focus = entity.read(cx).focus.clone();
                    w.focus(&focus, cx);
                });
                return;
            }
            if self.connection_manager.take().is_some() {
                self.shared.borrow_mut().shell_modal_open = false;
            }
            self.active = id;
            self.shared.borrow_mut().active_workspace = id;
            workspace
                .entity
                .update(cx, |s, cx| window.focus(&s.focus, cx));
            cx.notify();
        }
    }
    fn open(
        &mut self,
        profile: Option<ConnectionProfile>,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !crate::workspace_model::workspace_commands_allowed(
            self.initialized,
            self.closing.is_some()
                || self.restore.is_some()
                || self.connection_manager_blocks_workspaces(),
        ) {
            return;
        }
        let owner = profile.as_ref().map(|p| p.id.clone());
        let existing = self.ownership.find(&owner);
        let id = existing.unwrap_or_else(|| {
            let archive = self
                .archived
                .iter()
                .position(|a| a.owner == owner)
                .map(|i| self.archived.remove(i));
            let (tabs, layout) = archive
                .map(|a| (a.tabs, Some(a.layout)))
                .unwrap_or_default();
            self.create_workspace(owner, tabs, layout, Startup::default(), window, cx)
        });
        self.activate(id, window, cx);
        let workspace = self.workspaces.iter().find(|w| w.id == id).unwrap();
        workspace.update_in(window, cx, |s, window, cx| {
            if !s.transaction_known && !s.busy && s.form.is_none() {
                s.connection_settings(window, cx);
                return;
            }
            if s.database.is_some() || s.busy || s.form.is_some() {
                return;
            }
            if let (Some(p), Some(password)) = (&profile, password) {
                s.passwords.insert(p.id.clone(), password);
            }
            s.edit_connection(profile, window, cx);
        });
    }
    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shared.borrow_mut().shell_modal_open = self.live_diff.is_some()
            || self.closing.is_some()
            || self.restore.is_some()
            || self.connection_manager_blocks_workspaces();
        let preferences = self.shared.borrow().state.preferences.clone();
        match crate::preferences_ui::bind_shortcuts(&self.shortcut_bindings, &preferences, cx) {
            Ok(bindings) => self.shortcut_bindings = bindings,
            Err(e) => self.error = Some(format!("Unable to apply keyboard preferences: {e:#}")),
        }
        self.poll_live_diff(window, cx);
        for event in self.events.try_iter().collect::<Vec<_>>() {
            match event {
                Event::WriterStopped => {
                    self.writers_stopped += 1;
                    if !self.shutting_down {
                        self.recovery_writable = false;
                        self.shared.borrow_mut().writable = false;
                        self.pause_close(cx);
                        self.error =
                            Some("Recovery writer stopped before closing completed".into());
                    }
                }
                Event::StateLoaded(result) => {
                    self.storage_access.state = Some(result.is_ok());
                    match result {
                        Ok(state) => {
                            let mut shared = self.shared.borrow_mut();
                            shared.state = *state;
                            shared.writable = false;
                        }
                        Err(e) => {
                            self.error = Some(format!(
                                "Application state unavailable: {e}. Profile/history and SQL recovery writes are disabled; existing files are preserved."
                            ))
                        }
                    }
                }
                Event::RecoveryLoaded(result) => match result {
                    Ok(state) => {
                        self.recovery_loaded = Some(state);
                        self.storage_access.locked_recovery = Some(true);
                    }
                    Err(e) => {
                        self.recovery_loaded = Some(SessionState::default());
                        self.storage_access.locked_recovery = Some(false);
                        self.error = Some(format!(
                            "Exclusive SQL recovery unavailable: {e}. Profile/history and SQL recovery writes are disabled; existing files are preserved."
                        ));
                    }
                },
                Event::StateSaved(revision, result) => match result {
                    Ok(()) => self.shared.borrow_mut().saved_revision = revision,
                    Err(e) => {
                        self.shared.borrow_mut().writable = false;
                        if !self.shutting_down {
                            self.pause_close(cx);
                        }
                        self.error = Some(format!("Application state save failed: {e}"));
                    }
                },
                Event::RecoverySaved(revision, result) => match result {
                    Ok(()) => self.recovery_saved = revision,
                    Err(e) => {
                        self.recovery_writable = false;
                        self.shared.borrow_mut().writable = false;
                        if !self.shutting_down {
                            self.pause_close(cx);
                        }
                        self.error = Some(format!("SQL recovery save failed: {e}"));
                    }
                },
            }
            cx.notify();
        }
        crate::theme_controller::set_choice(&self.shared.borrow().state.preferences.theme, cx);
        self.initialize(window, cx);
        for command in self.commands.try_iter().collect::<Vec<_>>() {
            if self.reject_during_restore(&command, cx) {
                match command {
                    Command::PersistApply { ack, .. }
                    | Command::PersistConnectionRecovery { ack, .. } => {
                        let _ = ack.send(Err("Wait for SQLite restore to finish".into()));
                    }
                    Command::SaveProfile { route, .. } => {
                        if let Some(w) = self
                            .workspaces
                            .iter()
                            .find(|w| w.entity.read(cx).tx.route == route)
                        {
                            w.entity.update(cx, |s, cx| {
                                s.busy = false;
                                s.fail("Wait for SQLite restore to finish".into());
                                cx.notify();
                            });
                        }
                    }
                    Command::RestoreFile { route, .. } => {
                        self.reject_restore(route, "Another restore is already running".into(), cx)
                    }
                    _ => {}
                }
                continue;
            }
            match command {
                Command::Connections { new_connection } => {
                    self.show_connection_manager(window, cx);
                    if new_connection {
                        self.manager_edit(None, window, cx);
                    }
                }
                Command::ProfileRemoved { request, result } => {
                    self.manager_removed(request, result, window, cx)
                }
                Command::RestoreFile {
                    route,
                    endpoint,
                    path,
                    size,
                    modified,
                } => {
                    self.begin_file_restore(route, *endpoint, path, size, modified, window, cx);
                }
                Command::RestoreCompleted {
                    id,
                    attempted,
                    result,
                } => {
                    self.finish_file_restore(id, attempted, result, window, cx);
                }
                Command::PersistConnectionRecovery {
                    route,
                    request,
                    ack,
                } => {
                    let marker = Checkpoint::Connection(request);
                    if !self.recovery_writable
                        || self.closing.is_some()
                        || !self.workspaces.iter().any(|w| {
                            let s = w.entity.read(cx);
                            s.tx.route == route && marker.matches(s)
                        })
                    {
                        let _ = ack.send(Err(
                            "Recovery storage unavailable, closing, or replacement owner changed"
                                .into(),
                        ));
                    } else {
                        self.save_recovery(cx);
                        self.pending_applies.push(PendingApply {
                            revision: self.recovery_revision,
                            route,
                            marker,
                            ack,
                        });
                    }
                }
                Command::PersistApply { route, marker, ack } => {
                    if !self.recovery_writable
                        || self.closing.is_some()
                        || !self.workspaces.iter().any(|w| {
                            w.entity.read(cx).tx.route == route
                                && w.entity.read(cx).has_apply_marker(&marker)
                        })
                    {
                        let _ = ack.send(Err(
                            "Recovery storage unavailable, closing, or Apply owner changed".into(),
                        ));
                    } else {
                        self.save_recovery(cx);
                        self.pending_applies.push(PendingApply {
                            revision: self.recovery_revision,
                            route,
                            marker: Checkpoint::Apply(marker),
                            ack,
                        });
                    }
                }
                Command::SaveProfile { route, save } => {
                    if self.manager_save(route, &save, cx) {
                        continue;
                    }
                    if let Some(workspace) =
                        self.workspaces.iter().find(|w| w.id == route.workspace)
                    {
                        let state = workspace.entity.read(cx);
                        if state.tx.route != route {
                            continue;
                        }
                        let previous = state.owner.clone();
                        let assigned = Some(save.profile.id.clone());
                        let result = if self.closing.is_some()
                            || self.connection_manager_blocks_workspaces()
                            || !self.shared.borrow().writable
                        {
                            Err("Profile save blocked while closing or storage unavailable".into())
                        } else if previous.is_some() && previous != assigned {
                            Err("A retained workspace cannot change to another profile".into())
                        } else if previous != assigned
                            && self.archived.iter().any(|a| a.owner == assigned)
                        {
                            Err("Reopen the existing saved workspace instead of assigning its profile to a draft".into())
                        } else {
                            self.ownership.reassign(workspace.id, &previous, assigned)
                        };
                        workspace.entity.update(cx, |s, cx| match result {
                            Ok(()) => s.commit_profile_save(*save, cx),
                            Err(error) => {
                                s.busy = false;
                                s.fail(error);
                                cx.notify();
                            }
                        });
                    }
                }
                Command::OpenDatabase {
                    route,
                    host_epoch,
                    database,
                } => self.open_database_workspace(route, host_epoch, database, window, cx),
                Command::OpenWindow { profile } => self.open_window_workspace(*profile, window, cx),
                Command::Move { route, host_epoch } => {
                    if self.workspaces.iter().any(|w| {
                        w.id == route.workspace
                            && w.entity.read(cx).tx.route == route
                            && w.entity.read(cx).host_epoch == host_epoch
                    }) {
                        self.move_workspace(route.workspace, window, cx);
                    }
                }
                Command::CloseWindowWorkspace { id, host_epoch } => {
                    if self.workspaces.iter().any(|w| {
                        w.id == id && w.host.is_some() && w.entity.read(cx).host_epoch == host_epoch
                    }) {
                        window.activate_window();
                        self.request_close(Closing::Workspace(id), window, cx);
                    }
                }
                Command::Open { profile, password } => {
                    self.open(profile.map(|p| *p), password, window, cx)
                }
                Command::OpenQuery { owner, sql, title } => {
                    if !crate::workspace_model::workspace_commands_allowed(
                        self.initialized,
                        self.closing.is_some() || self.connection_manager_blocks_workspaces(),
                    ) {
                        continue;
                    }
                    let id = if let Some(id) = self.ownership.find(&owner) {
                        id
                    } else {
                        let archive = self
                            .archived
                            .iter()
                            .position(|a| a.owner == owner)
                            .map(|i| self.archived.remove(i));
                        let (tabs, layout) = archive
                            .map(|a| (a.tabs, Some(a.layout)))
                            .unwrap_or_default();
                        self.create_workspace(owner, tabs, layout, Startup::default(), window, cx)
                    };
                    self.activate(id, window, cx);
                    self.workspaces
                        .iter()
                        .find(|w| w.id == id)
                        .unwrap()
                        .update_in(window, cx, |s, window, cx| {
                            let count = s.tabs.len();
                            s.new_query(Some(sql), window, cx);
                            if s.tabs.len() > count {
                                s.tabs[s.active_tab].title = title;
                            }
                        });
                }
            }
        }
        self.poll_connection_manager(window, cx);
        let mut waiting = Vec::new();
        for PendingApply {
            revision,
            route,
            marker,
            ack,
        } in self.pending_applies.drain(..)
        {
            if !self.recovery_writable
                || self.closing.is_some()
                || !self.workspaces.iter().any(|w| {
                    w.entity.read(cx).tx.route == route && marker.matches(w.entity.read(cx))
                })
            {
                let _ = ack.send(Err(
                    "Apply intent was not acknowledged for the current workspace".into(),
                ));
            } else if self.recovery_saved >= revision {
                let _ = ack.send(Ok(()));
            } else {
                waiting.push(PendingApply {
                    revision,
                    route,
                    marker,
                    ack,
                });
            }
        }
        self.pending_applies = waiting;
        if self.initialized
            && !self.shutting_down
            && (self.closing_after_save || self.last_save.elapsed() > Duration::from_millis(600))
        {
            for workspace in &self.workspaces {
                workspace.entity.update(cx, |s, _| s.capture_recovery());
            }
            self.save_recovery(cx);
            self.last_save = Instant::now();
        }
        if self.closing_after_save {
            // Finishing work, late errors, or newly pending edits revoke the
            // previous choice. Never turn a previously clean close into consent.
            if !self.shutting_down
                && (!self.close_authorization_matches(cx)
                    || (self.close_choice != Some(CloseChoice::WithoutRecovery)
                        && !self.close_storage_ready()))
            {
                self.pause_close(cx);
                cx.notify();
            } else if self.saved(cx) {
                self.finish_close(window, cx);
            }
        }
        self.ensure_connection_manager(window, cx);
    }
    fn snapshot(&self, cx: &App) -> SessionState {
        let mut state = SessionState {
            version: crate::session::FORMAT_VERSION,
            ..Default::default()
        };
        for workspace in &self.workspaces {
            let mut snapshot = workspace.entity.read(cx).session_snapshot(cx);
            if self.close_choice == Some(CloseChoice::Discard) && self.close_affects(workspace.id) {
                for layout in &mut snapshot.workspaces {
                    layout.staged.clear();
                }
            }
            if workspace.id == self.active {
                state.active_tab = snapshot.active_tab;
            }
            state.tabs.extend(snapshot.tabs);
            state.workspaces.extend(snapshot.workspaces);
        }
        for archive in &self.archived {
            state.tabs.extend(archive.tabs.clone());
            state.workspaces.push(archive.layout.clone());
        }
        if !self.workspaces.iter().any(|w| w.id == self.active) {
            state.active_tab = state.tabs.first().map_or(0, |t| t.id);
        }
        if let Some(Closing::Workspace(id)) = self.closing.filter(|_| self.closing_after_save)
            && let Some(workspace) = self.workspaces.iter().find(|w| w.id == id)
            && let Some(layout) = state
                .workspaces
                .iter_mut()
                .find(|l| l.connection_id == workspace.entity.read(cx).owner)
        {
            layout.open = false;
        }
        state
    }
    fn save_recovery(&mut self, cx: &App) {
        if !self.recovery_writable {
            return;
        }
        let state = self.snapshot(cx);
        if self.last_snapshot.as_ref() == Some(&state) {
            if !self.closing_after_save {
                // The last successfully queued checkpoint already represents
                // the live, unfiltered workspaces; no compensation is needed.
                self.pending_close_checkpoint = false;
            }
            return;
        }
        self.recovery_revision += 1;
        if self
            .recovery_tx
            .send(Write::Save(self.recovery_revision, state.clone()))
            .is_err()
        {
            self.recovery_writable = false;
            self.pause_close(cx);
            self.error = Some("SQL recovery writer stopped".into());
        } else {
            self.last_snapshot = Some(state);
            // Keep this independent of authorization: a failed state write can
            // revoke Discard while its filtered recovery write is still queued.
            self.pending_close_checkpoint = self.closing_after_save;
        }
    }
    fn saved(&self, cx: &App) -> bool {
        if self.shutting_down {
            return self.shutdown_snapshot.as_ref() == Some(&self.snapshot(cx));
        }
        if self.close_choice == Some(CloseChoice::WithoutRecovery) {
            return true;
        }
        let shared = self.shared.borrow();
        self.close_storage_ready()
            && crate::workspace_model::checkpoint_acknowledged(
                self.recovery_saved,
                self.recovery_revision,
                self.last_snapshot.as_ref(),
                &self.snapshot(cx),
            )
            && shared.saved_revision >= shared.revision
    }
    fn close_affects(&self, id: u64) -> bool {
        match self.closing {
            Some(Closing::Workspace(target)) => id == target,
            Some(Closing::Window) => true,
            None => false,
        }
    }
    fn close_storage_ready(&self) -> bool {
        let shared = self.shared.borrow();
        self.recovery_writable
            && shared.writable
            && !self.recovery_tx.is_closed()
            && !shared.save.is_closed()
    }
    fn pause_close(&mut self, cx: &App) {
        self.closing_after_save = false;
        self.closing_silent = false;
        self.close_choice = None;
        self.close_authorized = None;
        self.close_discard_evidence = None;
        if self.pending_close_checkpoint {
            // FIFO compensation must precede any subsequent Cancel or explicit
            // fallback shutdown. A healthy recovery worker remains usable even
            // when the independent application-state writer has failed.
            // save_recovery clears this flag only after successfully enqueueing
            // the unfiltered snapshot (or finding it already queued).
            self.save_recovery(cx);
        }
    }
    fn discard_evidence(&self, cx: &App) -> Result<Vec<(u64, String)>, serde_json::Error> {
        self.workspaces
            .iter()
            .filter(|w| self.close_affects(w.id))
            .map(|w| {
                let s = w.entity.read(cx);
                serde_json::to_string(&(&s.changes, s.recovery_batches()))
                    .map(|edits| (w.id, edits))
            })
            .collect()
    }
    fn close_authorization_matches(&self, cx: &App) -> bool {
        self.close_authorized == Some(self.close_state(cx))
            && (self.close_choice != Some(CloseChoice::Discard)
                || self.discard_evidence(cx).ok().as_ref() == self.close_discard_evidence.as_ref())
    }
    fn close_state(&self, cx: &App) -> CloseState {
        let mut state = CloseState::aggregate(
            self.workspaces
                .iter()
                .filter(|w| self.close_affects(w.id))
                .map(|w| w.entity.read(cx).close_state()),
        );
        state.error |= self.error.is_some();
        state.drafts |= self.live_diff.is_some();
        state.busy |= self.live_diff.as_ref().is_some_and(|d| d.busy());
        state
    }
    fn request_close(&mut self, closing: Closing, window: &mut Window, cx: &mut Context<Self>) {
        if self.shutting_down || self.closing.is_some() || self.restore.is_some() {
            return;
        }
        if let Some(manager) = &self.connection_manager {
            if self.manager_busy(cx) {
                return;
            }
            if manager.has_pending_dialog() {
                // Back out of an idle profile form/confirmation first; never
                // abandon an in-flight save, import or keyring removal.
                self.dismiss_connection_manager(window, cx);
                return;
            }
            self.connection_manager = None;
        }
        self.closing = Some(closing);
        self.pause_close(cx);
        self.shared.borrow_mut().shell_modal_open = true;
        for w in self.workspaces.iter().filter(|w| self.close_affects(w.id)) {
            w.update_in(window, cx, |s, window, cx| {
                s.close_requested = true;
                s.invalidate_completions(cx);
                for tab in &s.tabs {
                    crate::sql_editor::set_frozen(&tab.editor, true, cx);
                }
                s.capture_recovery();
                window.focus(&s.focus, cx);
                cx.notify();
            });
        }
        window.focus(&self.focus, cx);
        if self.initialized && self.close_state(cx).is_clean() && self.close_storage_ready() {
            self.closing_silent = true;
            self.confirm_close(CloseChoice::Keep, window, cx);
        }
        cx.notify();
    }
    fn cancel_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shutting_down {
            return;
        }
        self.pause_close(cx);
        self.shared.borrow_mut().shell_modal_open = self.live_diff.is_some();
        for w in self.workspaces.iter().filter(|w| self.close_affects(w.id)) {
            w.entity.update(cx, |s, cx| {
                s.close_requested = false;
                for tab in &s.tabs {
                    crate::sql_editor::set_frozen(&tab.editor, false, cx);
                }
                cx.notify();
            });
        }
        self.closing = None;
        self.activate(self.active, window, cx);
        self.ensure_connection_manager(window, cx);
        cx.notify();
    }
    fn confirm_close(&mut self, choice: CloseChoice, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing.is_none()
            || !self.initialized
            || self.close_state(cx).busy
            || self.shutting_down
        {
            return;
        }
        for workspace in self.workspaces.iter().filter(|w| self.close_affects(w.id)) {
            workspace.entity.update(cx, |s, _| s.capture_recovery());
        }
        if (choice != CloseChoice::WithoutRecovery && !self.close_storage_ready())
            || (choice == CloseChoice::Keep && self.capture_failed(cx))
        {
            self.pause_close(cx);
            cx.notify();
            return;
        }
        // WithoutRecovery is an explicit loss acknowledgement, never a fallback
        // for a failed save or a way to claim that old staged recovery was deleted.
        if choice == CloseChoice::WithoutRecovery && self.close_storage_ready() {
            return;
        }
        self.close_choice = Some(choice);
        if choice == CloseChoice::Discard {
            match self.discard_evidence(cx) {
                Ok(evidence) => self.close_discard_evidence = Some(evidence),
                Err(_) => {
                    self.pause_close(cx);
                    self.error = Some("Could not verify the edits selected for discard".into());
                    cx.notify();
                    return;
                }
            }
        }
        self.close_authorized = Some(self.close_state(cx));
        self.closing_after_save = true;
        if choice != CloseChoice::WithoutRecovery {
            self.save_recovery(cx);
        }
        if self.closing_after_save && self.saved(cx) {
            self.finish_close(window, cx);
        }
        cx.notify();
    }
    fn finish_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_state(cx).busy || !self.saved(cx) {
            return;
        }
        if !self.shutting_down && !self.close_authorization_matches(cx) {
            self.pause_close(cx);
            cx.notify();
            return;
        }
        // The authorized checkpoint has been acknowledged and closing is now
        // committed. It must not be compensated while archiving the workspace.
        self.pending_close_checkpoint = false;
        if self.close_choice == Some(CloseChoice::Discard) {
            for workspace in self.workspaces.iter().filter(|w| self.close_affects(w.id)) {
                workspace
                    .entity
                    .update(cx, |s, _| s.discard_recovery_on_close());
            }
        }
        if matches!(self.closing, Some(Closing::Window)) {
            if !self.shutting_down {
                self.shutdown_snapshot = Some(self.snapshot(cx));
            }
            self.stop_writers();
            // Acknowledgements arrive only after both workers release their
            // lock guards. Keep pumping the UI instead of joining here.
            if self.writers_stopped < 2 {
                return;
            }
        }
        match self.closing.take() {
            Some(Closing::Window) => {
                for workspace in &self.workspaces {
                    if let Some(host) = workspace.host {
                        let _ = host.update(cx, |_, w, _| w.remove_window());
                    }
                }
                self.workspaces.clear();
                window.remove_window();
            }
            Some(Closing::Workspace(id)) => {
                if let Some(index) = self.workspaces.iter().position(|w| w.id == id) {
                    let workspace = self.workspaces.remove(index);
                    self.ownership.release(id);
                    let snapshot = workspace.entity.read(cx).session_snapshot(cx);
                    let mut layout = snapshot.workspaces.into_iter().next().unwrap();
                    if self.close_choice == Some(CloseChoice::Discard) {
                        layout.staged.clear();
                    }
                    layout.open = false;
                    self.archived.push(Archived {
                        owner: layout.connection_id.clone(),
                        tabs: snapshot.tabs,
                        layout,
                    });
                    if let Some(host) = workspace.host {
                        let _ = host.update(cx, |_, w, _| w.remove_window());
                    }
                }
                self.closing_after_save = false;
                if self.workspaces.iter().all(|w| w.host.is_some()) {
                    self.active = 0;
                } else if self.active == id {
                    self.active = self
                        .workspaces
                        .iter()
                        .find(|w| w.host.is_none())
                        .unwrap()
                        .id;
                }
                self.cancel_close(window, cx);
            }
            None => {}
        }
    }
    fn capture_failed(&self, cx: &App) -> bool {
        self.workspaces
            .iter()
            .filter(|w| self.close_affects(w.id))
            .any(|w| {
                w.entity.read(cx).recovery.error.is_some() && !w.entity.read(cx).changes.is_empty()
            })
    }
    fn stop_writers(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        let mut shared = self.shared.borrow_mut();
        shared.writable = false;
        self.recovery_writable = false;
        let _ = shared.save.send(Write::Shutdown);
        let _ = self.recovery_tx.send(Write::Shutdown);
    }
    fn open_database_workspace(
        &mut self,
        route: crate::workspace_model::Route,
        epoch: u64,
        database: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.initialized
            || self.closing.is_some()
            || self.restore.is_some()
            || self.live_diff.is_some()
            || self.connection_manager_blocks_workspaces()
            || !self.shared.borrow().writable
        {
            return;
        }
        let Some(source) = self.workspaces.iter().find(|w| w.id == route.workspace) else {
            return;
        };
        let state = source.entity.read(cx);
        if state.tx.route != route
            || state.host_epoch != epoch
            || !state.namespace_switch_allowed()
            || !state.database_choice_exists(&database)
        {
            return;
        }
        let Some(mut profile) = state.profile.clone().filter(|p| {
            p.engine.descriptor().database_selection != rowboat_core::DatabaseSelection::Unsupported
        }) else {
            return;
        };
        let password = state.session_password.clone();
        profile.id = uuid::Uuid::new_v4().to_string();
        profile.name = format!("{} · {}", profile.name, database);
        profile.database = database;
        let id = self.create_workspace(
            Some(profile.id.clone()),
            vec![],
            None,
            Startup::default(),
            window,
            cx,
        );
        self.activate(id, window, cx);
        if let Some(workspace) = self.workspaces.iter().find(|w| w.id == id) {
            workspace.update_in(window, cx, |s, _, cx| {
                s.commit_profile_save(
                    crate::app::ProfileSave {
                        profile,
                        password,
                        connect: true,
                        remember: false,
                        exists: false,
                    },
                    cx,
                )
            });
        }
        cx.notify();
    }
    fn move_workspace(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing.is_some()
            || self.connection_manager_blocks_workspaces()
            || self.live_diff.is_some()
            || self.restore.is_some()
            || !self.initialized
        {
            return;
        }
        let Some(index) = self.workspaces.iter().position(|w| w.id == id) else {
            return;
        };
        let workspace = &self.workspaces[index];
        if workspace.entity.read(cx).modal_open() || workspace.entity.read(cx).library.file_pending
        {
            self.error = Some("Close the workspace's dialog before moving its live session".into());
            return;
        }
        let entity = workspace.entity.clone();
        if let Some(old) = workspace.host {
            self.connection_manager = None;
            self.shared.borrow_mut().shell_modal_open = false;
            entity.update(cx, |s, cx| s.rehost(window, cx));
            self.workspaces[index].host = None;
            self.active = id;
            let _ = old.update(cx, |_, w, _| w.remove_window());
        } else {
            let epoch = entity.read(cx).host_epoch.wrapping_add(1);
            let tx = self.command_tx.clone();
            let content = entity.clone();
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Rowboat — live workspace".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(740.)),
                    cx,
                ))),
                window_min_size: Some(satellite_min_size()),
                ..Default::default()
            };
            match gpui_kit::open_window(options, cx, move |w, cx| {
                w.on_window_should_close(cx, move |_, _| {
                    let _ = tx.send(Command::CloseWindowWorkspace {
                        id,
                        host_epoch: epoch,
                    });
                    false
                });
                content
            }) {
                Ok((host, _)) => {
                    if host
                        .update(cx, |_, w, cx| entity.update(cx, |s, cx| s.rehost(w, cx)))
                        .is_ok()
                    {
                        self.workspaces[index].host = Some(host);
                        if self.active == id {
                            self.active = self
                                .workspaces
                                .iter()
                                .find(|w| w.host.is_none())
                                .map_or(0, |w| w.id);
                        }
                    } else {
                        let _ = host.update(cx, |_, w, _| w.remove_window());
                        self.error = Some("Could not attach workspace to its new host".into());
                    }
                }
                Err(error) => self.error = Some(format!("Could not move workspace: {error:#}")),
            }
        }
        self.shared.borrow_mut().active_workspace = id;
        self.ensure_connection_manager(window, cx);
        cx.notify();
    }
}
impl Drop for WorkspaceShell {
    fn drop(&mut self) {
        self.stop_writers();
    }
}
impl Workbench {
    pub fn close_state(&self) -> CloseState {
        CloseState {
            busy: self.busy || self.library.file_pending || self.diagram.is_loading(),
            transaction: self.transaction,
            unknown_transaction: !self.transaction_known,
            staged: self.changes.len(),
            recovered: if self.changes.is_empty() {
                self.recovery_batches().len()
            } else {
                self.recovery.detached.len()
            },
            error: self.error.is_some()
                || self.recovery.error.is_some()
                || self.tabs.iter().any(|t| t.error.is_some()),
            drafts: self.has_row_draft()
                || self.tabs.iter().any(|tab| tab.show_parameters)
                || self.library.form.is_some()
                || self.diagram.open
                || self.namespace.picker.is_some()
                || self.custom_metrics.editor.is_some()
                || self.snippet_form.is_some()
                || self.access_matrix.is_some()
                || self.column_form.is_some()
                || self.preferences_form.is_some()
                || self.filters.editor.is_some()
                || self.relationships.is_some()
                || self.objects.dialog.is_some()
                || self.data_transfer.is_some()
                || self.dashboards.editor.is_some()
                || self.explain.as_ref().is_some_and(|e| e.open)
                || self.form.is_some()
                || self.management.is_some()
                || self.url_input.is_some()
                || self.transfer.is_some()
                || self.advanced.designer.is_some(),
        }
    }
}
impl Render for WorkspaceShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if (self.closing.is_some() || self.live_diff.is_some() || self.connection_manager.is_some())
            && (self.focus.is_focused(window) || window.focused(cx).is_none())
        {
            window.focus(&self.modal_focus, cx);
        }
        let theme = cx.omarchy().clone();
        let available = self.initialized
            && self.closing.is_none()
            && self.live_diff.is_none()
            && self.restore.is_none()
            && self.connection_manager.is_none();
        let mut tabs = div().flex().items_end().gap_1().h(px(39.));
        for workspace in &self.workspaces {
            let id = workspace.id;
            let s = workspace.entity.read(cx);
            let name = s
                .owner
                .as_ref()
                .map(|id| s.connection_label(Some(id)))
                .unwrap_or_else(|| "Draft workspace".into());
            let target = s
                .profile
                .as_ref()
                .map(crate::profiles_ui::connection_target);
            let name = if let Some(target) = target.as_ref().filter(|_| {
                s.profile.as_ref().is_some_and(|p| {
                    p.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File
                })
            }) {
                format!("{} / {}", name, target.compact)
            } else if let Some(context) = &s.namespace.snapshot {
                format!("{} / {}", name, context.database)
            } else {
                name
            };
            let mut statuses = Vec::new();
            if s.busy {
                statuses.push(("Running".to_string(), theme.accent));
            } else if s.database.is_none() {
                statuses.push(("Offline".to_string(), theme.secondary));
            }
            if s.transaction {
                statuses.push(("TX".to_string(), theme.warning));
            } else if !s.transaction_known {
                statuses.push(("TX?".to_string(), theme.warning));
            }
            if !s.changes.is_empty() {
                statuses.push((format!("{} edits", s.changes.len()), theme.warning));
            }
            if s.error.is_some() {
                statuses.push(("Error".to_string(), theme.danger));
            }
            let active = id == self.active && workspace.host.is_none();
            let engine = s.profile.as_ref().map(|p| p.engine.descriptor().name);
            let mut tooltip = name.clone();
            for (status, _) in &statuses {
                tooltip.push_str(&format!(" · {status}"));
            }
            if let Some(engine) = engine {
                tooltip.push_str(&format!("\n{engine}"));
            }
            if let Some(path) = target.and_then(|target| target.file_path) {
                tooltip.push_str(&format!("\n{path}"));
            }
            if let Some(profile) = s.profile.as_ref().filter(|p| {
                p.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::Network
            }) {
                tooltip.push_str(&format!(
                    "\nHost: {}\nPort: {}\nDatabase: {}",
                    profile.host, profile.port, profile.database,
                ));
            }
            if workspace.host.is_some() {
                tooltip.push_str("\nOpen in another window — click to activate");
            }
            // The title and close action are siblings: closing never activates a tab.
            let tab = ui::Button::new(("workspace", id))
                .accessibility_label(tooltip.clone())
                .selected(active)
                .flex()
                .items_center()
                .gap_2()
                .h_full()
                .min_w_0()
                .max_w(px(340.))
                .px_2()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .font_weight(if active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .hover(|style| style.bg(theme.background))
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .child(
                    ui::icon(if workspace.host.is_some() {
                        ui::IconName::ExternalLink
                    } else {
                        ui::IconName::LayoutDashboard
                    })
                    .size(px(14.)),
                )
                .child(
                    div()
                        .min_w_0()
                        .max_w(px(210.))
                        .truncate()
                        .child(name.clone()),
                )
                // Status always keeps its width; only the title may ellipsize.
                .when(!statuses.is_empty(), |tab| {
                    tab.child(
                        div()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .gap_1()
                            .whitespace_nowrap()
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .children(statuses.iter().map(|(status, color)| {
                                div()
                                    .flex_shrink_0()
                                    .text_color(*color)
                                    .child(status.clone())
                            })),
                    )
                })
                // Engine is still in the tooltip when live state needs this space.
                .when_some(engine.filter(|_| statuses.is_empty()), |tab, engine| {
                    tab.child(
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded(px(3.))
                            .bg(theme.inset)
                            .text_size(px(10.))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(theme.secondary)
                            .child(engine),
                    )
                })
                .on_click(cx.listener(move |s, _, w, cx| s.activate(id, w, cx)));
            tabs = tabs.child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .h(px(if active { 36. } else { 32. }))
                    .rounded_t(px(6.))
                    .border_t_2()
                    .border_color(if active { theme.accent } else { theme.inset })
                    .bg(if active { theme.surface } else { theme.inset })
                    .child(ui::with_tooltip(tab, tooltip))
                    .when(workspace.host.is_some(), |tab| {
                        tab.child(ui::with_tooltip(
                            ui::icon_button(
                                ("move-workspace", id),
                                ui::IconName::ArrowLeft,
                                "Move workspace here",
                                ui::ButtonVariant::Secondary,
                                cx,
                            )
                            .size(px(28.))
                            .disabled(
                                self.closing.is_some()
                                    || self.live_diff.is_some()
                                    || s.modal_open(),
                            )
                            .on_click(cx.listener(move |s, _, w, cx| s.move_workspace(id, w, cx))),
                            "Move this live workspace into this window",
                        ))
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .border_l_1()
                            .border_color(theme.border)
                            .mx_1()
                            .child(ui::with_tooltip(
                                ui::icon_button(
                                    ("close-workspace", id),
                                    ui::IconName::Close,
                                    format!("Close workspace {name}"),
                                    ui::ButtonVariant::Secondary,
                                    cx,
                                )
                                .size(px(28.))
                                .on_click(cx.listener(
                                    move |s, _, w, cx| {
                                        s.request_close(Closing::Workspace(id), w, cx)
                                    },
                                )),
                                format!("Close workspace {name}"),
                            )),
                    ),
            );
        }
        let weak = cx.weak_entity();
        let comparison_available = self
            .workspaces
            .iter()
            .find(|w| w.id == self.active)
            .is_some_and(|w| {
                w.entity
                    .read(cx)
                    .admin_supported(rowboat_admin::providers::Operation::SchemaComparison)
            });
        let workspace_menu = ui::menu(
            "workspace-menu",
            ui::with_tooltip(
                ui::button(
                    "workspace-menu-trigger",
                    "",
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .h(px(30.))
                .accessibility_label("Workspace actions")
                .debug_selector(|| "workspace-menu-trigger".into())
                .child(ui::icon(ui::IconName::Menu).size(px(14.)))
                .child("Workspace")
                .child(ui::icon(ui::IconName::ChevronDown).size(px(12.))),
                "Workspace actions",
            ),
            vec![
                ui::MenuItem::new("New workspace").disabled(!available),
                ui::MenuItem::new("Compare schemas…").disabled(!available || !comparison_available),
                ui::MenuItem::new("Move active workspace to other window")
                    .disabled(!available || self.active == 0),
            ],
            move |index, w, cx| {
                let _ = weak.update(cx, |s, cx| {
                    if !s.initialized
                        || s.closing.is_some()
                        || s.live_diff.is_some()
                        || s.restore.is_some()
                        || s.connection_manager.is_some()
                    {
                        return;
                    }
                    match index {
                        0 => s.show_connection_manager(w, cx),
                        1 => s.show_live_diff(w, cx),
                        2 => s.move_workspace(s.active, w, cx),
                        _ => {}
                    }
                });
            },
        );
        let mut root = div()
            .relative()
            .key_context("Rowboat")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font.clone())
            .track_focus(&self.focus)
            .on_action(cx.listener(|s, _: &crate::app::CommandPalette, w, cx| {
                if s.connection_manager.is_none()
                    && s.closing.is_none()
                    && s.live_diff.is_none()
                    && let Some(workspace) = s
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == s.active)
                {
                    workspace.update_in(w, cx, |s, w, cx| s.show_palette(w, cx));
                }
            }))
            .on_action(cx.listener(|s, _: &crate::app::NewConnection, w, cx| {
                s.show_connection_manager(w, cx);
                s.manager_edit(None, w, cx);
            }))
            .on_action(cx.listener(|s, _: &crate::app::ModalNext, w, cx| {
                crate::keyboard_ui::modal_focus(false, &s.modal_focus, w, cx)
            }))
            .on_action(cx.listener(|s, _: &crate::app::ModalPrevious, w, cx| {
                crate::keyboard_ui::modal_focus(true, &s.modal_focus, w, cx)
            }))
            .on_action(cx.listener(|s, _: &crate::app::Dismiss, w, cx| {
                if s.connection_manager.is_some() {
                    s.dismiss_connection_manager(w, cx);
                } else if s.closing.is_some() && !s.shutting_down {
                    s.cancel_close(w, cx);
                } else if s.live_diff.as_ref().is_some_and(|d| !d.busy()) {
                    s.live_diff = None;
                    s.shared.borrow_mut().shell_modal_open = false;
                    s.activate(s.active, w, cx);
                    cx.notify();
                }
            }))
            .capture_key_down(cx.listener(|s, event: &KeyDownEvent, w, cx| {
                if s.closing.is_none() && s.live_diff.is_none() && s.connection_manager.is_none() {
                    return;
                }
                if event.keystroke.key == "escape" {
                    if s.connection_manager.is_some() {
                        s.dismiss_connection_manager(w, cx);
                    } else if s.closing.is_some() {
                        if !s.shutting_down {
                            s.cancel_close(w, cx);
                        }
                    } else if s.live_diff.as_ref().is_some_and(|d| !d.busy()) {
                        s.live_diff = None;
                        s.shared.borrow_mut().shell_modal_open = false;
                        s.activate(s.active, w, cx);
                    }
                    cx.stop_propagation();
                    cx.notify();
                } else if event.keystroke.key == "tab" {
                    for _ in 0..256 {
                        if event.keystroke.modifiers.shift {
                            w.focus_prev(cx);
                        } else {
                            w.focus_next(cx);
                        }
                        if s.modal_focus.contains_focused(w, cx) {
                            break;
                        }
                    }
                    if !s.modal_focus.contains_focused(w, cx) {
                        w.focus(&s.modal_focus, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .h(px(40.))
                    .gap_2()
                    .px_2()
                    .bg(theme.inset)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .gap_2()
                            .px_1()
                            .child(
                                svg()
                                    .path(crate::brand::LOGO_PATH)
                                    .size(px(24.))
                                    .flex_shrink_0()
                                    .text_color(theme.accent),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Rowboat"),
                            ),
                    )
                    .child(
                        div()
                            .id("live-workspace-tabs")
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll()
                            .child(tabs),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .gap_1()
                            .child(ui::with_tooltip(
                                ui::icon_button(
                                    "new-workspace",
                                    ui::IconName::Plus,
                                    "New workspace",
                                    ui::ButtonVariant::Secondary,
                                    cx,
                                )
                                .size(px(30.))
                                .debug_selector(|| "new-workspace".into())
                                .disabled(!available)
                                .on_click(
                                    cx.listener(|s, _, w, cx| s.show_connection_manager(w, cx)),
                                ),
                                "New workspace",
                            ))
                            .child(workspace_menu)
                            .child(
                                ui::button(
                                    "manage-connections",
                                    "Connections",
                                    ui::ButtonVariant::Secondary,
                                    cx,
                                )
                                .text_size(px(13.))
                                .h(px(30.))
                                .disabled(!available)
                                .debug_selector(|| "manage-connections".into())
                                .child(ui::icon(ui::IconName::Database).size(px(14.)))
                                .on_click(
                                    cx.listener(|s, _, w, cx| s.show_connection_manager(w, cx)),
                                ),
                            ),
                    ),
            );
        if let Some(workspace) = self
            .workspaces
            .iter()
            .find(|w| w.id == self.active && w.host.is_none())
        {
            root = root.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(workspace.entity.clone()),
            );
        } else if !self.initialized {
            let welcome = div()
                .p_6()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Opening Rowboat"),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.secondary)
                        .child("Loading profiles and SQL recovery…"),
                );
            root = root.child(welcome);
        }
        if let Some(error) = &self.error {
            root = root.child(div().p_2().text_color(theme.danger).child(error.clone()));
        }
        if self.restore.is_some() {
            root = root.child(div().p_3().child("Restoring SQLite snapshot · workspace actions are paused until the operation finishes"));
        }
        if self.live_diff.is_some() {
            root = root.child(self.render_live_diff(window, cx));
        }
        if self.connection_manager.is_some() {
            root = root.child(self.render_connection_manager(window, cx));
        }
        if self.closing.is_some() && !self.closing_silent {
            let state = self.close_state(cx);
            let has_edits = state.staged > 0 || state.recovered > 0;
            let storage_ready = self.close_storage_ready() || self.shutting_down;
            let capture_failed = self.capture_failed(cx);
            let waiting = self.closing_after_save;
            let blocked = !self.initialized || state.busy || waiting;
            let mut details = div()
                .flex()
                .flex_col()
                .gap_2()
                .text_size(px(13.))
                .text_color(theme.secondary);
            if !self.initialized {
                details = details.child("Wait for session recovery to finish loading.");
            }
            if state.busy {
                details = details.child(
                    "An operation is still running. Wait for it to finish, or cancel this close.",
                );
            }
            if state.staged > 0 {
                details = details.child(format!(
                    "{} pending row {}.",
                    state.staged,
                    if state.staged == 1 { "edit" } else { "edits" }
                ));
            }
            if state.recovered > 0 {
                details = details.child(format!(
                    "{} saved edit {}.",
                    state.recovered,
                    if state.recovered == 1 {
                        "draft"
                    } else {
                        "drafts"
                    }
                ));
            }
            if self
                .workspaces
                .iter()
                .filter(|w| self.close_affects(w.id))
                .any(|w| {
                    w.entity
                        .read(cx)
                        .recovery_batches()
                        .iter()
                        .any(|batch| batch.phase == crate::recovery_ui::RecoveryPhase::ApplyStarted)
                })
            {
                details = details.child(
                    "Some saved edits may already have been applied. Closing will not undo them.",
                );
            }
            if state.transaction {
                details = details
                    .child("Open transactions will be rolled back, even if you keep drafts.");
            } else if state.unknown_transaction {
                details = details.child("Transaction state is unknown. Closing disconnects the session; it does not commit.");
            }
            if has_edits {
                details = details.child("Keep drafts saves these locally, without applying them.");
            }
            if state.drafts {
                details = details.child("Unstaged form input and parameters will be discarded.");
            }
            if !storage_ready {
                details = details.child(div().text_color(theme.danger).child("Recovery is unavailable. Closing without saving may lose recent SQL and edits; older recovery remains."));
            } else if capture_failed {
                details = details.child(div().text_color(theme.danger).child("Latest edits could not be captured. Cancel to inspect them, or discard them to close."));
            } else if state.error {
                details =
                    details.child("An error needs attention. Cancel to inspect it, or close.");
            } else if state.is_clean() {
                details = details.child("Ready to close. SQL buffers reopen automatically.");
            }
            let buttons = div()
                .flex()
                .flex_shrink_0()
                .flex_wrap()
                .justify_end()
                .gap_2()
                .px_5()
                .py_3()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    ui::button(
                        "keep-workspaces",
                        "Cancel",
                        ui::ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(self.shutting_down)
                    .debug_selector(|| "keep-workspaces".into())
                    .on_click(cx.listener(|s, _, w, cx| s.cancel_close(w, cx))),
                )
                .when(storage_ready && (has_edits || state.drafts), |buttons| {
                    buttons.child(
                        ui::button(
                            "discard-workspace-close",
                            "Discard edits & close",
                            ui::ButtonVariant::Danger,
                            cx,
                        )
                        .debug_selector(|| "discard-workspace-close".into())
                        .disabled(blocked)
                        .on_click(
                            cx.listener(|s, _, w, cx| s.confirm_close(CloseChoice::Discard, w, cx)),
                        ),
                    )
                })
                .when(!storage_ready || has_edits || !state.drafts, |buttons| {
                    buttons.child(
                        ui::button(
                            "confirm-workspace-close",
                            if waiting {
                                "Closing…"
                            } else if !storage_ready {
                                "Close without saving"
                            } else if has_edits {
                                "Keep drafts & close"
                            } else if state.transaction {
                                "Roll back & close"
                            } else {
                                "Close"
                            },
                            ui::ButtonVariant::Primary,
                            cx,
                        )
                        .bg(theme.accent)
                        .text_color(theme.on_accent)
                        .hover(|style| style.bg(theme.accent).border_color(theme.foreground))
                        .focus_visible(|style| {
                            style.bg(theme.accent).border_color(theme.foreground)
                        })
                        .active(|style| style.bg(theme.accent))
                        .debug_selector(|| "confirm-workspace-close".into())
                        .disabled(blocked || (storage_ready && capture_failed))
                        .on_click(cx.listener(move |s, _, w, cx| {
                            s.confirm_close(
                                if storage_ready {
                                    CloseChoice::Keep
                                } else {
                                    CloseChoice::WithoutRecovery
                                },
                                w,
                                cx,
                            )
                        })),
                    )
                });
            root = root.child(
                div()
                    .occlude()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(black().opacity(0.6))
                    .child(
                        div()
                            .id("workspace-close-guard")
                            .debug_selector(|| "workspace-close-guard".into())
                            .track_focus(&self.modal_focus)
                            .w(px(480.).min(window.viewport_size().width - px(32.)))
                            .max_h(window.viewport_size().height - px(60.))
                            .overflow_y_scroll()
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                            .rounded(px(8.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .p_5()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_size(px(16.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(
                                                if matches!(self.closing, Some(Closing::Window)) {
                                                    "Close Rowboat?"
                                                } else {
                                                    "Close workspace?"
                                                },
                                            ),
                                    )
                                    .child(details),
                            )
                            .child(buttons),
                    ),
            );
        }
        root
    }
}
