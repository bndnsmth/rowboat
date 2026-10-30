use crate::forms::{self, ConnectionForm, RowEditor, RowTarget};
use gpui_kit::{
    base::{
        ResizableState,
        input::{EditorState, InputEvent, InputState, TextareaState},
    },
    *,
};
use rowboat_core::{Database, *};
use rowboat_services::{AppState, HistoryEntry, Tunnel};
use std::{collections::HashMap, sync::mpsc, time::Duration};

gpui_kit::actions!(
    rowboat,
    [
        RunQuery,
        RunCurrent,
        RunSelection,
        RunAll,
        NextSnippet,
        PreviousSnippet,
        ExpandSnippet,
        PreviousResult,
        NextResult,
        StageCell,
        NextCell,
        PreviousCell,
        CancelCell,
        ModalNext,
        ModalPrevious,
        ExplorerUp,
        ExplorerDown,
        ExplorerFirst,
        ExplorerLast,
        ExplorerOpen,
        FocusGrid,
        NamespaceUp,
        NamespaceDown,
        NamespaceFirst,
        NamespaceLast,
        NamespaceChoose,
        NewQuery,
        FormatQuery,
        QuickOpen,
        NewConnection,
        CopyCell,
        Dismiss,
        CompleteSql,
        CloseQuery,
        CommandPalette,
        ShowPreferences,
        SendAssistant
    ]
);

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Query,
    Table,
    History,
    Tools,
    Objects,
    Dashboard,
}
#[derive(Clone, Copy, PartialEq)]
pub enum TablePage {
    Content,
    Structure,
    Changes,
}

#[derive(Default)]
pub struct Startup {
    pub connection: Option<String>,
    pub password: String,
    pub password_supplied: bool,
    pub sql: Option<String>,
    pub autorun: bool,
    pub tab_id: Option<usize>,
}

pub struct QueryTab {
    pub id: usize,
    pub connection_id: Option<String>,
    pub title: String,
    pub editor: Entity<EditorState>,
    pub parameters: Entity<TextareaState>,
    pub show_parameters: bool,
    pub results: Vec<QueryResult>,
    pub result_index: usize,
    pub error: Option<String>,
    pub metadata_incomplete: Vec<usize>,
    pub query_progress: Option<crate::query_stream_ui::ResultProgress>,
}

pub struct ConnectedData {
    pub(crate) database: Database,
    pub(crate) catalog: rowboat_core::NamespaceCatalog,
    pub(crate) profile: ConnectionProfile,
    pub(crate) endpoint: ConnectionProfile,
    pub(crate) password: String,
    pub(crate) tunnel: Option<Tunnel>,
}
impl ConnectedData {
    pub(crate) async fn open(
        profile: ConnectionProfile,
        password: String,
    ) -> anyhow::Result<Box<Self>> {
        let (tunnel, endpoint) =
            if profile.engine.descriptor().connection_kind == ConnectionKind::File {
                (None, profile.clone())
            } else {
                let (tunnel, endpoint) = Tunnel::open(&profile).await.map_err(|e| {
                    rowboat_services::connection_issue(rowboat_services::ConnectionStage::Ssh, &e)
                })?;
                (Some(tunnel), endpoint)
            };
        let stage = if profile.engine == Engine::Sqlite {
            rowboat_services::ConnectionStage::File
        } else {
            rowboat_services::ConnectionStage::Database
        };
        let database = Database::connect(&endpoint, &password)
            .await
            .map_err(|e| rowboat_services::connection_issue(stage, &e))?;
        let catalog = database.namespace_catalog(None).await.map_err(|e| {
            rowboat_services::connection_issue(rowboat_services::ConnectionStage::Catalog, &e)
        })?;
        Ok(Box::new(Self {
            database,
            catalog,
            profile,
            endpoint,
            password,
            tunnel,
        }))
    }
}

pub struct Envelope {
    pub route: crate::workspace_model::Route,
    pub event: Event,
}
#[derive(Clone)]
pub struct EventSender {
    pub route: crate::workspace_model::Route,
    sender: mpsc::Sender<Envelope>,
}
impl EventSender {
    pub fn send(&self, event: Event) -> Result<(), mpsc::SendError<()>> {
        self.sender
            .send(Envelope {
                route: self.route,
                event,
            })
            .map_err(|_| mpsc::SendError(()))
    }
}

pub enum Event {
    Connection(Box<crate::connection_ui::ConnectionEvent>),
    CredentialLookup {
        request: uuid::Uuid,
        connection_id: String,
        result: Result<Option<String>, String>,
    },
    QueryApplied(Box<crate::query_edit_ui::Applied>),
    Namespace(Box<crate::namespace_ui::Loaded>),
    DatabaseContext(Box<crate::namespace_ui::DatabaseEvent>),
    CustomMetric(Box<crate::custom_metrics_ui::Sampled>),
    Column(Box<crate::columns_ui::ColumnEvent>),
    Access(Box<crate::access_ui::AccessEvent>),
    Recovery(Box<crate::recovery_ui::Validated>),
    Dashboard(Box<crate::dashboards_ui::Sampled>),
    DataTransfer(Box<crate::data_transfer_ui::DataTransferEvent>),
    Explained(Box<crate::explain_ui::Explained>),
    AgentChecked(Box<crate::preferences_ui::AgentChecked>),
    Ai(Box<crate::ai_ui::AiEvent>),
    Assistant(Box<crate::assistant_ui::AssistantEvent>),
    Object(Box<crate::objects_ui::ObjectEvent>),
    Relationships(Box<crate::relationships_ui::Loaded>),
    Profiles(crate::profiles_ui::ProfileEvent),
    Management(Box<crate::management_ui::ManagementEvent>),
    #[cfg(test)]
    #[allow(dead_code)] // Compatibility injection used by the separate actual-GPUI harness.
    CredentialLoaded {
        connection_id: String,
        result: Result<Option<String>, String>,
    },
    CredentialSaved {
        profile: Box<ConnectionProfile>,
        password: String,
        connect: bool,
        result: Result<(), String>,
    },
    TransferChosen {
        request: crate::services_ui::TransferRequest,
        result: Result<crate::services_ui::ChosenFile, String>,
    },
    TransferFinished {
        restore: bool,
        result: Result<String, String>,
    },
    TransactionState(Result<bool, String>),
    Objects(Result<Vec<rowboat_admin::objects::DatabaseObject>, String>),
    Metrics(Result<rowboat_admin::metrics::MetricsSample, String>),
    Connected(Result<Box<ConnectedData>, String>),
    Query {
        tab: usize,
        sql: String,
        connection: String,
        elapsed_ms: u64,
        result: Result<Vec<QueryResult>, String>,
    },
    Browse {
        table: TableRef,
        result: Result<(QueryResult, Structure), String>,
    },
    Applied(Result<u64, String>),
    Transaction {
        result: Result<(), String>,
    },
    Notice(Result<String, String>),
}

