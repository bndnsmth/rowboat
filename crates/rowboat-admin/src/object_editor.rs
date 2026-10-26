//! Shared exact-snapshot freshness and explicit reviewed-apply orchestration.
//! Providers supply catalog evidence and attribute-preserving SQL patches.
use crate::providers::objects::identity_present_on;
pub use crate::providers::objects::*;
pub(crate) use crate::providers::objects::{connection_origin, inspect_object_on, presence_sql};
use crate::providers::{Operation, operation_support};
use anyhow::{Result, ensure};
use rowboat_core::{AdministrativeStatus, Cell, Database, QuerySession};

pub async fn object_is_current(db: &Database, edit: &ObjectEdit) -> Result<bool> {
    object_is_current_on(db, edit).await
}
async fn object_is_current_on(db: &impl QuerySession, edit: &ObjectEdit) -> Result<bool> {
    operation_support(db.engine(), Operation::ObjectEdit)?;
    let original = edit.original();
    if db.engine() != original.identity().engine {
        return Ok(false);
    }
    if !identity_present_on(db, original.identity()).await? {
        return Ok(false);
    }
    Ok(inspect_object_on(db, original.identity()).await? == *original)
}

/// Explicit caller-authorized apply; never call during inspection or preview.
pub async fn apply_object_edit(db: &Database, edit: &ObjectEdit) -> Result<ObjectApplyOutcome> {
    operation_support(db.engine(), Operation::ObjectEdit)?;
    ensure!(
        db.engine() == edit.original().identity().engine,
        "Object edit belongs to a different engine"
    );
    let operation = db
        .administrative_operation(Some(presence_sql(edit.original().identity())?))
        .await?;
    apply_object_edit_on(operation, edit).await
}
pub(crate) async fn apply_object_edit_on(
    mut operation: rowboat_core::AdministrativeOperation,
    edit: &ObjectEdit,
) -> Result<ObjectApplyOutcome> {
    let current = match object_is_current_on(&operation, edit).await {
        Ok(current) => current,
        Err(error) => {
            operation.close().await?;
            return Err(error);
        }
    };
    if !current {
        let outcome = operation.close().await?;
        return Ok(ObjectApplyOutcome {
            status: ObjectApplyStatus::Stale,
            completed_statements: 0,
            failed_statement: None,
            rolled_back: outcome.rolled_back,
            presence: applied_presence(&outcome),
            error: None,
        });
    }
    for statement in &edit.plan().statements {
        if operation.execute(&statement.sql).await.is_err() {
            return Ok(applied_outcome(operation.close().await?));
        }
    }
    Ok(applied_outcome(operation.finish().await?))
}
fn applied_presence(outcome: &rowboat_core::AdministrativeOutcome) -> ObjectPresence {
    match outcome
        .observation
        .as_ref()
        .and_then(|r| r.first())
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
    {
        Some(Cell::Text(s)) if s == "present" => ObjectPresence::Present,
        Some(Cell::Text(s)) if s == "absent" => ObjectPresence::Absent,
        _ => ObjectPresence::Unknown,
    }
}
pub(crate) fn applied_outcome(outcome: rowboat_core::AdministrativeOutcome) -> ObjectApplyOutcome {
    let presence = applied_presence(&outcome);
    let status = match outcome.status {
        AdministrativeStatus::Committed | AdministrativeStatus::CompletedNonAtomic => {
            ObjectApplyStatus::Applied
        }
        AdministrativeStatus::OutcomeUnknown => ObjectApplyStatus::OutcomeUnknown,
        _ if presence == ObjectPresence::Unknown => ObjectApplyStatus::OutcomeUnknown,
        _ => ObjectApplyStatus::Failed,
    };
    ObjectApplyOutcome {
        status,
        completed_statements: outcome.completed_steps,
        failed_statement: outcome.failed_step,
        rolled_back: outcome.rolled_back,
        presence,
        error: outcome.error.or(outcome.observation_error),
    }
}
