//! Native EditorState provider with a memory-only, physical-context metadata cache.
use crate::{
    app::{Page, Workbench},
    workspace_model::Route,
};
use gpui_kit::{
    base::input::{CompletionProvider, Rope},
    *,
};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Position, TextEdit,
};
use rowboat_core::{Database, Engine, TableInfo};
use rowboat_services::{CatalogTable, CompletionCatalog};
use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub struct State {
    key: Option<(Route, u64)>,
    cache: Arc<tokio::sync::Mutex<Metadata>>,
    valid: Arc<AtomicBool>,
    pub note: String,
}
impl State {
    pub fn invalidate(&mut self) {
        self.valid.store(false, Ordering::Release);
        *self = Self::default();
    }
    fn cache(&mut self, key: (Route, u64)) -> (Arc<tokio::sync::Mutex<Metadata>>, Arc<AtomicBool>) {
        if self.key != Some(key) {
            self.invalidate();
            self.key = Some(key);
            self.valid.store(true, Ordering::Release);
        }
        (self.cache.clone(), self.valid.clone())
    }
    fn seed_snapshot(&self, snapshot: &rowboat_core::NamespaceCatalog) {
        // The core validates provenance under the physical-session lease before
        // using this snapshot. Explorer scope alone never defines SQL resolution.
        if let Ok(mut metadata) = self.cache.try_lock()
            && metadata.path.is_none()
        {
            metadata.snapshot = Some(snapshot.clone());
        }
    }
}
#[derive(Default)]
struct Metadata {
    snapshot: Option<rowboat_core::NamespaceCatalog>,
    path: Option<Vec<String>>,
    tables: Vec<CatalogTable>,
    attempted: HashSet<(String, String)>,
    failed: bool,
    catalog: Option<CompletionCatalog>,
    reads: usize,
}
struct RequestAlive(Arc<AtomicBool>);
impl Drop for RequestAlive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Metadata {
    async fn complete(
        &mut self,
        db: &Database,
        extra: Vec<TableInfo>,
        sql: &str,
        caret: usize,
        valid: &AtomicBool,
        alive: &AtomicBool,
    ) -> anyhow::Result<CompletionCatalog> {
        anyhow::ensure!(
            valid.load(Ordering::Acquire) && alive.load(Ordering::Acquire),
            "Obsolete request"
        );
        if self.failed {
            anyhow::bail!("Metadata unavailable; refresh SQL suggestions to retry");
        }
        if self.path.is_none()
            && let Some(snapshot) = self.snapshot.take()
            && let Ok(catalog) = db.completion_catalog_from_snapshot(&snapshot).await
        {
            let (path, tables) = crate::completion_ui::service_relations(catalog);
            self.path = Some(path);
            self.tables = tables;
        }
        anyhow::ensure!(
            valid.load(Ordering::Acquire) && alive.load(Ordering::Acquire),
            "Obsolete request"
        );
        if self.path.is_none() {
            self.reads += 2;
            let (path, tables) = crate::completion_ui::catalog_relations(db, extra, || {
                valid.load(Ordering::Acquire) && alive.load(Ordering::Acquire)
            })
            .await?;
            self.path = Some(path);
            self.tables = tables;
        }
        if self.catalog.is_none() {
            let mut catalog = CompletionCatalog::new(db.engine(), self.tables.clone())?;
            catalog.set_search_path(self.path.clone())?;
            self.catalog = Some(catalog);
        }
        // Relation-name suggestions do not use columns. Avoid whole-table
        // metadata loads while the user is still typing FROM/JOIN targets.
        if rowboat_services::complete_sql(sql, caret, self.catalog.as_ref().unwrap(), 1)?.context
            == rowboat_services::CompletionContext::Relation
        {
            return Ok(self.catalog.as_ref().unwrap().clone());
        }
        let lower = sql.to_lowercase();
        let mut changed = self.catalog.is_none();
        for table in &mut self.tables {
            anyhow::ensure!(
                valid.load(Ordering::Acquire) && alive.load(Ordering::Acquire),
                "Obsolete request"
            );
            if self.attempted.len() >= 32 {
                break;
            }
            let key = (table.table.schema.clone(), table.table.name.clone());
            if !lower.contains(&table.table.name.to_lowercase()) || !self.attempted.insert(key) {
                continue;
            }
            self.reads += 1;
            if let Ok(columns) = crate::completion_ui::column_names(db, &table.table).await {
                table.columns = columns;
                changed = true;
                self.catalog = None;
            }
        }
        if changed {
            let mut catalog = CompletionCatalog::new(db.engine(), self.tables.clone())?;
            catalog.set_search_path(self.path.clone())?;
            self.catalog = Some(catalog);
        }
        Ok(self.catalog.as_ref().expect("catalog built").clone())
    }
}
#[cfg(test)]
impl State {
    #[allow(dead_code)]
    pub(crate) fn metadata_reads(&self) -> usize {
        self.cache.try_lock().map_or(usize::MAX, |c| c.reads)
    }
    #[allow(dead_code)]
    pub(crate) fn metadata_probe(&self) -> impl Fn() -> Option<usize> + Send + Sync + use<> {
        let cache = self.cache.clone();
        move || cache.try_lock().ok().map(|c| c.reads)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Offered {
    pub sql: String,
    pub caret: usize,
    pub route: Route,
    pub host: u64,
    pub context: u64,
}
#[derive(Default)]
pub struct Menu {
    pub cursor: usize,
    pub deliberate: bool,
    pub revision: u64,
    pub offered: Option<Offered>,
    pub items: Vec<CompletionItem>,
}
pub struct Provider {
    workbench: WeakEntity<Workbench>,
    tab: usize,
    host: u64,
    menu: Rc<RefCell<Menu>>,
}
fn position(sql: &str, byte: usize) -> Position {
    let before = &sql[..byte];
    let line = before.bytes().filter(|b| *b == b'\n').count();
    Position::new(
        line as u32,
        before
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .encode_utf16()
            .count() as u32,
    )
}
impl CompletionProvider for Provider {
    fn is_completion_trigger(&self, _: usize, text: &str, _: &mut App) -> bool {
        // Native requests retain their old item vector until a response arrives.
        // Never let that old vector consume Enter/Tab during a new edit/debounce.
        let mut menu = self.menu.borrow_mut();
        menu.offered = None;
        menu.items.clear();
        menu.deliberate = false;
        menu.cursor = 0;
        // The provider API exposes inserted text, not paste/IME provenance.
        // Only a single identifier character or dot automatically opens a list.
        let mut chars = text.chars();
        chars
            .next()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
            && chars.next().is_none()
    }
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        *self.menu.borrow_mut() = Menu::default();
        let sql = text.to_string();
        let wb = self.workbench.clone();
        let tab = self.tab;
        let host = self.host;
        let menu = self.menu.clone();
        let timer = cx.background_executor().timer(Duration::from_millis(150));
        cx.spawn(async move |cx| {
            timer.await;
            let request = wb
                .update(cx, |s, _| {
                    if !s.inline_completion_allowed(tab, host) || sql.len() > 256 * 1024 {
                        return None;
                    }
                    let offered = Offered {
                        sql: sql.clone(),
                        caret: offset,
                        route: s.tx.route,
                        host,
                        context: s.namespace.request,
                    };
                    let engine = s.profile.as_ref().map_or(Engine::Postgres, |p| p.engine);
                    let (cache, valid) = s.advanced.inline.cache((s.tx.route, s.namespace.request));
                    if s.namespace.known
                        && let Some(snapshot) = &s.namespace.snapshot
                    {
                        s.advanced.inline.seed_snapshot(snapshot);
                    }
                    // Metadata reads must not enter an active/unknown user transaction.
                    let database = if s.namespace.known && s.transaction_known && !s.transaction {
                        s.database.clone()
                    } else {
                        None
                    };
                    Some((
                        offered,
                        engine,
                        cache,
                        valid,
                        database,
                        s.catalog.clone(),
                        s.runtime.clone(),
                    ))
                })
                .ok()
                .flatten();
            let Some((offered, engine, cache, valid, db, tables, runtime)) = request else {
                return Ok(CompletionResponse::Array(vec![]));
            };
            let mut catalog = CompletionCatalog::new(engine, vec![])?;
            let mut note = "Offline SQL keywords".to_string();
            let suppressed = rowboat_services::complete_sql(&sql, offset, &catalog, 1)?.context
                == rowboat_services::CompletionContext::Suppressed;
            if let Some(db) = db.filter(|_| !suppressed) {
                let query = sql.clone();
                let valid = valid.clone();
                let request = RequestAlive(Arc::new(AtomicBool::new(true)));
                let alive = request.0.clone();
                // One serialized incremental cache worker; cancelled UI requests
                // do not interrupt a physical-session read already in flight.
                let (send, receive) = std::sync::mpsc::channel();
                runtime.spawn(async move {
                    let mut cache = cache.lock().await;
                    let result = cache
                        .complete(&db, tables, &query, offset, &valid, &alive)
                        .await;
                    if result.is_err()
                        && valid.load(Ordering::Acquire)
                        && alive.load(Ordering::Acquire)
                    {
                        cache.failed = true;
                    }
                    let _ = send.send(result);
                });
                let result = loop {
                    match receive.try_recv() {
                        Ok(result) => break result,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            break Err(anyhow::anyhow!("Metadata worker stopped"));
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            cx.background_executor()
                                .timer(Duration::from_millis(16))
                                .await
                        }
                    }
                };
                match result {
                    Ok(found) => {
                        catalog = found;
                        note = "Live schema suggestions · cached in memory".into();
                    }
                    _ => {
                        note = "Metadata unavailable · Query → Refresh SQL suggestions".into();
                    }
                }
                drop(request);
            }
            let allowed = wb
                .update(cx, |s, _| {
                    s.advanced.inline.note = note;
                    s.inline_completion_allowed(tab, host)
                        && s.tx.route == offered.route
                        && s.namespace.request == offered.context
                        && valid.load(Ordering::Acquire)
                })
                .unwrap_or(false);
            if !allowed {
                return Ok(CompletionResponse::Array(vec![]));
            }
            let found = rowboat_services::complete_sql(&sql, offset, &catalog, 100)?;
            let range = lsp_types::Range::new(
                position(&sql, found.replacement.start),
                position(&sql, found.replacement.end),
            );
            let items = found
                .items
                .into_iter()
                .enumerate()
                .map(|(rank, item)| CompletionItem {
                    label: item.label.clone(),
                    detail: Some(item.detail),
                    sort_text: Some(format!("{rank:04}")),
                    filter_text: Some(item.label),
                    kind: Some(match item.kind {
                        rowboat_services::CompletionKind::Keyword => CompletionItemKind::KEYWORD,
                        rowboat_services::CompletionKind::Column => CompletionItemKind::FIELD,
                        _ => CompletionItemKind::REFERENCE,
                    }),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range,
                        new_text: item.insert_text,
                    })),
                    ..Default::default()
                })
                .collect();
            menu.borrow_mut().offered = Some(offered);
            Ok(CompletionResponse::Array(items))
        })
    }
}

