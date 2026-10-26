//! Selected-principal effective ACLs and immutable, freshness-checked grant reviews.
use crate::{
    app::{Event, Workbench},
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy as ui;
use rowboat_admin::{
    access::Privilege,
    access_matrix::{
        self as api, AccessChange, AccessContext, AccessObject, AccessReport, AccessRequest,
        AccessState, PageRequest, Principal, PrincipalPage,
    },
    ddl::DdlPlan,
};
use rowboat_core::{Database, Engine, TableRef};
use std::collections::BTreeMap;

pub struct AccessDialog {
    id: uuid::Uuid,
    route: Route,
    fields: BTreeMap<&'static str, Entity<InputState>>,
    context: AccessContext,
    grant_option: bool,
    principals: Option<PrincipalPage>,
    report: Option<(AccessRequest, AccessReport)>,
    review: Option<AccessReview>,
    error: Option<String>,
}
#[derive(Clone)]
pub struct AccessReview {
    request: AccessRequest,
    change: AccessChange,
    report: String,
    plan: DdlPlan,
}
pub enum AccessEvent {
    Principals {
        id: uuid::Uuid,
        result: Result<PrincipalPage, String>,
    },
    Inspected {
        id: uuid::Uuid,
        request: AccessRequest,
        result: Result<AccessReport, String>,
    },
    Reviewed {
        id: uuid::Uuid,
        result: Result<AccessReview, String>,
    },
    Applied {
        id: uuid::Uuid,
        result: Result<AccessReport, String>,
        transaction: Result<bool, String>,
    },
}
fn fingerprint<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("access structures serialize")
}
fn privilege(value: &str) -> Option<Privilege> {
    Some(match value.to_uppercase().as_str() {
        "SELECT" => Privilege::Select,
        "INSERT" => Privilege::Insert,
        "UPDATE" => Privilege::Update,
        "DELETE" => Privilege::Delete,
        "TRUNCATE" => Privilege::Truncate,
        "MAINTAIN" => Privilege::Maintain,
        "REFERENCES" => Privilege::References,
        "TRIGGER" => Privilege::Trigger,
        "USAGE" => Privilege::Usage,
        "CREATE" => Privilege::Create,
        "ALTER" => Privilege::Alter,
        "DROP" => Privilege::Drop,
        "INDEX" => Privilege::Index,
        "CREATE VIEW" => Privilege::CreateView,
        "SHOW VIEW" => Privilege::ShowView,
        "CREATE ROUTINE" => Privilege::CreateRoutine,
        "ALTER ROUTINE" => Privilege::AlterRoutine,
        "EXECUTE" => Privilege::Execute,
        "EVENT" => Privilege::Event,
        "LOCK TABLES" => Privilege::LockTables,
        "CREATE TEMPORARY TABLES" => Privilege::CreateTemporaryTables,
        _ => return None,
    })
}
fn state_label(state: AccessState) -> &'static str {
    match state {
        AccessState::Granted => "Granted",
        AccessState::Denied => "Denied",
        AccessState::Unknown => "Unknown (not proven)",
    }
}
impl AccessDialog {
    fn request(&self, engine: Engine, cx: &App) -> anyhow::Result<AccessRequest> {
        let value = |key| self.fields[key].read(cx).value().to_string();
        let host = matches!(
            engine.descriptor().tools.administration,
            rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::MySql)
        )
        .then(|| value("host"));
        let principal = api::principal(engine, value("principal"), host)?;
        let schema = value("schema");
        let name = value("table");
        let column = value("column");
        let mut objects = vec![AccessObject::Namespace(schema.clone())];
        if !name.is_empty() {
            let table = TableRef { schema, name };
            objects.push(if column.is_empty() {
                AccessObject::Table(table)
            } else {
                AccessObject::Column { table, column }
            });
        }
        Ok(AccessRequest {
            principal,
            objects,
            context: self.context,
        })
    }
}
async fn apply_access(db: &Database, review: &AccessReview) -> anyhow::Result<AccessReport> {
    anyhow::ensure!(
        !db.in_transaction().await?,
        "Finish the transaction before applying permissions"
    );
    let semantics = review.plan.execution_semantics(
        db.engine(),
        rowboat_admin::providers::Operation::AccessManagement,
    )?;
    let transactional = semantics.atomicity == rowboat_admin::providers::Atomicity::Transactional;
    if transactional {
        db.begin().await?;
    }
    let result = async {
        let report = api::inspect_access(db, &review.request).await?;
        anyhow::ensure!(
            fingerprint(&report) == review.report,
            "Effective access/provenance changed; inspect and review again"
        );
        let plan = api::preview_change(db, &review.request, &review.change).await?;
        anyhow::ensure!(
            plan.sql() == review.plan.sql(),
            "Grant plan/settings changed; review again"
        );
        anyhow::ensure!(!plan.is_empty(), "Empty permission plan");
        db.query(&plan.sql(), 0).await?;
        if transactional {
            db.commit().await?;
        }
        api::inspect_access(db, &review.request).await
    }
    .await;
    if transactional && result.is_err() && db.in_transaction().await.unwrap_or(false) {
        db.rollback().await?;
    }
    result
}
impl Workbench {
    pub fn show_access_matrix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.require_admin(rowboat_admin::providers::Operation::AccessManagement, cx) {
            return;
        }
        if !self.can_leave() || self.close_requested {
            return;
        }
        let Some(profile) = &self.profile else {
            return;
        };
        let mut fields = BTreeMap::new();
        for (key, value) in [
            ("principal", profile.username.clone()),
            ("host", "%".into()),
            (
                "schema",
                self.table
                    .as_ref()
                    .map(|t| t.schema.clone())
                    .unwrap_or_else(|| profile.database.clone()),
            ),
            (
                "table",
                self.table
                    .as_ref()
                    .map(|t| t.name.clone())
                    .unwrap_or_default(),
            ),
            ("column", String::new()),
        ] {
            fields.insert(key, forms::input(value, key, window, cx));
        }
        self.access_matrix = Some(AccessDialog {
            id: uuid::Uuid::new_v4(),
            route: self.tx.route,
            fields,
            context: AccessContext::DefaultRoles,
            grant_option: false,
            principals: None,
            report: None,
            review: None,
            error: None,
        });
        self.load_principals(0, cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn load_principals(&mut self, offset: u32, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(db), Some(dialog)) = (self.database.clone(), self.access_matrix.as_ref()) else {
            return;
        };
        let id = dialog.id;
        let tx = self.tx.clone();
        self.busy = true;
        self.runtime.spawn(async move {
            let result = api::list_principals(&db, PageRequest { offset, limit: 50 })
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Access(Box::new(AccessEvent::Principals {
                id,
                result,
            })));
        });
        cx.notify();
    }
    fn inspect_access(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.close_requested {
            return;
        }
        let (Some(db), Some(dialog)) = (self.database.clone(), self.access_matrix.as_ref()) else {
            return;
        };
        let request = match dialog.request(db.engine(), cx) {
            Ok(request) => request,
            Err(e) => {
                self.fail(e.to_string());
                cx.notify();
                return;
            }
        };
        let id = dialog.id;
        let tx = self.tx.clone();
        self.access_matrix.as_mut().unwrap().review = None;
        self.busy = true;
        self.runtime.spawn(async move {
            let result = api::inspect_access(&db, &request)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Access(Box::new(AccessEvent::Inspected {
                id,
                request,
                result,
            })));
        });
        cx.notify();
    }
    fn review_access(&mut self, index: usize, revoke: bool, cx: &mut Context<Self>) {
        if self.busy
            || self.close_requested
            || self.profile.as_ref().is_none_or(|p| p.read_only)
            || self.transaction
            || !self.transaction_known
        {
            return;
        }
        let (Some(db), Some(dialog)) = (self.database.clone(), self.access_matrix.as_ref()) else {
            return;
        };
        let Some((request, report)) = &dialog.report else {
            return;
        };
        if !dialog
            .request(db.engine(), cx)
            .is_ok_and(|current| fingerprint(request) == fingerprint(&current))
        {
            self.fail("Selection changed; inspect again before reviewing".into());
            return;
        }
        let Some(row) = report.rows.get(index) else {
            return;
        };
        let Some(privilege) = privilege(&row.privilege) else {
            return;
        };
        let change = AccessChange {
            object: row.object.clone(),
            privileges: vec![privilege],
            revoke,
            grant_option: dialog.grant_option,
        };
        let request = request.clone();
        let report = fingerprint(report);
        let id = dialog.id;
        let tx = self.tx.clone();
        self.busy = true;
        self.runtime.spawn(async move {
            let result = api::preview_change(&db, &request, &change)
                .await
                .map(|plan| AccessReview {
                    request,
                    change,
                    report,
                    plan,
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Access(Box::new(AccessEvent::Reviewed {
                id,
                result,
            })));
        });
        cx.notify();
    }
    fn apply_access_review(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.close_requested
            || self.profile.as_ref().is_none_or(|p| p.read_only)
            || self.transaction
            || !self.transaction_known
        {
            return;
        }
        let (Some(db), Some(dialog)) = (self.database.clone(), self.access_matrix.as_ref()) else {
            return;
        };
        let Some(review) = dialog.review.clone() else {
            return;
        };
        if dialog.route != self.tx.route
            || !dialog
                .request(db.engine(), cx)
                .is_ok_and(|current| fingerprint(&review.request) == fingerprint(&current))
            || review.change.grant_option != dialog.grant_option
        {
            self.fail("Permission draft changed; inspect and review again".into());
            return;
        }
        let id = dialog.id;
        let tx = self.tx.clone();
        self.busy = true;
        self.access_matrix.as_mut().unwrap().review = None;
        self.runtime.spawn(async move {
            let note = crate::provider_ui::plan_note(
                &review.plan,
                db.engine(),
                rowboat_admin::providers::Operation::AccessManagement,
            );
            let result = apply_access(&db, &review)
                .await
                .map_err(|e| format!("{e:#}; {note}"));
            let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Access(Box::new(AccessEvent::Applied {
                id,
                result,
                transaction,
            })));
        });
        cx.notify();
    }
    pub fn receive_access(&mut self, event: AccessEvent, cx: &mut Context<Self>) {
        let id = match &event {
            AccessEvent::Principals { id, .. }
            | AccessEvent::Inspected { id, .. }
            | AccessEvent::Reviewed { id, .. }
            | AccessEvent::Applied { id, .. } => *id,
        };
        let Some(dialog) = &mut self.access_matrix else {
            return;
        };
        if dialog.id != id || dialog.route != self.tx.route {
            return;
        }
        self.busy = false;
        self.pending_focus = Some(self.focus.clone());
        dialog.error = None;
        match event {
            AccessEvent::Principals { result, .. } => match result {
                Ok(page) => dialog.principals = Some(page),
                Err(e) => dialog.error = Some(e),
            },
            AccessEvent::Inspected {
                request, result, ..
            } => match result {
                Ok(report) => dialog.report = Some((request, report)),
                Err(e) => {
                    dialog.report = None;
                    dialog.error = Some(e);
                }
            },
            AccessEvent::Reviewed { result, .. } => match result {
                Ok(review) => dialog.review = Some(review),
                Err(e) => dialog.error = Some(e),
            },
            AccessEvent::Applied {
                result,
                transaction,
                ..
            } => {
                match result {
                    Ok(report) => {
                        if let Some((_, old)) = &mut dialog.report {
                            *old = report;
                        }
                        self.status =
                            "Permission change applied; effective matrix refreshed".into();
                    }
                    Err(e) => dialog.error = Some(e),
                }
                match transaction {
                    Ok(active) => {
                        self.transaction = active;
                        self.transaction_known = true;
                    }
                    Err(e) => {
                        self.transaction_known = false;
                        dialog.error = Some(e);
                    }
                }
            }
        }
        cx.notify();
    }
    pub fn render_access_matrix(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let dialog = self.access_matrix.as_ref().unwrap();
        let writable = !self.busy
            && !self.close_requested
            && !self.transaction
            && self.transaction_known
            && self.profile.as_ref().is_some_and(|p| !p.read_only);
        let header = row()
            .p_4()
            .flex_shrink_0()
            .child(label(
                "Effective permissions · selected principal / exact objects",
                cx,
            ))
            .child(div().flex_1())
            .child(action(
                "close-access-matrix",
                "Close · Esc",
                false,
                !self.busy,
                cx,
                |s, w, cx| {
                    s.dismiss_access(w, cx);
                },
            ));
        let mut body=column().p_5().gap_3()
            .child(label("ACLs do not prove SQL execution: schema prerequisites, RLS, definer security and login restrictions still apply. Inactive provenance is membership evidence only. Unknown is not Denied.",cx));
        let mut fields = row().flex_wrap().gap_3();
        for (key, input) in &dialog.fields {
            if *key == "host"
                && !self.profile.as_ref().is_some_and(|p| {
                    matches!(
                        p.engine.descriptor().tools.administration,
                        rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::MySql)
                    )
                })
            {
                continue;
            }
            fields = fields.child(
                column()
                    .w(px(260.))
                    .min_w(px(240.))
                    .flex_shrink_0()
                    .child(label(*key, cx))
                    .child(ui::input(format!("access-{key}"), input, window, cx)),
            );
        }
        body = body.child(fields);
        let mut contexts = row().flex_wrap();
        for (index, context) in [
            AccessContext::Principal,
            AccessContext::DefaultRoles,
            AccessContext::CurrentSession,
            AccessContext::AvailableRoles,
        ]
        .into_iter()
        .enumerate()
        {
            contexts = contexts.child(action(
                ("access-context", index),
                format!(
                    "{context:?}{}",
                    if context == AccessContext::AvailableRoles {
                        " (hypothetical)"
                    } else {
                        ""
                    }
                ),
                context == dialog.context,
                !self.busy,
                cx,
                move |s, _, cx| {
                    if let Some(d) = &mut s.access_matrix {
                        d.context = context;
                        d.review = None;
                    }
                    cx.notify();
                },
            ));
        }
        body = body.child(contexts).child(action(
            "inspect-access",
            "Inspect selected principal / objects",
            true,
            !self.busy,
            cx,
            |s, _, cx| s.inspect_access(cx),
        ));
        if let Some(page) = &dialog.principals {
            let mut principals = row().flex_wrap();
            for (i, principal) in page.principals.iter().enumerate() {
                let principal = principal.clone();
                principals = principals.child(action(
                    ("matrix-principal", i),
                    format!("{principal:?}"),
                    false,
                    !self.busy,
                    cx,
                    move |s, w, cx| {
                        if let Some(d) = &mut s.access_matrix {
                            let (name, host) = match &principal {
                                Principal::Postgres(name) => (name.clone(), String::new()),
                                Principal::MySql(a) => (a.user.clone(), a.host.clone()),
                            };
                            d.fields["principal"].update(cx, |v, cx| v.set_value(name, w, cx));
                            d.fields["host"].update(cx, |v, cx| v.set_value(host, w, cx));
                            d.review = None;
                        }
                        cx.notify();
                    },
                ));
            }
            body = body.child(principals);
            for warning in &page.warnings {
                body = body.child(label(warning.clone(), cx));
            }
            if let Some(offset) = page.next_offset {
                body = body.child(action(
                    "next-principals",
                    "Next principal page",
                    false,
                    !self.busy,
                    cx,
                    move |s, _, cx| s.load_principals(offset, cx),
                ));
            }
            body = body.child(action(
                "first-principals",
                "First principal page",
                false,
                !self.busy,
                cx,
                |s, _, cx| s.load_principals(0, cx),
            ));
        }
        if let Some((request, report)) = &dialog.report {
            body = body.child(label(
                format!(
                    "Inspected: {:?} · {:?} · {:?}",
                    request.principal, request.context, request.objects
                ),
                cx,
            ));
            for warning in &report.warnings {
                body = body.child(label(warning.clone(), cx));
            }
            for (i, entry) in report.rows.iter().enumerate() {
                body = body.child(
                    row()
                        .child(label(
                            format!(
                                "{:?} · {} · effective: {} · grantable: {}",
                                entry.object,
                                entry.privilege,
                                state_label(entry.effective),
                                state_label(entry.grantable)
                            ),
                            cx,
                        ))
                        .child(action(
                            ("matrix-grant", i),
                            "Review Grant",
                            false,
                            writable && privilege(&entry.privilege).is_some(),
                            cx,
                            move |s, _, cx| s.review_access(i, false, cx),
                        ))
                        .child(action(
                            ("matrix-revoke", i),
                            "Review Revoke",
                            false,
                            writable && privilege(&entry.privilege).is_some(),
                            cx,
                            move |s, _, cx| s.review_access(i, true, cx),
                        )),
                );
                for evidence in &entry.provenance {
                    body = body.child(label(
                        format!(
                            "  {:?} · {:?} · {} · {} · grant option: {}",
                            evidence.kind,
                            evidence.principal,
                            evidence.scope,
                            if evidence.active {
                                "ACTIVE"
                            } else {
                                "INACTIVE evidence only"
                            },
                            evidence.grantable
                        ),
                        cx,
                    ));
                }
            }
            for role in &report.roles {
                body = body.child(label(
                    format!(
                        "Role {:?} · inherited {} · SET ROLE {} · default {} · active {}",
                        role.principal,
                        state_label(role.inherited),
                        state_label(role.can_set_role),
                        state_label(role.default_role),
                        state_label(role.active)
                    ),
                    cx,
                ));
            }
        }
        body = body.child(action(
            "matrix-grant-option",
            format!("Grant option: {}", dialog.grant_option),
            dialog.grant_option,
            !self.busy,
            cx,
            |s, _, cx| {
                if let Some(d) = &mut s.access_matrix {
                    d.grant_option = !d.grant_option;
                    d.review = None;
                }
                cx.notify();
            },
        ));
        if let Some(review) = &dialog.review {
            body = body.child(label(review.plan.sql(), cx));
            if let Some(profile) = &self.profile {
                body = body.child(label(
                    crate::provider_ui::plan_note(
                        &review.plan,
                        profile.engine,
                        rowboat_admin::providers::Operation::AccessManagement,
                    ),
                    cx,
                ));
            }
            for warning in &review.plan.warnings {
                body = body.child(label(warning.clone(), cx));
            }
            body=body.child(label("Apply re-inspects effective access and regenerates the exact reviewed SQL. Revoking direct grants may leave inherited/PUBLIC/owner access. External concurrent DDL is not locked.",cx)).child(action("apply-access-review","Apply reviewed permission change",true,writable,cx,|s,_,cx|s.apply_access_review(cx)));
        }
        if let Some(error) = &dialog.error {
            body = body.child(label(
                format!("Inspection/change failed (not a denial): {error}"),
                cx,
            ));
        }
        self.modal(
            column()
                .w(window.viewport_size().width - px(70.))
                .h(window.viewport_size().height - px(60.))
                .child(header)
                .child(
                    div()
                        .id("matrix-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                )
                .into_any_element(),
            cx,
        )
    }
    pub fn dismiss_access(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.access_matrix.is_none() || self.busy {
            return false;
        }
        self.access_matrix = None;
        window.focus(&self.focus, cx);
        cx.notify();
        true
    }
}

