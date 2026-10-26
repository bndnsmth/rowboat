//! Shared typed plans and review semantics; SQL compilation is provider-owned.
pub use crate::providers::ddl::*;
pub(crate) use crate::providers::ddl::{validate_design, validate_name, validate_table};
use crate::providers::{
    Atomicity, ExecutionSemantics, Operation, UnsupportedOperation, operation_support,
};
use rowboat_core::Engine;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedStatement {
    pub sql: String,
    pub destructive: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DdlPlan {
    pub statements: Vec<PlannedStatement>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReview {
    pub implicit_commit: bool,
    pub requires_outside_transaction: bool,
    pub destructive_statement_count: usize,
}
impl DdlPlan {
    /// Provider support is checked even for an empty plan. Metadata is not
    /// authorization to execute: freshness/permissions/session checks still apply.
    pub fn execution_semantics(
        &self,
        engine: Engine,
        operation: Operation,
    ) -> Result<ExecutionSemantics, UnsupportedOperation> {
        let mut semantics = operation_support(engine, operation)?;
        if self.is_empty() {
            semantics.atomicity = Atomicity::ReadOnly;
            semantics.implicit_commit = false;
            semantics.confirmation_required = false;
        } else if self
            .statements
            .iter()
            .any(|s| s.sql.starts_with("CREATE DATABASE ") || s.sql.starts_with("DROP DATABASE "))
        {
            semantics.atomicity = Atomicity::NonTransactional;
        }
        Ok(semantics)
    }

    /// Compatibility review. New execution consumers must use the fallible
    /// execution_semantics API; unknown providers conservatively require an
    /// outside-transaction review rather than claiming atomic execution.
    pub fn review(&self, engine: Engine) -> PlanReview {
        let semantics = self.execution_semantics(engine, Operation::SchemaDdl);
        PlanReview {
            implicit_commit: semantics.as_ref().is_ok_and(|s| s.implicit_commit),
            requires_outside_transaction: semantics
                .as_ref()
                .map_or(true, |s| s.atomicity == Atomicity::NonTransactional),
            destructive_statement_count: self.statements.iter().filter(|s| s.destructive).count(),
        }
    }
    pub fn sql(&self) -> String {
        self.statements
            .iter()
            .map(|s| s.sql.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }
    pub(crate) fn push(&mut self, sql: String, destructive: bool, reason: &str) {
        self.statements.push(PlannedStatement {
            sql: format!("{};", sql.trim_end_matches(';')),
            destructive,
            reason: reason.into(),
        });
    }
}
