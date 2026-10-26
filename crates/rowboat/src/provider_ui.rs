//! Optional tool availability comes from installed providers, not engine names.
use crate::app::Workbench;
use gpui_kit::{Context, Window};
use rowboat_admin::providers::{self, Operation};

pub(crate) fn execution_note(semantics: providers::ExecutionSemantics) -> &'static str {
    if semantics.implicit_commit {
        "This provider implicitly commits DDL/account changes; failures may leave partial changes. Refresh and review before retrying."
    } else {
        match semantics.atomicity {
            providers::Atomicity::Transactional => {
                "This plan runs in a dedicated transaction; all statements must succeed before commit."
            }
            providers::Atomicity::NonTransactional => {
                "This plan executes outside a transaction; completed steps may remain after failure."
            }
            providers::Atomicity::ReadOnly => "Read-only inspection; no changes are applied.",
        }
    }
}

pub(crate) fn plan_note(
    plan: &rowboat_admin::ddl::DdlPlan,
    engine: rowboat_core::Engine,
    operation: Operation,
) -> String {
    match plan.execution_semantics(engine, operation) {
        Ok(semantics) => execution_note(semantics).into(),
        Err(error) => error.to_string(),
    }
}

impl Workbench {
    pub(crate) fn service_supported(&self, operation: rowboat_services::ServiceOperation) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|p| rowboat_services::service_support(p.engine, operation).is_ok())
    }
    pub(crate) fn require_service(
        &mut self,
        operation: rowboat_services::ServiceOperation,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(profile) = self.profile.as_ref() else {
            return false;
        };
        match rowboat_services::service_support(profile.engine, operation) {
            Ok(()) => true,
            Err(e) => {
                self.fail(e.to_string());
                cx.notify();
                false
            }
        }
    }
    pub(crate) fn inspect_admin(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_admin(operation, cx) {
            return;
        }
        let engine = self.profile.as_ref().unwrap().engine;
        use rowboat_admin::objects;
        let (title, sql) = match operation {
            Operation::ConnectionInfo => ("Connection info", objects::connection_info_sql(engine)),
            Operation::ServerActivity => ("Server activity", objects::server_activity_sql(engine)),
            Operation::Namespaces => ("Databases", objects::namespaces_sql(engine)),
            _ => return,
        };
        match sql {
            Ok(sql) => self.inspect_query(title, sql.into(), window, cx),
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
            }
        }
    }

    pub(crate) fn admin_supported(&self, operation: Operation) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|p| providers::operation_support(p.engine, operation).is_ok())
    }

    /// Check direct invocations as well as menu availability before scheduling I/O.
    pub(crate) fn require_admin(&mut self, operation: Operation, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile.as_ref() else {
            return false;
        };
        match providers::operation_support(profile.engine, operation) {
            Ok(_) => true,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                false
            }
        }
    }
}
