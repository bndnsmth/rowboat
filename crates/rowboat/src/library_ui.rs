//! Saved-query library and file documents. Each workspace owns its drafts;
//! mutations consult the shared writer rather than its cached AppState.
use crate::{
    app::{Page, Workbench},
    forms,
    view::{action, column, label, row},
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use library::{HistoryFilter, HistoryStatus, QueryMetadata, SavedQuery, SqlDocument};
use rowboat_services::{Snippet, query_library as library};
use std::{collections::BTreeMap, path::PathBuf, sync::mpsc};

pub struct LibraryState {
    pub documents: BTreeMap<usize, SqlDocument>,
    pub search: Entity<InputState>,
    pub from: Entity<InputState>,
    pub through: Entity<InputState>,
    pub connection: Option<String>,
    pub status: HistoryStatus,
    pub form: Option<QueryForm>,
    pub file_pending: bool,
    tx: mpsc::Sender<FileEvent>,
    rx: mpsc::Receiver<FileEvent>,
    _subscriptions: Vec<Subscription>,
}
pub struct QueryForm {
    expected: Option<SavedQuery>,
    id: String,
    sql: String,
    tab: Option<usize>,
    pub name: Entity<InputState>,
    pub folder: Entity<InputState>,
    connection: Option<String>,
}
enum FileEvent {
    Open {
        owner: crate::workspace_model::AsyncOwner,
        result: Result<Option<(String, SqlDocument)>, String>,
    },
    Save {
        tab: usize,
        prior: Option<SqlDocument>,
        result: Result<Option<SqlDocument>, String>,
    },
}

fn sql_preview(sql: &str) -> String {
    sql.lines()
        .take(3)
        .flat_map(|line| line.chars().chain(std::iter::once('\n')))
        .take(240)
        .collect::<String>()
        .trim_end()
        .to_owned()
}
impl LibraryState {
    pub fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let search = forms::input("", "Search SQL, name or folder", window, cx);
        let from = forms::input("", "From YYYY-MM-DD (UTC)", window, cx);
        let through = forms::input("", "Through YYYY-MM-DD (UTC)", window, cx);
        let subscriptions = [&search, &from, &through]
            .into_iter()
            .map(|e| cx.observe(e, |_, _, cx| cx.notify()))
            .collect();
        let (tx, rx) = mpsc::channel();
        Self {
            documents: BTreeMap::new(),
            search,
            from,
            through,
            connection: None,
            status: HistoryStatus::All,
            form: None,
            file_pending: false,
            tx,
            rx,
            _subscriptions: subscriptions,
        }
    }
}

impl Workbench {
    /// Snapshot only live tabs; closed tabs must not keep orphaned recovery entries.
    pub fn library_document_snapshot(&self) -> BTreeMap<usize, SqlDocument> {
        self.library
            .documents
            .iter()
            .filter(|(id, _)| self.tabs.iter().any(|tab| tab.id == **id))
            .map(|(id, document)| (*id, document.clone()))
            .collect()
    }

    /// Restore bindings without rereading files or replacing the recovered editor
    /// buffer. Its original fingerprint makes the next explicit Save detect edits
    /// made on disk while Rowboat was closed.
    pub fn restore_library_documents(&mut self, documents: &BTreeMap<usize, SqlDocument>) {
        self.library.documents = documents
            .iter()
            .filter(|(id, _)| self.tabs.iter().any(|tab| tab.id == **id))
            .map(|(id, document)| (*id, document.clone()))
            .collect();
    }

    fn library_navigation_allowed(&self) -> bool {
        self.session_ready
            && !self.busy
            && !self.close_requested
            && !self.has_row_draft()
            && self.changes.is_empty()
            && !self.library.file_pending
    }

    pub fn open_library_query(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_leave() {
            cx.notify();
            return;
        }
        let query = library::saved_query(&self.shared.borrow().state, id);
        if let Some(query) = query {
            self.new_query(Some(query.snippet.sql.clone()), window, cx);
            let tab = &mut self.tabs[self.active_tab];
            tab.title = query.snippet.name.clone();
            self.library
                .documents
                .insert(tab.id, SqlDocument::Saved { baseline: query });
            self.status = "Opened saved copy in this workspace · connection association does not connect or run SQL".into();
            self.queue_session_save(cx);
        }
    }