impl Workbench {
    pub fn inline_offer_current(&self, menu: &Menu) -> bool {
        menu.offered.as_ref().is_some_and(|o| {
            o.route == self.tx.route
                && o.host == self.host_epoch
                && o.context == self.namespace.request
        })
    }
    pub fn inline_completion_allowed(&self, tab: usize, host: u64) -> bool {
        self.host_epoch == host
            && self.tabs[self.active_tab].id == tab
            && self.page == Page::Query
            && !self.busy
            && !self.close_requested
            && !self.shared.borrow().shell_modal_open
            && !self.modal_open()
            && !self.has_row_draft()
            && !self.snippet_active(tab)
    }
    pub fn dismiss_completions(&self, cx: &mut App) {
        for menu in self.advanced.completion_menus.values() {
            *menu.borrow_mut() = Menu::default();
        }
        for tab in &self.tabs {
            tab.editor.update(cx, |e, cx| e.dismiss_lsp_overlays(cx));
        }
    }
    pub fn invalidate_completions(&mut self, cx: &mut App) {
        self.advanced.inline.invalidate();
        self.dismiss_completions(cx);
    }
    pub fn install_completion(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let menu = Rc::new(RefCell::new(Menu::default()));
        let provider = Rc::new(Provider {
            workbench: cx.entity().downgrade(),
            tab: id,
            host: self.host_epoch,
            menu: menu.clone(),
        });
        tab.editor.update(cx, |e, cx| {
            e.dismiss_lsp_overlays(cx);
            e.lsp_mut().completion_provider = Some(provider);
            // Enter/arrows are captured by our editor wrapper, where the editor
            // can be inspected and updated synchronously (no deferred insertion).
            e.set_overlay_action_handler(|_, action, _, _| {
                action.partial_eq(&gpui_kit::base::input::Escape)
            });
        });
        // Renderer state is associated with the exact retained editor entity.
        self.advanced.completion_menus.insert(id, menu);
    }
    pub fn request_context_completion(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let id = self.tabs[self.active_tab].id;
        if !self.inline_completion_allowed(id, self.host_epoch) {
            return;
        }
        self.dismiss_completions(cx);
        self.tabs[self.active_tab]
            .editor
            .update(cx, |e, cx| e.show_completions(w, cx));
    }
    pub fn completion_action(
        &self,
        id: usize,
        action: &dyn Action,
        w: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(menu) = self.advanced.completion_menus.get(&id) else {
            return false;
        };
        if !self.inline_completion_allowed(id, self.host_epoch)
            || !self.inline_offer_current(&menu.borrow())
        {
            *menu.borrow_mut() = Menu::default();
            return false;
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return false;
        };
        tab.editor.update(cx, |e, cx| {
            crate::completion_popup::handle_action(menu, action, e, w, cx)
        })
    }
    pub fn refresh_completions(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.invalidate_completions(cx);
        self.request_context_completion(w, cx);
    }
}