pub type Workbench = ConnectionWorkspace;
pub struct ProfileSave {
    pub profile: ConnectionProfile,
    pub(crate) password: String,
    pub(crate) connect: bool,
    pub(crate) remember: bool,
    pub(crate) exists: bool,
}
pub struct ConnectionWorkspace {
    pub diagram: crate::diagram_ui::DiagramState,
    pub connection_flow: crate::connection_ui::State,
    pub query_edit: crate::query_edit_ui::State,
    pub workspace_id: u64,
    pub owner: Option<String>,
    pub shared: crate::workspace_shared::Shared,
    pub shell_tx: mpsc::Sender<crate::workspace_shell::Command>,
    pub panes: crate::workspace_model::SplitPanes,
    pub split_layout: Entity<ResizableState>,
    pub editor_focus: HashMap<usize, Subscription>,
    pub url_input: Option<Entity<InputState>>,
    pub management: Option<crate::management_ui::ManagementDialog>,
    pub access_matrix: Option<crate::access_ui::AccessDialog>,
    pub column_form: Option<crate::columns_ui::ColumnDialog>,
    pub advanced: crate::advanced::AdvancedState,
    pub objects: crate::objects_ui::ObjectState,
    pub dashboards: crate::dashboards_ui::DashboardState,
    pub explain: Option<crate::explain_ui::ExplainState>,
    pub(crate) assistant: crate::assistant_ui::Assistant,
    pub(crate) startup: Startup,
    pub(crate) startup_connect: Option<ConnectionProfile>,
    pub(crate) session_ready: bool,
    pub runtime: tokio::runtime::Handle,
    pub tx: EventSender,
    rx: mpsc::Receiver<Envelope>,
    pub state: AppState,
    pub state_revision_cache: u64,
    pub loaded: bool,
    pub database: Option<Database>,
    pub profile: Option<ConnectionProfile>,
    pub endpoint: Option<ConnectionProfile>,
    pub transfer: Option<crate::services_ui::TransferDialog>,
    pub transfer_running: bool,
    pub(crate) password_subscription: Option<Subscription>,
    pub session_generation: u64,
    pub activity_revision: u64,
    pub(crate) tunnel: Option<Tunnel>,
    pub passwords: HashMap<String, String>,
    pub session_password: String,
    pub catalog: Vec<TableInfo>,
    pub namespace: crate::namespace_ui::NamespaceState,
    pub page: Page,
    /// Temporary shell-owned profile form; never a retained database workspace.
    pub profile_editor: bool,
    /// History and saved SQL share local persistence, but have distinct destinations.
    pub saved_queries_view: bool,
    pub database_destination: bool,
    pub table_page: TablePage,
    pub table: Option<TableRef>,
    pub structure: Structure,
    pub table_result: QueryResult,
    pub browse_request: BrowseRequest,
    pub tabs: Vec<QueryTab>,
    pub active_tab: usize,
    pub query_tab_scroll: ScrollHandle,
    /// Active document, tab count and measured strip width last revealed.
    pub query_tab_reveal: Option<(usize, usize, Pixels)>,
    pub busy: bool,
    #[cfg(test)]
    pub query_run_invocations: usize,
    pub query_stream: Option<crate::query_stream_ui::RunningQuery>,
    pub execution: crate::execution_ui::State,
    pub results: crate::result_snapshots::ResultsState,
    pub library: crate::library_ui::LibraryState,
    pub data_transfer: Option<crate::data_transfer_ui::DataTransfer>,
    pub transaction: bool,
    pub transaction_known: bool,
    pub status: String,
    pub error: Option<String>,
    pub form: Option<ConnectionForm>,
    pub row_editor: Option<RowEditor>,
    pub selected: Option<crate::staging::GridSelection>,
    pub grid: crate::grid_ui::GridState,
    pub changes: Vec<RowChange>,
    pub recovery: crate::recovery_ui::RecoveryState,
    pub change_target: Option<RowTarget>,
    pub review_offset: usize,
    pub review_details: std::collections::HashSet<usize>,
    pub grid_focus: FocusHandle,
    pub grid_scroll: UniformListScrollHandle,
    pub grid_x_scroll: ScrollHandle,
    pub close_requested: bool,
    pub close_tab_requested: Option<usize>,
    pub explorer_filter: Entity<InputState>,
    pub explorer_page: usize,
    pub filters: crate::filters_ui::FilterState,
    pub relationships: Option<crate::relationships_ui::Navigation>,
    pub relationship_origin: Option<crate::relationships_ui::Origin>,
    pub relationship_return: Option<crate::relationships_ui::Origin>,
    pub result_filter: Entity<InputState>,
    pub query_sort: Option<(usize, bool)>,
    pub quick_open: bool,
    pub palette: crate::palette::PaletteState,
    pub preferences_form: Option<crate::preferences_ui::PreferencesForm>,
    pub snippet_form: Option<crate::snippets_ui::SnippetForm>,
    pub snippet_session: Option<crate::snippets_ui::SnippetSession>,
    pub quick_filter: Entity<InputState>,
    pub widths: Vec<f32>,
    pub resizing: Option<(usize, f32, f32)>,
    pub sidebar_split: Entity<ResizableState>,
    pub editor_split: Entity<ResizableState>,
    pub focus: FocusHandle,
    pub custom_metrics: crate::custom_metrics_ui::CustomMetrics,
    pub modal_focus: FocusHandle,
    pub(crate) dialogs: crate::dialog::DialogLayout,
    pub viewport: Size<Pixels>,
    pub host_epoch: u64,
    pub explorer_cursor: usize,
    pub explorer_scroll: ScrollHandle,
    host_subscriptions: Vec<Subscription>,
    pub pending_focus: Option<FocusHandle>,
    pub _subscriptions: Vec<Subscription>,
    pub(crate) editor_observers: HashMap<usize, Subscription>,
    _poll: Task<()>,
}

impl Workbench {
    pub fn navigate_result(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.results_read_only()
            || self.close_requested
            || self.modal_open()
            || self.page != Page::Query
        {
            return;
        }
        let tab = &mut self.tabs[self.active_tab];
        let n = tab.results.len();
        if n == 0 {
            return;
        }
        tab.result_index = if forward {
            (tab.result_index + 1) % n
        } else {
            (tab.result_index + n - 1) % n
        };
        self.selected = None;
        self.widths.clear();
        self.query_sort = None;
        cx.notify();
    }
    pub fn modal_open(&self) -> bool {
        self.has_extension_dialog()
            || self.form.is_some()
            || self.row_editor.is_some()
            || self.grid.inspector.is_some()
            || self.url_input.is_some()
            || self.management.is_some()
            || self.transfer.is_some()
            || self.quick_open
            || self.advanced.designer.is_some()
            || self.close_tab_requested.is_some()
            || self.tabs[self.active_tab].show_parameters
    }
    pub fn has_extension_dialog(&self) -> bool {
        self.namespace.picker.is_some()
            || self.diagram.open
            || self.diagram.is_loading()
            || self.library.form.is_some()
            || self.custom_metrics.editor.is_some()
            || self.column_form.is_some()
            || self.access_matrix.is_some()
            || self.snippet_form.is_some()
            || self.recovery.open
            || self.preferences_form.is_some()
            || self.filters.editor.is_some()
            || self.relationships.is_some()
            || self.objects.dialog.is_some()
            || self.explain.as_ref().is_some_and(|e| e.open)
            || self.data_transfer.is_some()
            || self.dashboards.editor.is_some()
    }
    pub fn current_origin(&self) -> Option<crate::staging::GridOrigin> {
        if self.results_read_only() {
            return None;
        }
        match self.page {
            Page::Table => Some(crate::staging::GridOrigin::Table(self.current_target()?)),
            Page::Query => Some(crate::staging::GridOrigin::Query {
                generation: self.session_generation,
                tab: self.tabs[self.active_tab].id,
                result: self.tabs[self.active_tab].result_index,
            }),
            _ => None,
        }
    }
    pub fn selected_cell(&self) -> Option<(usize, usize)> {
        self.selected.as_ref()?.for_origin(&self.current_origin()?)
    }
    pub fn current_target(&self) -> Option<RowTarget> {
        Some(RowTarget {
            connection_id: self.profile.as_ref()?.id.clone(),
            session_generation: self.session_generation,
            table: self.table.clone()?,
        })
    }

    pub fn review_pending(&mut self, cx: &mut Context<Self>) {
        if self.has_extension_dialog() {
            return;
        }
        if self.has_row_draft() {
            self.fail("Stage or cancel the row editor before reviewing the batch.".into());
            cx.notify();
            return;
        }
        if self.review_query_changes(cx) {
            return;
        }
        if let Some(target) = &self.change_target {
            if self.current_target() != Some(target.clone()) {
                self.fail("Pending changes belong to a different connection/table.".into());
                cx.notify();
                return;
            }
            self.page = Page::Table;
            self.table_page = TablePage::Changes;
            self.selected = None;
            self.widths.clear();
        }
        cx.notify();
    }

