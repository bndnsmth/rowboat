use super::{Operation, ServerKind, server};
use crate::{
    ddl::{DdlPlan, validate_name},
    objects::NamespaceOperation,
};
use anyhow::{Result, ensure};
use rowboat_core::{Engine, sql::quote_ident};

pub(crate) fn compile(engine: Engine, op: &NamespaceOperation) -> Result<DdlPlan> {
    let provider = server(engine, Operation::NamespaceDdl)?;
    use NamespaceOperation::*;
    let name = match op {
        CreateDatabase { name }
        | RenameDatabase { name, .. }
        | DropDatabase { name }
        | CreateSchema { name }
        | RenameSchema { name, .. }
        | DropSchema { name, .. } => name,
    };
    validate_name(name)?;
    let name = quote_ident(engine, name);
    let mut plan = DdlPlan::default();
    match op {
        CreateDatabase { .. } => {
            plan.push(format!("CREATE DATABASE {name}"), false, "Create database")
        }
        DropDatabase { .. } => plan.push(
            format!("DROP DATABASE {name}"),
            true,
            "Drop database and all its data",
        ),
        RenameDatabase { new_name, .. } | RenameSchema { new_name, .. } => {
            ensure!(
                provider == ServerKind::Postgres,
                "MySQL does not support renaming databases/schemas; use an explicit migration"
            );
            validate_name(new_name)?;
            plan.push(
                format!(
                    "ALTER {} {name} RENAME TO {}",
                    if matches!(op, RenameDatabase { .. }) {
                        "DATABASE"
                    } else {
                        "SCHEMA"
                    },
                    quote_ident(engine, new_name)
                ),
                false,
                "Rename namespace",
            );
        }
        CreateSchema { .. } => plan.push(format!("CREATE SCHEMA {name}"), false, "Create schema"),
        DropSchema { cascade, .. } => {
            ensure!(
                provider == ServerKind::Postgres || !cascade,
                "MySQL DROP SCHEMA does not accept CASCADE"
            );
            plan.push(
                format!(
                    "DROP SCHEMA {name}{}",
                    if *cascade { " CASCADE" } else { "" }
                ),
                true,
                "Drop schema",
            );
        }
    }
    if provider == ServerKind::MySql {
        plan.warnings.push(
            "MySQL SCHEMA is an alias for DATABASE; namespace DDL implicitly commits.".into(),
        );
    } else if matches!(
        op,
        CreateDatabase { .. } | DropDatabase { .. } | RenameDatabase { .. }
    ) {
        plan.warnings.push("Database operations may require a separate maintenance connection; CREATE/DROP DATABASE cannot run inside a transaction. Active connections can prevent rename/drop.".into());
    }
    Ok(plan)
}
