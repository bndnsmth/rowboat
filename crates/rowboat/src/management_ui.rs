//! Native review/apply dialogs for schema and access management.
use crate::{
    app::*,
    forms,
    management::{self, Draft, Field, Flag, Operation, Review, Target},
    view::{action, column, label, row, section},
};
use gpui_kit::{
    base::input::{InputEvent, InputState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::{Engine, QueryResult, Structure};
use std::collections::{BTreeMap, BTreeSet};

pub struct ManagementDialog {
    pub target: Target,
    pub operation: Operation,
    fields: BTreeMap<Field, Entity<InputState>>,
    flags: BTreeSet<Flag>,
    revision: u64,
    pub(crate) review: Option<Review>,
    pub(crate) inspection: Vec<QueryResult>,
    pub(crate) message: Option<String>,
    _subscriptions: Vec<Subscription>,
}
pub enum ManagementEvent {
    TablePreview {
        target: Target,
        result: Result<Box<Review>, String>,
    },
    Preview {
        target: Target,
        revision: u64,
        result: Result<Box<Review>, String>,
    },
    Inspected {
        target: Target,
        revision: u64,
        result: Result<management::Inspection, String>,
    },
    Applied(Box<management::Applied>),
}
impl ManagementDialog {
    fn draft(&self, cx: &App) -> Draft {
        Draft {
            operation: self.operation,
            values: self
                .fields
                .iter()
                .map(|(field, value)| (*field, value.read(cx).value().to_string()))
                .collect(),
            flags: self.flags.clone(),
        }
    }
}

pub fn structure_operations() -> &'static [Operation] {
    use Operation::*;
    &[
        AddColumn,
        AlterColumn,
        SetDefault,
        SetNullable,
        SetType,
        RenameColumn,
        DropColumn,
        CreateIndex,
        CreateUniqueIndex,
        RenameIndex,
        DropIndex,
        AddPrimaryKey,
        DropPrimaryKey,
        AddForeignKey,
        DropForeignKey,
        AddCheck,
        DropCheck,
        AddUnique,
        DropUnique,
        InspectConstraints,
        RenameTable,
        DropTable,
    ]
}
fn access_operations(engine: Engine) -> Vec<Operation> {
    use Operation::*;
    let mut ops = vec![InspectPrivileges, CreateAccount, SetPassword, AccountLimits];
    match engine.descriptor().tools.administration {
        rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::Postgres) => {
            ops.extend([AlterRole, ClearPassword, GrantMembership, RevokeMembership]);
        }
        rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::MySql) => {
            ops.extend([LockAccount, UnlockAccount])
        }
        _ => return Vec::new(),
    }
    ops.extend([RenameAccount, DropAccount, Grant, Revoke]);
    ops
}

// Keep provider policy out of the renderer. This is the integration boundary for
// the admin owner's supported-action descriptors.
fn management_operations(operation: Operation, engine: Engine) -> Vec<Operation> {
    if rowboat_admin::providers::operation_support(engine, operation.provider_operation()).is_err()
    {
        return Vec::new();
    }
    let mut operations = if operation.table_bound() {
        structure_operations().to_vec()
    } else if operation.access() {
        access_operations(engine)
    } else {
        use Operation::*;
        match engine.descriptor().tools.administration {
            rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::Postgres) => vec![
                CreateDatabase,
                DropDatabase,
                CreateSchema,
                RenameSchema,
                DropSchema,
            ],
            rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::MySql) => {
                vec![CreateDatabase, DropDatabase]
            }
            _ => Vec::new(),
        }
    };
    if engine == Engine::MySql {
        operations.retain(|op| !matches!(op, Operation::SetNullable | Operation::SetType));
    }
    operations.retain(|op| op.supported(engine));
    operations
}

