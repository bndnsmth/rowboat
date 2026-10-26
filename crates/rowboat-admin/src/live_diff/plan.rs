use super::*;
use crate::ddl::*;
use anyhow::{Context, bail};
use rowboat_core::{
    Cell,
    sql::{qualified_table, quote_ident},
};
use std::collections::BTreeSet;

fn named<T: Clone>(desired: &mut Vec<T>, old: &[T], name: impl Fn(&T) -> &str) {
    for item in old {
        if !desired.iter().any(|n| name(n) == name(item)) {
            desired.push(item.clone());
        }
    }
}
fn equivalent(a: &TableSnapshot, b: &TableSnapshot) -> bool {
    a.definition == b.definition
        && a.mysql_columns == b.mysql_columns
        && a.mysql_options == b.mysql_options
        && a.uniques == b.uniques
        && a.checks == b.checks
}
fn difference(
    out: &mut Vec<ObjectDifference>,
    table: &str,
    kind: DifferenceKind,
    name: &str,
    change: ChangeKind,
    detail: &str,
) {
    out.push(ObjectDifference {
        table: table.into(),
        kind,
        name: name.into(),
        change,
        detail: detail.into(),
    });
}
fn child_changes<T: PartialEq>(
    out: &mut Vec<ObjectDifference>,
    table: &str,
    kind: DifferenceKind,
    old: &[T],
    new: &[T],
    name: impl Fn(&T) -> &str,
) {
    for n in new {
        match old.iter().find(|o| name(o) == name(n)) {
            None => difference(
                out,
                table,
                kind,
                name(n),
                ChangeKind::Create,
                "Create source object in mapped destination",
            ),
            Some(o) if o != n => difference(
                out,
                table,
                kind,
                name(n),
                ChangeKind::Alter,
                "Replace changed definition",
            ),
            _ => (),
        }
    }
    for o in old {
        if !new.iter().any(|n| name(o) == name(n)) {
            difference(
                out,
                table,
                kind,
                name(o),
                ChangeKind::Drop,
                "Destination-only drop explicitly enabled",
            );
        }
    }
}
fn details(
    out: &mut Vec<ObjectDifference>,
    old: &TableSnapshot,
    new: &TableSnapshot,
    engine: Engine,
) {
    let Some(a) = &old.definition else { return };
    let Some(b) = &new.definition else { return };
    let t = &new.name;
    if engine == Engine::MySql {
        child_changes(
            out,
            t,
            DifferenceKind::Column,
            &old.mysql_columns,
            &new.mysql_columns,
            |c| &c.column.name,
        );
    } else {
        child_changes(
            out,
            t,
            DifferenceKind::Column,
            &a.columns,
            &b.columns,
            |c| &c.name,
        );
    }
    child_changes(out, t, DifferenceKind::Index, &a.indexes, &b.indexes, |i| {
        &i.name
    });
    child_changes(
        out,
        t,
        DifferenceKind::ForeignKey,
        &a.foreign_keys,
        &b.foreign_keys,
        |f| &f.name,
    );
    child_changes(
        out,
        t,
        DifferenceKind::Unique,
        &old.uniques,
        &new.uniques,
        |u| &u.name,
    );
    child_changes(
        out,
        t,
        DifferenceKind::Check,
        &old.checks,
        &new.checks,
        |c| &c.name,
    );
    if a.primary_key != b.primary_key {
        difference(
            out,
            t,
            DifferenceKind::PrimaryKey,
            b.primary_key
                .as_ref()
                .or(a.primary_key.as_ref())
                .map(|p| p.name.as_str())
                .unwrap_or(""),
            if a.primary_key.is_none() {
                ChangeKind::Create
            } else if b.primary_key.is_none() {
                ChangeKind::Drop
            } else {
                ChangeKind::Alter
            },
            "Primary key definition differs",
        );
    }
    if old.mysql_options != new.mysql_options {
        difference(
            out,
            t,
            DifferenceKind::TableOptions,
            t,
            ChangeKind::Blocked,
            "Changing existing table options is outside this planner's supported subset",
        );
    }
}