    pub fn new(
        runtime: tokio::runtime::Handle,
        mut startup: Startup,
        init: crate::workspace_shell::WorkspaceInit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (sender, rx) = mpsc::channel();
        let tx = EventSender {
            sender,
            route: crate::workspace_model::Route {
                workspace: init.id,
                generation: 0,
            },
        };
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let Some(this) = this.upgrade() else {
                    break;
                };
                let _ = this.update_in(cx, |this, window, cx| this.poll(window, cx));
            }
        });
        let explorer_filter = forms::input("", "Filter loaded objects…", window, cx);
        let quick_filter = forms::input(
            "",
            "Search commands, tables, favorites, connections…",
            window,
            cx,
        );
        let result_filter = forms::input("", "Filter displayed results…", window, cx);
        let subscriptions = vec![
            cx.subscribe(&explorer_filter, |s, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    s.namespace.collapsed = [false, false];
                    s.expand_loaded_sidebar_groups();
                }
                s.explorer_page = 0;
                s.explorer_cursor = 0;
                cx.notify();
            }),
            cx.subscribe(&result_filter, |this, _, _: &InputEvent, cx| {
                this.selected = None;
                cx.notify();
            }),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let initial_id = init.shared.borrow_mut().tab_id();
        let initial_sql = startup
            .sql
            .clone()
            .unwrap_or_else(|| "-- Write SQL, then press Ctrl+Enter to run.\n".into());
        if startup.sql.is_some() {
            startup.tab_id = Some(initial_id);
        }
        let initial_editor = crate::sql_editor::create(initial_sql, window, cx);
        let editor_observers = HashMap::from([(
            initial_id,
            cx.observe(&initial_editor, |_, _, cx| cx.notify()),
        )]);
        let state = init.shared.borrow().state.clone();
        let loaded = init.shared.borrow().writable;
        let state_revision_cache = init.shared.borrow().revision;
        let mut this = Self {
            diagram: Default::default(),
            connection_flow: Default::default(),
            query_edit: Default::default(),
            workspace_id: init.id,
            owner: init.owner.clone(),
            shared: init.shared,
            shell_tx: init.shell_tx,
            panes: crate::workspace_model::SplitPanes {
                tabs: [Some(initial_id), None],
                focused: 0,
            },
            split_layout: cx.new(|_| ResizableState::default()),
            editor_focus: HashMap::new(),
            url_input: None,
            management: None,
            access_matrix: None,
            column_form: None,
            session_ready: true,
            advanced: crate::advanced::AdvancedState::new(window, cx),
            objects: Default::default(),
            dashboards: Default::default(),
            explain: None,
            assistant: crate::assistant_ui::Assistant::new(window, cx),
            startup,
            startup_connect: None,
            runtime,
            tx,
            rx,
            state_revision_cache,
            state,
            loaded,
            database: None,
            profile: None,
            endpoint: None,
            transfer: None,
            transfer_running: false,
            password_subscription: None,
            session_generation: 0,
            activity_revision: 0,
            tunnel: None,
            passwords: HashMap::new(),
            session_password: String::new(),
            catalog: vec![],
            namespace: Default::default(),
            page: Page::Query,
            profile_editor: false,
            saved_queries_view: false,
            database_destination: false,
            table_page: TablePage::Content,
            table: None,
            structure: Structure::default(),
            table_result: QueryResult::default(),
            browse_request: BrowseRequest {
                limit: 200,
                ..Default::default()
            },
            tabs: vec![QueryTab {
                id: initial_id,
                connection_id: init.owner,
                title: "Query 1".into(),
                editor: initial_editor,
                parameters: forms::textarea("{}", window, cx),
                show_parameters: false,
                results: vec![],
                metadata_incomplete: vec![],
                query_progress: None,
                result_index: 0,
                error: None,
            }],
            active_tab: 0,
            query_tab_scroll: ScrollHandle::new(),
            query_tab_reveal: None,
            busy: false,
            #[cfg(test)]
            query_run_invocations: 0,
            query_stream: None,
            execution: Default::default(),
            results: Default::default(),
            library: crate::library_ui::LibraryState::new(window, cx),
            data_transfer: None,
            transaction: false,
            transaction_known: true,
            status: "Ready · connect to a database to begin".into(),
            error: None,
            form: None,
            row_editor: None,
            grid: Default::default(),
            selected: None,
            changes: vec![],
            change_target: None,
            recovery: Default::default(),
            review_offset: 0,
            review_details: Default::default(),
            grid_focus: cx.focus_handle().tab_stop(true),
            grid_scroll: UniformListScrollHandle::new(),
            grid_x_scroll: ScrollHandle::new(),
            close_requested: false,
            close_tab_requested: None,
            explorer_filter,
            explorer_page: 0,
            filters: Default::default(),
            relationships: None,
            relationship_origin: None,
            relationship_return: None,
            result_filter,
            query_sort: None,
            quick_open: false,
            palette: Default::default(),
            preferences_form: None,
            snippet_form: None,
            snippet_session: None,
            quick_filter,
            widths: vec![],
            resizing: None,
            sidebar_split: cx.new(|_| ResizableState::default()),
            editor_split: cx.new(|_| ResizableState::default()),
            focus,
            custom_metrics: Default::default(),
            modal_focus: cx.focus_handle(),
            dialogs: Default::default(),
            viewport: window.viewport_size(),
            host_epoch: 0,
            explorer_cursor: 0,
            explorer_scroll: ScrollHandle::new(),
            host_subscriptions: Vec::new(),
            pending_focus: None,
            _subscriptions: subscriptions,
            editor_observers,
            _poll: poll,
        };
        this.bind_host_inputs(window, cx);
        this
    }

    fn bind_host_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let epoch = self.host_epoch;
        self.host_subscriptions = vec![cx.subscribe_in(
            &self.quick_filter,
            window,
            move |s, _, event: &InputEvent, w, cx| {
                if s.host_epoch != epoch {
                    return;
                }
                if matches!(event, InputEvent::PressEnter { .. }) {
                    s.accept_quick_open(w, cx);
                }
                if matches!(event, InputEvent::Change) {
                    s.palette.cursor.index = s
                        .palette_items(cx)
                        .iter()
                        .position(|i| i.enabled)
                        .unwrap_or(0);
                    s.palette.scroll.scroll_to_item(s.palette.cursor.index);
                }
                cx.notify();
            },
        )];
        self.editor_focus.clear();
        for id in self.tabs.iter().map(|t| t.id).collect::<Vec<_>>() {
            self.observe_editor_focus(id, window, cx);
        }
    }
    pub fn rehost(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.host_epoch = self.host_epoch.wrapping_add(1);
        let epoch = self.host_epoch;
        self._poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let Some(this) = this.upgrade() else {
                    break;
                };
                if this
                    .update_in(cx, |s, w, cx| {
                        if s.host_epoch != epoch {
                            false
                        } else {
                            s.poll(w, cx);
                            true
                        }
                    })
                    .ok()
                    != Some(true)
                {
                    break;
                }
            }
        });
        self.bind_host_inputs(window, cx);
        self.pending_focus = None;
        // Focus must belong to the new host's visible action tree. The SQL
        // editor remains retained but is not rendered on Review/other pages.
        if !self.close_requested
            && self.page == Page::Table
            && self.table_page == TablePage::Content
            && let Some(draft) = &self.grid.draft
            && self.current_target().as_ref() == Some(&draft.target)
        {
            draft.value.update(cx, |e, cx| e.focus(window, cx));
        } else if !self.close_requested && self.page == Page::Query {
            self.tabs[self.active_tab]
                .editor
                .update(cx, |e, cx| e.focus(window, cx));
        } else if !self.close_requested
            && self.page == Page::Table
            && self.table_page == TablePage::Content
            && !self.table_result.columns.is_empty()
        {
            window.focus(&self.grid_focus, cx);
        } else {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }
    pub fn request_move(&mut self, cx: &mut Context<Self>) {
        if self.close_requested || self.modal_open() || self.library.file_pending {
            return;
        }
        let _ = self.shell_tx.send(crate::workspace_shell::Command::Move {
            route: self.tx.route,
            host_epoch: self.host_epoch,
        });
        cx.notify();
    }

    pub(crate) fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The diagram retains its read lease until cancellation is acknowledged,
        // including after the overlay was dismissed.
        self.poll_diagram(cx);
        self.poll_library(window, cx);
        self.poll_result_comparison(cx);
        self.poll_connection_checkpoint(cx);
        if window.is_window_active() && self.focus.contains_focused(window, cx) {
            self.shared.borrow_mut().active_workspace = self.workspace_id;
        }
        self.poll_query_stream(cx);
        self.poll_data_transfer(cx);
        self.poll_dashboard(cx);
        let shared = self.shared.borrow();
        if self.state_revision_cache != shared.revision || self.loaded != shared.writable {
            self.state = shared.state.clone();
            self.state_revision_cache = shared.revision;
            self.loaded = shared.writable;
            cx.notify();
        }
        drop(shared);
        self.sync_assistant_scope();
        let events: Vec<_> = self.rx.try_iter().collect();
        if events.is_empty() {
            return;
        }
        for envelope in events {
            if envelope.route != self.tx.route {
                continue;
            }
            let event = envelope.event;
            self.activity_revision = self.activity_revision.wrapping_add(1);
            match event {
                Event::Connection(event) => self.receive_connection(*event, cx),
                Event::CredentialLookup {
                    request,
                    connection_id,
                    result,
                } => {
                    if self
                        .form
                        .as_ref()
                        .is_some_and(|f| f.credential_request == Some(request) && !f.is_file())
                    {
                        self.form.as_mut().unwrap().credential_request = None;
                        self.receive_credential(connection_id, result, window, cx);
                    }
                }
                Event::QueryApplied(applied) => self.receive_query_applied(*applied, cx),
                Event::CustomMetric(event) => self.receive_custom_metric(*event, cx),
                Event::Column(event) => self.receive_column(*event, window, cx),
                Event::Access(event) => self.receive_access(*event, cx),
                Event::Recovery(event) => self.receive_recovery(*event, cx),
                Event::Dashboard(event) => self.receive_dashboard(*event, cx),
                Event::DataTransfer(event) => self.receive_data_transfer(*event, cx),
                Event::Explained(event) => self.receive_explained(*event, window, cx),
                Event::AgentChecked(event) => self.receive_agent_checked(*event, cx),
                Event::Ai(event) => self.receive_ai(*event, cx),
                Event::Assistant(event) => self.receive_assistant(*event, cx),
                Event::Object(event) => self.receive_object(*event, cx),
                Event::Relationships(loaded) => self.receive_relationships(*loaded, cx),
                Event::Profiles(event) => self.receive_profiles(event, cx),
                Event::Management(event) => self.receive_management(*event, cx),
                #[cfg(test)]
                Event::CredentialLoaded {
                    connection_id,
                    result,
                } => self.receive_credential(connection_id, result, window, cx),
                Event::CredentialSaved {
                    profile,
                    password,
                    connect,
                    result,
                } => {
                    self.busy = false;
                    match result {
                        Ok(()) => {
                            if let Some(form) = self.form.as_mut().filter(|f| f.id == profile.id) {
                                form.credential_exists = form.remember;
                            }
                            if connect {
                                self.connect_profile(*profile, password, cx);
                            } else {
                                self.form = None;
                                self.status = "Connection saved".into();
                            }
                        }
                        Err(e) => self.fail(format!("Credential operation failed: {e}")),
                    }
                }
                Event::TransferChosen { request, result } => {
                    self.receive_transfer_file(request, result, cx)
                }
                Event::TransferFinished { restore, result } => {
                    self.busy = false;
                    self.transfer_running = false;
                    if restore {
                        // MySQL restore may partially commit even when the client reports failure.
                        self.invalidate_restore_results();
                    }
                    match result {
                        Ok(message) => {
                            self.transfer = None;
                            self.status = message;
                            if restore {
                                self.refresh_catalog(cx);
                            }
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::TransactionState(state) => match state {
                    Ok(active) => {
                        self.transaction = active;
                        self.transaction_known = true;
                    }
                    Err(e) => {
                        self.transaction_known = false;
                        self.fail(format!("Session state is unknown: {e}. Try Rollback if the session is available, or Connection → Settings / reconnect to explicitly replace it. The previous transaction outcome is not confirmed."));
                    }
                },
                Event::Objects(result) => {
                    self.busy = false;
                    match result {
                        Ok(items) => {
                            self.status = format!("{} database objects loaded", items.len());
                            self.advanced.objects = items;
                            self.advanced.objects_loaded = true;
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::Metrics(result) => {
                    self.busy = false;
                    match result {
                        Ok(sample) => {
                            self.advanced.metrics = Some(sample);
                            self.status = "Server metrics sampled".into();
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::Connected(result) => {
                    self.busy = false;
                    match result {
                        Ok(connected) => {
                            let ConnectedData {
                                database: db,
                                catalog,
                                profile,
                                endpoint,
                                password,
                                tunnel,
                            } = *connected;
                            self.status = match profile.engine.descriptor().connection_kind {
                                rowboat_core::ConnectionKind::File => format!(
                                    "Connected to {} · {}",
                                    profile.name,
                                    profile
                                        .file
                                        .as_ref()
                                        .map(|f| f.path.display().to_string())
                                        .unwrap_or_default()
                                ),
                                rowboat_core::ConnectionKind::Network => format!(
                                    "Connected to {} · {}:{}",
                                    profile.name, profile.host, profile.port
                                ),
                            };
                            if self.connection_flow.replaced_unknown {
                                self.status.push_str(
                                    " · previous session outcome unconfirmed; no SQL replayed",
                                );
                                self.connection_flow.replaced_unknown = false;
                            }
                            if !self.connection_flow.reconnecting {
                                self.edit_state(|state| {
                                    state.last_connection_id = Some(profile.id.clone())
                                });
                            }
                            self.database = Some(db);
                            self.transaction = false;
                            self.transaction_known = true;
                            self.advanced.reset_connection();
                            self.profile = Some(profile);
                            self.endpoint = Some(endpoint);
                            self.session_password = password;
                            self.tunnel = tunnel;
                            self.accept_namespace(catalog, None, cx);
                            self.form = None;
                            self.connection_flow.reconnecting = false;
                            self.table = None;
                            self.table_result = QueryResult::default();
                            self.structure = Structure::default();
                            self.selected = None;
                            self.page = Page::Query;
                            let connection_id = self.profile.as_ref().unwrap().id.clone();
                            let autorun = self.startup.autorun
                                && self.tabs.iter().any(|t| {
                                    Some(t.id) == self.startup.tab_id
                                        && t.connection_id.as_deref()
                                            == Some(connection_id.as_str())
                                });
                            self.select_connection_workspace(&connection_id, window, cx);
                            self.startup.sql = None;
                            self.startup.tab_id = None;
                            self.startup.autorun = false;
                            if autorun {
                                self.run_query(cx);
                            }
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::Namespace(loaded) => self.receive_namespace(*loaded, cx),
                Event::DatabaseContext(event) => self.receive_database_context(*event, cx),
                Event::Query {
                    tab,
                    sql,
                    connection,
                    elapsed_ms,
                    result,
                } => {
                    self.busy = false;
                    let (row_count, error) = match &result {
                        Ok(rs) => (
                            rs.iter()
                                .map(|r| {
                                    if r.columns.is_empty() {
                                        r.rows_affected
                                    } else {
                                        r.rows.len() as u64
                                    }
                                })
                                .sum(),
                            None,
                        ),
                        Err(e) => (0, Some(e.clone())),
                    };
                    let entry = HistoryEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        connection_id: connection,
                        sql,
                        executed_at: chrono::Utc::now().to_rfc3339(),
                        elapsed_ms,
                        row_count,
                        error: error.clone(),
                    };
                    self.edit_state(|state| {
                        state.history.push(entry);
                        if state.history.len() > 1000 {
                            state.history.remove(0);
                        }
                    });
                    match result {
                        Ok(results) => {
                            self.status = format!(
                                "Fetched · {row_count} rows · {elapsed_ms} ms so far · verifying context…"
                            );
                            if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                                t.results = results;
                                t.result_index = 0;
                                t.error = None;
                            }
                            self.selected = None;
                            self.widths.clear();
                        }
                        Err(e) => {
                            if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                                t.error = Some(e.clone());
                            }
                            self.fail(e);
                        }
                    }
                    self.sync_terminal_namespace(tab, cx);
                }
                Event::Browse { table, result } => {
                    self.busy = false;
                    match result {
                        Ok((result, structure)) => {
                            if !self.table_result_fits(&result) {
                                self.table_result.truncated = true;
                                self.fail("Shared result memory budget exhausted; table result was not retained. Close results or pins before refreshing.".into());
                                cx.notify();
                                continue;
                            }
                            self.status = format!(
                                "{}.{} · {} rows · {} ms",
                                table.schema,
                                table.name,
                                result.rows.len(),
                                result.elapsed_ms
                            );
                            self.table = Some(table);
                            self.table_result = result;
                            self.structure = structure;
                            self.selected = None;
                            self.restore_relationship_selection(cx);
                            self.widths.clear();
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::Applied(result) => {
                    self.busy = false;
                    self.finish_apply_marker(
                        result.is_ok() && self.transaction_known && !self.transaction,
                    );
                    match result {
                        Ok(n) => {
                            self.changes.clear();
                            self.change_target = None;
                            self.status = format!(
                                "Applied {n} row changes{}",
                                if self.transaction {
                                    " inside transaction — commit to persist"
                                } else {
                                    ""
                                }
                            );
                            self.table_page = TablePage::Content;
                            self.refresh_table(cx);
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::Transaction { result } => {
                    self.busy = false;
                    match result {
                        Ok(()) => {
                            self.status = if self.transaction {
                                "Transaction opened — changes await Commit or Rollback"
                            } else {
                                "Transaction closed"
                            }
                            .into();
                            if self.table.is_some() {
                                self.refresh_table(cx);
                            }
                        }
                        Err(e) => self.fail(e),
                    }
                }
                Event::Notice(result) => match result {
                    Ok(s) => self.status = s,
                    Err(e) => self.fail(e),
                },
            }
        }
        cx.notify();
    }

    pub fn edit_state(&mut self, f: impl FnOnce(&mut AppState)) {
        self.shared.borrow_mut().edit(f);
        let shared = self.shared.borrow();
        self.state = shared.state.clone();
        self.state_revision_cache = shared.revision;
    }
    pub fn fail(&mut self, error: String) {
        self.status = "Operation failed".into();
        self.error = Some(error);
    }
    pub fn can_leave(&mut self) -> bool {
        if self.library.file_pending {
            self.status = "Finish choosing or saving the SQL file before navigating.".into();
            return false;
        }
        if self.has_extension_dialog() {
            self.fail("Finish or close the active dialog before navigating.".into());
            return false;
        }
        if self.url_input.is_some() {
            self.fail("Finish or close the connection URL form first.".into());
            return false;
        }
        if self.management.is_some() {
            self.fail("Finish or close the management form before navigating.".into());
            return false;
        }
        if !self.session_ready {
            self.status = "Loading SQL buffer recovery…".into();
            return false;
        }
        if self.transfer.is_some() {
            self.fail("Finish or close the backup/restore dialog before navigating.".into());
            return false;
        }
        if self.advanced.designer.is_some() {
            self.fail("Generate SQL or cancel the table designer before navigating.".into());
            false
        } else if self.has_row_draft() {
            self.fail("Finish or cancel the open row editor before navigating.".into());
            false
        } else if self.busy {
            self.fail("Wait for the current operation or cancel it first.".into());
            false
        } else if !self.changes.is_empty() {
            self.fail(
                "Review and apply or revert staged changes before changing tables or connections."
                    .into(),
            );
            false
        } else {
            true
        }
    }

    pub fn show_connection(
        &mut self,
        profile: Option<ConnectionProfile>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_extension_dialog() {
            return;
        }
        if profile.is_none() {
            let _ = self
                .shell_tx
                .send(crate::workspace_shell::Command::Connections {
                    new_connection: true,
                });
            self.quick_open = false;
            cx.notify();
            return;
        }
        let password = profile
            .as_ref()
            .and_then(|p| self.passwords.get(&p.id).cloned());
        let _ = self.shell_tx.send(crate::workspace_shell::Command::Open {
            profile: profile.map(Box::new),
            password,
        });
        self.quick_open = false;
        cx.notify();
    }
    pub fn edit_connection(
        &mut self,
        profile: Option<ConnectionProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !(self.connection_settings_enabled() && !self.transaction_known) && !self.can_leave() {
            cx.notify();
            return;
        }
        let existing = profile.is_some();
        self.connection_flow.reconnecting = false;
        let profile = profile.unwrap_or_default();
        let password = self.passwords.get(&profile.id).cloned().unwrap_or_default();
        self.form = Some(ConnectionForm::new(profile, password, window, cx));
        if let Some(form) = &self.form {
            let focus = if form.is_file() {
                &form.fields["file_path"]
            } else if existing {
                &form.password
            } else {
                &form.fields["name"]
            };
            focus.update(cx, |s, cx| s.focus(window, cx));
        }
        self.tabs[self.active_tab].show_parameters = false;
        self.invalidate_completions(cx);
        let password = self.form.as_ref().unwrap().password.clone();
        self.password_subscription =
            Some(cx.subscribe(&password, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(form) = &mut this.form
                {
                    form.password_changed();
                    form.draft_changed();
                    cx.notify();
                }
            }));
        let url = self.form.as_ref().unwrap().url.clone();
        let subscription = cx.subscribe(&url, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change)
                && let Some(form) = &mut this.form
            {
                form.url_error = None;
                form.draft_changed();
                cx.notify();
            }
        });
        self.form.as_mut().unwrap().url_subscription = Some(subscription);
        let fields = self
            .form
            .as_ref()
            .unwrap()
            .fields
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let subscriptions = fields
            .iter()
            .map(|field| {
                cx.subscribe(field, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change)
                        && let Some(form) = &mut this.form
                    {
                        form.draft_changed();
                        cx.notify();
                    }
                })
            })
            .collect();
        self.form.as_mut().unwrap().field_subscriptions = subscriptions;
        self.error = None;
        self.quick_open = false;
        self.request_credential(cx);
        cx.notify();
    }

    pub fn save_connection(&mut self, connect: bool, cx: &mut Context<Self>) {
        if self.close_requested {
            return;
        }
        if !(connect && self.connection_flow.reconnecting) && !self.shared.borrow().writable {
            self.fail("Local metadata writes are disabled because the data directory is not exclusively available or could not be loaded.".into());
            cx.notify();
            return;
        }
        let Some(form) = &self.form else {
            return;
        };
        let profile = match form.profile(cx) {
            Ok(p) => p,
            Err(e) => {
                self.fail(e.to_string());
                cx.notify();
                return;
            }
        };
        let password = if form.is_file() {
            String::new()
        } else {
            form.password_value(cx)
        };
        if form.keyring_loading || form.file_request.is_some() || self.busy {
            return;
        }
        let remember = !form.is_file() && form.remember;
        let exists = !form.is_file() && form.credential_exists;
        if connect && (self.transaction || !self.transaction_known) {
            self.fail("Commit or roll back the active transaction before reconnecting".into());
            cx.notify();
            return;
        }
        if connect && self.connection_flow.reconnecting {
            self.startup.autorun = false;
            if self.database.is_some() {
                self.probe_reconnect(cx);
            } else {
                self.connect_profile(profile, password, cx);
            }
            return;
        }
        self.busy = true;
        self.status = "Assigning saved profile to this workspace…".into();
        if self
            .shell_tx
            .send(crate::workspace_shell::Command::SaveProfile {
                route: self.tx.route,
                save: Box::new(ProfileSave {
                    profile,
                    password,
                    connect,
                    remember,
                    exists,
                }),
            })
            .is_err()
        {
            self.busy = false;
            self.fail("Workspace coordinator unavailable; profile was not saved".into());
        }
        cx.notify();
    }
    pub(crate) fn commit_profile_save(&mut self, save: ProfileSave, cx: &mut Context<Self>) {
        let ProfileSave {
            profile,
            password,
            connect,
            remember,
            exists,
        } = save;
        self.busy = false;
        self.passwords.insert(profile.id.clone(), password.clone());
        self.edit_state(|state| {
            if let Some(p) = state.connections.iter_mut().find(|p| p.id == profile.id) {
                *p = profile.clone();
            } else {
                state.connections.push(profile.clone());
            }
        });
        if self.owner.is_none() {
            self.owner = Some(profile.id.clone());
            for tab in &mut self.tabs {
                tab.connection_id = Some(profile.id.clone());
            }
        }
        if connect {
            if self.transaction || !self.transaction_known {
                self.fail(
                    "Commit or roll back the active transaction before switching connections."
                        .into(),
                );
                cx.notify();
                return;
            }
            self.save_credential_then_connect(profile, password, true, remember, exists, cx);
        } else {
            self.save_credential_then_connect(profile, password, false, remember, exists, cx);
        }
        cx.notify();
    }

    pub(crate) fn retire_unknown_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.cancel_test();
            form.credential_request = None;
        }
        self.session_generation = self.session_generation.wrapping_add(1);
        self.results.reset();
        self.execution = Default::default();
        self.tx.route.generation = self.session_generation;
        self.database = None;
        self.tunnel = None;
        self.endpoint = None;
        self.session_password.clear();
        self.query_edit = Default::default();
        self.objects = Default::default();
        self.advanced.reset_connection();
        self.explain = None;
        self.relationships = None;
        self.relationship_origin = None;
        self.relationship_return = None;
        self.review_details.clear();
        self.invalidate_completions(cx);
    }

    pub(crate) fn connect_profile(
        &mut self,
        profile: ConnectionProfile,
        password: String,
        cx: &mut Context<Self>,
    ) {
        if self.close_requested
            || self.busy
            || self.transaction
            || !self.transaction_known
            || !self.changes.is_empty()
            || self.has_row_draft()
        {
            self.fail("Finish this workspace's running operation, transaction and pending edits before reconnecting.".into());
            cx.notify();
            return;
        }
        if self.owner.as_ref() != Some(&profile.id) {
            self.fail("Connection belongs to a different workspace.".into());
            cx.notify();
            return;
        }
        if let Some(form) = &mut self.form {
            form.cancel_test();
        }
        self.session_generation = self.session_generation.wrapping_add(1);
        self.results.reset();
        self.execution = Default::default();
        self.tx.route.generation = self.session_generation;
        self.invalidate_completions(cx);
        self.query_edit = Default::default();
        self.objects = Default::default();
        self.advanced.reset_connection();
        self.relationships = None;
        self.relationship_origin = None;
        self.relationship_return = None;
        self.explain = None;
        self.review_details.clear();
        self.database = None;
        self.tunnel = None;
        self.endpoint = None;
        self.session_password.clear();
        self.profile = Some(profile.clone());
        self.namespace = Default::default();
        self.catalog.clear();
        self.table = None;
        self.structure = Structure::default();
        self.table_result = QueryResult::default();
        self.selected = None;
        for tab in &mut self.tabs {
            tab.results.clear();
        }
        self.busy = true;
        self.error = None;
        self.status = format!("Connecting to {}…", profile.name);
        let password =
            if profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File {
                String::new()
            } else {
                password
            };
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let result = ConnectedData::open(profile, password)
                    .await
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::Connected(result));
            })
        });
        cx.notify();
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        if !self.can_leave() {
            cx.notify();
            return;
        }
        if self.transaction || !self.transaction_known {
            self.fail("Commit or roll back before disconnecting.".into());
            cx.notify();
            return;
        }
        self.database = None;
        // Keep the workspace's identity/profile while disconnected.
        self.session_generation = self.session_generation.wrapping_add(1);
        self.results.reset();
        self.execution = Default::default();
        self.tx.route.generation = self.session_generation;
        self.endpoint = None;
        self.session_password.clear();
        self.tunnel = None;
        self.catalog.clear();
        self.table = None;
        self.table_result = QueryResult::default();
        for tab in &mut self.tabs {
            tab.results.clear();
        }
        self.status = "Disconnected".into();
        cx.notify();
    }

    pub fn new_query(&mut self, sql: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_extension_dialog() || self.library.file_pending {
            return;
        }
        if self.url_input.is_some() {
            return;
        }
        if self.management.is_some() {
            return;
        }
        if !self.session_ready {
            self.status = "Loading SQL buffer recovery…".into();
            cx.notify();
            return;
        }
        if self.has_row_draft()
            || self.form.is_some()
            || self.close_requested
            || self.close_tab_requested.is_some()
            || self.transfer.is_some()
        {
            self.fail("Finish or close the open dialog first.".into());
            cx.notify();
            return;
        }
        let id = self.shared.borrow_mut().tab_id();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.show_parameters = false;
        }
        self.tabs.push(QueryTab {
            id,
            connection_id: self.owner.clone(),
            title: format!("Query {}", id + 1),
            editor: crate::sql_editor::create(sql.unwrap_or_default(), window, cx),
            parameters: forms::textarea("{}", window, cx),
            show_parameters: false,
            results: vec![],
            metadata_incomplete: vec![],
            query_progress: None,
            result_index: 0,
            error: None,
        });
        self.active_tab = self.tabs.len() - 1;
        self.panes.select(id);
        self.editor_observers.insert(
            id,
            cx.observe(&self.tabs[self.active_tab].editor, |_, _, cx| cx.notify()),
        );
        self.observe_editor_focus(id, window, cx);
        self.page = Page::Query;
        self.results.view = crate::result_snapshots::ResultsView::Live;
        self.selected = None;
        self.quick_open = false;
        self.database_destination = false;
        if self.shared.borrow().active_workspace == self.workspace_id {
            self.tabs[self.active_tab]
                .editor
                .update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    pub fn run_query(&mut self, cx: &mut Context<Self>) {
        self.run_query_with_intent(false, cx);
    }
    pub fn run_query_with_intent(&mut self, for_editing: bool, cx: &mut Context<Self>) {
        self.run_query_scope(
            rowboat_core::execution::RunScope::SelectionOrAll,
            for_editing,
            cx,
        );
    }
    pub fn run_query_scope(
        &mut self,
        scope: rowboat_core::execution::RunScope,
        for_editing: bool,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        {
            self.query_run_invocations += 1;
        }
        self.dismiss_completions(cx);
        if self.has_extension_dialog() {
            return;
        }
        if self.url_input.is_some() {
            return;
        }
        if self.management.is_some() {
            return;
        }
        if rowboat_services::environment_os("ROWBOAT_TRACE_KEYS").is_some() {
            eprintln!(
                "rowboat-run busy={} modal={} tabs={} active={}",
                self.busy,
                self.form.is_some() || self.has_row_draft() || self.close_requested,
                self.tabs.len(),
                self.active_tab
            );
        }
        if self.library.file_pending
            || self.busy
            || self.form.is_some()
            || self.has_row_draft()
            || self.close_requested
            || self.close_tab_requested.is_some()
            || self.quick_open
            || self.advanced.designer.is_some()
            || self.transfer.is_some()
        {
            return;
        }
        let Some(db) = self.database.clone() else {
            self.fail("Connect to a database before running SQL.".into());
            cx.notify();
            return;
        };
        if !self.changes.is_empty() {
            self.fail("Apply or revert staged changes before running SQL.".into());
            cx.notify();
            return;
        }
        let tab = &self.tabs[self.active_tab];
        if tab.connection_id.as_deref() != self.profile.as_ref().map(|p| p.id.as_str()) {
            self.fail(format!(
                "This SQL tab belongs to {}. Connect to that profile before running it.",
                self.connection_label(tab.connection_id.as_deref())
            ));
            cx.notify();
            return;
        }
        let editor = tab.editor.read(cx);
        let selected = editor.selected_range();
        let range = match rowboat_core::execution::execution_range(
            db.engine(),
            &editor.value(),
            selected.clone(),
            editor.cursor(),
            scope,
        ) {
            Ok(range) => range,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        let sql = editor.value()[range.clone()].to_owned();
        if rowboat_services::environment_os("ROWBOAT_TRACE_KEYS").is_some() {
            eprintln!(
                "rowboat-run accepted sql_bytes={} selection={}",
                sql.len(),
                !selected.is_empty()
            );
        }
        if sql.trim().is_empty() {
            return;
        }
        let id = tab.id;
        let execution_sql = match forms::parameters(&tab.parameters.read(cx).value())
            .and_then(|values| rowboat_admin::query::expand_named(db.engine(), &sql, &values))
        {
            Ok(sql) => sql,
            Err(error) => {
                self.tabs[self.active_tab].show_parameters = true;
                self.pending_focus = Some(
                    self.tabs[self.active_tab]
                        .parameters
                        .read(cx)
                        .focus_handle(cx),
                );
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        let options = match self.query_stream_options(for_editing) {
            Ok(options) => options,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        self.capture_execution_source(range, &execution_sql, scope, cx);
        self.invalidate_completions(cx);
        self.tabs[self.active_tab].results.clear();
        let context = self
            .namespace
            .snapshot
            .as_ref()
            .map(|s| {
                format!(
                    "Run started: {}{}",
                    s.database,
                    s.current_schema
                        .as_ref()
                        .map(|n| format!(" · SQL schema {n}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "Run context unverified".into());
        self.namespace.result_context.insert(id, context);
        self.tabs[self.active_tab].metadata_incomplete.clear();
        self.tabs[self.active_tab].query_progress = Some(Default::default());
        self.tabs[self.active_tab].error = None;
        self.selected = None;
        self.busy = true;
        self.error = None;
        self.status = if for_editing {
            "Running SELECT for editing · capturing a fresh snapshot…"
        } else {
            "Running SQL…"
        }
        .into();
        self.transaction_known = false;
        self.page = Page::Query;
        self.tabs[self.active_tab].result_index = 0;
        self.capture_query_result_origin(id, &execution_sql);
        self.begin_query_stream(db, id, sql, execution_sql, for_editing, options);
        cx.notify();
    }

    pub fn format_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_extension_dialog() || self.quick_open {
            return;
        }
        if self.url_input.is_some() {
            return;
        }
        if self.management.is_some() {
            return;
        }
        if !self.session_ready
            || self.transfer.is_some()
            || self.form.is_some()
            || self.has_row_draft()
            || self.close_requested
            || self.close_tab_requested.is_some()
            || self.advanced.designer.is_some()
        {
            return;
        }
        let input = &self.tabs[self.active_tab].editor;
        let engine = self.profile.as_ref().map(|p| p.engine).unwrap_or_default();
        let sql = match rowboat_services::format_sql_for_engine(engine, &input.read(cx).value()) {
            Ok(sql) => sql,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        input.update(cx, |s, cx| s.replace_all(sql, window, cx));
        self.status = "SQL formatted".into();
        cx.notify();
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.cancel_data_transfer() {
            self.status = "Cancelling this file transfer; waiting for cleanup…".into();
            cx.notify();
            return;
        }
        if self.cancel_query_stream() {
            self.status = "Cancelling this SQL stream and waiting for cleanup…".into();
            cx.notify();
            return;
        }
        if self.transfer_running {
            self.status =
                "Backup/restore is using a separate connection; wait for it to finish.".into();
            cx.notify();
            return;
        }
        if let Some(db) = self.database.clone() {
            let tx = self.tx.clone();
            self.status = "Requesting cancellation…".into();
            let runtime = self.runtime.clone();
            self.runtime.spawn_blocking(move || {
                runtime.block_on(async move {
                    let result = db
                        .cancel()
                        .await
                        .map(|_| "Cancellation requested".into())
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::TransactionState(
                        db.in_transaction().await.map_err(|e| format!("{e:#}")),
                    ));
                    let _ = tx.send(Event::Notice(result));
                })
            });
        }
        cx.notify();
    }

    pub fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.close_requested
            || self.has_row_draft()
            || !self.changes.is_empty()
            || self.modal_open()
        {
            return;
        }
        self.invalidate_sidebar_objects();
        self.load_namespace(self.namespace.selected.clone(), false, cx);
    }

    pub fn open_table(&mut self, table: TableRef, cx: &mut Context<Self>) {
        if self.table.as_ref() == Some(&table) && !self.has_row_draft() {
            if self.busy {
                self.fail(
                    "Wait for the active operation before returning to table content.".into(),
                );
                cx.notify();
                return;
            }
            self.selected = None;
            self.widths.clear();
            self.page = Page::Table;
            self.quick_open = false;
            if self.changes.is_empty() {
                self.table_page = TablePage::Content;
                self.refresh_table(cx);
            }
            cx.notify();
            return;
        }
        if !self.can_leave() {
            cx.notify();
            return;
        }
        self.table = Some(table);
        self.filters = Default::default();
        self.table_result = QueryResult::default();
        self.structure = Structure::default();
        self.selected = None;
        self.page = Page::Table;
        self.table_page = TablePage::Content;
        self.browse_request = BrowseRequest {
            limit: 200,
            ..Default::default()
        };
        self.quick_open = false;
        self.refresh_table(cx);
    }

    pub fn refresh_table(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.has_row_draft() {
            return;
        }
        let (Some(db), Some(table)) = (self.database.clone(), self.table.clone()) else {
            return;
        };
        if self.browse_request.limit != self.state.preferences.page_size {
            self.browse_request.limit = self.state.preferences.page_size;
            self.browse_request.offset = 0;
        }
        let request = self.browse_request.clone();
        let expression = self.filter_expression();
        self.table_result = QueryResult::default();
        self.selected = None;
        let tx = self.tx.clone();
        self.busy = true;
        self.error = None;
        self.status = format!("Loading {}.{}…", table.schema, table.name);
        self.capture_table_result_origin();
        self.runtime.spawn(async move {
            let result = async {
                let structure = db.structure(&table).await?;
                let result = db
                    .browse_filtered(&table, &request, expression.as_ref())
                    .await?;
                Ok::<_, anyhow::Error>((result, structure))
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Browse { table, result });
        });
        cx.notify();
    }

    pub fn sort_column(&mut self, name: String, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        if self.busy || self.close_requested || self.has_row_draft() {
            return;
        }
        if self.page == Page::Query {
            // Compatibility for named callers; headers use unambiguous ordinals.
            let column = self.current_result().and_then(|result| {
                let mut matches = result
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| column.name == name);
                let (index, _) = matches.next()?;
                matches.next().is_none().then_some(index)
            });
            if let Some(column) = column {
                self.sort_grid_column(column, cx);
            }
            return;
        }
        if self.page != Page::Table || !self.can_leave() {
            cx.notify();
            return;
        }
        let ascending = self
            .browse_request
            .sort
            .as_ref()
            .filter(|(column, _)| column == &name)
            .is_none_or(|(_, ascending)| !ascending);
        self.browse_request.sort = Some((name, ascending));
        self.browse_request.offset = 0;
        self.refresh_table(cx);
    }

    pub fn current_result(&self) -> Option<&QueryResult> {
        if self.page == Page::Table {
            Some(&self.table_result)
        } else {
            let tab = &self.tabs[self.active_tab];
            tab.results.get(tab.result_index)
        }
    }
    pub fn copy_cell(&mut self, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        self.copy_grid(false, cx);
    }
    pub fn copy_row(&mut self, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        self.copy_grid(true, cx);
    }

    pub fn edit_row(&mut self, insert: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        if self.page == Page::Query {
            self.edit_query_row(insert, window, cx);
            return;
        }
        self.query_edit.draft = None;
        if !self.grid_writable() || self.grid.draft.is_some() {
            return;
        }
        if self.structure.columns.is_empty() {
            self.fail("Load the table structure before editing rows.".into());
            cx.notify();
            return;
        }
        let original = if insert {
            None
        } else {
            if !self.structure.columns.iter().any(|c| c.primary_key) {
                self.fail(
                    "This table has no primary key; safe row updates and deletes are unavailable."
                        .into(),
                );
                cx.notify();
                return;
            }
            let Some((row, _)) = self.selected_cell() else {
                return;
            };
            let Some(values) = self.table_result.rows.get(row) else {
                return;
            };
            Some(
                self.table_result
                    .columns
                    .iter()
                    .zip(values)
                    .map(|(c, v)| (c.name.clone(), v.clone()))
                    .collect(),
            )
        };
        let Some(target) = self.current_target() else {
            return;
        };
        let mut editor = RowEditor::new(target, &self.structure, original.clone(), window, cx);
        if let Some(original) = original {
            for change in &self.changes {
                match change {
                    RowChange::Delete { original: old } if old == &original => {
                        self.fail("This row is already staged for deletion. Remove that change from the review first.".into());
                        cx.notify();
                        return;
                    }
                    RowChange::Update {
                        original: old,
                        values,
                    } if old == &original => {
                        for (name, value) in values {
                            if let Some(field) = editor.fields.iter_mut().find(|f| &f.name == name)
                            {
                                field.null = matches!(value, Cell::Null);
                                field.include = true;
                                field.value.update(cx, |s, cx| {
                                    s.set_value(
                                        if field.null {
                                            String::new()
                                        } else {
                                            value.display()
                                        },
                                        window,
                                        cx,
                                    )
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.row_editor = Some(editor);
        if let Some(editor) = &self.row_editor {
            let name = self
                .selected_cell()
                .and_then(|(_, col)| self.table_result.columns.get(col))
                .map(|c| c.name.as_str());
            if let Some(field) = editor
                .fields
                .iter()
                .find(|f| Some(f.name.as_str()) == name)
                .or_else(|| editor.fields.first())
            {
                field.value.update(cx, |s, cx| s.focus(window, cx));
            }
        }
        cx.notify();
    }

    pub fn stage_editor(&mut self, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        if self.busy || self.close_requested {
            return;
        }
        let Some(editor) = &self.row_editor else {
            return;
        };
        let target = editor.target.clone();
        if self.editing_target() != Some(target.clone())
            || !self.query_draft_current()
            || self.change_target.as_ref().is_some_and(|t| t != &target)
        {
            self.fail(
                "The row editor belongs to another connection or table. No change was staged."
                    .into(),
            );
            cx.notify();
            return;
        }
        match editor.change(cx) {
            Ok(change) => {
                let mut changes = self.changes.clone();
                crate::staging::stage(&mut changes, change);
                if let Err(error) = self.validate_query_changes(&changes) {
                    self.fail(error);
                    cx.notify();
                    return;
                }
                self.changes = changes;
                self.bind_query_changes();
                self.change_target = (!self.changes.is_empty()).then_some(target);
                self.row_editor = None;
                self.status = format!(
                    "{} changes staged · review before applying",
                    self.changes.len()
                );
            }
            Err(e) => self.fail(e.to_string()),
        }
        cx.notify();
    }

    pub fn stage_delete(&mut self, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        if self.page == Page::Query {
            return;
        }
        if !self.grid_writable() || self.has_row_draft() {
            return;
        }
        if !self.structure.columns.iter().any(|c| c.primary_key) {
            self.fail("A primary key is required for safe deletes.".into());
            cx.notify();
            return;
        }
        let Some(target) = self.current_target() else {
            return;
        };
        if self.change_target.as_ref().is_some_and(|t| t != &target) {
            self.fail("Staged changes belong to a different table.".into());
            cx.notify();
            return;
        }
        if let Some((row, _)) = self.selected_cell()
            && let Some(values) = self.table_result.rows.get(row)
        {
            let original = self
                .table_result
                .columns
                .iter()
                .zip(values)
                .map(|(c, v)| (c.name.clone(), v.clone()))
                .collect();
            crate::staging::stage(&mut self.changes, RowChange::Delete { original });
            self.change_target = Some(target);
            self.status = "Delete staged · review before applying".into();
        }
        cx.notify();
    }

    pub fn apply_changes(&mut self, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        if self.query_edit.batch.is_some() {
            self.apply_query_edits(cx);
            return;
        }
        if !self.grid_writable() || self.has_row_draft() || self.changes.is_empty() {
            return;
        }
        let Some(target) = self.change_target.clone() else {
            self.fail("The pending change batch has no bound target.".into());
            cx.notify();
            return;
        };
        if self.current_target() != Some(target.clone()) {
            self.fail("The active connection/table no longer matches the staged batch. No changes were applied.".into());
            cx.notify();
            return;
        }
        let (Some(db), table) = (self.database.clone(), target.table) else {
            return;
        };
        let changes = self.changes.clone();
        let marker = match self.prepare_apply_marker() {
            Ok(id) => id,
            Err(error) => {
                self.fail(format!(
                    "Apply blocked: cannot durably record intent: {error:#}"
                ));
                cx.notify();
                return;
            }
        };
        let (ack, persisted) = tokio::sync::oneshot::channel();
        if self
            .shell_tx
            .send(crate::workspace_shell::Command::PersistApply {
                route: self.tx.route,
                marker,
                ack,
            })
            .is_err()
        {
            self.finish_apply_marker(false);
            self.fail("Apply did not start: recovery coordinator unavailable".into());
            cx.notify();
            return;
        }
        let tx = self.tx.clone();
        self.busy = true;
        self.error = None;
        self.status = "Persisting Apply intent before mutation…".into();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let result = crate::recovery_ui::after_durable_intent(persisted, async {
                    db.apply_changes(&table, &changes)
                        .await
                        .map_err(|e| format!("{e:#}"))
                })
                .await;
                let _ = tx.send(Event::TransactionState(
                    db.in_transaction().await.map_err(|e| format!("{e:#}")),
                ));
                let _ = tx.send(Event::Applied(result));
            })
        });
        cx.notify();
    }

    pub fn transaction(&mut self, command: &'static str, cx: &mut Context<Self>) {
        if !self.can_leave() {
            cx.notify();
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        let tx = self.tx.clone();
        self.busy = true;
        self.error = None;
        self.status = format!("{command}…");
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let result = match command {
                    "BEGIN" => db.begin().await,
                    "COMMIT" => db.commit().await,
                    _ => db.rollback().await,
                }
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::TransactionState(
                    db.in_transaction().await.map_err(|e| format!("{e:#}")),
                ));
                let _ = tx.send(Event::Transaction { result });
            })
        });
        cx.notify();
    }

    pub fn favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_library_query(window, cx);
    }

    pub fn export(&mut self, format: &'static str, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        let Some(mut result) = self.current_result().cloned() else {
            return;
        };
        if self.page == Page::Query {
            let filter = self.result_filter.read(cx).value().to_lowercase();
            if !filter.is_empty() {
                result.rows.retain(|row| {
                    row.iter()
                        .any(|cell| cell.display().to_lowercase().contains(&filter))
                });
            }
        }
        let engine = self.profile.as_ref().map(|p| p.engine).unwrap_or_default();
        let table = if self.page == Page::Table {
            self.table.clone()
        } else {
            None
        };
        if format == "sql" && table.is_none() {
            self.fail("SQL INSERT export requires a table content view.".into());
            cx.notify();
            return;
        }
        let dialog = cx.prompt_for_new_path(
            std::path::Path::new(std::env::var("HOME").as_deref().unwrap_or(".")),
            Some(&format!("rowboat-export.{format}")),
        );
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(path))) = dialog.await {
                runtime.spawn(async move {
                    let result = async {
                        let data = match format {
                            "csv" => rowboat_services::export_csv(&result)?,
                            "json" => rowboat_services::export_json(&result)?,
                            _ => rowboat_services::export_sql(engine, &table.unwrap(), &result)?,
                        };
                        tokio::fs::write(&path, data).await?;
                        Ok::<_, anyhow::Error>(format!(
                            "Exported displayed result to {}",
                            path.display()
                        ))
                    }
                    .await
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Notice(result));
                });
            }
        })
        .detach();
    }

    pub fn import(&mut self, format: &'static str, cx: &mut Context<Self>) {
        if self.results_read_only() {
            return;
        }
        self.show_data_transfer(
            true,
            if format == "csv" {
                rowboat_services::TransferFormat::Csv
            } else {
                rowboat_services::TransferFormat::JsonLines
            },
            cx,
        );
    }

    pub fn import_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_library_file(window, cx);
    }
}