impl Workbench {
    pub fn management_target(&self, table_bound: bool) -> Option<Target> {
        let p = self.profile.as_ref()?;
        Some(Target {
            session: self.session_generation,
            connection: p.id.clone(),
            database: management::target_database(p),
            engine: p.engine,
            table: if table_bound {
                Some(self.table.clone()?)
            } else {
                None
            },
        })
    }
    pub(crate) fn management_guard(&self, inspection: bool) -> Result<(), String> {
        if self.database.is_none() {
            return Err("Connect to a database first".into());
        }
        if self.busy || self.close_requested || !self.changes.is_empty() || self.has_row_draft() {
            return Err(
                "Finish the current operation and apply or revert pending row changes first".into(),
            );
        }
        if !inspection && self.profile.as_ref().is_none_or(|p| p.read_only) {
            return Err("Management changes are disabled for read-only profiles".into());
        }
        if !inspection && (self.transaction || !self.transaction_known) {
            return Err(
                "Commit or roll back and confirm transaction state before management changes"
                    .into(),
            );
        }
        Ok(())
    }
    pub(crate) fn structure_action_enabled(&self, operation: Operation) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|p| operation.supported(p.engine))
            && self.management_guard(operation.inspection()).is_ok()
            && (operation.inspection() || self.structure_target_supported())
    }

    fn structure_target_supported(&self) -> bool {
        let Some(table) = &self.table else {
            return false;
        };
        if self
            .catalog
            .iter()
            .any(|t| t.table == *table && t.kind.to_lowercase().contains("view"))
        {
            return false;
        }
        if self
            .profile
            .as_ref()
            .is_some_and(|p| p.engine == Engine::Sqlite)
        {
            return !table.name.to_ascii_lowercase().starts_with("sqlite_")
                && !self
                    .structure
                    .definition
                    .to_ascii_uppercase()
                    .contains("CREATE VIRTUAL TABLE")
                && !self.catalog.iter().any(|t| {
                    t.table == *table
                        && (t.kind.to_lowercase().contains("virtual")
                            || t.kind.to_lowercase().contains("shadow"))
                });
        }
        true
    }
    pub fn show_management(
        &mut self,
        operation: Operation,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_admin(operation.provider_operation(), cx) {
            return;
        }
        if self
            .profile
            .as_ref()
            .is_none_or(|p| !operation.supported(p.engine))
        {
            self.fail("This SQLite action is unavailable: type/default/nullability and constraint changes require a table rebuild; index rename is unsupported. Use the supported column, table and index actions.".into());
            cx.notify();
            return;
        }
        if self.management.is_none() && !self.can_leave() {
            cx.notify();
            return;
        }
        if self.form.is_some()
            || self.transfer.is_some()
            || self.close_requested
            || self.close_tab_requested.is_some()
        {
            return;
        }
        if let Err(e) = self.management_guard(operation.inspection()) {
            self.fail(e);
            cx.notify();
            return;
        }
        let Some(target) = self.management_target(operation.table_bound()) else {
            return;
        };
        if operation.table_bound() && !operation.inspection() && !self.structure_target_supported()
        {
            self.fail("These structure actions apply to ordinary tables. Views, virtual, shadow and internal tables are unsupported.".into());
            cx.notify();
            return;
        }
        let previous_name = self
            .management
            .as_ref()
            .and_then(|d| d.fields.get(&Field::Name))
            .map(|v| v.read(cx).value().to_string());
        let selected_account = self
            .advanced
            .selected_object
            .and_then(|i| self.advanced.objects.get(i))
            .filter(|o| o.kind == rowboat_admin::objects::ObjectKind::Role);
        let name = name
            .or(previous_name)
            .or_else(|| {
                if operation.access() {
                    selected_account.map(|o| o.name.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let schema = self
            .table
            .as_ref()
            .map(|t| t.schema.clone())
            .unwrap_or_else(|| self.namespace_scope().unwrap_or_default().to_owned());
        let mut flags = BTreeSet::from([Flag::Nullable, Flag::Inherit]);
        let column_info = self.structure.columns.iter().find(|c| c.name == name);
        if operation == Operation::AlterColumn && column_info.is_some_and(|c| !c.nullable) {
            flags.remove(&Flag::Nullable);
        }
        let mut fields = BTreeMap::new();
        let mut subscriptions = Vec::new();
        for field in Draft::fields(operation, target.engine) {
            let value = match field {
                Field::Name => {
                    if operation == Operation::DropPrimaryKey
                        && target.engine == Engine::MySql
                        && name.is_empty()
                    {
                        "PRIMARY".into()
                    } else {
                        name.clone()
                    }
                }
                Field::Type => {
                    if operation == Operation::AlterColumn {
                        column_info
                            .map(|c| c.data_type.trim_end_matches(" unsigned").to_owned())
                            .unwrap_or_else(|| "text".into())
                    } else {
                        "text".into()
                    }
                }
                Field::DefaultKind => {
                    if operation == Operation::AlterColumn && target.engine == Engine::MySql {
                        "Keep"
                    } else if operation == Operation::AlterColumn
                        && column_info.is_some_and(|c| c.default_value.is_some())
                    {
                        "Expression"
                    } else {
                        "None"
                    }
                    .into()
                }
                Field::Default => {
                    if operation == Operation::AlterColumn {
                        column_info
                            .and_then(|c| c.default_value.clone())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                }
                Field::Columns | Field::RefColumns => "[]".into(),
                Field::Schema | Field::RefSchema => schema.clone(),
                Field::Object => self
                    .table
                    .as_ref()
                    .map(|t| t.name.clone())
                    .unwrap_or_default(),
                Field::Host | Field::NewHost => selected_account
                    .and_then(|o| o.schema.clone())
                    .unwrap_or_else(|| "localhost".into()),
                Field::OnUpdate | Field::OnDelete => "NO ACTION".into(),
                Field::Limit => {
                    if matches!(operation, Operation::AccountLimits | Operation::AlterRole) {
                        ""
                    } else {
                        "-1"
                    }
                    .into()
                }
                Field::PgLogin
                | Field::PgSuperuser
                | Field::PgCreateDb
                | Field::PgCreateRole
                | Field::PgInherit
                | Field::PgReplication
                | Field::PgBypassRls => "Unchanged".into(),
                Field::Expiration => "Unchanged".into(),
                Field::LockDuration => "Unchanged".into(),
                Field::Scope => "Table".into(),
                _ => String::new(),
            };
            let input = if field == Field::Password {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .masked(true)
                        .placeholder("Memory only; never saved with SQL")
                })
            } else {
                forms::input(value, field.label(), window, cx)
            };
            subscriptions.push(cx.subscribe(&input, |s, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(d) = &mut s.management
                {
                    d.revision += 1;
                    d.review = None;
                    d.inspection.clear();
                    d.message = None;
                    cx.notify();
                }
            }));
            fields.insert(field, input);
        }
        self.management = Some(ManagementDialog {
            target,
            operation,
            fields,
            flags,
            revision: 0,
            review: None,
            inspection: vec![],
            message: None,
            _subscriptions: subscriptions,
        });
        self.quick_open = false;
        self.dismiss_completions(cx);
        self.tabs[self.active_tab].show_parameters = false;
        self.error = None;
        if let Some(input) = self
            .management
            .as_ref()
            .and_then(|d| d.fields.values().next())
        {
            input.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }
    pub fn preview_management(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.management else {
            return;
        };
        let inspect = dialog.operation.inspection();
        if let Err(e) = self.management_guard(inspect) {
            self.fail(e);
            cx.notify();
            return;
        }
        let target = dialog.target.clone();
        if self
            .profile
            .as_ref()
            .is_none_or(|p| !target.matches(self.session_generation, p, self.table.as_ref()))
        {
            self.fail("Management target changed; reopen the form".into());
            cx.notify();
            return;
        }
        let revision = dialog.revision;
        let draft = dialog.draft(cx);
        let db = self.database.clone().unwrap();
        if let Some(dialog) = &self.management {
            for input in dialog.fields.values() {
                input.update(cx, |v, cx| v.set_disabled(true, cx));
            }
        }
        self.busy = true;
        self.error = None;
        self.status = if inspect {
            "Inspecting current database state…"
        } else {
            "Preparing management preview…"
        }
        .into();
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let event = if inspect {
                    let result = management::inspect(&db, &target, draft)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    ManagementEvent::Inspected {
                        target,
                        revision,
                        result,
                    }
                } else {
                    let result = management::prepare(&db, target.clone(), draft)
                        .await
                        .map(Box::new)
                        .map_err(|e| format!("{e:#}"));
                    ManagementEvent::Preview {
                        target,
                        revision,
                        result,
                    }
                };
                let _ = tx.send(Event::Management(Box::new(event)));
            })
        });
        cx.notify();
    }
    pub fn apply_management(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            self.management_guard(false)?;
            let dialog = self.management.as_ref().ok_or("No management form")?;
            let review = dialog
                .review
                .as_ref()
                .ok_or("Review the generated SQL first")?;
            let profile = self.profile.as_ref().ok_or("Connection closed")?;
            if !review
                .target
                .matches(self.session_generation, profile, self.table.as_ref())
            {
                return Err("Connection or table changed after review".to_string());
            }
            review
                .validate(
                    &dialog.target,
                    dialog.operation,
                    profile.read_only,
                    self.busy,
                    !self.changes.is_empty() || self.has_row_draft(),
                    self.transaction_known.then_some(self.transaction),
                )
                .map_err(|e| e.to_string())
        })();
        if let Err(e) = result {
            self.fail(e);
            cx.notify();
            return;
        }
        let review = self.management.as_mut().unwrap().review.take().unwrap();
        self.start_management_apply(review, cx);
    }
    pub(crate) fn start_management_apply(&mut self, review: Review, cx: &mut Context<Self>) {
        self.set_designer_disabled(true, cx);
        if let Some(dialog) = &self.management {
            for input in dialog.fields.values() {
                input.update(cx, |v, cx| v.set_disabled(true, cx));
            }
        }
        let db = self.database.clone().unwrap();
        self.busy = true;
        self.transaction_known = false;
        self.error = None;
        self.status = "Applying reviewed management change…".into();
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let result = management::apply(db, review).await;
                let _ = tx.send(Event::Management(Box::new(ManagementEvent::Applied(
                    Box::new(result),
                ))));
            })
        });
        cx.notify();
    }
    pub fn receive_management(&mut self, event: ManagementEvent, cx: &mut Context<Self>) {
        self.busy = false;
        self.set_designer_disabled(false, cx);
        if let Some(dialog) = &self.management {
            for input in dialog.fields.values() {
                input.update(cx, |v, cx| v.set_disabled(false, cx));
            }
        }
        match event {
            ManagementEvent::TablePreview { target, result } => {
                if self.management_target(false).as_ref() == Some(&target)
                    && let Some(designer) = &mut self.advanced.designer
                    && designer.target.as_ref() == Some(&target)
                {
                    match result {
                        Ok(review) => {
                            designer.review = Some(*review);
                            self.status = "Review prepared · nothing executed".into();
                        }
                        Err(error) => self.fail(error),
                    }
                }
            }
            ManagementEvent::Preview {
                target,
                revision,
                result,
            } => {
                if let Some(dialog) = &mut self.management
                    && dialog.target == target
                {
                    if dialog.revision != revision {
                        dialog.message =
                            Some("Form changed while preparing the preview. Review again.".into());
                    } else {
                        match result {
                            Ok(review) => {
                                dialog.review = Some(*review);
                                dialog.message = None;
                                self.status = "Review prepared · nothing executed".into();
                            }
                            Err(e) => {
                                dialog.message = Some(e);
                                self.status = "Preview failed · correct the form and retry".into();
                            }
                        }
                    }
                }
            }
            ManagementEvent::Inspected {
                target,
                revision,
                result,
            } => {
                if let Some(dialog) = &mut self.management
                    && dialog.target == target
                    && dialog.revision == revision
                {
                    match result {
                        Ok(inspection) => {
                            dialog.inspection = inspection.results;
                            dialog.message = inspection.notice;
                            self.status = "Inspection loaded from database".into();
                        }
                        Err(e) => dialog.message = Some(e),
                    }
                }
            }
            ManagementEvent::Applied(result) => {
                if self.profile.as_ref().is_none_or(|p| {
                    !result
                        .target
                        .matches(self.session_generation, p, self.table.as_ref())
                }) {
                    self.fail(
                        "Management finished for a previous target; reconnect to refresh".into(),
                    );
                    cx.notify();
                    return;
                }
                self.transaction_known = result.transaction.is_ok();
                if let Ok(active) = result.transaction {
                    self.transaction = active;
                }
                self.selected = None;
                self.widths.clear();
                self.table_result = Default::default();
                self.structure = Structure::default();
                self.advanced.reset_connection();
                self.query_edit = Default::default();
                self.change_target = None;
                self.grid = Default::default();
                self.relationships = None;
                self.relationship_origin = None;
                self.relationship_return = None;
                self.namespace.result_context.clear();
                self.explain = None;
                for tab in &mut self.tabs {
                    if tab.connection_id.as_ref() == Some(&result.target.connection) {
                        tab.results.clear();
                        tab.result_index = 0;
                        tab.metadata_incomplete.clear();
                        tab.query_progress = None;
                    }
                }
                let mut refresh_errors = Vec::new();
                match result.catalog {
                    Ok(c) => self.catalog = self.scope_catalog(c),
                    Err(e) => {
                        self.catalog.clear();
                        refresh_errors.push(format!("Catalog refresh: {e}"));
                    }
                }
                if let Some((table, structure)) = result.structure {
                    self.table = Some(table);
                    match structure {
                        Ok(s) => self.structure = s,
                        Err(e) => refresh_errors.push(format!("Structure refresh: {e}")),
                    }
                } else {
                    self.table = None;
                    if self.page == Page::Table {
                        self.page = Page::Tools;
                    }
                }
                if !self.transaction_known {
                    refresh_errors.push("Transaction state could not be confirmed; use Rollback before another change.".into());
                }
                let applied = result.result.is_ok();
                match result.result {
                    Ok(()) => {
                        self.management = None;
                        self.advanced.designer = None;
                        self.status =
                            "Management change applied · catalog and structure refreshed".into();
                        if self.table.is_some() {
                            self.table_page = TablePage::Structure;
                        }
                    }
                    Err(e) => {
                        if let Some(designer) = &mut self.advanced.designer {
                            designer.review = None;
                            self.error = Some(e.clone());
                        }
                        self.status = "Management change failed · refresh inspected; review again before retrying".into();
                        if let Some(d) = &mut self.management {
                            d.message = Some(
                                if rowboat_admin::providers::operation_support(
                                    result.target.engine,
                                    d.operation.provider_operation(),
                                )
                                .is_ok_and(|s| s.implicit_commit)
                                {
                                    format!(
                                        "{e}\nThis provider may have committed changes before this failure. Inspect current state and generate a new review before retrying."
                                    )
                                } else {
                                    e
                                },
                            );
                            d.review = None;
                        }
                    }
                }
                if !refresh_errors.is_empty() {
                    self.fail(format!(
                        "{}\n{}",
                        if applied {
                            "The change was applied, but refresh was incomplete."
                        } else {
                            "The change failed; refresh also reported errors."
                        },
                        refresh_errors.join("\n")
                    ));
                }
                self.activity_revision = self.activity_revision.wrapping_add(1);
                if result.target.engine == Engine::Sqlite && self.transaction_known {
                    // Refresh completion scope/catalog metadata only. Never execute an editor buffer.
                    self.load_namespace(self.namespace.selected.clone(), false, cx);
                }
            }
        }
        cx.notify();
    }
    pub fn render_management(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let dialog = self.management.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let engine = dialog.target.engine;
        let free = !self.busy;
        let mut body = column()
            .p_4()
            .gap_3()
            .text_size(px(13.))
            .child(
                row()
                    .flex_wrap()
                    .child(ui::icon(ui::IconName::Database).text_color(theme.accent))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(dialog.operation.label()),
                    )
                    .child(div().flex_1())
                    .child(
                        action("management-close", "Close", false, free, cx, |s, w, cx| {
                            s.management = None;
                            w.focus(&s.focus, cx);
                            cx.notify();
                        })
                        .debug_selector(|| "management-close".into()),
                    ),
            )
            .child(label(
                format!(
                    "{} · {} · session {}",
                    self.connection_label(Some(&dialog.target.connection)),
                    dialog.target.label(),
                    dialog.target.session
                ),
                cx,
            ))
            .child(div().h(px(1.)).bg(theme.divider()));
        if let Some(message) = &dialog.message {
            body = body.child(
                div()
                    .p_3()
                    .bg(theme.inset)
                    .text_color(theme.danger)
                    .child(message.clone()),
            );
        }
        if let Some(review) = &dialog.review {
            body = body
                .child(section("Review · nothing has been executed", cx))
                .child(label(review.execution_note(), cx));
            for warning in &review.display.warnings {
                body = body.child(div().text_color(theme.warning).child(warning.clone()));
            }
            for statement in &review.display.statements {
                body = body
                    .child(label(
                        format!(
                            "{}{}",
                            if statement.destructive {
                                "DESTRUCTIVE · "
                            } else {
                                ""
                            },
                            statement.reason
                        ),
                        cx,
                    ))
                    .child(
                        div()
                            .p_3()
                            .bg(theme.inset)
                            .font_family(theme.mono_font.clone())
                            .text_size(px(12.))
                            .child(statement.sql.clone()),
                    );
            }
            if dialog.operation.secret() {
                body = body.child(label("Password literals are redacted here. Real credentials execute from memory only; no management SQL is added to query tabs, history, or recovery.", cx));
            }
            body = body.child(
                row()
                    .flex_wrap()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.divider())
                    .child(action(
                        "management-back",
                        "Edit form",
                        false,
                        free,
                        cx,
                        |s, _, cx| {
                            if let Some(d) = &mut s.management {
                                d.review = None;
                            }
                            cx.notify();
                        },
                    ))
                    .child(div().flex_1())
                    .child(
                        action(
                            "management-apply",
                            "Apply reviewed change",
                            true,
                            free,
                            cx,
                            |s, _, cx| s.apply_management(cx),
                        )
                        .debug_selector(|| "management-apply".into()),
                    ),
            );
        } else {
            let mut choices = row().flex_wrap().p_2().bg(theme.surface);
            for operation in management_operations(dialog.operation, engine) {
                choices = choices.child(
                    action(
                        operation.label(),
                        operation.label(),
                        false,
                        free,
                        cx,
                        move |s, w, cx| s.show_management(operation, None, w, cx),
                    )
                    .selected(operation == dialog.operation)
                    .styles(|s| {
                        s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))
                    })
                    .debug_selector(move || operation.label().into()),
                );
            }
            body = body.child(choices);
            if engine == Engine::Sqlite {
                body = body.child(label("SQLite · ordinary tables and normal/unique indexes only. Type, default, nullability and constraint alterations require an unsupported table rebuild. Index rename, virtual/internal objects and CASCADE are unavailable. Names are scoped to the captured file and namespace.", cx));
            }
            if dialog.operation == Operation::AlterColumn {
                body = body.child(label("Replaces type, NULL rule and default. MySQL reads and preserves unsigned, AUTO_INCREMENT, charset/collation, comment, ON UPDATE and visibility attributes; unmodeled/generated columns are rejected. Keep preserves its current default. PostgreSQL conversions requiring USING need manual SQL.",cx));
            }
            if dialog.operation == Operation::AlterRole {
                body=body.child(label("Only selected attributes change. Unchanged flags and blank limits retain server settings. Password is unchanged; use Set password separately.",cx));
            }
            for field in Draft::fields(dialog.operation, engine) {
                let input = &dialog.fields[&field];
                let selected_value = |field| {
                    dialog
                        .fields
                        .get(&field)
                        .map(|v| v.read(cx).value().to_string())
                        .unwrap_or_default()
                };
                if (field == Field::Default
                    && !matches!(
                        selected_value(Field::DefaultKind).as_str(),
                        "Literal" | "Expression"
                    ))
                    || (field == Field::ExpiryDays && selected_value(Field::Expiration) != "Days")
                    || (field == Field::LockDays && selected_value(Field::LockDuration) != "Days")
                    || (field == Field::Schema && selected_value(Field::Scope) == "Global")
                    || (field == Field::Object
                        && !matches!(selected_value(Field::Scope).as_str(), "Table" | "Sequence"))
                {
                    continue;
                }
                let choices = field.choices(engine);
                let mut line = column().gap_1().child(label(field.label(), cx));
                if !choices.is_empty() {
                    let value = input.read(cx).value();
                    let mut options = row().flex_wrap();
                    for choice in choices {
                        let choice = *choice;
                        if choice == "Keep"
                            && !(dialog.operation == Operation::AlterColumn
                                && engine == Engine::MySql)
                        {
                            continue;
                        }
                        options = options.child(
                            action(
                                format!("management-{field:?}-{choice}"),
                                choice,
                                false,
                                free,
                                cx,
                                move |s, w, cx| {
                                    if let Some(input) =
                                        s.management.as_ref().and_then(|d| d.fields.get(&field))
                                    {
                                        input.update(cx, |v, cx| v.set_value(choice, w, cx));
                                    }
                                },
                            )
                            .selected(value.as_ref() == choice)
                            .styles(|s| {
                                s.selected(|s| {
                                    s.text_color(theme.accent).border_color(theme.accent)
                                })
                            }),
                        );
                    }
                    line = line.child(options);
                } else if field == Field::Privileges {
                    let selected: Vec<_> = input
                        .read(cx)
                        .value()
                        .split(',')
                        .map(str::to_owned)
                        .collect();
                    let mut options = row().flex_wrap();
                    for privilege in [
                        "ALL",
                        "SELECT",
                        "INSERT",
                        "UPDATE",
                        "DELETE",
                        "REFERENCES",
                        "TRIGGER",
                        "USAGE",
                        "CREATE",
                        "CONNECT",
                        "TEMPORARY",
                        "EXECUTE",
                        "ALTER",
                        "DROP",
                        "INDEX",
                        "CREATE VIEW",
                        "SHOW VIEW",
                        "CREATE ROUTINE",
                        "ALTER ROUTINE",
                        "CREATE USER",
                        "PROCESS",
                        "RELOAD",
                    ] {
                        options = options.child(
                            action(
                                format!("privilege-{privilege}"),
                                privilege,
                                false,
                                free,
                                cx,
                                move |s, w, cx| {
                                    if let Some(input) = s
                                        .management
                                        .as_ref()
                                        .and_then(|d| d.fields.get(&Field::Privileges))
                                    {
                                        let mut values: Vec<_> = input
                                            .read(cx)
                                            .value()
                                            .split(',')
                                            .filter(|p| !p.is_empty())
                                            .map(str::to_owned)
                                            .collect();
                                        if values.iter().any(|p| p == privilege) {
                                            values.retain(|p| p != privilege);
                                        } else {
                                            if privilege == "ALL" {
                                                values.clear();
                                            } else {
                                                values.retain(|p| p != "ALL");
                                            }
                                            values.push(privilege.into());
                                        }
                                        input.update(cx, |v, cx| {
                                            v.set_value(values.join(","), w, cx)
                                        });
                                    }
                                },
                            )
                            .selected(selected.iter().any(|p| p == privilege))
                            .styles(|s| {
                                s.selected(|s| {
                                    s.text_color(theme.accent).border_color(theme.accent)
                                })
                            }),
                        );
                    }
                    line=line.child(options).child(label("The typed builder validates privileges against the selected engine and scope.",cx));
                } else {
                    line = line.child(
                        ui::input(format!("management-{field:?}"), input, window, cx)
                            .debug_selector(move || format!("management-{field:?}")),
                    );
                }
                body = body.child(line);
                if field == Field::Columns && !self.structure.columns.is_empty() {
                    let mut picker = row().flex_wrap();
                    let selected: Vec<String> =
                        serde_json::from_str(&input.read(cx).value()).unwrap_or_default();
                    for col in &self.structure.columns {
                        let name = col.name.clone();
                        let selector = format!("pick-column-{name}");
                        let active = selected.contains(&name);
                        picker = picker.child(
                            action(
                                format!("pick-column-{name}"),
                                name.clone(),
                                false,
                                free,
                                cx,
                                move |s, w, cx| {
                                    if let Some(input) = s
                                        .management
                                        .as_ref()
                                        .and_then(|d| d.fields.get(&Field::Columns))
                                    {
                                        let mut values: Vec<String> =
                                            serde_json::from_str(&input.read(cx).value())
                                                .unwrap_or_default();
                                        if values.contains(&name) {
                                            values.retain(|v| v != &name);
                                        } else {
                                            values.push(name.clone());
                                        }
                                        input.update(cx, |v, cx| {
                                            v.set_value(
                                                serde_json::to_string(&values).unwrap(),
                                                w,
                                                cx,
                                            )
                                        });
                                    }
                                },
                            )
                            .selected(active)
                            .styles(|s| {
                                s.selected(|s| {
                                    s.text_color(theme.accent).border_color(theme.accent)
                                })
                            })
                            .debug_selector(move || selector.clone()),
                        );
                    }
                    body=body.child(picker).child(label("Click columns in key order. JSON arrays preserve names containing commas or spaces.",cx));
                }
            }
            let mut flags = row().flex_wrap();
            for flag in Draft::toggles(dialog.operation, engine) {
                flags = flags.child(
                    action(
                        format!("management-flag-{flag:?}"),
                        flag.label(),
                        false,
                        free,
                        cx,
                        move |s, _, cx| {
                            if let Some(d) = &mut s.management {
                                if !d.flags.remove(&flag) {
                                    d.flags.insert(flag);
                                }
                                d.revision += 1;
                                d.review = None;
                            }
                            cx.notify();
                        },
                    )
                    .selected(dialog.flags.contains(&flag))
                    .styles(|s| {
                        s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))
                    })
                    .child(ui::icon(if dialog.flags.contains(&flag) {
                        ui::IconName::Check
                    } else {
                        ui::IconName::Plus
                    })),
                );
            }
            body = body.child(flags);
            if dialog.operation.inspection() {
                body=body.child(label(if dialog.operation==Operation::InspectPrivileges{"Server-visible account attributes, memberships and direct/PUBLIC grants. Visibility depends on your privileges; inherited/effective permissions may exceed listed explicit grants."}else if engine == Engine::Sqlite {"Native SQLite schema SQL, table columns, indexes, index columns and foreign keys, in that order. These are live catalog values for the captured file and namespace."}else{"Live constraint names, types and definitions for the captured table."},cx));
                for (i, result) in dialog.inspection.iter().enumerate() {
                    body = body
                        .child(section(
                            format!(
                                "RESULT {} · {} rows{}",
                                i + 1,
                                result.rows.len(),
                                if result.truncated {
                                    " · truncated by server client limit"
                                } else {
                                    ""
                                }
                            ),
                            cx,
                        ))
                        .child(label(
                            result
                                .columns
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(" | "),
                            cx,
                        ));
                    for values in result.rows.iter().take(200) {
                        body = body.child(
                            div()
                                .font_family(theme.mono_font.clone())
                                .text_size(px(11.))
                                .child(
                                    values
                                        .iter()
                                        .map(|c| c.display())
                                        .collect::<Vec<_>>()
                                        .join(" | "),
                                ),
                        );
                    }
                    if result.rows.len() > 200 {
                        body = body.child(label(
                            "Showing first 200 rows of this inspection result.",
                            cx,
                        ));
                    }
                }
            }
            body = body.child(
                row()
                    .flex_wrap()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.divider())
                    .child(div().flex_1())
                    .child(
                        action(
                            "management-preview",
                            if dialog.operation.inspection() {
                                "Inspect current database state"
                            } else {
                                "Review SQL"
                            },
                            true,
                            free,
                            cx,
                            |s, _, cx| s.preview_management(cx),
                        )
                        .debug_selector(|| "management-preview".into()),
                    ),
            );
        }
        self.modal(
            div()
                .id("management-scroll")
                .w(px(940.).min(window.viewport_size().width - px(48.)))
                .max_h((window.viewport_size().height - px(64.)).min(px(920.)))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}
