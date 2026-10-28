//! Live-to-live schema review. The shell leases both retained workspace sessions.
use super::WorkspaceShell;
use crate::{forms, workspace_model::AsyncOwner};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_admin::live_diff::{self, CompareOptions, LiveComparison, NamespaceMapping};
use rowboat_admin::{
    object_editor::{self as object_api, ObjectEdit, ObjectIdentity},
    objects::ObjectKind,
};
use rowboat_core::{
    BrowseRequest, Database, FilterExpr, QueryResult, Structure, TableInfo, TableRef,
};
use std::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoint {
    owner: AsyncOwner,
    revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    definition: Option<live_diff::definitions::Scope>,
    attribute: Option<(ObjectKind, String, Option<String>, Option<String>)>,
    column_table: Option<String>,
    source: Endpoint,
    destination: Endpoint,
    mapping: NamespaceMapping,
    options: CompareOptions,
}
struct Review {
    binding: Binding,
    comparison: Comparison,
}
#[derive(Clone, serde::Serialize)]
struct Comparison {
    plan: rowboat_admin::ddl::DdlPlan,
    objects: Vec<live_diff::ObjectDifference>,
    blockers: Vec<String>,
    warnings: Vec<String>,
    original: ComparisonOriginal,
}
#[derive(Clone, serde::Serialize)]
enum ComparisonOriginal {
    Definition(Box<live_diff::definitions::DefinitionComparison>),
    Schema(LiveComparison),
    Columns(live_diff::columns::ColumnComparison),
    Attributes(ObjectEdit),
}
impl Comparison {
    fn definition(c: live_diff::definitions::DefinitionComparison) -> Self {
        Self {
            objects: vec![],
            blockers: c.blockers().to_vec(),
            warnings: c
                .warnings()
                .iter()
                .cloned()
                .chain(
                    c.differences()
                        .iter()
                        .map(|d| format!("Definition difference: {d}")),
                )
                .collect(),
            plan: c.plan().clone(),
            original: ComparisonOriginal::Definition(Box::new(c)),
        }
    }
    fn attributes(c: object_api::ObjectAttributeComparison) -> Self {
        Self {
            plan: c.edit().plan().clone(),
            objects: vec![],
            blockers: vec![],
            warnings: c
                .differences
                .iter()
                .map(|d| format!("Attribute difference: {d}"))
                .collect(),
            original: ComparisonOriginal::Attributes(c.edit().clone()),
        }
    }
    fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }
    async fn current(&self, db: &Database) -> anyhow::Result<bool> {
        match &self.original {
            ComparisonOriginal::Definition(_) => {
                anyhow::bail!("Definition freshness requires both endpoints")
            }
            ComparisonOriginal::Attributes(edit) => object_api::object_is_current(db, edit).await,
            ComparisonOriginal::Schema(c) => live_diff::destination_is_current(db, c).await,
            ComparisonOriginal::Columns(c) => {
                live_diff::columns::columns_destination_is_current(db, c).await
            }
        }
    }
}
impl From<LiveComparison> for Comparison {
    fn from(c: LiveComparison) -> Self {
        Self {
            plan: c.plan.clone(),
            objects: c.objects.clone(),
            blockers: c.blockers.clone(),
            warnings: c.warnings.clone(),
            original: ComparisonOriginal::Schema(c),
        }
    }
}
impl From<live_diff::columns::ColumnComparison> for Comparison {
    fn from(c: live_diff::columns::ColumnComparison) -> Self {
        Self {
            plan: c.plan.clone(),
            objects: c.objects.clone(),
            blockers: c.blockers.clone(),
            warnings: c.warnings.clone(),
            original: ComparisonOriginal::Columns(c),
        }
    }
}
pub(super) struct LiveDiffDialog {
    definition: Option<live_diff::definitions::Scope>,
    attribute_kind: Option<ObjectKind>,
    attribute_name: Entity<InputState>,
    attribute_signature: Entity<InputState>,
    attribute_parent: Entity<InputState>,
    source: Option<u64>,
    destination: u64,
    source_schema: Entity<InputState>,
    column_table: Entity<InputState>,
    destination_schema: Entity<InputState>,
    confirmation: Entity<InputState>,
    drop_extra: bool,
    preview: Entity<TextareaState>,
    review: Option<Review>,
    pending: Option<Binding>,
    receiver: mpsc::Receiver<Completion>,
    sender: mpsc::Sender<Completion>,
    notice: String,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}
struct Completion {
    binding: Binding,
    outcome: Outcome,
}
enum Outcome {
    Compared(Result<Box<Comparison>, String>),
    Applied(Box<Applied>),
}
struct Applied {
    detail: Option<String>,
    result: Result<(), String>,
    transaction: Result<bool, String>,
    catalog: Result<Vec<TableInfo>, String>,
    table: Option<TableRefresh>,
}
impl LiveDiffDialog {
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some()
    }
}
struct TableRefresh {
    browse: BrowseSnapshot,
    result: Result<(Structure, QueryResult), String>,
}
#[derive(Clone)]
struct BrowseSnapshot {
    table: TableRef,
    request: BrowseRequest,
    expression: Option<FilterExpr>,
}
impl BrowseSnapshot {
    fn matches(
        &self,
        table: Option<&TableRef>,
        request: &BrowseRequest,
        expression: Option<&FilterExpr>,
    ) -> bool {
        table == Some(&self.table)
            && serde_json::to_value((&self.request, self.expression.as_ref())).is_ok_and(|saved| {
                serde_json::to_value((request, expression)).is_ok_and(|current| saved == current)
            })
    }
}