#[cfg(test)]
mod event_routing_tests {
    use super::{Envelope, Event, EventSender};
    use crate::workspace_model::Route;
    use std::sync::mpsc;
    #[test]
    fn switching_visible_workspace_does_not_redirect_running_job_events() {
        let (a_tx, a_rx) = mpsc::channel::<Envelope>();
        let (b_tx, b_rx) = mpsc::channel::<Envelope>();
        let a = EventSender {
            route: Route {
                workspace: 1,
                generation: 3,
            },
            sender: a_tx,
        };
        let b = EventSender {
            route: Route {
                workspace: 2,
                generation: 3,
            },
            sender: b_tx,
        };
        let running_job = a.clone();
        let now_visible = b.clone();
        running_job.send(Event::TransactionState(Ok(true))).unwrap();
        now_visible
            .send(Event::TransactionState(Ok(false)))
            .unwrap();
        let a_event = a_rx.try_recv().unwrap();
        let b_event = b_rx.try_recv().unwrap();
        assert_eq!(a_event.route, a.route);
        assert_eq!(b_event.route, b.route);
        assert!(matches!(a_event.event, Event::TransactionState(Ok(true))));
        assert!(matches!(b_event.event, Event::TransactionState(Ok(false))));
        let mut reconnect = a.clone();
        reconnect.route.generation += 1;
        running_job
            .send(Event::Notice(Ok("late completion".into())))
            .unwrap();
        assert_ne!(a_rx.try_recv().unwrap().route, reconnect.route);
    }
}