#[cfg(test)]
mod tests {
    async fn live_access(engine: rowboat_core::Engine) -> anyhow::Result<()> {
        use crate::pass5_tests::{cleanup, fixture, table};
        use rowboat_admin::{
            access::{MySqlAccount, Privilege},
            access_matrix::{
                self as api, AccessChange, AccessContext, AccessObject, AccessRequest, AccessState,
                Principal,
            },
        };
        use rowboat_core::{
            Engine,
            sql::{qualified_table, quote_ident},
        };
        let (db, _, schema) = fixture(engine).await?;
        let role = format!("{schema}_role");
        let account = if engine == Engine::Postgres {
            quote_ident(engine, &role)
        } else {
            format!("'{role}'@'%'")
        };
        let result = async {
            db.query(
                &format!(
                    "CREATE {} {account}",
                    if engine == Engine::Postgres {
                        "ROLE"
                    } else {
                        "USER"
                    }
                ),
                0,
            )
            .await?;
            let table = table(&schema, "acl_rows");
            db.query(
                &format!("CREATE TABLE {}(id INT)", qualified_table(engine, &table)),
                0,
            )
            .await?;
            let request = AccessRequest {
                principal: if engine == Engine::Postgres {
                    Principal::Postgres(role.clone())
                } else {
                    Principal::MySql(MySqlAccount {
                        user: role.clone(),
                        host: "%".into(),
                    })
                },
                objects: vec![AccessObject::Table(table.clone())],
                context: AccessContext::DefaultRoles,
            };
            let report = api::inspect_access(&db, &request).await?;
            let change = AccessChange {
                object: AccessObject::Table(table),
                privileges: vec![Privilege::Select],
                revoke: false,
                grant_option: false,
            };
            let review = super::AccessReview {
                request: request.clone(),
                change: change.clone(),
                report: super::fingerprint(&report),
                plan: api::preview_change(&db, &request, &change).await?,
            };
            let report = super::apply_access(&db, &review).await?;
            anyhow::ensure!(
                report
                    .rows
                    .iter()
                    .any(|r| r.privilege == "SELECT" && r.effective == AccessState::Granted)
            );
            anyhow::ensure!(
                super::apply_access(&db, &review).await.is_err(),
                "Stale grant report was replayed"
            );
            let change = AccessChange {
                revoke: true,
                ..change
            };
            let plan = api::preview_change(&db, &request, &change).await?;
            let review = super::AccessReview {
                request,
                change,
                report: super::fingerprint(&report),
                plan,
            };
            db.begin().await?;
            anyhow::ensure!(super::apply_access(&db, &review).await.is_err());
            db.rollback().await?;
            let report = super::apply_access(&db, &review).await?;
            anyhow::ensure!(
                report
                    .rows
                    .iter()
                    .any(|r| r.privilege == "SELECT" && r.effective == AccessState::Denied)
            );
            anyhow::Ok(())
        }
        .await;
        cleanup(&db, &schema).await?;
        db.query(
            &format!(
                "DROP {} IF EXISTS {account}",
                if engine == Engine::Postgres {
                    "ROLE"
                } else {
                    "USER"
                }
            ),
            0,
        )
        .await?;
        result
    }
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL on 15435"]
    async fn gui_pass5_access_postgres_review_staleness_and_apply() -> anyhow::Result<()> {
        live_access(rowboat_core::Engine::Postgres).await
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL on 13309"]
    async fn gui_pass5_access_mysql_review_staleness_and_apply() -> anyhow::Result<()> {
        live_access(rowboat_core::Engine::MySql).await
    }
    use super::{fingerprint, state_label};
    use rowboat_admin::access_matrix::{
        AccessContext, AccessObject, AccessRequest, AccessState, Principal,
    };
    #[test]
    fn unknown_is_not_denied_and_review_identity_includes_principal_object_and_role_context() {
        assert_ne!(
            state_label(AccessState::Unknown),
            state_label(AccessState::Denied)
        );
        let request = AccessRequest {
            principal: Principal::Postgres("alice".into()),
            objects: vec![AccessObject::Namespace("public".into())],
            context: AccessContext::DefaultRoles,
        };
        let mut changed = request.clone();
        changed.principal = Principal::Postgres("bob".into());
        assert_ne!(fingerprint(&request), fingerprint(&changed));
        changed = request.clone();
        changed.context = AccessContext::CurrentSession;
        assert_ne!(fingerprint(&request), fingerprint(&changed));
        changed = request.clone();
        changed.objects = vec![AccessObject::Namespace("other".into())];
        assert_ne!(fingerprint(&request), fingerprint(&changed));
    }
}
