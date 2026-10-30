//! Catalog identities, full inspection and revision-bound typed object edits.
use crate::{
    app::{Event, Page, Workbench},
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_admin::{
    object_editor::{
        self as api, ObjectEdit, ObjectIdentity, ObjectPatch, ObjectSnapshot, SequencePatch,
        TriggerState,
    },
    objects::ObjectKind,
};
use rowboat_core::{Database, Engine, TableInfo};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct ObjectState {
    scope: Option<String>,
    schema_page: usize,
    route: Option<Route>,
    schemas: Vec<String>,
    loaded: bool,
    error: Option<String>,
    request: Option<uuid::Uuid>,
    pub(crate) groups: Vec<Group>,
    loading: Option<(String, ObjectKind)>,
    closed: BTreeSet<String>,
    pub dialog: Option<DefinitionDialog>,
}
pub(crate) struct Group {
    page: usize,
    pub(crate) schema: String,
    pub(crate) kind: ObjectKind,
    pub(crate) result: Option<Result<Vec<ObjectIdentity>, String>>,
    pub(crate) sidebar_expanded: bool,
}
pub struct DefinitionDialog {
    id: uuid::Uuid,
    route: Route,
    identity: ObjectIdentity,
    snapshot: Option<ObjectSnapshot>,
    error: Option<String>,
    patch: usize,
    revision: u64,
    review: Option<(u64, ObjectEdit)>,
    text: Entity<TextareaState>,
    sequence: BTreeMap<&'static str, Entity<InputState>>,
    cycle: Option<bool>,
    definer: bool,
    trigger: TriggerState,
    concurrently: bool,
    with_data: bool,
    unique_index: bool,
    outcome: Option<String>,
    recovery_sql: Option<String>,
    _subscriptions: Vec<Subscription>,
}
pub enum ObjectEvent {
    Schemas {
        request: uuid::Uuid,
        result: Result<Vec<String>, String>,
    },
    Group {
        request: uuid::Uuid,
        schema: String,
        kind: ObjectKind,
        result: Result<Vec<ObjectIdentity>, String>,
    },
    Inspected {
        id: uuid::Uuid,
        result: Result<ObjectSnapshot, String>,
    },
    Applied {
        id: uuid::Uuid,
        result: Result<String, String>,
        snapshot: Result<ObjectSnapshot, String>,
        catalog: Result<Vec<TableInfo>, String>,
        transaction: Result<bool, String>,
    },
}
fn kinds(engine: Engine) -> Vec<ObjectKind> {
    // Preserve explorer presentation order; availability belongs to the provider.
    [
        ObjectKind::View,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
        ObjectKind::MaterializedView,
        ObjectKind::Sequence,
    ]
    .into_iter()
    .filter(|kind| rowboat_admin::providers::supported_object_kinds(engine).contains(kind))
    .collect()
}
pub(crate) const SIDEBAR_OBJECT_KINDS: &[(ObjectKind, &str)] = &[
    (ObjectKind::Procedure, "Procedures"),
    (ObjectKind::Function, "Functions"),
    (ObjectKind::Trigger, "Triggers"),
    (ObjectKind::Sequence, "Sequences"),
];
fn groups_for(engine: Engine, schemas: &[String]) -> Vec<Group> {
    schemas
        .iter()
        .flat_map(|schema| {
            kinds(engine).into_iter().map(move |kind| Group {
                page: 0,
                schema: schema.clone(),
                kind,
                result: None,
                sidebar_expanded: false,
            })
        })
        .collect()
}
pub(crate) fn identity_label(id: &ObjectIdentity) -> String {
    format!(
        "{}{}{}",
        id.name,
        id.signature
            .as_ref()
            .map(|s| format!("({s})"))
            .unwrap_or_default(),
        id.parent
            .as_ref()
            .map(|p| format!(" · on {p}"))
            .unwrap_or_default()
    )
}
fn patch_names(id: &ObjectIdentity) -> &'static [&'static str] {
    if rowboat_admin::providers::operation_support(
        id.engine,
        rowboat_admin::providers::Operation::ObjectEdit,
    )
    .is_err()
    {
        return &[];
    }
    let rowboat_core::ToolSupport::Supported(provider) =
        id.engine.descriptor().tools.administration
    else {
        return &[];
    };
    use rowboat_core::ToolProvider;
    match (provider, id.kind) {
        (_, ObjectKind::View) => &["View query body"],
        (ToolProvider::Postgres, ObjectKind::Function | ObjectKind::Procedure) => {
            &["Routine body (SQL / plpgsql)", "Comment", "Security"]
        }
        (ToolProvider::MySql, ObjectKind::Function | ObjectKind::Procedure) => {
            &["MySQL body replacement", "Comment", "Security"]
        }
        (ToolProvider::MySql, ObjectKind::Trigger) => &["MySQL body replacement"],
        (ToolProvider::Postgres, ObjectKind::MaterializedView) => &[
            "Materialized refresh",
            "Materialized index",
            "Materialized query replacement",
        ],
        (ToolProvider::Postgres, ObjectKind::Trigger) => &["Trigger enabled mode"],
        (ToolProvider::Postgres, ObjectKind::Sequence) => &["Sequence options"],
        _ => &[],
    }
}
fn reviewed_current(
    route: Route,
    current: Route,
    review_revision: u64,
    draft_revision: u64,
) -> bool {
    route == current && review_revision == draft_revision
}
impl DefinitionDialog {
    fn changed(&mut self) {
        self.revision += 1;
        self.review = None;
        self.error = None;
    }
    fn patch(&self, cx: &App) -> anyhow::Result<ObjectPatch> {
        let text = self.text.read(cx).value().to_string();
        let name = patch_names(&self.identity)
            .get(self.patch)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("This object is inspect-only"))?;
        Ok(match name {
            "View query body" => ObjectPatch::ViewQuery { query: text },
            "Routine body (SQL / plpgsql)" => ObjectPatch::RoutineBody { body: text },
            "MySQL body replacement" => ObjectPatch::MySqlBody { body: text },
            "Materialized query replacement" => ObjectPatch::MaterializedQuery { query: text },
            "Materialized refresh" => ObjectPatch::MaterializedRefresh {
                concurrently: self.concurrently,
                with_data: self.with_data,
            },
            "Materialized index" => ObjectPatch::MaterializedIndex {
                index: rowboat_admin::ddl::IndexDesign {
                    name: self.sequence["index_name"].read(cx).value().to_string(),
                    columns: serde_json::from_str(
                        &self.sequence["index_columns"].read(cx).value(),
                    )?,
                    unique: self.unique_index,
                },
            },
            "Comment" => ObjectPatch::RoutineComment { comment: text },
            "Security" => ObjectPatch::RoutineSecurity {
                definer: self.definer,
            },
            "Trigger enabled mode" => ObjectPatch::TriggerEnabled {
                state: self.trigger,
            },
            _ => {
                let number = |key| -> anyhow::Result<Option<i64>> {
                    let value = self.sequence[key].read(cx).value();
                    if value.trim().is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(value.trim().parse()?))
                    }
                };
                ObjectPatch::Sequence {
                    patch: SequencePatch {
                        increment: number("increment")?,
                        min_value: number("min_value")?,
                        max_value: number("max_value")?,
                        start: number("start")?,
                        cache: number("cache")?,
                        restart: number("restart")?,
                        cycle: self.cycle,
                    },
                }
            }
        })
    }
}
async fn apply_object(db: &Database, edit: &ObjectEdit) -> Result<String, String> {
    let outcome = api::apply_object_edit(db, edit)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let detail = format!(
        "{:?} · completed steps: {} · failed step: {:?} · rolled back: {} · object presence: {:?}{}",
        outcome.status,
        outcome.completed_statements,
        outcome.failed_statement,
        outcome.rolled_back,
        outcome.presence,
        outcome.error.map(|e| format!(" · {e}")).unwrap_or_default()
    );
    if outcome.status == api::ObjectApplyStatus::Applied {
        Ok(detail)
    } else {
        Err(detail)
    }
}
impl Workbench {
    /// Initialize from the already observed namespace, without issuing catalog SQL
    /// during rendering. Object groups load only when explicitly expanded/refreshed.
    pub(crate) fn ensure_sidebar_objects(&mut self) {
        let Some(engine) = self.profile.as_ref().map(|p| p.engine) else {
            return;
        };
        if !self.namespace.known || self.database.is_none() || kinds(engine).is_empty() {
            return;
        }
        let Some(scope) = self.namespace_scope().map(str::to_owned) else {
            return;
        };
        if self.objects.route == Some(self.tx.route)
            && self.objects.scope.as_deref() == Some(&scope)
        {
            return;
        }
        let schemas = vec![scope.clone()];
        self.objects = ObjectState {
            scope: Some(scope),
            route: Some(self.tx.route),
            groups: groups_for(engine, &schemas),
            schemas,
            loaded: true,
            ..Default::default()
        };
    }
    pub(crate) fn sidebar_groups(&self) -> impl Iterator<Item = (usize, &Group)> {
        SIDEBAR_OBJECT_KINDS.iter().filter_map(|(kind, _)| {
            self.objects.groups.iter().enumerate().find(|(_, group)| {
                self.namespace.known
                    && self.objects.route == Some(self.tx.route)
                    && Some(group.schema.as_str()) == self.namespace_scope()
                    && *kind == group.kind
            })
        })
    }
    pub(crate) fn toggle_sidebar_object_group(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy || self.modal_open() || self.close_requested || !self.namespace.known {
            return;
        }
        let Some((_, group)) = self.sidebar_groups().find(|(i, _)| *i == index) else {
            return;
        };
        let expanded = !group.sidebar_expanded;
        self.objects.groups[index].sidebar_expanded = expanded;
        self.explorer_cursor = 0;
        self.explorer_page = 0;
        self.load_next_sidebar_group(cx);
        cx.notify();
    }
    pub(crate) fn refresh_sidebar_object_group(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy
            || self.modal_open()
            || self.close_requested
            || !self.namespace.known
            || !self.sidebar_groups().any(|(i, _)| i == index)
        {
            return;
        }
        let group = &mut self.objects.groups[index];
        group.sidebar_expanded = true;
        group.result = None;
        self.load_next_sidebar_group(cx);
    }
    pub(crate) fn expand_loaded_sidebar_groups(&mut self) {
        for group in &mut self.objects.groups {
            if group.result.is_some() {
                group.sidebar_expanded = true;
            }
        }
    }
    pub(crate) fn invalidate_sidebar_objects(&mut self) {
        for group in &mut self.objects.groups {
            group.result = None;
        }
    }
    pub(crate) fn load_next_sidebar_group(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.has_extension_dialog() || self.close_requested {
            return;
        }
        let index = self
            .sidebar_groups()
            .find(|(_, group)| group.sidebar_expanded && group.result.is_none())
            .map(|(i, _)| i);
        if let Some(index) = index {
            self.load_object_group(index, cx);
        }
    }
    pub(crate) fn selected_sidebar_object(&self, identity: &ObjectIdentity) -> bool {
        self.objects
            .dialog
            .as_ref()
            .is_some_and(|d| d.route == self.tx.route && d.identity == *identity)
    }
    pub fn show_objects_page(&mut self, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::Namespaces, cx) {
            return;
        }
        if !self.can_leave() {
            cx.notify();
            return;
        }
        self.page = Page::Objects;
        if self.objects.route != Some(self.tx.route) || !self.objects.loaded {
            self.refresh_objects(cx);
        }
        cx.notify();
    }
    pub fn refresh_objects(&mut self, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::Namespaces, cx) {
            return;
        }
        if self.busy || self.close_requested || self.has_extension_dialog() {
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        let request = uuid::Uuid::new_v4();
        let scope = self.namespace_scope().map(str::to_owned);
        self.objects = ObjectState {
            scope: scope.clone(),
            route: Some(self.tx.route),
            request: Some(request),
            ..Default::default()
        };
        self.busy = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = async {
                let snapshot = db.namespace_catalog(scope.as_deref()).await?;
                let names = scope
                    .or_else(|| {
                        if db.engine().descriptor().namespaces
                            == rowboat_core::NamespaceBehavior::Databases
                        {
                            Some(snapshot.database)
                        } else {
                            snapshot.current_schema
                        }
                    })
                    .into_iter()
                    .collect();
                Ok::<_, anyhow::Error>(names)
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Object(Box::new(ObjectEvent::Schemas {
                request,
                result,
            })));
        });
        cx.notify();
    }
    fn load_object_group(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy || self.has_extension_dialog() || self.close_requested {
            return;
        }
        let (Some(db), Some(group)) = (self.database.clone(), self.objects.groups.get_mut(index))
        else {
            return;
        };
        let schema = group.schema.clone();
        let kind = group.kind;
        group.result = None;
        self.objects.loading = Some((schema.clone(), kind));
        let request = uuid::Uuid::new_v4();
        self.objects.request = Some(request);
        self.busy = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = api::list_objects(&db, &schema, kind)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Object(Box::new(ObjectEvent::Group {
                request,
                schema,
                kind,
                result,
            })));
        });
        cx.notify();
    }
    pub(crate) fn inspect_object(
        &mut self,
        identity: ObjectIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_leave() {
            cx.notify();
            return;
        }
        let Some(db) = self.database.clone() else {
            return;
        };
        let id = uuid::Uuid::new_v4();
        let text = forms::textarea("", window, cx);
        let sequence = [
            "increment",
            "min_value",
            "max_value",
            "start",
            "cache",
            "restart",
            "index_name",
            "index_columns",
        ]
        .into_iter()
        .map(|name| (name, forms::input("", "Unchanged", window, cx)))
        .collect::<BTreeMap<_, _>>();
        let mut subscriptions = vec![cx.subscribe(&text, |s, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change)
                && let Some(d) = &mut s.objects.dialog
            {
                d.changed();
            }
            cx.notify();
        })];
        subscriptions.extend(sequence.values().map(|input| {
            cx.subscribe(input, |s, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(d) = &mut s.objects.dialog
                {
                    d.changed();
                }
                cx.notify();
            })
        }));
        self.objects.dialog = Some(DefinitionDialog {
            id,
            route: self.tx.route,
            identity: identity.clone(),
            snapshot: None,
            error: None,
            patch: 0,
            revision: 0,
            review: None,
            text,
            sequence,
            cycle: None,
            definer: false,
            trigger: TriggerState::Enabled,
            concurrently: false,
            with_data: true,
            unique_index: false,
            outcome: None,
            recovery_sql: None,
            _subscriptions: subscriptions,
        });
        self.busy = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = api::inspect_object(&db, &identity)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Object(Box::new(ObjectEvent::Inspected {
                id,
                result,
            })));
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub fn receive_object(&mut self, event: ObjectEvent, cx: &mut Context<Self>) {
        match event {
            ObjectEvent::Schemas { request, result } => {
                if self.objects.request != Some(request) {
                    return;
                }
                self.busy = false;
                self.objects.request = None;
                self.objects.loaded = true;
                match result {
                    Ok(schemas) => {
                        self.objects.closed = schemas.iter().skip(1).cloned().collect();
                        let engine = self.profile.as_ref().unwrap().engine;
                        self.objects.groups = groups_for(engine, &schemas);
                        self.objects.schemas = schemas;
                    }
                    Err(e) => self.objects.error = Some(e),
                }
            }
            ObjectEvent::Group {
                request,
                schema,
                kind,
                result,
            } => {
                if self.objects.request != Some(request) {
                    return;
                }
                self.busy = false;
                self.objects.request = None;
                if let Some(group) = self
                    .objects
                    .groups
                    .iter_mut()
                    .find(|g| g.schema == schema && g.kind == kind)
                {
                    group.result = Some(result);
                }
                self.objects.loading = None;
                self.load_next_sidebar_group(cx);
            }
            ObjectEvent::Inspected { id, result } => {
                if let Some(d) = &mut self.objects.dialog
                    && d.id == id
                {
                    self.busy = false;
                    match result {
                        Ok(snapshot) => d.snapshot = Some(snapshot),
                        Err(e) => d.error = Some(e),
                    }
                }
            }
            ObjectEvent::Applied {
                id,
                result,
                snapshot,
                catalog,
                transaction,
            } => {
                if self.objects.dialog.as_ref().is_none_or(|d| d.id != id) {
                    return;
                }
                self.busy = false;
                match transaction {
                    Ok(active) => {
                        self.transaction = active;
                        self.transaction_known = true;
                    }
                    Err(e) => {
                        self.transaction_known = false;
                        self.fail(e);
                    }
                }
                match catalog {
                    Ok(catalog) => self.catalog = self.scope_catalog(catalog),
                    Err(e) => self.fail(format!("Catalog refresh: {e}")),
                }
                self.table_result = Default::default();
                self.structure = Default::default();
                self.selected = None;
                self.advanced.reset_connection();
                let d = self.objects.dialog.as_mut().unwrap();
                d.review = None;
                d.snapshot = None;
                match result {
                    Ok(outcome) => {
                        d.outcome = Some(outcome);
                        d.error = None;
                    }
                    Err(error) => {
                        d.error = Some(error);
                        d.outcome = None;
                    }
                }
                match snapshot {
                    Ok(snapshot) => d.snapshot = Some(snapshot),
                    Err(e) => {
                        d.error = Some(format!(
                            "{} Refresh: {e}",
                            d.error.take().unwrap_or_default()
                        ))
                    }
                }
                self.status=if d.error.is_none(){"Object edit applied; definition/catalog refreshed. Table data will reload on navigation."}else{"Object edit attempt finished; inspect the reported error and refreshed definition."}.into();
            }
        }
        cx.notify();
    }
    fn review_object(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.close_requested {
            return;
        }
        if let Some(d) = &mut self.objects.dialog {
            d.review = None;
            let result = (|| {
                anyhow::ensure!(
                    d.route == self.tx.route,
                    "Connection changed; reopen inspection"
                );
                let snapshot = d
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Inspect the full definition first"))?;
                api::plan_edit(snapshot, d.patch(cx)?)
            })();
            match result {
                Ok(edit) => {
                    d.review = Some((d.revision, edit));
                    d.error = None;
                }
                Err(e) => d.error = Some(format!("{e:#}")),
            }
        }
        cx.notify();
    }
    fn apply_object_review(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.close_requested
            || self.transaction
            || !self.transaction_known
            || !self.changes.is_empty()
            || self.has_row_draft()
            || self.profile.as_ref().is_none_or(|p| p.read_only)
        {
            return;
        }
        let (Some(db), Some(d)) = (self.database.clone(), self.objects.dialog.as_ref()) else {
            return;
        };
        let Some((revision, edit)) = &d.review else {
            return;
        };
        if !reviewed_current(d.route, self.tx.route, *revision, d.revision) {
            return;
        }
        let edit = edit.clone();
        let id = d.id;
        self.objects.dialog.as_mut().unwrap().recovery_sql = edit.recovery_plan().map(|p| p.sql());
        self.busy = true;
        self.transaction_known = false;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = apply_object(&db, &edit).await;
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let snapshot = api::inspect_object(&db, edit.original().identity())
                .await
                .map_err(|e| format!("{e:#}"));
            let catalog = db.catalog().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Object(Box::new(ObjectEvent::Applied {
                id,
                result,
                snapshot,
                catalog,
                transaction,
            })));
        });
        cx.notify();
    }
    pub fn render_objects_page(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let mut body = column()
            .p_4()
            .gap_4()
            .text_size(px(13.))
            .child(
                row()
                    .flex_wrap()
                    .child(ui::icon(ui::IconName::Layers).text_color(theme.accent))
                    .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child("Object explorer"))
                    .child(div().flex_1())
                    .child(action(
                        "refresh-object-schemas",
                        "Refresh schemas",
                        false,
                        !self.busy && self.database.is_some(),
                        cx,
                        |s, _, cx| s.refresh_objects(cx),
                    )),
            )
            .child(label("Browse schema objects and inspect their definitions. Expand a group to load its catalog.", cx))
            .child(div().h(px(1.)).bg(theme.divider()))
            .child(div().p_3().bg(theme.surface).child(ui::input(
                "object-explorer-search",
                &self.advanced.object_filter,
                window,
                cx,
            )));
        if self.objects.route != Some(self.tx.route) {
            body = body.child(label(
                "Connect and refresh to load this session's schemas.",
                cx,
            ));
        } else if let Some(error) = &self.objects.error {
            body = body.child(label(format!("Schema load failed: {error}"), cx));
        } else if !self.objects.loaded {
            body = body.child(label("Loading schema catalog…", cx));
        } else if self.objects.schemas.is_empty() {
            body = body.child(label("No accessible user schemas returned.", cx));
        }
        let filter = self.advanced.object_filter.read(cx).value().to_lowercase();
        let schema_page = self
            .objects
            .schema_page
            .min(self.objects.schemas.len().saturating_sub(1) / 10);
        body = body.child(
            row()
                .flex_wrap()
                .child(label(
                    format!(
                        "{} schemas · page {} · expand to load groups",
                        self.objects.schemas.len(),
                        schema_page + 1
                    ),
                    cx,
                ))
                .child(action(
                    "schema-page-prev",
                    "Previous schemas",
                    false,
                    schema_page > 0,
                    cx,
                    move |s, _, cx| {
                        s.objects.schema_page = schema_page.saturating_sub(1);
                        cx.notify();
                    },
                ))
                .child(action(
                    "schema-page-next",
                    "Next schemas",
                    false,
                    (schema_page + 1) * 10 < self.objects.schemas.len(),
                    cx,
                    move |s, _, cx| {
                        s.objects.schema_page = schema_page + 1;
                        cx.notify();
                    },
                )),
        );
        for (schema_index, schema) in self
            .objects
            .schemas
            .iter()
            .enumerate()
            .filter(|_| self.objects.route == Some(self.tx.route))
            .skip(schema_page * 10)
            .take(10)
        {
            let name = schema.clone();
            let closed = self.objects.closed.contains(schema);
            body = body.child(
                action(
                    ("object-schema", schema_index),
                    schema.clone(),
                    false,
                    true,
                    cx,
                    move |s, _, cx| {
                        if !s.objects.closed.remove(&name) {
                            s.objects.closed.insert(name.clone());
                        }
                        cx.notify();
                    },
                )
                .justify_start()
                .bg(theme.inset)
                .font_weight(FontWeight::SEMIBOLD)
                .child(ui::icon(if closed {
                    ui::IconName::ChevronRight
                } else {
                    ui::IconName::ChevronDown
                })),
            );
            if closed {
                continue;
            }
            let mut group_body = column()
                .pl_4()
                .gap_2()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Tables"))
                .child(label(
                    "First 100 matches · use the sidebar to search the full catalog",
                    cx,
                ));
            for (i, table) in self
                .catalog
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    &t.table.schema == schema
                        && format!("{} {}", t.table.name, t.kind)
                            .to_lowercase()
                            .contains(&filter)
                })
                .take(100)
            {
                let table = table.table.clone();
                group_body = group_body.child(
                    action(
                        ("explorer-table", i),
                        table.name.clone(),
                        false,
                        !self.busy,
                        cx,
                        move |s, _, cx| s.open_table(table.clone(), cx),
                    )
                    .justify_start(),
                );
            }
            for (index, group) in self
                .objects
                .groups
                .iter()
                .enumerate()
                .filter(|(_, g)| &g.schema == schema)
            {
                group_body = group_body.child(
                    action(
                        ("object-kind", index),
                        format!(
                            "{:?} · {}",
                            group.kind,
                            if group.result.is_some() {
                                "Reload"
                            } else if self.objects.loading.as_ref()
                                == Some(&(group.schema.clone(), group.kind))
                            {
                                "loading…"
                            } else {
                                "Load"
                            }
                        ),
                        false,
                        !self.busy,
                        cx,
                        move |s, _, cx| s.load_object_group(index, cx),
                    )
                    .justify_start(),
                );
                match &group.result {
                    None => {
                        group_body = group_body.child(label(
                            if self.objects.loading.as_ref()
                                == Some(&(group.schema.clone(), group.kind))
                            {
                                "Loading catalog…"
                            } else {
                                "Not loaded"
                            },
                            cx,
                        ));
                    }
                    Some(Err(e)) => {
                        group_body = group_body.child(label(format!("Load failed: {e}"), cx));
                    }
                    Some(Ok(objects)) => {
                        if objects.is_empty() {
                            group_body = group_body.child(label("No objects returned", cx));
                        }
                        let matching = objects
                            .iter()
                            .filter(|id| identity_label(id).to_lowercase().contains(&filter))
                            .count();
                        let page = group.page.min(matching.saturating_sub(1) / 200);
                        group_body = group_body.child(
                            row()
                                .child(label(
                                    format!(
                                        "{matching} matches · page {} (200 maximum rendered)",
                                        page + 1
                                    ),
                                    cx,
                                ))
                                .child(action(
                                    ("object-page-prev", index),
                                    "Previous",
                                    false,
                                    page > 0,
                                    cx,
                                    move |s, _, cx| {
                                        if let Some(g) = s.objects.groups.get_mut(index) {
                                            g.page = page.saturating_sub(1);
                                        }
                                        cx.notify();
                                    },
                                ))
                                .child(action(
                                    ("object-page-next", index),
                                    "Next",
                                    false,
                                    (page + 1) * 200 < matching,
                                    cx,
                                    move |s, _, cx| {
                                        if let Some(g) = s.objects.groups.get_mut(index) {
                                            g.page = page + 1;
                                        }
                                        cx.notify();
                                    },
                                )),
                        );
                        for (item, id) in objects
                            .iter()
                            .enumerate()
                            .filter(|(_, id)| identity_label(id).to_lowercase().contains(&filter))
                            .skip(page * 200)
                            .take(200)
                        {
                            let identity = id.clone();
                            group_body = group_body.child(
                                action(
                                    SharedString::from(format!("object-{index}-{item}")),
                                    identity_label(id),
                                    false,
                                    !self.busy,
                                    cx,
                                    move |s, w, cx| s.inspect_object(identity.clone(), w, cx),
                                )
                                .justify_start()
                                .pl_4(),
                            );
                        }
                    }
                }
            }
            body = body.child(group_body);
        }
        div()
            .id("object-explorer-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(body)
            .into_any_element()
    }
    pub fn render_object_definition(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let d = self.objects.dialog.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let free = !self.busy;
        let mut body=column().p_4().gap_3().text_size(px(13.)).child(row().flex_wrap().child(ui::icon(ui::IconName::Layers).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(format!("{:?} · {}.{}",d.identity.kind,d.identity.schema,identity_label(&d.identity)))).child(div().flex_1()).child(action("close-object-definition","Close",false,free,cx,|s,w,cx|{s.objects.dialog=None;w.focus(&s.focus,cx);cx.notify();})))
            .child(label("Full definition is inspection-only. Supported typed patches preserve native attributes; generated SQL requires Review → Apply. Freshness checks do not lock concurrent DDL.",cx))
            .child(div().h(px(1.)).bg(theme.divider()));
        if self.busy {
            body = body.child(label("Database operation running…", cx));
        }
        if let Some(error) = &d.error {
            body = body.child(div().text_color(theme.danger).child(error.clone()));
        }
        if let Some(outcome) = &d.outcome {
            body = body.child(label(outcome.clone(), cx));
        }
        if let Some(sql) = &d.recovery_sql {
            let sql = sql.clone();
            body=body.child(label("Original recovery SQL retained after apply attempt. Inspect partial outcomes before any manual recovery; never automatically replayed.",cx))
                .child(action("copy-object-recovery","Copy original recovery SQL",false,true,cx,move|_,_,cx|cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()))));
        }
        if let Some(snapshot) = &d.snapshot {
            let definition = snapshot.definition().to_string();
            let evidence = serde_json::to_string_pretty(snapshot.catalog()).unwrap_or_default();
            body = body
                .child(
                    row()
                        .child(
                            action(
                                "copy-full-definition",
                                "Copy full definition",
                                false,
                                true,
                                cx,
                                {
                                    let text = definition.clone();
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            text.clone(),
                                        ))
                                    }
                                },
                            )
                            .debug_selector(|| "copy-full-definition".into()),
                        )
                        .child(action(
                            "copy-object-catalog",
                            "Copy catalog evidence",
                            false,
                            true,
                            cx,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(evidence.clone()))
                            },
                        )),
                )
                .child(
                    div()
                        .p_3()
                        .bg(theme.inset)
                        .font_family(theme.mono_font.clone())
                        .text_size(px(12.))
                        .child(definition),
                );
            for warning in snapshot.warnings() {
                body = body.child(label(warning.clone(), cx));
            }
            let names = patch_names(&d.identity);
            if names.is_empty() {
                body = body.child(label(
                    "No typed patch is available for this engine/object identity.",
                    cx,
                ));
            } else {
                if d.identity.engine == Engine::MySql
                    && matches!(
                        d.identity.kind,
                        ObjectKind::Function | ObjectKind::Procedure
                    )
                {
                    body=body.child(label("MySQL body replacement is non-atomic DROP/CREATE with preserved inspected attributes and privileges. Supported grammar is bounded; unsupported procedural control/dynamic SQL is rejected.",cx));
                }
                let mut modes = row().flex_wrap().p_2().bg(theme.surface);
                for (index, name) in names.iter().enumerate() {
                    modes = modes.child(
                        action(
                            ("object-patch", index),
                            *name,
                            false,
                            free,
                            cx,
                            move |s, _, cx| {
                                if let Some(d) = &mut s.objects.dialog {
                                    d.patch = index;
                                    d.changed();
                                }
                                cx.notify();
                            },
                        )
                        .selected(d.patch == index)
                        .styles(|s| {
                            s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))
                        }),
                    );
                }
                body = body.child(modes);
                match names[d.patch] {
                    "Materialized refresh" => {
                        body = body.child(
                            row()
                                .child(action(
                                    "matview-concurrently",
                                    format!("Concurrently: {}", d.concurrently),
                                    d.concurrently,
                                    free,
                                    cx,
                                    |s, _, cx| {
                                        if let Some(d) = &mut s.objects.dialog {
                                            d.concurrently = !d.concurrently;
                                            d.changed();
                                        }
                                        cx.notify();
                                    },
                                ))
                                .child(action(
                                    "matview-data",
                                    format!("With data: {}", d.with_data),
                                    d.with_data,
                                    free,
                                    cx,
                                    |s, _, cx| {
                                        if let Some(d) = &mut s.objects.dialog {
                                            d.with_data = !d.with_data;
                                            d.changed();
                                        }
                                        cx.notify();
                                    },
                                )),
                        );
                    }
                    "Materialized index" => {
                        body = body
                            .child(label(
                                "Index name and JSON array of exact column names (e.g. [\"id\"])",
                                cx,
                            ))
                            .child(ui::input(
                                "matview-index-name",
                                &d.sequence["index_name"],
                                window,
                                cx,
                            ))
                            .child(ui::input(
                                "matview-index-columns",
                                &d.sequence["index_columns"],
                                window,
                                cx,
                            ))
                            .child(action(
                                "matview-index-unique",
                                format!("Unique: {}", d.unique_index),
                                d.unique_index,
                                free,
                                cx,
                                |s, _, cx| {
                                    if let Some(d) = &mut s.objects.dialog {
                                        d.unique_index = !d.unique_index;
                                        d.changed();
                                    }
                                    cx.notify();
                                },
                            ));
                    }
                    "Security" => {
                        body = body.child(action(
                            "object-security",
                            if d.definer {
                                "SECURITY DEFINER"
                            } else {
                                "SECURITY INVOKER"
                            },
                            false,
                            free,
                            cx,
                            |s, _, cx| {
                                if let Some(d) = &mut s.objects.dialog {
                                    d.definer = !d.definer;
                                    d.changed();
                                }
                                cx.notify();
                            },
                        ));
                    }
                    "Trigger enabled mode" => {
                        body = body.child(action(
                            "trigger-mode",
                            format!("Mode: {:?}", d.trigger),
                            false,
                            free,
                            cx,
                            |s, _, cx| {
                                if let Some(d) = &mut s.objects.dialog {
                                    d.trigger = match d.trigger {
                                        TriggerState::Enabled => TriggerState::Disabled,
                                        TriggerState::Disabled => TriggerState::Replica,
                                        TriggerState::Replica => TriggerState::Always,
                                        TriggerState::Always => TriggerState::Enabled,
                                    };
                                    d.changed();
                                }
                                cx.notify();
                            },
                        ));
                    }
                    "Sequence options" => {
                        for (name, input) in &d.sequence {
                            if name.starts_with("index_") {
                                continue;
                            }
                            body = body.child(row().child(div().w(px(170.)).child(*name)).child(
                                ui::input(
                                    SharedString::from(format!("sequence-{name}")),
                                    input,
                                    window,
                                    cx,
                                ),
                            ));
                        }
                        body = body.child(action(
                            "sequence-cycle",
                            format!("Cycle: {:?} · None keeps current", d.cycle),
                            false,
                            free,
                            cx,
                            |s, _, cx| {
                                if let Some(d) = &mut s.objects.dialog {
                                    d.cycle = match d.cycle {
                                        None => Some(true),
                                        Some(true) => Some(false),
                                        Some(false) => None,
                                    };
                                    d.changed();
                                }
                                cx.notify();
                            },
                        ));
                    }
                    _ => body = body
                        .child(label(
                            "Enter the replacement body / comment only, not CREATE / ALTER DDL.",
                            cx,
                        ))
                        .child(ui::textarea("typed-object-patch", &d.text, window, cx).h(px(170.))),
                }
                body = body.child(action(
                    "review-object-patch",
                    "Review generated SQL",
                    true,
                    free,
                    cx,
                    |s, _, cx| s.review_object(cx),
                ));
                if let Some((revision, edit)) = &d.review {
                    if let Some(recovery) = edit.recovery_plan() {
                        body=body.child(label("NON-ATOMIC replacement · review original recovery SQL (manual only)",cx)).child(label(recovery.sql(),cx));
                    }
                    body = body.child(label(
                        if edit.non_atomic() {
                            "This plan is non-atomic; completed steps can remain after failure."
                        } else {
                            "This plan uses a dedicated transaction."
                        },
                        cx,
                    ));
                    for warning in &edit.plan().warnings {
                        body = body.child(label(warning.clone(), cx));
                    }
                    for statement in &edit.plan().statements {
                        body = body.child(label(statement.reason.clone(), cx)).child(
                            div()
                                .p_3()
                                .bg(theme.inset)
                                .font_family(theme.mono_font.clone())
                                .child(statement.sql.clone()),
                        );
                    }
                    body = body
                        .child(label(
                            crate::provider_ui::plan_note(
                                edit.plan(),
                                d.identity.engine,
                                rowboat_admin::providers::Operation::ObjectEdit,
                            ),
                            cx,
                        ))
                        .child(action(
                            "apply-object-patch",
                            "Apply reviewed object change",
                            true,
                            free && reviewed_current(d.route, self.tx.route, *revision, d.revision)
                                && self.transaction_known
                                && !self.transaction
                                && self.profile.as_ref().is_some_and(|p| !p.read_only),
                            cx,
                            |s, _, cx| s.apply_object_review(cx),
                        ));
                }
            }
        }
        self.modal(
            div()
                .id("object-definition-scroll")
                .w(px(960.).min(window.viewport_size().width - px(48.)))
                .max_h(window.viewport_size().height - px(60.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL on 15435"]
    async fn gui_pass5_materialized_review_index_refresh_replace() -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture};
        use rowboat_admin::{
            ddl::IndexDesign,
            object_editor::{self as api, ObjectPatch},
            objects::ObjectKind,
        };
        use rowboat_core::{Engine, sql::quote_ident};
        let (db, _, schema) = fixture(Engine::Postgres).await?;
        let ns = quote_ident(Engine::Postgres, &schema);
        let result=async {
            db.query(&format!("CREATE TABLE {ns}.base(id INT); INSERT INTO {ns}.base VALUES(1); CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id FROM {ns}.base"),0).await?;
            let id=api::list_objects(&db,&schema,ObjectKind::MaterializedView).await?.remove(0);
            for patch in [ObjectPatch::MaterializedIndex{index:IndexDesign{name:"mv_pk".into(),columns:vec!["id".into()],unique:true}},ObjectPatch::MaterializedRefresh{concurrently:true,with_data:true},ObjectPatch::MaterializedQuery{query:format!("SELECT id FROM {ns}.base WHERE id>0")}] {
                let snapshot=api::inspect_object(&db,&id).await?;let edit=api::plan_edit(&snapshot,patch)?;
                let message=super::apply_object(&db,&edit).await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(message.contains("Applied")&&message.contains("Present"));
            }
            anyhow::Ok(())
        }.await;
        cleanup(&db, &schema).await?;
        result
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL on 13309"]
    async fn gui_pass5_mysql_routine_and_trigger_review_replacement() -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture};
        use rowboat_admin::{
            object_editor::{self as api, ObjectPatch},
            objects::ObjectKind,
        };
        use rowboat_core::{Engine, sql::quote_ident};
        let (db, _, schema) = fixture(Engine::MySql).await?;
        let ns = quote_ident(Engine::MySql, &schema);
        let result=async {
            db.query(&format!("CREATE TABLE {ns}.base(id INT); CREATE FUNCTION {ns}.f(x INT) RETURNS INT DETERMINISTIC RETURN x+1; CREATE TRIGGER {ns}.tr BEFORE INSERT ON {ns}.base FOR EACH ROW SET NEW.id=NEW.id+1"),0).await?;
            for(kind,body)in [(ObjectKind::Function,"RETURN x+2"),(ObjectKind::Trigger,"SET NEW.id=NEW.id+2")] {
                let id=api::list_objects(&db,&schema,kind).await?.remove(0);let snapshot=api::inspect_object(&db,&id).await?;
                let edit=api::plan_edit(&snapshot,ObjectPatch::MySqlBody{body:body.into()})?;
                anyhow::ensure!(edit.non_atomic()&&edit.recovery_plan().is_some());
                let message=super::apply_object(&db,&edit).await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(message.contains("Applied")&&message.contains("Present"));
                anyhow::ensure!(super::apply_object(&db,&edit).await.is_err());
            }anyhow::Ok(())
        }.await;
        cleanup(&db, &schema).await?;
        result
    }
    use super::{
        Engine, ObjectIdentity, ObjectKind, Route, identity_label, patch_names, reviewed_current,
    };
    #[test]
    fn definition_review_rejects_changed_draft_and_reconnected_or_other_workspace() {
        let route = Route {
            workspace: 2,
            generation: 5,
        };
        assert!(reviewed_current(route, route, 3, 3));
        assert!(!reviewed_current(route, route, 3, 4));
        assert!(!reviewed_current(
            route,
            Route {
                generation: 6,
                ..route
            },
            3,
            3
        ));
        assert!(!reviewed_current(
            route,
            Route {
                workspace: 7,
                ..route
            },
            3,
            3
        ));
    }
    #[test]
    fn object_labels_preserve_overloads_and_parent_tables_and_patch_matrix_is_honest() {
        let mut id = ObjectIdentity {
            engine: Engine::Postgres,
            schema: "public".into(),
            kind: ObjectKind::Function,
            name: "f".into(),
            signature: Some("x integer".into()),
            parent: None,
        };
        assert_eq!(identity_label(&id), "f(x integer)");
        id.signature = Some("x text".into());
        assert_eq!(identity_label(&id), "f(x text)");
        id.kind = ObjectKind::Trigger;
        id.signature = None;
        id.parent = Some("table_a".into());
        assert_eq!(identity_label(&id), "f · on table_a");
        id.engine = Engine::MySql;
        assert!(patch_names(&id).contains(&"MySQL body replacement"));
        id.kind = ObjectKind::Function;
        assert!(patch_names(&id).iter().any(|n| n.contains("body")));
        id.kind = ObjectKind::MaterializedView;
        assert!(patch_names(&id).is_empty());
    }
    async fn exercise_apply(engine: Engine) -> anyhow::Result<()> {
        use rowboat_admin::object_editor::{self as api, ObjectPatch};
        use rowboat_core::{ConnectionProfile, Database, sql::quote_ident};
        let db = Database::connect(
            &ConnectionProfile {
                engine,
                host: "127.0.0.1".into(),
                port: if engine == Engine::Postgres {
                    15435
                } else {
                    13309
                },
                database: "rowboat".into(),
                username: if engine == Engine::Postgres {
                    "postgres"
                } else {
                    "root"
                }
                .into(),
                ssl_mode: "disable".into(),
                ..Default::default()
            },
            "rowboat-test",
        )
        .await?;
        let schema = format!("gui_object_{}", uuid::Uuid::new_v4().simple());
        let quoted = quote_ident(engine, &schema);
        db.query(&format!("CREATE SCHEMA {quoted}"), 0).await?;
        let outcome = async {
            db.query(&format!("CREATE VIEW {quoted}.v AS SELECT 1 AS id"), 0)
                .await?;
            let id = api::resolve_object(&db, &schema, ObjectKind::View, "v", None, None).await?;
            let snapshot = api::inspect_object(&db, &id).await?;
            let edit = api::plan_edit(
                &snapshot,
                ObjectPatch::ViewQuery {
                    query: "SELECT 2 AS id".into(),
                },
            )?;
            db.query(
                &format!("CREATE OR REPLACE VIEW {quoted}.v AS SELECT 3 AS id"),
                0,
            )
            .await?;
            anyhow::ensure!(
                super::apply_object(&db, &edit).await.is_err(),
                "Stale GUI review must fail"
            );
            anyhow::ensure!(!db.in_transaction().await?);
            let snapshot = api::inspect_object(&db, &id).await?;
            let edit = api::plan_edit(
                &snapshot,
                ObjectPatch::ViewQuery {
                    query: "SELECT 4 AS id".into(),
                },
            )?;
            db.begin().await?;
            anyhow::ensure!(super::apply_object(&db, &edit).await.is_err());
            anyhow::ensure!(
                db.in_transaction().await?,
                "Existing user transaction must survive rejection"
            );
            db.rollback().await?;
            super::apply_object(&db, &edit)
                .await
                .map_err(anyhow::Error::msg)?;
            let value = db.query(&format!("SELECT id FROM {quoted}.v"), 1).await?;
            anyhow::ensure!(value[0].rows[0][0] == rowboat_core::Cell::Text("4".into()));
            anyhow::ensure!(!db.in_transaction().await?);
            let snapshot = api::inspect_object(&db, &id).await?;
            let bad = api::plan_edit(
                &snapshot,
                ObjectPatch::ViewQuery {
                    query: "SELECT definitely_missing_column AS id".into(),
                },
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "Planning second edit after successful apply: {e:#}; definition: {}",
                    snapshot.definition()
                )
            })?;
            anyhow::ensure!(super::apply_object(&db, &bad).await.is_err());
            anyhow::ensure!(
                !db.in_transaction().await?,
                "Failed DDL must finish its owned transaction"
            );
            let value = db.query(&format!("SELECT id FROM {quoted}.v"), 1).await?;
            anyhow::ensure!(value[0].rows[0][0] == rowboat_core::Cell::Text("4".into()));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = db.rollback().await;
        db.query(
            &format!(
                "DROP SCHEMA {quoted}{}",
                if engine == Engine::Postgres {
                    " CASCADE"
                } else {
                    ""
                }
            ),
            0,
        )
        .await?;
        outcome
    }
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL fixture on 15435"]
    async fn gui_object_postgres_stale_transaction_rejection_and_atomic_apply() -> anyhow::Result<()>
    {
        exercise_apply(Engine::Postgres).await
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL fixture on 13309"]
    async fn gui_object_mysql_stale_transaction_rejection_and_apply() -> anyhow::Result<()> {
        exercise_apply(Engine::MySql).await
    }
}
