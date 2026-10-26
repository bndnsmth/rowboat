//! Name-based comparison of explicitly modeled schemas. Never auto-applies.
use crate::ddl::*;
use anyhow::{Result, ensure};
use rowboat_core::{Engine, TableRef};
use std::collections::{BTreeMap, BTreeSet};

type Key = (String, String);
fn key(t: &TableRef) -> Key {
    (t.schema.clone(), t.name.clone())
}
fn same_members<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().all(|value| b.contains(value))
}
fn same_definition(a: &TableDesign, b: &TableDesign) -> bool {
    same_members(&a.columns, &b.columns)
        && a.primary_key == b.primary_key
        && same_members(&a.indexes, &b.indexes)
        && same_members(&a.foreign_keys, &b.foreign_keys)
}
fn map(engine: Engine, tables: &[TableDesign]) -> Result<BTreeMap<Key, &TableDesign>> {
    let mut out = BTreeMap::new();
    for t in tables {
        validate_design(engine, t)?;
        ensure!(
            out.insert(key(&t.table), t).is_none(),
            "Duplicate table in schema snapshot"
        );
    }
    Ok(out)
}

/// Compare full typed snapshots. Foreign keys are dropped before any structural
/// changes and restored after all tables exist, including cyclic dependencies.
/// Snapshots must include all affected inbound foreign keys.
pub fn compare(
    engine: Engine,
    current: &[TableDesign],
    desired: &[TableDesign],
) -> Result<DdlPlan> {
    crate::providers::operation_support(engine, crate::providers::Operation::SchemaComparison)?;
    let old = map(engine, current)?;
    let new = map(engine, desired)?;
    for t in new.values() {
        for fk in &t.foreign_keys {
            let target = key(&fk.referenced_table);
            ensure!(
                !old.contains_key(&target) || new.contains_key(&target),
                "Foreign key {} still references a table removed from the desired schema",
                fk.name
            );
            if let Some(parent) = new.get(&target) {
                for column in &fk.referenced_columns {
                    ensure!(
                        parent.columns.iter().any(|c| c.name == *column),
                        "Foreign key {} references missing column {}",
                        fk.name,
                        column
                    );
                }
            }
        }
    }
    let mut ops = Vec::new();
    let mut affected = BTreeSet::new();
    for (k, t) in &old {
        if new.get(k).is_none_or(|n| !same_definition(n, t)) {
            affected.insert(k.clone());
        }
    }
    for k in new.keys() {
        if !old.contains_key(k) {
            affected.insert(k.clone());
        }
    }
    // Drop inbound/outbound FKs touching changed tables, as well as changed FKs.
    for (k, t) in &old {
        for fk in &t.foreign_keys {
            let changed = new.get(k).is_none_or(|n| !n.foreign_keys.contains(fk));
            if changed || affected.contains(k) || affected.contains(&key(&fk.referenced_table)) {
                ops.push(DdlOperation::DropForeignKey {
                    table: t.table.clone(),
                    name: fk.name.clone(),
                });
            }
        }
    }
    for (k, t) in &old {
        let Some(n) = new.get(k) else {
            ops.push(DdlOperation::DropTable {
                table: t.table.clone(),
                cascade: false,
            });
            continue;
        };
        let column_changes = !same_members(&t.columns, &n.columns);
        // Rebuild indexes around column changes; this avoids references to
        // dropped columns, and intentionally doesn't infer expression indexes.
        for index in &t.indexes {
            if column_changes || !n.indexes.contains(index) {
                ops.push(DdlOperation::DropIndex {
                    table: t.table.clone(),
                    name: index.name.clone(),
                });
            }
        }
        if (column_changes || t.primary_key != n.primary_key)
            && let Some(p) = &t.primary_key
        {
            ops.push(DdlOperation::DropPrimaryKey {
                table: t.table.clone(),
                name: p.name.clone(),
            });
        }
        for c in &t.columns {
            if !n.columns.iter().any(|v| v.name == c.name) {
                ops.push(DdlOperation::DropColumn {
                    table: t.table.clone(),
                    name: c.name.clone(),
                });
            }
        }
        for c in &n.columns {
            match t.columns.iter().find(|v| v.name == c.name) {
                None => ops.push(DdlOperation::AddColumn {
                    table: t.table.clone(),
                    column: c.clone(),
                }),
                Some(old) if old != c => ops.push(DdlOperation::AlterColumn {
                    table: t.table.clone(),
                    column: c.clone(),
                }),
                _ => (),
            }
        }
        if (column_changes || t.primary_key != n.primary_key)
            && let Some(p) = &n.primary_key
        {
            ops.push(DdlOperation::AddPrimaryKey {
                table: n.table.clone(),
                primary_key: p.clone(),
            });
        }
        for index in &n.indexes {
            if column_changes || !t.indexes.contains(index) {
                ops.push(DdlOperation::CreateIndex {
                    table: n.table.clone(),
                    index: index.clone(),
                });
            }
        }
    }
    for (k, t) in &new {
        if !old.contains_key(k) {
            let mut table = (*t).clone();
            table.foreign_keys.clear();
            ops.push(DdlOperation::CreateTable(table));
        }
    }
    for (k, t) in &new {
        for fk in &t.foreign_keys {
            let changed = old.get(k).is_none_or(|n| !n.foreign_keys.contains(fk));
            if changed || affected.contains(k) || affected.contains(&key(&fk.referenced_table)) {
                ops.push(DdlOperation::AddForeignKey {
                    table: t.table.clone(),
                    foreign_key: fk.clone(),
                });
            }
        }
    }
    let mut plan = preview(engine, &ops)?;
    if !plan.is_empty() {
        plan.warnings.push("Review before applying. Names are matched exactly; dropped/added names are not inferred renames. Snapshots must include affected inbound foreign keys. Unmodeled objects, dependencies, triggers, identity/generated attributes, permissions, partitioning and expression indexes are not compared.".into());
    }
    for (k, t) in &old {
        if let Some(n) = new.get(k) {
            let before: Vec<_> = t
                .columns
                .iter()
                .map(|c| &c.name)
                .filter(|name| n.columns.iter().any(|c| &c.name == *name))
                .collect();
            let after: Vec<_> = n
                .columns
                .iter()
                .map(|c| &c.name)
                .filter(|name| t.columns.iter().any(|c| &c.name == *name))
                .collect();
            if before != after {
                plan.warnings.push(format!(
                    "Column order changes for {}.{} are not applied.",
                    k.0, k.1
                ));
            }
        }
    }
    Ok(plan)
}