    /// Save updates a bound identity. A new draft opens the naming form.
    pub fn save_library_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.library_navigation_allowed() || self.modal_open() {
            return;
        }
        let tab = &self.tabs[self.active_tab];
        if let Some(SqlDocument::Saved { baseline }) = self.library.documents.get(&tab.id).cloned()
        {
            let mut query = baseline.clone();
            query.snippet.sql = tab.editor.read(cx).value().to_string();
            let tab_id = tab.id;
            match self.commit_library_query(query.clone(), Some(&baseline)) {
                Ok(()) => {
                    self.library
                        .documents
                        .insert(tab_id, SqlDocument::Saved { baseline: query });
                    self.status = "Saved query updated · same identity".into();
                    self.queue_session_save(cx);
                }
                Err(error) => self.fail(format!("{error:#}")),
            }
            cx.notify();
        } else {
            self.save_library_query_as(window, cx);
        }
    }

    fn commit_library_query(
        &mut self,
        query: SavedQuery,
        expected: Option<&SavedQuery>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.shared.borrow().writable,
            "Application data is not writable"
        );
        let mut result = Ok(());
        self.edit_state(|state| result = library::save_query(state, query, expected));
        result
    }

    pub fn save_library_query_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.library_navigation_allowed() || self.modal_open() {
            return;
        }
        let tab = &self.tabs[self.active_tab];
        let metadata = match self.library.documents.get(&tab.id) {
            Some(SqlDocument::Saved { baseline }) => baseline.metadata.clone(),
            _ => QueryMetadata {
                folder: String::new(),
                connection_id: self.owner.clone(),
            },
        };
        self.library.form = Some(QueryForm {
            expected: None,
            id: uuid::Uuid::new_v4().to_string(),
            sql: tab.editor.read(cx).value().to_string(),
            tab: Some(tab.id),
            name: forms::input(tab.title.clone(), "Query name", window, cx),
            folder: forms::input(metadata.folder, "Folder (optional)", window, cx),
            connection: metadata.connection_id,
        });
        self.library
            .form
            .as_ref()
            .unwrap()
            .name
            .update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn rename_library_query(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.library_navigation_allowed() || self.modal_open() {
            return;
        }
        let Some(query) = library::saved_query(&self.shared.borrow().state, id) else {
            return;
        };
        self.library.form = Some(QueryForm {
            id: query.snippet.id.clone(),
            sql: query.snippet.sql.clone(),
            tab: None,
            name: forms::input(query.snippet.name.clone(), "Query name", window, cx),
            folder: forms::input(
                query.metadata.folder.clone(),
                "Folder (optional)",
                window,
                cx,
            ),
            connection: query.metadata.connection_id.clone(),
            expected: Some(query),
        });
        self.library
            .form
            .as_ref()
            .unwrap()
            .name
            .update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn submit_library_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.library.form else {
            return;
        };
        let query = SavedQuery {
            snippet: Snippet {
                id: form.id.clone(),
                name: form.name.read(cx).value().trim().to_string(),
                sql: form.sql.clone(),
            },
            metadata: QueryMetadata {
                folder: form.folder.read(cx).value().trim().to_string(),
                connection_id: form.connection.clone(),
            },
        };
        let expected = form.expected.clone();
        let tab = form.tab;
        match self.commit_library_query(query.clone(), expected.as_ref()) {
            Ok(()) => {
                if let Some(tab) = tab.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
                    tab.title = query.snippet.name.clone();
                    self.library.documents.insert(
                        tab.id,
                        SqlDocument::Saved {
                            baseline: query.clone(),
                        },
                    );
                }
                // Rename keeps open bindings current without discarding edited SQL.
                for (id, document) in &mut self.library.documents {
                    if let SqlDocument::Saved { baseline } = document
                        && Some(&*baseline) == expected.as_ref()
                    {
                        *baseline = query.clone();
                        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == *id) {
                            tab.title = query.snippet.name.clone();
                        }
                    }
                }
                self.library.form = None;
                self.status =
                    "Named query saved locally · association does not connect or execute".into();
                window.focus(&self.focus, cx);
                self.queue_session_save(cx);
            }
            Err(error) => self.fail(format!("{error:#}")),
        }
        cx.notify();
    }

    pub fn dismiss_library(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.library.form.take().is_none() {
            return false;
        }
        window.focus(&self.focus, cx);
        cx.notify();
        true
    }

    fn delete_library_query(&mut self, expected: SavedQuery, cx: &mut Context<Self>) {
        if !self.shared.borrow().writable {
            self.fail("Application data is not writable".into());
            return;
        }
        let mut result = Ok(());
        self.edit_state(|state| result = library::remove_query(state, &expected));
        match result {
            Ok(()) => self.status = "Saved copy removed · open SQL drafts retained".into(),
            Err(error) => self.fail(format!("{error:#}")),
        }
        cx.notify();
    }

    pub fn open_library_file(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_leave() || self.library.file_pending {
            cx.notify();
            return;
        }
        let owner = crate::workspace_model::AsyncOwner {
            route: self.tx.route,
            connection: self.owner.clone(),
        };
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        let tx = self.library.tx.clone();
        let runtime = self.runtime.clone();
        self.library.file_pending = true;
        cx.spawn(async move |_, _| {
            let result = async {
                let Some(path) = dialog.await??.and_then(|paths| paths.into_iter().next()) else {
                    return anyhow::Ok(None);
                };
                Ok(Some(
                    runtime
                        .spawn_blocking(move || library::open_sql_document(&path))
                        .await??,
                ))
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(FileEvent::Open { owner, result });
        })
        .detach();
        cx.notify();
    }

    pub fn save_library_file(&mut self, save_as: bool, cx: &mut Context<Self>) {
        if !self.library_navigation_allowed() || self.modal_open() {
            return;
        }
        let tab = &self.tabs[self.active_tab];
        let tab_id = tab.id;
        let sql = tab.editor.read(cx).value().to_string();
        let prior = self.library.documents.get(&tab.id).cloned();
        let file = prior
            .clone()
            .filter(|d| matches!(d, SqlDocument::File { .. }) && !save_as);
        let dialog = file.is_none().then(|| {
            cx.prompt_for_new_path(
                std::path::Path::new(std::env::var("HOME").as_deref().unwrap_or(".")),
                Some("query.sql"),
            )
        });
        let tx = self.library.tx.clone();
        let runtime = self.runtime.clone();
        self.library.file_pending = true;
        cx.spawn(async move |_, _| {
            let result = async {
                let path: Option<PathBuf> = if let Some(dialog) = dialog {
                    dialog.await??
                } else {
                    None
                };
                if file.is_none() && path.is_none() {
                    return anyhow::Ok(None);
                }
                Ok(Some(
                    runtime
                        .spawn_blocking(move || match file {
                            Some(document) => library::save_sql_document(&document, &sql),
                            None => library::create_sql_document(&path.unwrap(), &sql),
                        })
                        .await??,
                ))
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(FileEvent::Save {
                tab: tab_id,
                prior,
                result,
            });
        })
        .detach();
        cx.notify();
    }

    /// Call from the existing UI poll. No connection/session event can redirect a
    /// completed open into another workspace; a save updates only its original tab.
    pub fn poll_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Ok(event) = self.library.rx.try_recv() {
            self.library.file_pending = false;
            match event {
                FileEvent::Open { owner, result } => {
                    if owner.route != self.tx.route
                        || owner.connection != self.owner
                        || self.close_requested
                    {
                        self.status =
                            "Workspace changed while choosing SQL file; open it again".into();
                    } else {
                        match result {
                            Ok(Some((sql, document))) => {
                                if self.can_leave() {
                                    self.new_query(Some(sql), window, cx);
                                    let tab = &mut self.tabs[self.active_tab];
                                    if let SqlDocument::File { path, .. } = &document {
                                        tab.title = path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .into_owned();
                                    }
                                    self.library.documents.insert(tab.id, document);
                                    self.status = "File-backed SQL opened · not executed".into();
                                    self.queue_session_save(cx);
                                }
                            }
                            Ok(None) => (),
                            Err(error) => self.fail(error),
                        }
                    }
                }
                FileEvent::Save { tab, prior, result } => match result {
                    Ok(Some(document)) => {
                        if self.tabs.iter().any(|t| t.id == tab)
                            && self.library.documents.get(&tab) == prior.as_ref()
                        {
                            self.library.documents.insert(tab, document);
                        }
                        self.status =
                            "SQL file saved · edits made during saving remain dirty".into();
                        self.queue_session_save(cx);
                    }
                    Ok(None) => (),
                    Err(error) => self.fail(error),
                },
            }
            cx.notify();
        }
    }

    fn library_connection_cycle(&mut self, association: bool, cx: &mut Context<Self>) {
        let state = self.shared.borrow();
        let mut ids = state
            .state
            .connections
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>();
        if !association {
            ids.extend(state.state.history.iter().map(|h| h.connection_id.clone()));
        }
        ids.sort();
        ids.dedup();
        let current = if association {
            self.library
                .form
                .as_ref()
                .and_then(|f| f.connection.as_ref())
        } else {
            self.library.connection.as_ref()
        };
        let next = current
            .and_then(|id| ids.iter().position(|candidate| candidate == id))
            .map_or(0, |i| i + 1);
        let next = ids.get(next).cloned();
        if association {
            if let Some(form) = &mut self.library.form {
                form.connection = next;
            }
        } else {
            self.library.connection = next;
        }
        cx.notify();
    }

    fn library_history_filter(&self, cx: &App) -> anyhow::Result<HistoryFilter> {
        let parse = |input: &Entity<InputState>| -> anyhow::Result<_> {
            let text = input.read(cx).value();
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(
                    chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
                        .map_err(|_| anyhow::anyhow!("Use dates in YYYY-MM-DD format (UTC)"))?,
                ))
            }
        };
        let from = parse(&self.library.from)?;
        let through = parse(&self.library.through)?;
        anyhow::ensure!(
            !matches!((from, through), (Some(a), Some(b)) if a > b),
            "From date must not follow Through date"
        );
        Ok(HistoryFilter {
            text: self.library.search.read(cx).value().to_string(),
            connection_id: self.library.connection.clone(),
            from,
            through,
            status: self.library.status,
        })
    }

    pub fn render_library(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let saved = self.saved_queries_view;
        let shared = self.shared.borrow();
        let state = &shared.state;
        let enabled = self.library_navigation_allowed();
        // The scroll viewport owns the bounded height; this content column keeps
        // its intrinsic height. Otherwise flexbox squeezes hundreds of cards into
        // one viewport and their text paints over adjacent rows.
        let mut body = column().w_full().flex_shrink_0().p_4().gap_3().debug_selector(||"library-content".into())
            .child(div().flex_shrink_0().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child(if saved { "Saved queries" } else { "Query history" }))
            .child(label(if saved { "Stored locally · Open loads SQL into this workspace without connecting or executing" } else { "Stored locally · Open loads SQL into its originating workspace without connecting or executing" }, cx).flex_shrink_0())
            .child(div().w_full().flex_shrink_0().child(ui::input("library-search", &self.library.search, window, cx)));
        if saved {
            let queries = state
                .snippets
                .iter()
                .filter_map(|s| library::saved_query(state, &s.id))
                .filter(|q| library::query_matches(q, &self.library.search.read(cx).value()))
                .collect::<Vec<_>>();
            body = body.child(label(
                format!(
                    "{} of {} saved queries",
                    queries.len(),
                    state.snippets.len()
                ),
                cx,
            ));
            for query in queries {
                let open = query.snippet.id.clone();
                let rename = open.clone();
                let delete = query.clone();
                let association = query
                    .metadata
                    .connection_id
                    .as_deref()
                    .map(|id| self.connection_label(Some(id)))
                    .unwrap_or_else(|| "No connection association".into());
                body = body.child(
                    column()
                        .flex_shrink_0()
                        .min_h(px(88.))
                        .debug_selector({
                            let id = format!("saved-library-row-{}", query.snippet.id);
                            move || id.clone()
                        })
                        .gap_2()
                        .p_3()
                        .border_b_1()
                        .border_color(theme.divider())
                        .child(
                            row()
                                .flex_shrink_0()
                                .flex_wrap()
                                .child(
                                    div()
                                        .flex_1()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(query.snippet.name.clone()),
                                )
                                .child(
                                    action(
                                        format!("library-open-{open}"),
                                        "Open",
                                        false,
                                        enabled,
                                        cx,
                                        move |s, w, cx| s.open_library_query(&open, w, cx),
                                    )
                                    .debug_selector({
                                        let selector =
                                            format!("saved-query-open-{}", query.snippet.id);
                                        move || selector.clone()
                                    }),
                                )
                                .child(
                                    action(
                                        format!("library-rename-{rename}"),
                                        "Rename / organize",
                                        false,
                                        enabled && shared.writable,
                                        cx,
                                        move |s, w, cx| s.rename_library_query(&rename, w, cx),
                                    )
                                    .debug_selector({
                                        let selector =
                                            format!("library-rename-{}", query.snippet.id);
                                        move || selector.clone()
                                    }),
                                )
                                .child(action(
                                    format!("library-delete-{}", query.snippet.id),
                                    "Remove",
                                    false,
                                    enabled && shared.writable,
                                    cx,
                                    move |s, _, cx| s.delete_library_query(delete.clone(), cx),
                                )),
                        )
                        .child(label(
                            format!(
                                "Saved copy · {} · {association}",
                                if query.metadata.folder.is_empty() {
                                    "Unfiled"
                                } else {
                                    &query.metadata.folder
                                }
                            ),
                            cx,
                        ))
                        .child(
                            label(sql_preview(&query.snippet.sql), cx)
                                .w_full()
                                .flex_shrink_0()
                                .whitespace_normal()
                                .font_family(theme.mono_font.clone()),
                        ),
                );
            }
        } else {
            body = body.child(
                row()
                    .flex_shrink_0()
                    .flex_wrap()
                    .gap_2()
                    .child(action(
                        "history-connection",
                        format!(
                            "Connection: {} · next",
                            self.library
                                .connection
                                .as_deref()
                                .map(|id| self.connection_label(Some(id)))
                                .unwrap_or_else(|| "All".into())
                        ),
                        false,
                        true,
                        cx,
                        |s, _, cx| s.library_connection_cycle(false, cx),
                    ))
                    .child(action(
                        "history-status",
                        format!("Status: {:?} · next", self.library.status),
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            s.library.status = match s.library.status {
                                HistoryStatus::All => HistoryStatus::Success,
                                HistoryStatus::Success => HistoryStatus::Failure,
                                HistoryStatus::Failure => HistoryStatus::All,
                            };
                            cx.notify();
                        },
                    ))
                    .child(ui::input("history-from", &self.library.from, window, cx).w(px(220.)))
                    .child(
                        ui::input("history-through", &self.library.through, window, cx).w(px(220.)),
                    ),
            );
            body = body.child(action(
                "clear-history",
                "Clear history",
                false,
                shared.writable && !state.history.is_empty(),
                cx,
                |s, _, cx| {
                    if s.shared.borrow().writable {
                        s.edit_state(|state| state.history.clear());
                    }
                    cx.notify();
                },
            ));
            match self.library_history_filter(cx) {
                Err(error) => {
                    body = body.child(label(error.to_string(), cx).text_color(theme.danger))
                }
                Ok(filter) => {
                    let entries = library::history_matches(state, &filter).collect::<Vec<_>>();
                    body = body.child(label(
                        format!(
                            "{} matches across all {} retained executions (limit {})",
                            entries.len(),
                            state.history.len(),
                            rowboat_services::HISTORY_LIMIT
                        ),
                        cx,
                    ));
                    for entry in entries {
                        let sql = entry.sql.clone();
                        let owner = entry.connection_id.clone();
                        body = body.child(
                            column()
                                .flex_shrink_0()
                                .min_h(px(88.))
                                .debug_selector({ let id = format!("history-library-row-{}",entry.id); move || id.clone() })
                                .gap_2()
                                .p_3()
                                .border_b_1()
                                .border_color(theme.divider())
                                .child(
                                    row()
                                        .flex_shrink_0()
                                        .flex_wrap()
                                        .child(
                                            label(
                                                format!(
                                                    "{} · {} · {} · {} ms",
                                                    entry.executed_at,
                                                    self.connection_label(Some(
                                                        &entry.connection_id
                                                    )),
                                                    if entry.error.is_some() {
                                                        "Failed"
                                                    } else {
                                                        "Succeeded"
                                                    },
                                                    entry.elapsed_ms
                                                ),
                                                cx,
                                            )
                                            .flex_1(),
                                        )
                                        .child(action(
                                            format!("library-history-{}", entry.id),
                                            "Open",
                                            false,
                                            enabled,
                                            cx,
                                            move |s, _, cx| {
                                                if s.library_navigation_allowed() {
                                                    let _ = s.shell_tx.send(crate::workspace_shell::Command::OpenQuery { owner: Some(owner.clone()), sql: sql.clone(), title: "History query".into() });
                                                    cx.notify();
                                                }
                                            },
                                        ).debug_selector({ let selector = format!("history-query-open-{}",entry.id); move || selector.clone() })),
                                )
                                .child(label(sql_preview(&entry.sql), cx).w_full().flex_shrink_0().whitespace_normal().font_family(theme.mono_font.clone())
                                    .debug_selector({ let id=format!("history-library-preview-{}",entry.id);move ||id.clone() })),
                        );
                    }
                }
            }
        }
        div()
            .id("query-library-scroll")
            .size_full()
            .overflow_y_scroll()
            .debug_selector(|| "library-scroll".into())
            .child(body)
            .into_any_element()
    }

    pub fn render_library_form(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let form = self.library.form.as_ref().unwrap();
        self.modal(column().id("saved-query-form").w(px(640.).min(window.viewport_size().width-px(48.))).p_4().gap_3()
            .child(label(if form.expected.is_some() {"Rename / organize saved query"} else {"Save SQL as a named copy"},cx))
            .child(ui::input("library-name",&form.name,window,cx))
            .child(ui::input("library-folder",&form.folder,window,cx))
            .child(action("library-association",format!("Association: {} · next",form.connection.as_deref().map(|id|self.connection_label(Some(id))).unwrap_or_else(||"None".into())),false,true,cx,|s,_,cx|s.library_connection_cycle(true,cx)))
            .child(label("Association is a label only. SQL opens in the chosen workspace and never auto-connects or runs.",cx))
            .child(row().child(action("library-submit","Save",true,self.shared.borrow().writable,cx,|s,w,cx|s.submit_library_form(w,cx)).debug_selector(||"library-submit".into()))
                .child(action("library-cancel","Cancel",false,true,cx,|s,w,cx|{s.dismiss_library(w,cx);}).debug_selector(||"library-cancel".into()))).into_any_element(),cx)
    }

    /// Render alongside the SQL editor toolbar: file path and dirty state are
    /// deliberately distinct from a named saved copy and from an unbound draft.
    pub fn render_library_document(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[self.active_tab];
        let sql = tab.editor.read(cx).value();
        let text = match self.library.documents.get(&tab.id) {
            Some(document) => format!(
                "{}{}",
                match document {
                    SqlDocument::Saved { baseline } =>
                        format!("Saved copy: {}", baseline.snippet.name),
                    SqlDocument::File { path, .. } => format!("File: {}", path.display()),
                },
                if document.dirty(&sql) {
                    " · modified"
                } else {
                    " · saved"
                }
            ),
            None => "SQL draft · no saved-copy identity or file path".into(),
        };
        let enabled = self.library_navigation_allowed() && self.page == Page::Query;
        row()
            .flex_wrap()
            .gap_2()
            .child(label(text, cx).flex_1())
            .child(action(
                "library-save-query",
                "Save query",
                false,
                enabled,
                cx,
                |s, w, cx| s.save_library_query(w, cx),
            ))
            .child(action(
                "library-save-as",
                "Save query as…",
                false,
                enabled,
                cx,
                |s, w, cx| s.save_library_query_as(w, cx),
            ))
            .child(action(
                "library-save-file",
                "Save file",
                false,
                enabled,
                cx,
                |s, _, cx| s.save_library_file(false, cx),
            ))
            .child(action(
                "library-save-file-as",
                "Save file as…",
                false,
                enabled,
                cx,
                |s, _, cx| s.save_library_file(true, cx),
            ))
            .into_any_element()
    }
}