impl WorkspaceShell {
    pub(super) fn show_live_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.initialized
            || self.closing.is_some()
            || self.live_diff.is_some()
            || self.connection_manager.is_some()
        {
            return;
        }
        let Some(destination) = self.workspaces.iter().find(|w| w.id == self.active) else {
            return;
        };
        let dest = destination.entity.read(cx);
        let Some(db) = &dest.database else {
            self.error = Some("Connect the destination workspace first.".into());
            cx.notify();
            return;
        };
        let engine = db.engine();
        if let Err(error) = rowboat_admin::providers::operation_support(
            engine,
            rowboat_admin::providers::Operation::SchemaComparison,
        ) {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let destination_schema = dest.namespace_scope().unwrap_or_default().to_owned();
        let source = self.workspaces.iter().find(|w| {
            w.id != self.active
                && w.entity
                    .read(cx)
                    .database
                    .as_ref()
                    .is_some_and(|d| d.engine() == engine)
        });
        let source_id = source.map(|w| w.id);
        let source_schema = source
            .and_then(|w| w.entity.read(cx).namespace_scope())
            .unwrap_or_default()
            .to_owned();
        self.shared.borrow_mut().shell_modal_open = true;
        let (sender, receiver) = mpsc::channel();
        let preview = forms::textarea("", window, cx);
        preview.update(cx, |s, cx| s.set_readonly(true, cx));
        let source_schema = forms::input(source_schema, "Source namespace", window, cx);
        let column_table = forms::input(
            "",
            "Optional same-name MySQL table for advanced column-only diff",
            window,
            cx,
        );
        let destination_schema =
            forms::input(destination_schema, "Destination namespace", window, cx);
        let confirmation =
            forms::input("", "Type destination namespace before applying", window, cx);
        let mut subscriptions: Vec<Subscription> = [
            &source_schema,
            &destination_schema,
            &confirmation,
            &column_table,
        ]
        .into_iter()
        .map(|input| cx.subscribe(input, |_, _, _: &InputEvent, cx| cx.notify()))
        .collect();
        let attribute_name = forms::input("", "Exact object name", window, cx);
        let attribute_signature = forms::input(
            "",
            "Exact routine signature (PG; empty for none)",
            window,
            cx,
        );
        let attribute_parent =
            forms::input("", "Trigger parent table (empty for none)", window, cx);
        subscriptions.extend(
            [&attribute_name, &attribute_signature, &attribute_parent]
                .into_iter()
                .map(|input| cx.subscribe(input, |_, _, _: &InputEvent, cx| cx.notify())),
        );
        self.live_diff=Some(LiveDiffDialog{source:source_id,destination:self.active,definition:None,attribute_kind:None,attribute_name,attribute_signature,attribute_parent,
            source_schema,destination_schema,confirmation,column_table,_subscriptions:subscriptions,
            drop_extra:false,preview,review:None,pending:None,receiver,sender,
            notice:"Both sessions must be idle, with no open transaction or pending row edits. Source may be read-only. Compare is read-only; Apply targets only this destination workspace.".into(),error:None});
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn diff_endpoint(&self, id: u64, cx: &App) -> Result<Endpoint, String> {
        let state = self
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or("Workspace closed")?
            .entity
            .read(cx);
        if state.database.is_none() {
            return Err("Workspace disconnected".into());
        }
        Ok(Endpoint {
            owner: AsyncOwner {
                route: state.tx.route,
                connection: state.owner.clone(),
            },
            revision: state.activity_revision,
        })
    }
    fn diff_binding(&self, cx: &App) -> Result<Binding, String> {
        let dialog = self.live_diff.as_ref().ok_or("No comparison dialog")?;
        let source = dialog
            .source
            .ok_or("Open another live workspace using the same database engine")?;
        if source == dialog.destination {
            return Err("Choose a different source workspace".into());
        }
        let mapping = NamespaceMapping {
            source: dialog.source_schema.read(cx).value().trim().into(),
            destination: dialog.destination_schema.read(cx).value().trim().into(),
        };
        if mapping.source.is_empty() || mapping.destination.is_empty() {
            return Err("Enter explicit source and destination namespaces".into());
        }
        Ok(Binding {
            definition: dialog.definition,
            attribute: dialog.attribute_kind.map(|kind| {
                let optional = |input: &Entity<InputState>| {
                    let value = input.read(cx).value().to_string();
                    (!value.is_empty()).then_some(value)
                };
                (
                    kind,
                    dialog.attribute_name.read(cx).value().to_string(),
                    optional(&dialog.attribute_signature),
                    optional(&dialog.attribute_parent),
                )
            }),
            column_table: {
                let value = dialog.column_table.read(cx).value().trim().to_string();
                (!value.is_empty()).then_some(value)
            },
            source: self.diff_endpoint(source, cx)?,
            destination: self.diff_endpoint(dialog.destination, cx)?,
            mapping,
            options: CompareOptions {
                drop_destination_only: dialog.drop_extra,
            },
        })
    }
    fn lease_diff_sessions(
        &mut self,
        binding: &Binding,
        apply: bool,
        cx: &mut Context<Self>,
    ) -> Result<(Database, Database), String> {
        if self.closing.is_some() {
            return Err("Close confirmation is open".into());
        }
        let mut databases = vec![];
        for (index, endpoint) in [&binding.source, &binding.destination].iter().enumerate() {
            let state = self
                .workspaces
                .iter()
                .find(|w| w.id == endpoint.owner.route.workspace)
                .ok_or("Workspace closed")?
                .entity
                .read(cx);
            let hazards = state.close_state();
            if hazards.busy
                || hazards.transaction
                || hazards.unknown_transaction
                || hazards.staged > 0
                || hazards.drafts
                || state.close_requested
            {
                return Err(format!(
                    "{} must be idle with confirmed transaction state, no open transaction, dialogs or pending edits.",
                    if index == 0 { "Source" } else { "Destination" }
                ));
            }
            if apply && index == 1 && state.profile.as_ref().is_none_or(|p| p.read_only) {
                return Err("Destination is read-only".into());
            }
            databases.push(state.database.clone().ok_or("Workspace disconnected")?);
        }
        if databases[0].engine() != databases[1].engine() {
            return Err("Source and destination must use the same engine".into());
        }
        for endpoint in [&binding.source, &binding.destination] {
            self.workspaces
                .iter()
                .find(|w| w.id == endpoint.owner.route.workspace)
                .unwrap()
                .entity
                .update(cx, |s, cx| {
                    s.busy = true;
                    s.status = if apply {
                        "Schema comparison apply / refresh…"
                    } else {
                        "Reading live schema comparison…"
                    }
                    .into();
                    cx.notify();
                });
        }
        Ok((databases.remove(0), databases.remove(0)))
    }
    fn start_diff(&mut self, apply: bool, cx: &mut Context<Self>) {
        let prepared = (|| {
            if self.live_diff.as_ref().is_none_or(|d| d.pending.is_some()) {
                return Err("Comparison operation already running".into());
            }
            let binding = self.diff_binding(cx)?;
            let comparison = if apply {
                let dialog = self.live_diff.as_ref().unwrap();
                let review = dialog
                    .review
                    .as_ref()
                    .ok_or("Compare and review the SQL first")?;
                validate_review(review, &binding)?;
                if dialog.confirmation.read(cx).value().as_ref() != binding.mapping.destination {
                    return Err(
                        "Type the exact destination namespace to confirm this reviewed plan".into(),
                    );
                }
                Some(review.comparison.clone())
            } else {
                None
            };
            let databases = self.lease_diff_sessions(&binding, apply, cx)?;
            Ok((binding, databases, comparison))
        })();
        let (binding, (source, destination), comparison) = match prepared {
            Ok(value) => value,
            Err(e) => {
                if let Some(d) = &mut self.live_diff {
                    d.error = Some(e);
                }
                cx.notify();
                return;
            }
        };
        let table = self
            .workspaces
            .iter()
            .find(|w| w.id == binding.destination.owner.route.workspace)
            .and_then(|w| {
                let s = w.entity.read(cx);
                s.table.clone().map(|table| BrowseSnapshot {
                    table,
                    request: s.browse_request.clone(),
                    expression: s.filter_expression(),
                })
            });
        let dialog = self.live_diff.as_mut().unwrap();
        dialog.pending = Some(binding.clone());
        for input in [
            &dialog.source_schema,
            &dialog.destination_schema,
            &dialog.confirmation,
            &dialog.column_table,
            &dialog.attribute_name,
            &dialog.attribute_signature,
            &dialog.attribute_parent,
        ] {
            input.update(cx, |s, cx| s.set_readonly(true, cx));
        }
        dialog.error = None;
        if !apply {
            dialog.review = None;
        }
        let tx = dialog.sender.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let outcome = if let Some(comparison) = comparison {
                    Outcome::Applied(Box::new(
                        apply_review(source, destination, &comparison, table).await,
                    ))
                } else {
                    let result = async {
                        anyhow::ensure!(
                            !source.in_transaction().await?
                                && !destination.in_transaction().await?,
                            "Source or destination reports an open transaction"
                        );
                        if let Some((kind,name,signature,parent))=&binding.attribute {
                            anyhow::ensure!(binding.column_table.is_none()&&!binding.options.drop_destination_only,"Attribute-only comparison cannot combine column mode or Drop extras");
                            let identity=ObjectIdentity{engine:source.engine(),schema:binding.mapping.source.clone(),kind:*kind,name:name.clone(),signature:signature.clone(),parent:parent.clone()};
                            if let Some(scope)=binding.definition {
                                return live_diff::definitions::compare_definitions(&source,&identity,&destination,&ObjectIdentity{schema:binding.mapping.destination.clone(),..identity.clone()},&binding.mapping,scope).await.map(Comparison::definition);
                            }
                            let a=object_api::inspect_object(&source,&identity).await?;
                            let b=object_api::inspect_object(&destination,&ObjectIdentity{schema:binding.mapping.destination.clone(),..identity}).await?;
                            let comparison=object_api::compare_object_attributes(&a,&b,&binding.mapping)?;
                            Ok(Comparison::attributes(comparison))
                        } else if let Some(table)=&binding.column_table {
                            anyhow::ensure!(!binding.options.drop_destination_only,"Column-only diff always preserves destination-only objects; turn off Drop extras");
                            live_diff::columns::compare_mysql_columns(&source,&destination,&binding.mapping,table).await.map(Comparison::from)
                        } else { live_diff::compare_live(
                            &source,
                            &destination,
                            &binding.mapping,
                            binding.options,
                        )
                        .await.map(Comparison::from) }
                    }
                    .await
                    .map(Box::new)
                    .map_err(|e| format!("{e:#}"));
                    Outcome::Compared(result)
                };
                let _ = tx.send(Completion { binding, outcome });
            });
        });
        cx.notify();
    }
    pub(super) fn poll_live_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self
            .live_diff
            .as_ref()
            .map(|d| d.receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for event in events {
            let current = self.diff_binding(cx).ok();
            for endpoint in [&event.binding.source, &event.binding.destination] {
                if let Some(workspace) = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == endpoint.owner.route.workspace)
                {
                    workspace.entity.update(cx, |s, cx| {
                        if endpoint
                            .owner
                            .accepts(s.tx.route, s.owner.as_deref(), false)
                        {
                            s.busy = false;
                            s.status = "Schema comparison operation finished".into();
                            cx.notify();
                        }
                    });
                }
            }
            if let Outcome::Applied(applied) = &event.outcome
                && let Some(workspace) = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == event.binding.destination.owner.route.workspace)
            {
                workspace.entity.update(cx, |s, cx| {
                    if !event.binding.destination.owner.accepts(
                        s.tx.route,
                        s.owner.as_deref(),
                        false,
                    ) {
                        return;
                    }
                    s.activity_revision = s.activity_revision.wrapping_add(1);
                    s.selected = None;
                    s.structure = Structure::default();
                    s.table_result = QueryResult::default();
                    s.advanced.reset_connection();
                    match &applied.transaction {
                        Ok(value) => {
                            s.transaction = *value;
                            s.transaction_known = true;
                        }
                        Err(_) => s.transaction_known = false,
                    }
                    match &applied.catalog {
                        Ok(catalog) => s.catalog = s.scope_catalog(catalog.clone()),
                        Err(_) => s.catalog.clear(),
                    }
                    if let Some(TableRefresh {
                        browse,
                        result: Ok((structure, result)),
                    }) = &applied.table
                        && browse.matches(
                            s.table.as_ref(),
                            &s.browse_request,
                            s.filter_expression().as_ref(),
                        )
                    {
                        s.structure = structure.clone();
                        s.table_result = result.clone();
                    }
                    if s.table
                        .as_ref()
                        .is_some_and(|table| !s.catalog.iter().any(|t| &t.table == table))
                    {
                        s.table = None;
                    }
                    s.status = if applied.result.is_ok() {
                        "Schema plan applied; destination refreshed"
                    } else {
                        "Schema apply failed; destination refreshed; compare again"
                    }
                    .into();
                    cx.notify();
                });
            }
            let Some(dialog) = &mut self.live_diff else {
                continue;
            };
            dialog.pending = None;
            for input in [
                &dialog.source_schema,
                &dialog.destination_schema,
                &dialog.confirmation,
                &dialog.column_table,
                &dialog.attribute_name,
                &dialog.attribute_signature,
                &dialog.attribute_parent,
            ] {
                input.update(cx, |s, cx| s.set_readonly(false, cx));
            }
            match event.outcome{
                Outcome::Compared(Ok(comparison)) if current.as_ref()==Some(&event.binding)=>{
                    let text=if comparison.is_blocked(){"BLOCKED · no executable SQL".into()}else{comparison.plan.sql()};
                    dialog.preview.update(cx,|p,cx|p.set_value(text,window,cx));
                    dialog.notice=format!("{} object differences · {} statements · {} blockers. Exact-name matching; unsupported definitions block the complete plan.",comparison.objects.len(),comparison.plan.statements.len(),comparison.blockers.len());
                    dialog.review=Some(Review{binding:event.binding,comparison:*comparison});
                },
                Outcome::Compared(Ok(_))=>dialog.error=Some("Workspace, session, revision or namespace changed during comparison. Compare again.".into()),
                Outcome::Compared(Err(e))=>dialog.error=Some(e),
                Outcome::Applied(applied)=>{
                    dialog.review=None;dialog.preview.update(cx,|p,cx|p.set_value("",window,cx));
                    dialog.confirmation.update(cx,|p,cx|p.set_value("",window,cx));
                    dialog.notice="Destination refreshed after the attempt. Compare again to review the remaining differences.".into();
                    if let Some(detail)=applied.detail {dialog.notice.push('\n');dialog.notice.push_str(&detail);}
                    let mut errors=vec![];
                    if let Err(e)=applied.result{errors.push(e);}
                    if let Err(e)=applied.transaction{errors.push(format!("Transaction state unconfirmed: {e}"));}
                    if let Err(e)=applied.catalog{errors.push(format!("Catalog refresh: {e}"));}
                    if let Some(TableRefresh{result:Err(e),..})=applied.table{errors.push(format!("Previous table unavailable after refresh: {e}"));}
                    dialog.error=(!errors.is_empty()).then(||errors.join("\n"));
                }
            }
            cx.notify();
        }
    }
    pub(super) fn render_live_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.diff_binding(cx).ok();
        let dialog = self.live_diff.as_ref().unwrap();
        let busy = dialog.pending.is_some();
        let theme = cx.omarchy().clone();
        let destination = self
            .workspaces
            .iter()
            .find(|w| w.id == dialog.destination)
            .map(|w| w.entity.read(cx));
        let engine = destination
            .and_then(|s| s.database.as_ref())
            .map(Database::engine);
        let destination_label = destination
            .map(|s| s.connection_label(s.owner.as_deref()))
            .unwrap_or_else(|| "Closed workspace".into());
        let mut sources = div().flex().flex_wrap().gap_2();
        for workspace in &self.workspaces {
            let state = workspace.entity.read(cx);
            if workspace.id == dialog.destination
                || state.database.as_ref().map(Database::engine) != engine
                || state.database.is_none()
            {
                continue;
            }
            let id = workspace.id;
            let selected = dialog.source == Some(id);
            let schema = state.namespace_scope().unwrap_or_default().to_owned();
            sources = sources.child(
                ui::button(
                    ("diff-source", id),
                    format!(
                        "{}{}",
                        if selected { "● " } else { "" },
                        state.connection_label(state.owner.as_deref())
                    ),
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .disabled(busy)
                .on_click(cx.listener(move |s, _, w, cx| {
                    if let Some(d) = &mut s.live_diff {
                        d.source = Some(id);
                        d.review = None;
                        d.source_schema
                            .update(cx, |v, cx| v.set_value(schema.clone(), w, cx));
                        d.preview.update(cx, |v, cx| v.set_value("", w, cx));
                    }
                    cx.notify();
                })),
            );
        }
        let ready = dialog.review.as_ref().is_some_and(|r| {
            current
                .as_ref()
                .is_some_and(|b| validate_review(r, b).is_ok())
        });
        let writable =
            destination.is_some_and(|s| s.profile.as_ref().is_some_and(|p| !p.read_only));
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(22.))
                            .child(format!("Compare → {destination_label}")),
                    )
                    .child(
                        ui::button("close-diff", "Close", ui::ButtonVariant::Secondary, cx)
                            .disabled(busy)
                            .on_click(cx.listener(|s, _, w, cx| {
                                if s.live_diff.as_ref().is_some_and(|d| d.pending.is_none()) {
                                    s.live_diff = None;
                                    s.shared.borrow_mut().shell_modal_open = false;
                                    s.activate(s.active, w, cx);
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .child("SOURCE · choose another connected workspace of the same engine")
            .child(sources)
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child("Source namespace").child(ui::input(
                        "diff-source-schema",
                        &dialog.source_schema,
                        window,
                        cx,
                    )))
                    .child(
                        div()
                            .flex_1()
                            .child("Destination namespace")
                            .child(ui::input(
                                "diff-destination-schema",
                                &dialog.destination_schema,
                                window,
                                cx,
                            )),
                    ),
            )
            .child(
                ui::button(
                    "diff-drop-extra",
                    if dialog.drop_extra {
                        "☑ Drop destination-only tables / columns / constraints / indexes"
                    } else {
                        "☐ Preserve destination-only objects"
                    },
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .disabled(busy)
                .on_click(cx.listener(|s, _, _, cx| {
                    if let Some(d) = &mut s.live_diff {
                        d.drop_extra = !d.drop_extra;
                        d.review = None;
                    }
                    cx.notify();
                })),
            )
            .child(dialog.notice.clone())
            .child("Advanced MySQL columns: enter an exact same-name existing table below. Blank selects the original whole-schema subset. Column mode retains all destination-only columns, keys, options and other objects.")
            .child(ui::input("diff-column-table",&dialog.column_table,window,cx))
            .child(
                ui::button(
                    "compare-live",
                    if busy {
                        "Working…"
                    } else {
                        "Compare live schemas / refresh review"
                    },
                    ui::ButtonVariant::Primary,
                    cx,
                )
                .disabled(busy || current.is_none())
                .on_click(cx.listener(|s, _, _, cx| s.start_diff(false, cx))),
            );
        if let Some(error) = &dialog.error {
            body = body.child(div().text_color(theme.danger).child(error.clone()));
        }
        let mut attribute_modes = div().flex().gap_2().flex_wrap();
        for (i, kind) in [
            None,
            Some(ObjectKind::Function),
            Some(ObjectKind::Procedure),
            Some(ObjectKind::Trigger),
            Some(ObjectKind::Sequence),
        ]
        .into_iter()
        .enumerate()
        {
            attribute_modes = attribute_modes.child(
                ui::button(
                    ("diff-attribute-mode", i),
                    kind.map(|k| format!("{k:?} attributes"))
                        .unwrap_or_else(|| "Schema / columns mode".into()),
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .selected(dialog.attribute_kind == kind && dialog.definition.is_none())
                .disabled(busy)
                .on_click(cx.listener(move |s, _, _, cx| {
                    if let Some(d) = &mut s.live_diff {
                        d.attribute_kind = kind;
                        d.definition = None;
                        d.review = None;
                    }
                    cx.notify();
                })),
            );
        }
        for (i, kind, scope, title) in [
            (
                0,
                ObjectKind::View,
                live_diff::definitions::Scope::ViewQuery,
                "View query",
            ),
            (
                1,
                ObjectKind::Function,
                live_diff::definitions::Scope::RoutineBody,
                "Function body",
            ),
            (
                2,
                ObjectKind::Procedure,
                live_diff::definitions::Scope::RoutineBody,
                "Procedure body",
            ),
        ] {
            attribute_modes = attribute_modes.child(
                ui::button(
                    ("diff-definition-mode", i as usize),
                    title,
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .disabled(busy)
                .selected(dialog.definition == Some(scope) && dialog.attribute_kind == Some(kind))
                .on_click(cx.listener(move |s, _, w, cx| {
                    if let Some(d) = &mut s.live_diff {
                        d.definition = Some(scope);
                        d.attribute_kind = Some(kind);
                        d.drop_extra = false;
                        d.column_table.update(cx, |e, cx| e.set_value("", w, cx));
                        d.review = None;
                    }
                    cx.notify();
                })),
            );
        }
        body = body.child(attribute_modes);
        if dialog.attribute_kind.is_some() {
            body=body.child(if dialog.definition.is_some(){"Reviewed existing-object definition sync: bounded view SELECT or scalar routine body. Exact same-name identities and routine signatures are required. Both endpoints and dependencies are rechecked under exclusive session leases. Unsupported definitions block. Destination attributes/ACLs are preserved."}else{"Attribute-only synchronization: routine comment/security; PG trigger enabled mode; PG sequence declarations. Bodies, owners and grants are retained, not compared. Exact signature/parent identities are required."})
            .child(ui::input("diff-attribute-name",&dialog.attribute_name,window,cx))
            .child(ui::input("diff-attribute-signature",&dialog.attribute_signature,window,cx))
            .child(ui::input("diff-attribute-parent",&dialog.attribute_parent,window,cx));
        }
        if let Some(review) = &dialog.review {
            let comparison = &review.comparison;
            if let ComparisonOriginal::Definition(c) = &comparison.original {
                body = body.child(format!(
                    "DEFINITION SCOPE · {:?} · {} → {} · {}",
                    c.source().scope(),
                    c.mapping().source,
                    c.mapping().destination,
                    if c.non_atomic() {
                        "NON-ATOMIC replacement; completed steps and manual recovery are reported"
                    } else {
                        "transactional / single-statement apply"
                    }
                ));
                for (index, title, snapshot) in [
                    (0usize, "SOURCE ORIGINAL", c.source()),
                    (1, "DESTINATION ORIGINAL", c.destination()),
                ] {
                    let original = snapshot.object().definition().to_string();
                    let copy = original.clone();
                    body = body
                        .child(
                            div().flex().gap_2().child(title).child(
                                ui::button(
                                    ("copy-definition-original", index),
                                    "Copy full original",
                                    ui::ButtonVariant::Secondary,
                                    cx,
                                )
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                }),
                            ),
                        )
                        .child(
                            div()
                                .id(("definition-original", index))
                                .max_h(px(160.))
                                .overflow_y_scroll()
                                .font_family(theme.mono_font.clone())
                                .child(original.chars().take(16384).collect::<String>()),
                        );
                    let evidence =
                        serde_json::to_string_pretty(snapshot.evidence()).unwrap_or_default();
                    body=body.child(format!("{title} · dependency / environment / ACL evidence (first 16 KiB; full evidence in Copy report JSON)"))
                        .child(div().id(("definition-evidence",index)).max_h(px(160.)).overflow_y_scroll().font_family(theme.mono_font.clone()).child(evidence.chars().take(16384).collect::<String>()));
                }
                if let Some(recovery) = c.recovery_plan() {
                    body = body
                        .child("MANUAL RECOVERY PLAN · never replayed automatically")
                        .child(
                            div()
                                .id("definition-recovery-preview")
                                .max_h(px(160.))
                                .overflow_y_scroll()
                                .child(recovery.sql()),
                        );
                }
            }
            body=body.child(format!("REVIEWED MAPPING · {} → {} · source session {} / revision {} · destination session {} / revision {}",review.binding.mapping.source,review.binding.mapping.destination,review.binding.source.owner.route.generation,review.binding.source.revision,review.binding.destination.owner.route.generation,review.binding.destination.revision));
            if !ready && !comparison.is_blocked() && !comparison.plan.is_empty() {
                body=body.child(div().text_color(theme.warning).child("Inputs or workspace revision changed; compare again before applying or copying SQL."));
            }
            for blocker in comparison.blockers.iter().take(200) {
                body = body.child(
                    div()
                        .text_color(theme.danger)
                        .child(format!("BLOCKER · {blocker}")),
                );
            }
            for warning in comparison
                .warnings
                .iter()
                .chain(&comparison.plan.warnings)
                .take(200)
            {
                body = body.child(
                    div()
                        .text_color(theme.warning)
                        .child(format!("WARNING · {warning}")),
                );
            }
            for object in comparison.objects.iter().take(200) {
                body = body.child(format!(
                    "{:?} · {} · {:?} {} · {}",
                    object.change, object.table, object.kind, object.name, object.detail
                ));
            }
            body=body.child("Showing up to 200 summaries / warnings / blockers. Copy report JSON includes the full comparison.");
            body = body
                .child(div().h(px(240.)).child(ui::textarea(
                    "diff-sql-preview",
                    &dialog.preview,
                    window,
                    cx,
                )))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            ui::button(
                                "copy-diff-sql",
                                "Copy reviewed SQL",
                                ui::ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(!ready || busy)
                            .on_click(cx.listener(|s, _, _, cx| {
                                if let (Some(d), Ok(binding)) = (&s.live_diff, s.diff_binding(cx))
                                    && let Some(review) = &d.review
                                    && validate_review(review, &binding).is_ok()
                                {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        review.comparison.plan.sql(),
                                    ));
                                }
                            })),
                        )
                        .child(
                            ui::button(
                                "copy-diff-report",
                                "Copy report JSON",
                                ui::ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(busy)
                            .on_click(cx.listener(|s, _, _, cx| {
                                if let Some(review) =
                                    s.live_diff.as_ref().and_then(|d| d.review.as_ref())
                                    && let Ok(text) =
                                        serde_json::to_string_pretty(&review.comparison)
                                {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                }
                            })),
                        ),
                );
            if comparison.is_blocked() {
                body=body.child("Blocked comparison: no executable plan. Resolve unsupported definitions before comparing again.");
            } else if comparison.plan.is_empty() {
                body=body.child("No executable changes in the supported subset; preserved objects may still differ.");
            } else {
                let destructive = comparison
                    .plan
                    .statements
                    .iter()
                    .filter(|s| s.destructive)
                    .count();
                let execution_note = engine
                    .map(|engine| {
                        crate::provider_ui::plan_note(
                            &comparison.plan,
                            engine,
                            rowboat_admin::providers::Operation::SchemaComparison,
                        )
                    })
                    .unwrap_or_else(|| "Choose a destination provider".into());
                body=body.child(format!("{} statements · {} destructive. {}",comparison.plan.statements.len(),destructive,execution_note))
                    .child(if dialog.definition.is_some(){"Both source and destination snapshots/dependencies are rechecked under exclusive physical-session leases before apply. External-connection DDL races remain possible."}else{"Destination freshness is checked immediately before apply. This detects intervening catalog changes; it is not a lock against concurrent external DDL."})
                    .child(ui::input("diff-confirmation",&dialog.confirmation,window,cx))
                    .child(ui::button("apply-live-diff",if dialog.definition.is_some(){"Recheck BOTH endpoints & apply reviewed plan"}else{"Recheck destination & apply reviewed plan"},ui::ButtonVariant::Danger,cx).disabled(busy||!ready||!writable).on_click(cx.listener(|s,_,_,cx|s.start_diff(true,cx))));
            }
        }
        let surface = div()
            .id("live-schema-comparison")
            .key_context("RowboatModal")
            .w(px(1020.).min(window.viewport_size().width - px(32.)))
            .max_h(window.viewport_size().height - px(60.))
            .overflow_y_scroll()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .child(body);
        self.dialogs.overlay(
            "schema-comparison",
            window.viewport_size(),
            &self.modal_focus,
            surface,
            cx,
        )
    }
}
fn validate_review(review: &Review, current: &Binding) -> Result<(), String> {
    if &review.binding != current {
        return Err("Comparison ownership or inputs changed; compare again".into());
    }
    if review.comparison.is_blocked() {
        return Err("Blocked comparison has no executable plan".into());
    }
    if review.comparison.plan.is_empty() {
        return Err("There are no executable schema changes".into());
    }
    Ok(())
}
async fn apply_review(
    source: Database,
    destination: Database,
    comparison: &(impl Clone + Into<Comparison>),
    table: Option<BrowseSnapshot>,
) -> Applied {
    let comparison: Comparison = comparison.clone().into();
    let mut began = false;
    let mut detail = None;
    let result = async {
        anyhow::ensure!(
            !comparison.is_blocked() && !comparison.plan.is_empty(),
            "No executable reviewed plan"
        );
        anyhow::ensure!(
            !source.in_transaction().await? && !destination.in_transaction().await?,
            "Source or destination has an open transaction"
        );
        if let ComparisonOriginal::Definition(c) = &comparison.original {
            let outcome =
                live_diff::definitions::apply_definition_comparison(&source, &destination, c)
                    .await?;
            detail = Some(serde_json::to_string_pretty(&outcome)?);
            anyhow::ensure!(
                matches!(
                    outcome.status,
                    live_diff::definitions::DefinitionApplyStatus::Applied
                        | live_diff::definitions::DefinitionApplyStatus::Unchanged
                ),
                "Definition apply {:?}: {}",
                outcome.status,
                outcome
                    .detail
                    .as_deref()
                    .unwrap_or("See retained apply report")
            );
            return Ok(());
        }
        anyhow::ensure!(
            comparison.current(&destination).await?,
            "Destination schema changed after review; compare again"
        );
        let semantics = comparison.plan.execution_semantics(
            destination.engine(),
            rowboat_admin::providers::Operation::SchemaComparison,
        )?;
        if semantics.atomicity == rowboat_admin::providers::Atomicity::Transactional {
            destination.begin().await?;
            began = true;
        }
        destination.query(&comparison.plan.sql(), 0).await?;
        if began {
            destination.commit().await?;
            began = false;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let mut result = result.map_err(|e| format!("{e:#}"));
    if began && let Err(e) = destination.rollback().await {
        result = Err(format!(
            "{}; rollback failed: {e:#}",
            result.err().unwrap_or_default()
        ));
    }
    let transaction = destination
        .in_transaction()
        .await
        .map_err(|e| format!("{e:#}"));
    let catalog = destination.catalog().await.map_err(|e| format!("{e:#}"));
    let table = if let Some(browse) = table {
        let refreshed = async {
            let structure = destination.structure(&browse.table).await?;
            let result = destination
                .browse_filtered(&browse.table, &browse.request, browse.expression.as_ref())
                .await?;
            Ok::<_, anyhow::Error>((structure, result))
        }
        .await
        .map_err(|e| format!("{e:#}"));
        Some(TableRefresh {
            browse,
            result: refreshed,
        })
    } else {
        None
    };
    Applied {
        detail,
        result,
        transaction,
        catalog,
        table,
    }
}

#[cfg(test)]
mod tests {
    async fn definition_worker(engine: rowboat_core::Engine) -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture};
        use rowboat_admin::{
            live_diff::{NamespaceMapping, definitions},
            object_editor::ObjectIdentity,
            objects::ObjectKind,
        };
        use rowboat_core::{Cell, sql::quote_ident};
        let (source, _, a) = fixture(engine).await?;
        let (destination, _, b) = fixture(engine).await?;
        let result = async {
            let sa = quote_ident(engine, &a);
            let sb = quote_ident(engine, &b);
            source
                .query(&format!("CREATE VIEW {sa}.v AS SELECT 2 AS n"), 0)
                .await?;
            destination
                .query(&format!("CREATE VIEW {sb}.v AS SELECT 1 AS n"), 0)
                .await?;
            let id = ObjectIdentity {
                engine,
                schema: a.clone(),
                kind: ObjectKind::View,
                name: "v".into(),
                signature: None,
                parent: None,
            };
            let dst = ObjectIdentity {
                schema: b.clone(),
                ..id.clone()
            };
            let mapping = NamespaceMapping {
                source: a.clone(),
                destination: b.clone(),
            };
            let comparison = super::Comparison::definition(
                definitions::compare_definitions(
                    &source,
                    &id,
                    &destination,
                    &dst,
                    &mapping,
                    definitions::Scope::ViewQuery,
                )
                .await?,
            );
            anyhow::ensure!(!comparison.is_blocked(), "{:?}", comparison.blockers);
            source
                .query(
                    &format!("CREATE OR REPLACE VIEW {sa}.v AS SELECT 3 AS n"),
                    0,
                )
                .await?;
            let stale =
                super::apply_review(source.clone(), destination.clone(), &comparison, None).await;
            anyhow::ensure!(
                stale.result.is_err()
                    && stale
                        .detail
                        .as_ref()
                        .is_some_and(|d| d.contains("StaleSource")),
                "{:?} {:?}",
                stale.result,
                stale.detail
            );
            anyhow::ensure!(
                destination
                    .query(&format!("SELECT n FROM {sb}.v"), 2)
                    .await?[0]
                    .rows[0][0]
                    == Cell::Text("1".into())
            );
            let comparison = super::Comparison::definition(
                definitions::compare_definitions(
                    &source,
                    &id,
                    &destination,
                    &dst,
                    &mapping,
                    definitions::Scope::ViewQuery,
                )
                .await?,
            );
            let applied =
                super::apply_review(source.clone(), destination.clone(), &comparison, None).await;
            anyhow::ensure!(
                applied.result.is_ok(),
                "{:?} {:?}",
                applied.result,
                applied.detail
            );
            anyhow::ensure!(
                applied
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.contains("Applied"))
            );
            anyhow::ensure!(
                destination
                    .query(&format!("SELECT n FROM {sb}.v"), 2)
                    .await?[0]
                    .rows[0][0]
                    == Cell::Text("3".into())
            );
            anyhow::Ok(())
        }
        .await;
        cleanup(&source, &a).await?;
        cleanup(&destination, &b).await?;
        result
    }
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL on 15435"]
    async fn pass6_pg_definition_worker_rechecks_both_endpoints() -> anyhow::Result<()> {
        definition_worker(rowboat_core::Engine::Postgres).await
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL on 13309"]
    async fn pass6_mysql_definition_worker_rechecks_both_endpoints() -> anyhow::Result<()> {
        definition_worker(rowboat_core::Engine::MySql).await
    }
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL on 15435"]
    async fn gui_pass5_attribute_diff_preserves_body_and_rejects_stale_destination()
    -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture};
        use rowboat_admin::{
            live_diff::NamespaceMapping, object_editor as api, objects::ObjectKind,
        };
        use rowboat_core::{Cell, Engine, sql::quote_ident};
        let (source, _, a) = fixture(Engine::Postgres).await?;
        let (destination, _, b) = fixture(Engine::Postgres).await?;
        let result=async {
            let src=quote_ident(Engine::Postgres,&a);let dst=quote_ident(Engine::Postgres,&b);
            source.query(&format!("CREATE FUNCTION {src}.f() RETURNS INTEGER LANGUAGE sql SECURITY DEFINER AS 'SELECT 99'; COMMENT ON FUNCTION {src}.f() IS 'source comment'"),0).await?;
            destination.query(&format!("CREATE FUNCTION {dst}.f() RETURNS INTEGER LANGUAGE sql SECURITY INVOKER AS 'SELECT 1'"),0).await?;
            let source_id=api::list_objects(&source,&a,ObjectKind::Function).await?.remove(0);let destination_id=api::list_objects(&destination,&b,ObjectKind::Function).await?.remove(0);
            let source_snapshot=api::inspect_object(&source,&source_id).await?;let destination_snapshot=api::inspect_object(&destination,&destination_id).await?;
            let comparison=super::Comparison::attributes(api::compare_object_attributes(&source_snapshot,&destination_snapshot,&NamespaceMapping{source:a.clone(),destination:b.clone()})?);
            let applied=super::apply_review(source.clone(),destination.clone(),&comparison,None).await;anyhow::ensure!(applied.result.is_ok(),"{:?}",applied.result);
            let rows=destination.query(&format!("SELECT {dst}.f()"),2).await?;anyhow::ensure!(rows[0].rows[0][0]==Cell::Text("1".into()),"Attribute sync replaced body");
            let stale=super::apply_review(source.clone(),destination.clone(),&comparison,None).await;anyhow::ensure!(stale.result.is_err());anyhow::Ok(())
        }.await;
        cleanup(&source, &a).await?;
        cleanup(&destination, &b).await?;
        result
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL on 13309"]
    async fn gui_pass5_column_diff_review_stale_apply_and_convergence() -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture, table};
        use rowboat_admin::live_diff::{self, NamespaceMapping};
        use rowboat_core::{Engine, sql::qualified_table};
        let (source, _, a) = fixture(Engine::MySql).await?;
        let (destination, _, b) = fixture(Engine::MySql).await?;
        let result=async {
            let src=qualified_table(Engine::MySql,&table(&a,"items"));let dst=qualified_table(Engine::MySql,&table(&b,"items"));
            source.query(&format!("CREATE TABLE {src}(id INT,g INT GENERATED ALWAYS AS (id+2) VIRTUAL, choice ENUM('a','b','c') DEFAULT 'a')"),0).await?;
            destination.query(&format!("CREATE TABLE {dst}(id INT,g INT GENERATED ALWAYS AS (id+1) VIRTUAL, choice ENUM('a','b') DEFAULT 'a')"),0).await?;
            let mapping=NamespaceMapping{source:a.clone(),destination:b.clone()};
            let comparison=live_diff::columns::compare_mysql_columns(&source,&destination,&mapping,"items").await?;
            anyhow::ensure!(!comparison.is_blocked(),"{:?}",comparison.blockers);
            let comparison=super::Comparison::from(comparison);
            let applied=super::apply_review(source.clone(),destination.clone(),&comparison,None).await;
            anyhow::ensure!(applied.result.is_ok(),"{:?}",applied.result);
            let stale=super::apply_review(source.clone(),destination.clone(),&comparison,None).await;
            anyhow::ensure!(stale.result.is_err());
            let next=live_diff::columns::compare_mysql_columns(&source,&destination,&mapping,"items").await?;
            anyhow::ensure!(next.plan.is_empty()&&!next.is_blocked());anyhow::Ok(())
        }.await;
        cleanup(&source, &a).await?;
        cleanup(&destination, &b).await?;
        result
    }
    use super::{Binding, BrowseSnapshot, Endpoint, Review, apply_review, validate_review};
    use crate::workspace_model::{AsyncOwner, Route};
    use rowboat_admin::{
        ddl::PlannedStatement,
        live_diff::{self, CompareOptions, NamespaceMapping, SchemaSnapshot},
    };
    use rowboat_core::{
        BrowseRequest, Cell, ConnectionProfile, Database, Engine, Filter, FilterExpr, FilterOp,
        TableRef, sql::quote_ident,
    };
    fn nested_id_filter() -> FilterExpr {
        FilterExpr::And(vec![FilterExpr::Or(vec![
            FilterExpr::Leaf(Filter {
                column: "id".into(),
                op: FilterOp::Eq,
                value: Cell::Text("2".into()),
            }),
            FilterExpr::Leaf(Filter {
                column: "id".into(),
                op: FilterOp::Eq,
                value: Cell::Text("99".into()),
            }),
        ])])
    }
    #[test]
    fn schema_apply_refresh_is_bound_to_original_table_page_sort_and_nested_expression() {
        let snapshot = BrowseSnapshot {
            table: TableRef {
                schema: "public".into(),
                name: "items".into(),
            },
            request: BrowseRequest::default(),
            expression: Some(nested_id_filter()),
        };
        assert!(snapshot.matches(
            Some(&snapshot.table),
            &snapshot.request,
            snapshot.expression.as_ref()
        ));
        assert!(!snapshot.matches(None, &snapshot.request, snapshot.expression.as_ref()));
        assert!(!snapshot.matches(Some(&snapshot.table), &snapshot.request, None));
        assert!(!snapshot.matches(
            Some(&snapshot.table),
            &snapshot.request,
            Some(&FilterExpr::And(vec![]))
        ));
        let mut request = snapshot.request.clone();
        request.offset += 1;
        assert!(!snapshot.matches(
            Some(&snapshot.table),
            &request,
            snapshot.expression.as_ref()
        ));
        request = snapshot.request.clone();
        request.sort = Some(("id".into(), true));
        assert!(!snapshot.matches(
            Some(&snapshot.table),
            &request,
            snapshot.expression.as_ref()
        ));
    }
    #[test]
    fn reviewed_plan_cannot_cross_either_workspace_session_revision_mapping_or_drop_option() {
        let endpoint = |id| Endpoint {
            owner: AsyncOwner {
                route: Route {
                    workspace: id,
                    generation: 3,
                },
                connection: Some(format!("profile-{id}")),
            },
            revision: 7,
        };
        let binding = Binding {
            definition: None,
            attribute: None,
            column_table: None,
            source: endpoint(1),
            destination: endpoint(2),
            mapping: NamespaceMapping {
                source: "src".into(),
                destination: "dst".into(),
            },
            options: CompareOptions::default(),
        };
        let source = SchemaSnapshot {
            engine: Engine::Postgres,
            schema: "src".into(),
            origin: serde_json::json!({}),
            tables: vec![],
            unsupported: vec![],
            warnings: vec![],
            catalog: Default::default(),
        };
        let destination = SchemaSnapshot {
            schema: "dst".into(),
            ..source.clone()
        };
        let comparison =
            live_diff::compare_snapshots(&source, &destination, &binding.mapping, binding.options)
                .unwrap();
        let mut review = Review {
            binding: binding.clone(),
            comparison: comparison.into(),
        };
        assert!(validate_review(&review, &binding).is_err()); // Empty is not an apply/copy success.
        review.comparison.plan.statements.push(PlannedStatement {
            sql: "CREATE TABLE dst.items(id int)".into(),
            destructive: false,
            reason: "test".into(),
        });
        assert!(validate_review(&review, &binding).is_ok());
        let mut changed = binding.clone();
        changed.source.owner.route.generation += 1;
        assert!(validate_review(&review, &changed).is_err());
        changed = binding.clone();
        changed.destination.owner.route.workspace += 1;
        assert!(validate_review(&review, &changed).is_err());
        changed = binding.clone();
        changed.source.revision += 1;
        assert!(validate_review(&review, &changed).is_err());
        changed = binding.clone();
        changed.destination.revision += 1;
        assert!(validate_review(&review, &changed).is_err());
        changed = binding.clone();
        changed.mapping.destination = "elsewhere".into();
        assert!(validate_review(&review, &changed).is_err());
        changed = binding.clone();
        changed.options.drop_destination_only = true;
        assert!(validate_review(&review, &changed).is_err());
        review
            .comparison
            .blockers
            .push("Unsupported generated column".into());
        assert!(validate_review(&review, &binding).is_err());
    }
    async fn exercise_executor(engine: Engine) -> anyhow::Result<()> {
        let profile = ConnectionProfile {
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
        };
        let source = Database::connect(&profile, "rowboat-test").await?;
        let destination = Database::connect(&profile, "rowboat-test").await?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mapping = NamespaceMapping {
            source: format!("rowboat_gui_{id}_s"),
            destination: format!("rowboat_gui_{id}_d"),
        };
        let qs = quote_ident(engine, &mapping.source);
        let qd = quote_ident(engine, &mapping.destination);
        source.query(&format!("CREATE SCHEMA {qs}"), 0).await?;
        if let Err(e) = destination.query(&format!("CREATE SCHEMA {qd}"), 0).await {
            let _ = source.query(&format!("DROP SCHEMA {qs}"), 0).await;
            return Err(e);
        }
        let outcome = async {
            source
                .query(
                    &format!("CREATE TABLE {qs}.items(id INTEGER NOT NULL PRIMARY KEY)"),
                    0,
                )
                .await?;
            let plan =
                live_diff::compare_live(&source, &destination, &mapping, CompareOptions::default())
                    .await?;
            anyhow::ensure!(
                !plan.is_blocked() && !plan.plan.is_empty(),
                "Fixture comparison blocked: {:?}",
                plan.blockers
            );
            // Open source transactions must prevent even destination inspection/apply.
            source.begin().await?;
            let blocked = apply_review(source.clone(), destination.clone(), &plan, None).await;
            source.rollback().await?;
            anyhow::ensure!(
                blocked.result.is_err(),
                "Open source transaction was accepted"
            );
            // A catalog change after review must prevent the CREATE from running.
            destination
                .query(&format!("CREATE TABLE {qd}.external_change(id INTEGER)"), 0)
                .await?;
            let stale = apply_review(source.clone(), destination.clone(), &plan, None).await;
            anyhow::ensure!(stale.result.is_err(), "Stale comparison was applied");
            anyhow::ensure!(
                !destination
                    .catalog()
                    .await?
                    .iter()
                    .any(|t| t.table.schema == mapping.destination && t.table.name == "items"),
                "Stale apply executed SQL"
            );
            destination
                .query(&format!("DROP TABLE {qd}.external_change"), 0)
                .await?;
            let mut failing =
                live_diff::compare_live(&source, &destination, &mapping, CompareOptions::default())
                    .await?;
            failing.plan.statements.push(PlannedStatement {
                sql: format!("ALTER TABLE {qd}.missing_table ADD COLUMN failure INTEGER"),
                destructive: false,
                reason: "Exercise execution failure after the first DDL".into(),
            });
            let failed = apply_review(source.clone(), destination.clone(), &failing, None).await;
            anyhow::ensure!(
                failed.result.is_err(),
                "Deliberately invalid second DDL succeeded"
            );
            anyhow::ensure!(
                failed.transaction == Ok(false),
                "Failure left a transaction open"
            );
            let exists = failed
                .catalog
                .as_ref()
                .map_err(|e| anyhow::anyhow!(e.clone()))?
                .iter()
                .any(|t| t.table.schema == mapping.destination && t.table.name == "items");
            anyhow::ensure!(
                exists == (engine == Engine::MySql),
                "PG rollback / MySQL partial-commit contract violated"
            );
            if exists {
                destination
                    .query(&format!("DROP TABLE {qd}.items"), 0)
                    .await?;
            }
            let plan =
                live_diff::compare_live(&source, &destination, &mapping, CompareOptions::default())
                    .await?;
            let applied = apply_review(source.clone(), destination.clone(), &plan, None).await;
            applied.result.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                applied.transaction == Ok(false),
                "Apply left a transaction open"
            );
            anyhow::ensure!(
                live_diff::compare_live(&source, &destination, &mapping, CompareOptions::default())
                    .await?
                    .plan
                    .is_empty(),
                "Applied schema did not converge"
            );
            destination.query(&format!("INSERT INTO {qd}.items(id) VALUES(1),(2),(3)"),0).await?;
            source.query(&format!("ALTER TABLE {qs}.items ADD COLUMN note VARCHAR(20)"),0).await?;
            let plan=live_diff::compare_live(&source,&destination,&mapping,CompareOptions::default()).await?;
            let browse=BrowseSnapshot{table:TableRef{schema:mapping.destination.clone(),name:"items".into()},request:BrowseRequest::default(),expression:Some(nested_id_filter())};
            let applied=apply_review(source.clone(),destination.clone(),&plan,Some(browse)).await;
            applied.result.map_err(anyhow::Error::msg)?;
            let (_,filtered)=applied.table.ok_or_else(||anyhow::anyhow!("Missing filtered refresh"))?.result.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(filtered.rows.len()==1&&filtered.rows[0][0]==Cell::Text("2".into()),"Schema apply refreshed unfiltered rows instead of the captured nested/FK expression");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        // Only these uniquely named, test-owned namespaces are removed.
        for (db, schema) in [(&source, &qs), (&destination, &qd)] {
            let sql = if engine == Engine::Postgres {
                format!("DROP SCHEMA {schema} CASCADE")
            } else {
                format!("DROP DATABASE {schema}")
            };
            db.query(&sql, 0).await?;
        }
        outcome
    }
    #[tokio::test]
    #[ignore = "Requires explicit disposable Podman PostgreSQL fixture on 15435"]
    async fn gui_live_diff_postgres_executor_stale_check_rollback_and_convergence()
    -> anyhow::Result<()> {
        exercise_executor(Engine::Postgres).await
    }
    #[tokio::test]
    #[ignore = "Requires explicit disposable Podman MySQL fixture on 13309"]
    async fn gui_live_diff_mysql_executor_stale_check_partial_commit_and_convergence()
    -> anyhow::Result<()> {
        exercise_executor(Engine::MySql).await
    }
}
