//! Explicit column-only synchronization between existing MySQL tables.
//! Keys, table options and other objects are not claimed equivalent or dropped.
use super::{ChangeKind, DifferenceKind, NamespaceMapping, ObjectDifference};
use crate::{
    ddl::{DdlPlan, MySqlColumnSnapshot, inspect_mysql_column, plan_mysql_column_edit},
    objects::cell_text,
};
use anyhow::{Context, Result, ensure};
use rowboat_core::{
    Cell, Database, Engine, TableRef,
    sql::{literal, qualified_table},
};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColumnSetSnapshot {
    table: TableRef,
    columns: Vec<MySqlColumnSnapshot>,
}
impl ColumnSetSnapshot {
    pub fn table(&self) -> &TableRef {
        &self.table
    }
    pub fn columns(&self) -> &[MySqlColumnSnapshot] {
        &self.columns
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct ColumnComparison {
    pub plan: DdlPlan,
    pub objects: Vec<ObjectDifference>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    original_destination: ColumnSetSnapshot,
}
impl ColumnComparison {
    pub fn original_destination(&self) -> &ColumnSetSnapshot {
        &self.original_destination
    }
    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }
}
pub async fn snapshot_columns(db: &Database, table: &TableRef) -> Result<ColumnSetSnapshot> {
    crate::providers::operation_support(
        db.engine(),
        crate::providers::Operation::SchemaComparison,
    )?;
    ensure!(
        db.engine() == Engine::MySql,
        "Advanced column comparison requires MySQL"
    );
    crate::ddl::validate_table(table)?;
    ensure!(!table.schema.is_empty(), "Explicit namespace required");
    let value = |s: &str| literal(Engine::MySql, &Cell::Text(s.into()));
    let results=db.query(&format!("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} AND TABLE_NAME={} ORDER BY ORDINAL_POSITION",value(&table.schema),value(&table.name)),100_000).await?;
    let mut columns = vec![];
    for result in results {
        ensure!(!result.truncated, "Column catalog truncated");
        for row in result.rows {
            columns.push(
                inspect_mysql_column(
                    db,
                    table,
                    &cell_text(&row, 0)?.context("Missing column name")?,
                )
                .await?,
            );
        }
    }
    ensure!(
        !columns.is_empty(),
        "Table absent/inaccessible or has no inspected columns"
    );
    Ok(ColumnSetSnapshot {
        table: table.clone(),
        columns,
    })
}
pub async fn compare_mysql_columns(
    source: &Database,
    destination: &Database,
    mapping: &NamespaceMapping,
    table_name: &str,
) -> Result<ColumnComparison> {
    let a = snapshot_columns(
        source,
        &TableRef {
            schema: mapping.source.clone(),
            name: table_name.into(),
        },
    )
    .await?;
    let b = snapshot_columns(
        destination,
        &TableRef {
            schema: mapping.destination.clone(),
            name: table_name.into(),
        },
    )
    .await?;
    compare_column_snapshots(&a, &b, mapping)
}
pub fn compare_column_snapshots(
    source: &ColumnSetSnapshot,
    destination: &ColumnSetSnapshot,
    mapping: &NamespaceMapping,
) -> Result<ColumnComparison> {
    ensure!(
        source.table.schema == mapping.source
            && destination.table.schema == mapping.destination
            && source.table.name == destination.table.name,
        "Explicit same-name table namespace mapping required"
    );
    let mut result=ColumnComparison{plan:DdlPlan::default(),objects:vec![],blockers:vec![],warnings:vec!["Column-only comparison: all destination-only columns, keys, constraints, table options and other objects are retained; this is not whole-schema equivalence. MySQL DDL implicitly commits; data/dependency validation may fail. Review/stale-check before apply.".into()],original_destination:destination.clone()};
    let a = source.columns.first().context("Empty source")?.origin();
    let b = destination
        .columns
        .first()
        .context("Empty destination")?
        .origin();
    for setting in ["sql_mode", "encoding", "client_encoding", "collation"] {
        if a[setting] != b[setting] {
            result
                .blockers
                .push(format!("Source/destination {setting} differs"));
        }
    }
    let source_names: Vec<_> = source
        .columns
        .iter()
        .map(|c| c.definition().base.column.name.as_str())
        .collect();
    let target_names: Vec<_> = destination
        .columns
        .iter()
        .map(|c| c.definition().base.column.name.as_str())
        .collect();
    let projected: Vec<_> = target_names
        .iter()
        .filter(|n| source_names.contains(n))
        .copied()
        .chain(
            source_names
                .iter()
                .filter(|n| !target_names.contains(n))
                .copied(),
        )
        .collect();
    if projected != source_names {
        result
            .blockers
            .push("Column synchronization would require reordering/middle insertion".into());
    }
    for desired in &source.columns {
        let definition = desired.definition();
        let name = &definition.base.column.name;
        for expression in definition
            .generation
            .iter()
            .map(|g| g.expression.as_str())
            .chain(
                definition
                    .base
                    .column
                    .default
                    .iter()
                    .filter_map(|d| match d {
                        crate::ddl::DefaultValue::RawExpression(v) => Some(v.as_str()),
                        _ => None,
                    }),
            )
        {
            if expression.contains(&format!("{}.", mapping.source))
                || expression.contains(&format!(
                    "{}.",
                    rowboat_core::sql::quote_ident(Engine::MySql, &mapping.source)
                ))
            {
                result.blockers.push(format!(
                    "Column {name} expression needs explicit namespace rewriting"
                ));
            }
        }
        let old = destination
            .columns
            .iter()
            .find(|c| c.definition().base.column.name == *name);
        let change = match old {
            Some(old) if old.definition() == definition => ChangeKind::Unchanged,
            Some(old) => {
                match plan_mysql_column_edit(old, definition.clone()) {
                    Ok(edit) => result
                        .plan
                        .statements
                        .extend(edit.plan().statements.clone()),
                    Err(error) => result.blockers.push(format!("Column {name}: {error:#}")),
                }
                ChangeKind::Alter
            }
            None => {
                if definition.base.auto_increment {
                    result.blockers.push(format!(
                        "Column {name} requires atomic AUTO_INCREMENT key migration"
                    ));
                } else {
                    result.plan.push(
                        format!(
                            "ALTER TABLE {} ADD COLUMN {}",
                            qualified_table(Engine::MySql, &destination.table),
                            definition.sql()?
                        ),
                        false,
                        "Append supported advanced source column",
                    );
                }
                ChangeKind::Create
            }
        };
        result.objects.push(ObjectDifference {
            table: source.table.name.clone(),
            kind: DifferenceKind::Column,
            name: name.clone(),
            change,
            detail: "Advanced column definition comparison; no dependency drops".into(),
        });
    }
    for name in target_names {
        if !source_names.contains(&name) {
            result.objects.push(ObjectDifference {
                table: source.table.name.clone(),
                kind: DifferenceKind::Column,
                name: name.into(),
                change: ChangeKind::Preserve,
                detail: "Destination-only column retained".into(),
            });
        }
    }
    if result.is_blocked() {
        result.plan = DdlPlan::default();
    }
    result.plan.warnings.extend(result.warnings.clone());
    Ok(result)
}
pub async fn columns_destination_is_current(
    db: &Database,
    comparison: &ColumnComparison,
) -> Result<bool> {
    crate::providers::operation_support(
        db.engine(),
        crate::providers::Operation::SchemaComparison,
    )?;
    if db.engine() != Engine::MySql {
        return Ok(false);
    }
    Ok(
        snapshot_columns(db, &comparison.original_destination.table).await?
            == comparison.original_destination,
    )
}