pub fn compare_snapshots(
    source: &SchemaSnapshot,
    destination: &SchemaSnapshot,
    mapping: &NamespaceMapping,
    options: CompareOptions,
) -> Result<LiveComparison> {
    crate::providers::operation_support(
        source.engine,
        crate::providers::Operation::SchemaComparison,
    )?;
    crate::providers::operation_support(
        destination.engine,
        crate::providers::Operation::SchemaComparison,
    )?;
    ensure!(
        source.engine == destination.engine,
        "Cross-engine live comparison is not supported"
    );
    ensure!(
        source.schema == mapping.source && destination.schema == mapping.destination,
        "Explicit namespace mapping must match both snapshot namespaces"
    );
    validate_name(&mapping.source)?;
    validate_name(&mapping.destination)?;
    let engine = source.engine;
    let mut result = LiveComparison {
        plan: DdlPlan::default(),
        objects: vec![],
        warnings: source
            .warnings
            .iter()
            .chain(&destination.warnings)
            .cloned()
            .collect(),
        blockers: vec![],
        original_destination: destination.clone(),
    };
    let parse_settings: &[&str] = if engine == Engine::Postgres {
        &[
            "date_style",
            "interval_style",
            "standard_conforming_strings",
            "encoding",
        ]
    } else {
        &["sql_mode", "encoding", "lower_case_table_names"]
    };
    for setting in parse_settings {
        if source.origin[*setting] != destination.origin[*setting] {
            result.blockers.push(format!("Source and destination {setting} differ; deparsed expressions need an explicit semantic conversion policy"));
        }
    }
    for (label, snapshot) in [("source", source), ("destination", destination)] {
        result
            .blockers
            .extend(snapshot.unsupported.iter().map(|s| format!("{label}: {s}")));
        let mut seen = BTreeSet::new();
        for t in &snapshot.tables {
            ensure!(seen.insert(&t.name), "Duplicate table in {label} snapshot");
            result.blockers.extend(
                t.unsupported
                    .iter()
                    .map(|s| format!("{label} {}: {s}", t.name)),
            );
            if t.definition.is_none() {
                result.blockers.push(format!(
                    "{label} {} has an incomplete typed definition",
                    t.name
                ));
            }
            if let Some(d) = &t.definition {
                ensure!(
                    d.table.name == t.name && d.table.schema == snapshot.schema,
                    "Table identity does not match its snapshot namespace/name"
                );
                if engine == Engine::MySql {
                    ensure!(
                        t.mysql_options.is_some()
                            && t.mysql_columns.len() == d.columns.len()
                            && t.mysql_columns
                                .iter()
                                .zip(&d.columns)
                                .all(|(a, b)| &a.column == b),
                        "MySQL snapshot has inconsistent or incomplete rich column metadata"
                    );
                }
            }
        }
    }
    let mut desired = BTreeMap::new();
    for table in &source.tables {
        let mut t = table.clone();
        if let Some(d) = &mut t.definition {
            d.table.schema = mapping.destination.clone();
            for fk in &mut d.foreign_keys {
                if fk.referenced_table.schema == mapping.source {
                    fk.referenced_table.schema = mapping.destination.clone();
                } else {
                    result.blockers.push(format!(
                        "Foreign key {} needs an explicit external namespace mapping",
                        fk.name
                    ));
                }
            }
            for c in &d.columns {
                if let Some(DefaultValue::RawExpression(e)) = &c.default
                    && requires_mapping(engine, e, &mapping.source)
                {
                    result.blockers.push(format!(
                        "Default on {}.{} needs semantic namespace/dependency mapping",
                        t.name, c.name
                    ));
                }
            }
            for c in &t.checks {
                if requires_mapping(engine, &c.expression, &mapping.source) {
                    result.blockers.push(format!(
                        "Check {} needs semantic namespace/dependency mapping",
                        c.name
                    ));
                }
            }
        }
        desired.insert(t.name.clone(), t);
    }
    let old: BTreeMap<_, _> = destination
        .tables
        .iter()
        .map(|t| (t.name.clone(), t.clone()))
        .collect();
    if !options.drop_destination_only {
        for (name, current) in &old {
            let Some(wanted) = desired.get_mut(name) else {
                desired.insert(name.clone(), current.clone());
                difference(
                    &mut result.objects,
                    name,
                    DifferenceKind::Table,
                    name,
                    ChangeKind::Preserve,
                    "Destination-only table retained (drop_destination_only=false)",
                );
                continue;
            };
            if let (Some(a), Some(b)) = (&current.definition, &mut wanted.definition) {
                let mut retained = Vec::new();
                for c in &a.columns {
                    if !b.columns.iter().any(|n| n.name == c.name) {
                        retained.push((DifferenceKind::Column, c.name.clone()));
                    }
                }
                for c in &a.indexes {
                    if !b.indexes.iter().any(|n| n.name == c.name) {
                        retained.push((DifferenceKind::Index, c.name.clone()));
                    }
                }
                for c in &a.foreign_keys {
                    if !b.foreign_keys.iter().any(|n| n.name == c.name) {
                        retained.push((DifferenceKind::ForeignKey, c.name.clone()));
                    }
                }
                for c in &current.uniques {
                    if !wanted.uniques.iter().any(|n| n.name == c.name) {
                        retained.push((DifferenceKind::Unique, c.name.clone()));
                    }
                }
                for c in &current.checks {
                    if !wanted.checks.iter().any(|n| n.name == c.name) {
                        retained.push((DifferenceKind::Check, c.name.clone()));
                    }
                }
                named(&mut b.columns, &a.columns, |c| &c.name);
                named(&mut b.indexes, &a.indexes, |c| &c.name);
                named(&mut b.foreign_keys, &a.foreign_keys, |c| &c.name);
                named(&mut wanted.mysql_columns, &current.mysql_columns, |c| {
                    &c.column.name
                });
                named(&mut wanted.uniques, &current.uniques, |c| &c.name);
                named(&mut wanted.checks, &current.checks, |c| &c.name);
                if b.primary_key.is_none()
                    && let Some(primary) = &a.primary_key
                {
                    b.primary_key = Some(primary.clone());
                    retained.push((DifferenceKind::PrimaryKey, primary.name.clone()));
                }
                // Preserve original positions of retained destination-only columns;
                // append new source columns. Existing source relative order is checked.
                let source_columns = &source
                    .tables
                    .iter()
                    .find(|t| &t.name == name)
                    .unwrap()
                    .definition
                    .as_ref()
                    .unwrap()
                    .columns;
                let source_positions: Vec<_> =
                    source_columns.iter().map(|c| c.name.clone()).collect();
                let actual_positions: Vec<_> = a
                    .columns
                    .iter()
                    .filter(|c| source_positions.contains(&c.name))
                    .map(|c| c.name.clone())
                    .chain(
                        source_columns
                            .iter()
                            .filter(|c| !a.columns.iter().any(|o| o.name == c.name))
                            .map(|c| c.name.clone()),
                    )
                    .collect();
                if source_positions != actual_positions {
                    result.blockers.push(format!(
                        "Table {name} requires reordering/inserting source columns in the middle"
                    ));
                }
                let mut columns = a
                    .columns
                    .iter()
                    .map(|c| b.columns.iter().find(|n| n.name == c.name).unwrap().clone())
                    .collect::<Vec<_>>();
                columns.extend(
                    b.columns
                        .iter()
                        .filter(|c| !a.columns.iter().any(|o| o.name == c.name))
                        .cloned(),
                );
                b.columns = columns;
                wanted
                    .mysql_columns
                    .sort_by_key(|c| b.columns.iter().position(|n| n.name == c.column.name));
                for (kind, n) in retained {
                    difference(
                        &mut result.objects,
                        name,
                        kind,
                        &n,
                        ChangeKind::Preserve,
                        "Destination-only object retained; extra constraints may affect semantics",
                    );
                }
            }
        }
    }
    for t in desired.values_mut() {
        if let Some(d) = &mut t.definition {
            d.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            d.foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
        }
        t.uniques.sort_by(|a, b| a.name.cmp(&b.name));
        t.checks.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let mut schema_relations = BTreeSet::new();
    let mut mysql_foreign_keys = BTreeSet::new();
    let mut mysql_checks = BTreeSet::new();
    if engine == Engine::Postgres {
        for t in desired.values() {
            schema_relations.insert(t.name.clone());
        }
    }
    for (name, t) in &desired {
        let Some(d) = &t.definition else {
            difference(
                &mut result.objects,
                name,
                DifferenceKind::Table,
                name,
                ChangeKind::Blocked,
                "Incomplete table inspection",
            );
            continue;
        };
        if let Err(error) = validate_design(engine, d) {
            result
                .blockers
                .push(format!("Desired table {name} is invalid: {error}"));
        }
        let mut constraint_names = BTreeSet::new();
        for constraint in d
            .primary_key
            .iter()
            .map(|p| &p.name)
            .chain(d.foreign_keys.iter().map(|f| &f.name))
            .chain(t.uniques.iter().map(|u| &u.name))
            .chain(t.checks.iter().map(|c| &c.name))
        {
            validate_name(constraint)?;
            let key = if engine == Engine::MySql {
                constraint.to_lowercase()
            } else {
                constraint.clone()
            };
            if !constraint_names.insert(key) {
                result.blockers.push(format!(
                    "Constraint name {constraint} collides in destination table {name}"
                ));
            }
        }
        for unique in &t.uniques {
            if unique.columns.is_empty()
                || unique
                    .columns
                    .iter()
                    .any(|n| !d.columns.iter().any(|c| &c.name == n))
            {
                result.blockers.push(format!(
                    "Unique constraint {} has missing columns",
                    unique.name
                ));
            }
        }
        if engine == Engine::MySql {
            for foreign_key in &d.foreign_keys {
                if !mysql_foreign_keys.insert(foreign_key.name.to_lowercase()) {
                    result.blockers.push(format!(
                        "Destination schema-wide MySQL foreign key name {} collides",
                        foreign_key.name
                    ));
                }
            }
            for check in &t.checks {
                if !mysql_checks.insert(check.name.to_lowercase()) {
                    result.blockers.push(format!(
                        "Destination schema-wide MySQL check name {} collides",
                        check.name
                    ));
                }
            }
            let mut columns = BTreeSet::new();
            for c in &d.columns {
                if !columns.insert(c.name.to_lowercase()) {
                    result
                        .blockers
                        .push(format!("Case-insensitive MySQL column collision in {name}"));
                }
            }
            let mut indexes = BTreeSet::new();
            for n in d
                .indexes
                .iter()
                .map(|i| &i.name)
                .chain(t.uniques.iter().map(|u| &u.name))
            {
                if !indexes.insert(n.to_lowercase()) {
                    result
                        .blockers
                        .push(format!("Case-insensitive MySQL index collision in {name}"));
                }
            }
        }
        if engine == Engine::Postgres {
            for idx in d
                .indexes
                .iter()
                .map(|i| &i.name)
                .chain(d.primary_key.iter().map(|p| &p.name))
                .chain(t.uniques.iter().map(|u| &u.name))
            {
                if !schema_relations.insert(idx.clone()) {
                    result
                        .blockers
                        .push(format!("Destination relation/index name {idx} collides"));
                }
            }
        }
        for fk in &d.foreign_keys {
            let parent = desired
                .get(&fk.referenced_table.name)
                .and_then(|t| t.definition.as_ref());
            if fk.referenced_table.schema != mapping.destination
                || parent.is_none_or(|p| {
                    fk.referenced_columns
                        .iter()
                        .any(|n| !p.columns.iter().any(|c| &c.name == n))
                })
            {
                result.blockers.push(format!(
                    "Foreign key {} has an unresolved desired target",
                    fk.name
                ));
            }
        }
        match old.get(name) {
            None => {
                difference(
                    &mut result.objects,
                    name,
                    DifferenceKind::Table,
                    name,
                    ChangeKind::Create,
                    "Create source table in explicitly mapped destination namespace",
                );
                if engine == Engine::Postgres
                    && t.catalog
                        .get("relation")
                        .and_then(|v| v.first())
                        .and_then(|v| v["owner"].as_str())
                        != destination.origin["user"].as_str()
                {
                    result.blockers.push(format!(
                        "New table {name} would require explicit owner mapping"
                    ));
                }
            }
            Some(current) => {
                difference(
                    &mut result.objects,
                    name,
                    DifferenceKind::Table,
                    name,
                    if equivalent(current, t) {
                        ChangeKind::Unchanged
                    } else {
                        ChangeKind::Alter
                    },
                    "Supported table definition comparison",
                );
                details(&mut result.objects, current, t, engine);
                if current.mysql_options != t.mysql_options {
                    result.blockers.push(format!(
                        "Table {name} options differ; table-option alteration is not supported"
                    ));
                }
                if let Some(a) = &current.definition {
                    let actual_order = a
                        .columns
                        .iter()
                        .filter(|c| d.columns.iter().any(|n| n.name == c.name))
                        .map(|c| c.name.clone())
                        .chain(
                            d.columns
                                .iter()
                                .filter(|c| !a.columns.iter().any(|o| o.name == c.name))
                                .map(|c| c.name.clone()),
                        )
                        .collect::<Vec<_>>();
                    if actual_order != d.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
                    {
                        result.blockers.push(format!("Table {name} requires inserting/reordering columns in the middle; rebuild migration is not generated"));
                    }
                    if engine == Engine::MySql {
                        for auto in current
                            .mysql_columns
                            .iter()
                            .chain(&t.mysql_columns)
                            .filter(|c| c.auto_increment)
                        {
                            let col = &auto.column.name;
                            let stable_key = a.primary_key.as_ref().is_some_and(|p| {
                                Some(p) == d.primary_key.as_ref() && p.columns.first() == Some(col)
                            }) || current
                                .uniques
                                .iter()
                                .any(|u| t.uniques.contains(u) && u.columns.first() == Some(col))
                                || a.indexes.iter().any(|i| {
                                    d.indexes.contains(i) && i.columns.first() == Some(col)
                                });
                            if !stable_key || !a.columns.iter().any(|c| &c.name == col) {
                                result.blockers.push(format!("Table {name} changes/adds the support key for AUTO_INCREMENT column {col}; an atomic engine-specific ALTER is required"));
                            }
                        }
                    }
                }
            }
        }
    }
    for name in old.keys() {
        if !desired.contains_key(name) {
            difference(
                &mut result.objects,
                name,
                DifferenceKind::Table,
                name,
                ChangeKind::Drop,
                "Destination-only table/data deletion explicitly enabled",
            );
        }
    }
    if result
        .objects
        .iter()
        .any(|o| o.change == ChangeKind::Preserve)
    {
        result.warnings.push("Destination-only objects were preserved; this is an additive synchronization, not an assertion that both full schemas are identical.".into());
    }
    if result.blockers.is_empty() {
        match build_plan(engine, &old, &desired) {
            Ok(plan) => result.plan = plan,
            Err(e) => result.blockers.push(format!(
                "Cannot generate a faithful supported DDL plan: {e:#}"
            )),
        };
    }
    if !result.blockers.is_empty() {
        result.plan = DdlPlan::default();
        result.plan.warnings.push(
            "BLOCKED: unsupported or ambiguous metadata; no executable SQL was generated.".into(),
        );
    }
    result.blockers.sort();
    result.blockers.dedup();
    result.warnings.sort();
    result.warnings.dedup();
    result.plan.warnings.extend(result.warnings.clone());
    Ok(result)
}

fn requires_mapping(engine: Engine, expression: &str, schema: &str) -> bool {
    let lower = expression.to_ascii_lowercase();
    expression.contains(&format!("{schema}."))
        || expression.contains(&format!("{}.", quote_ident(engine, schema)))
        || [
            "nextval",
            "currval",
            "setval",
            "regclass",
            "regtype",
            "to_reg",
            "current_schema",
            "search_path",
        ]
        .iter()
        .any(|s| lower.contains(s))
}
fn append(plan: &mut DdlPlan, engine: Engine, ops: Vec<DdlOperation>) -> Result<()> {
    let next = preview(engine, &ops)?;
    plan.statements.extend(next.statements);
    for w in next.warnings {
        if !plan.warnings.contains(&w) {
            plan.warnings.push(w);
        }
    }
    Ok(())
}

fn build_plan(
    engine: Engine,
    old: &BTreeMap<String, TableSnapshot>,
    desired: &BTreeMap<String, TableSnapshot>,
) -> Result<DdlPlan> {
    let mut plan = DdlPlan::default();
    if old.len() == desired.len()
        && old
            .iter()
            .all(|(n, t)| desired.get(n).is_some_and(|d| equivalent(t, d)))
    {
        return Ok(plan);
    }
    // Drop the complete inspected internal FK graph before altering keys/types,
    // restore only after all referenced tables and uniqueness constraints exist.
    for t in old.values() {
        for f in &t.definition.as_ref().unwrap().foreign_keys {
            append(
                &mut plan,
                engine,
                vec![DdlOperation::DropForeignKey {
                    table: t.definition.as_ref().unwrap().table.clone(),
                    name: f.name.clone(),
                }],
            )?;
        }
    }
    // PostgreSQL index names (including constraint-backed indexes) are
    // schema-wide. Complete every obsolete-object removal before any creates,
    // so transfers/swaps across tables do not collide with a later drop.
    for (name, current) in old {
        let a = current.definition.as_ref().unwrap();
        let table = &a.table;
        let Some(next) = desired.get(name) else {
            append(
                &mut plan,
                engine,
                vec![DdlOperation::DropTable {
                    table: table.clone(),
                    cascade: false,
                }],
            )?;
            continue;
        };
        let b = next.definition.as_ref().unwrap();
        let mut ops = Vec::new();
        for c in &current.checks {
            if !next.checks.contains(c) {
                ops.push(DdlOperation::DropCheck {
                    table: table.clone(),
                    name: c.name.clone(),
                });
            }
        }
        for u in &current.uniques {
            if !next.uniques.contains(u) {
                ops.push(DdlOperation::DropUnique {
                    table: table.clone(),
                    name: u.name.clone(),
                });
            }
        }
        for i in &a.indexes {
            if !b.indexes.contains(i) {
                ops.push(DdlOperation::DropIndex {
                    table: table.clone(),
                    name: i.name.clone(),
                });
            }
        }
        if a.primary_key != b.primary_key
            && let Some(p) = &a.primary_key
        {
            ops.push(DdlOperation::DropPrimaryKey {
                table: table.clone(),
                name: p.name.clone(),
            });
        }
        for c in &a.columns {
            if !b.columns.iter().any(|n| n.name == c.name) {
                ops.push(DdlOperation::DropColumn {
                    table: table.clone(),
                    name: c.name.clone(),
                });
            }
        }
        append(&mut plan, engine, ops)?;
    }
    for (name, current) in old {
        let Some(next) = desired.get(name) else {
            continue;
        };
        let a = current.definition.as_ref().unwrap();
        let b = next.definition.as_ref().unwrap();
        let table = &a.table;
        for c in &b.columns {
            let before = a.columns.iter().find(|o| o.name == c.name);
            if engine == Engine::MySql {
                let full = next
                    .mysql_columns
                    .iter()
                    .find(|f| f.column.name == c.name)
                    .context("Missing rich MySQL definition")?;
                if current
                    .mysql_columns
                    .iter()
                    .find(|f| f.column.name == c.name)
                    != Some(full)
                {
                    plan.push(
                        format!(
                            "ALTER TABLE {} {} COLUMN {}",
                            qualified_table(engine, table),
                            if before.is_some() { "MODIFY" } else { "ADD" },
                            mysql_column_sql(full)?
                        ),
                        before.is_some(),
                        "Preserve the complete inspected MySQL column definition",
                    );
                }
            } else {
                match before {
                    None => append(
                        &mut plan,
                        engine,
                        vec![DdlOperation::AddColumn {
                            table: table.clone(),
                            column: c.clone(),
                        }],
                    )?,
                    Some(old) => {
                        let mut ops = Vec::new();
                        // Existing defaults can prevent an otherwise valid type
                        // change; explicitly remove and restore the desired one.
                        if old.data_type != c.data_type {
                            if old.default.is_some() {
                                ops.push(DdlOperation::SetColumnDefault {
                                    table: table.clone(),
                                    name: c.name.clone(),
                                    default: None,
                                });
                            }
                            ops.push(DdlOperation::SetColumnType {
                                table: table.clone(),
                                name: c.name.clone(),
                                data_type: c.data_type.clone(),
                                using: None,
                            });
                        }
                        if old.nullable != c.nullable {
                            ops.push(DdlOperation::SetColumnNullable {
                                table: table.clone(),
                                name: c.name.clone(),
                                nullable: c.nullable,
                            });
                        }
                        if old.default != c.default || old.data_type != c.data_type {
                            ops.push(DdlOperation::SetColumnDefault {
                                table: table.clone(),
                                name: c.name.clone(),
                                default: c.default.clone(),
                            });
                        }
                        append(&mut plan, engine, ops)?;
                    }
                }
            }
        }
        let mut ops = Vec::new();
        if a.primary_key != b.primary_key
            && let Some(p) = &b.primary_key
        {
            ops.push(DdlOperation::AddPrimaryKey {
                table: table.clone(),
                primary_key: p.clone(),
            });
        }
        for u in &next.uniques {
            if !current.uniques.contains(u) {
                ops.push(DdlOperation::AddUnique {
                    table: table.clone(),
                    unique: u.clone(),
                });
            }
        }
        for c in &next.checks {
            if !current.checks.contains(c) {
                ops.push(DdlOperation::AddCheck {
                    table: table.clone(),
                    check: c.clone(),
                });
            }
        }
        for i in &b.indexes {
            if !a.indexes.contains(i) {
                ops.push(DdlOperation::CreateIndex {
                    table: table.clone(),
                    index: i.clone(),
                });
            }
        }
        append(&mut plan, engine, ops)?;
    }
    for (name, t) in desired {
        if old.contains_key(name) {
            continue;
        }
        let mut d = t.definition.clone().unwrap();
        d.foreign_keys.clear();
        if engine == Engine::Postgres {
            append(
                &mut plan,
                engine,
                vec![DdlOperation::CreateTable(d.clone())],
            )?;
        } else {
            let mut columns = t
                .mysql_columns
                .iter()
                .map(mysql_column_sql)
                .collect::<Result<Vec<_>>>()?;
            if let Some(pk) = &d.primary_key {
                columns.push(format!(
                    "PRIMARY KEY ({})",
                    identifiers(engine, &pk.columns)
                ));
            }
            // InnoDB requires an AUTO_INCREMENT column's support key in the
            // same CREATE TABLE, even when it is a secondary UNIQUE/nonunique key.
            for unique in &t.uniques {
                columns.push(format!(
                    "CONSTRAINT {} UNIQUE ({})",
                    quote_ident(engine, &unique.name),
                    identifiers(engine, &unique.columns)
                ));
            }
            for index in &d.indexes {
                columns.push(format!(
                    "{}KEY {} ({})",
                    if index.unique { "UNIQUE " } else { "" },
                    quote_ident(engine, &index.name),
                    identifiers(engine, &index.columns)
                ));
            }
            let options = t
                .mysql_options
                .as_ref()
                .context("Missing MySQL table options")?;
            plan.push(format!("CREATE TABLE {} (\n {}\n) ENGINE=InnoDB DEFAULT CHARACTER SET {} COLLATE {} COMMENT '{}'",qualified_table(engine,&d.table),columns.join(",\n "),quote_ident(engine,&options.character_set),quote_ident(engine,&options.collation),options.comment.replace('\'',"''")),false,"Create fully inspected MySQL table without guessing omitted attributes");
        }
        let mut ops = t
            .uniques
            .iter()
            .filter(|_| engine == Engine::Postgres)
            .map(|u| DdlOperation::AddUnique {
                table: d.table.clone(),
                unique: u.clone(),
            })
            .collect::<Vec<_>>();
        ops.extend(t.checks.iter().map(|c| DdlOperation::AddCheck {
            table: d.table.clone(),
            check: c.clone(),
        }));
        append(&mut plan, engine, ops)?;
    }
    for t in desired.values() {
        let d = t.definition.as_ref().unwrap();
        append(
            &mut plan,
            engine,
            d.foreign_keys
                .iter()
                .map(|f| DdlOperation::AddForeignKey {
                    table: d.table.clone(),
                    foreign_key: f.clone(),
                })
                .collect(),
        )?;
    }
    if engine == Engine::MySql
        && !plan.is_empty()
        && !plan
            .warnings
            .iter()
            .any(|s| s.contains("implicitly commit"))
    {
        plan.warnings.push("MySQL DDL implicitly commits; a failed multi-statement apply can leave partial changes.".into());
    }
    if !plan.is_empty() {
        plan.warnings.push("Review data-dependent conversions, key validation and locking costs. This plan is not applied automatically; recheck the original destination snapshot immediately before an explicit apply.".into());
    }
    Ok(plan)
}
fn identifiers(engine: Engine, names: &[String]) -> String {
    names
        .iter()
        .map(|n| quote_ident(engine, n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Ordinary catalog defaults must stay literals, not become new CONVERT default
/// expressions. This renderer rejects string-mode ambiguity instead of rewriting.
fn mysql_column_sql(full: &MySqlColumnDefinition) -> Result<String> {
    match &full.column.default {
        Some(DefaultValue::Literal(Cell::Text(value))) => {
            ensure!(
                !value.contains('\\') && !value.contains('\0'),
                "Literal default needs explicit SQL-mode handling"
            );
            let mut column = full.clone();
            column.column.default = None;
            Ok(format!(
                "{} DEFAULT '{}'",
                column.sql()?,
                value.replace('\'', "''")
            ))
        }
        Some(DefaultValue::Literal(Cell::Binary(_))) => {
            bail!("Binary defaults are not represented losslessly by this inspector")
        }
        _ => full.sql(),
    }
}
