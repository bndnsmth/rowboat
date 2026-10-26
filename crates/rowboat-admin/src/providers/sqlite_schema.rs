//! Native, reviewed SQLite schema changes. Never reconstruct existing tables.
use crate::{
    ddl::*,
    providers::ddl::{column_sql, foreign_sql, names, primary_sql},
};
use anyhow::{Result, bail, ensure};
use rowboat_core::{
    AdministrativeOutcome, Cell, Database, Engine, QueryResult, QuerySession, TableRef,
    sql::{literal, qualified_table, quote_ident},
};

const ENGINE: Engine = Engine::Sqlite;
const LIMIT: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CreateTable,
    RenameTable,
    DropTable,
    AddColumn,
    RenameColumn,
    DropColumn,
    CreateIndex,
    DropIndex,
    AlterColumn,
    RenameIndex,
    AddConstraint,
    DropConstraint,
}
pub fn supported_action(action: Action) -> bool {
    matches!(
        action,
        Action::CreateTable
            | Action::RenameTable
            | Action::DropTable
            | Action::AddColumn
            | Action::RenameColumn
            | Action::DropColumn
            | Action::CreateIndex
            | Action::DropIndex
    )
}

pub(crate) fn type_sql(ty: &DataType) -> Result<String> {
    // Explicit SQLite declarations. No server-only/custom types leak through a fallback.
    Ok(match ty {
        DataType::SmallInt => "SMALLINT".into(),
        DataType::Integer => "INTEGER".into(),
        DataType::BigInt => "BIGINT".into(),
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Text => "TEXT".into(),
        DataType::Blob => "BLOB".into(),
        DataType::Real => "REAL".into(),
        DataType::Double => "DOUBLE".into(),
        DataType::Date => "DATE".into(),
        DataType::VarChar(n) | DataType::Char(n) => {
            ensure!(*n > 0, "Type size must be positive");
            format!(
                "{}({n})",
                if matches!(ty, DataType::VarChar(_)) {
                    "VARCHAR"
                } else {
                    "CHAR"
                }
            )
        }
        DataType::Decimal { precision, scale } => {
            ensure!(
                *precision > 0 && scale <= precision,
                "Invalid numeric precision/scale"
            );
            format!("DECIMAL({precision}, {scale})")
        }
        DataType::Time {
            precision: None,
            timezone: false,
        } => "TIME".into(),
        DataType::Timestamp {
            precision: None,
            timezone: false,
        } => "TIMESTAMP".into(),
        _ => bail!(
            "This type has no supported SQLite declaration; choose INTEGER, REAL, TEXT, BLOB or a supported affinity declaration"
        ),
    })
}

fn ident(name: &str) -> Result<String> {
    validate_name(name)?;
    Ok(quote_ident(ENGINE, name))
}
fn val(value: &str) -> String {
    literal(ENGINE, &Cell::Text(value.into()))
}
fn text(cell: &Cell) -> Result<&str> {
    match cell {
        Cell::Text(s) => Ok(s),
        _ => bail!("Expected textual SQLite catalog value"),
    }
}
fn target(op: &DdlOperation) -> &TableRef {
    use DdlOperation::*;
    match op {
        CreateTable(t) => &t.table,
        AddCheck { table, .. }
        | DropCheck { table, .. }
        | AddUnique { table, .. }
        | DropUnique { table, .. }
        | SetColumnDefault { table, .. }
        | SetColumnNullable { table, .. }
        | SetColumnType { table, .. }
        | ModifyMySqlColumn { table, .. }
        | DropTable { table, .. }
        | RenameTable { table, .. }
        | AddColumn { table, .. }
        | DropColumn { table, .. }
        | RenameColumn { table, .. }
        | AlterColumn { table, .. }
        | CreateIndex { table, .. }
        | DropIndex { table, .. }
        | RenameIndex { table, .. }
        | AddForeignKey { table, .. }
        | DropForeignKey { table, .. }
        | AddPrimaryKey { table, .. }
        | DropPrimaryKey { table, .. } => table,
    }
}
fn scope(table: &TableRef) -> Result<()> {
    validate_table(table)?;
    ensure!(
        !table.schema.is_empty(),
        "SQLite schema tools require an explicit main/temp/attached namespace"
    );
    ensure!(
        !table.name.to_ascii_lowercase().starts_with("sqlite_"),
        "SQLite internal objects cannot be changed"
    );
    Ok(())
}

pub fn supported_operation(operation: &DdlOperation) -> Result<()> {
    use DdlOperation::*;
    scope(target(operation))?;
    match operation {
        CreateTable(t) => {
            validate_design(ENGINE, t)?;
            for fk in &t.foreign_keys {
                ensure!(
                    fk.referenced_table.schema == t.table.schema,
                    "SQLite foreign keys must reference the same explicit namespace"
                );
            }
        }
        DropTable { cascade, .. } => ensure!(!cascade, "SQLite DROP TABLE CASCADE is unsupported"),
        RenameTable { new_name, .. } => {
            ident(new_name)?;
            ensure!(
                !new_name.to_ascii_lowercase().starts_with("sqlite_"),
                "SQLite internal names are reserved"
            );
        }
        AddColumn { column, .. } => {
            column_sql(ENGINE, column)?;
        }
        RenameColumn { name, new_name, .. } => {
            ident(name)?;
            ident(new_name)?;
        }
        DropColumn { name, .. } | DropIndex { name, .. } => {
            ident(name)?;
        }
        CreateIndex { index, .. } => {
            ident(&index.name)?;
            names(ENGINE, &index.columns)?;
        }
        _ => bail!(
            "SQLite does not support this native ALTER operation; changing types/defaults/nullability/constraints or renaming indexes requires an explicitly designed rebuild, which this tool never performs"
        ),
    }
    Ok(())
}

/// Offline SQL only. Apply must use the opaque review API below.
pub fn preview(operations: &[DdlOperation]) -> Result<DdlPlan> {
    use DdlOperation::*;
    let mut plan = DdlPlan::default();
    let mut iter = operations.iter().peekable();
    while let Some(op) = iter.next() {
        supported_operation(op)?;
        let table = target(op);
        let t = qualified_table(ENGINE, table);
        let alter = format!("ALTER TABLE {t}");
        match op {
            CreateTable(design) => {
                let mut parts = design
                    .columns
                    .iter()
                    .map(|c| column_sql(ENGINE, c))
                    .collect::<Result<Vec<_>>>()?;
                if let Some(pk) = &design.primary_key {
                    parts.push(primary_sql(ENGINE, pk)?);
                }
                for fk in &design.foreign_keys {
                    parts.push(foreign_sql(ENGINE, fk)?);
                }
                // The shared TableDesign predates checks/uniques. Fold immediately
                // following typed additions into CREATE, never ALTER an existing table.
                while let Some(next) = iter.peek() {
                    match next {
                        AddCheck {
                            table: owner,
                            check,
                        } if owner == table => {
                            crate::query::validate_expression(ENGINE, &check.expression)?;
                            parts.push(format!(
                                "CONSTRAINT {} CHECK ({})",
                                ident(&check.name)?,
                                check.expression
                            ));
                        }
                        AddUnique {
                            table: owner,
                            unique,
                        } if owner == table => {
                            ensure!(
                                unique
                                    .columns
                                    .iter()
                                    .all(|name| design.columns.iter().any(|c| &c.name == name)),
                                "Unknown UNIQUE column"
                            );
                            parts.push(format!(
                                "CONSTRAINT {} UNIQUE ({})",
                                ident(&unique.name)?,
                                names(ENGINE, &unique.columns)?
                            ));
                        }
                        _ => break,
                    }
                    iter.next();
                }
                plan.push(
                    format!("CREATE TABLE {t} (\n    {}\n)", parts.join(",\n    ")),
                    false,
                    "Create SQLite table",
                );
                for index in &design.indexes {
                    push_index(&mut plan, table, index)?;
                }
            }
            DropTable { .. } => {
                plan.push(format!("DROP TABLE {t}"), true, "Drop table and its data")
            }
            RenameTable { new_name, .. } => plan.push(
                format!("{alter} RENAME TO {}", ident(new_name)?),
                false,
                "Native rename preserves SQLite attributes and updates dependencies",
            ),
            AddColumn { column, .. } => plan.push(
                format!("{alter} ADD COLUMN {}", column_sql(ENGINE, column)?),
                false,
                "Native ADD COLUMN; SQLite validates defaults and existing rows",
            ),
            RenameColumn { name, new_name, .. } => plan.push(
                format!(
                    "{alter} RENAME COLUMN {} TO {}",
                    ident(name)?,
                    ident(new_name)?
                ),
                false,
                "Native column rename; dependency ambiguity rejects",
            ),
            DropColumn { name, .. } => plan.push(
                format!("{alter} DROP COLUMN {}", ident(name)?),
                true,
                "Native DROP COLUMN; SQLite rejects dependent constraints/indexes/triggers",
            ),
            CreateIndex { index, .. } => push_index(&mut plan, table, index)?,
            DropIndex { name, .. } => plan.push(
                format!("DROP INDEX {}.{}", ident(&table.schema)?, ident(name)?),
                true,
                "Drop index; uniqueness enforcement may be removed",
            ),
            _ => unreachable!("supported operation checked"),
        }
    }
    if !plan.is_empty() {
        plan.warnings.push("SQLite native DDL only; existing tables are never reconstructed. SQLite affinity declarations do not enforce server-style sizes or precision. Dependencies and native version requirements are rechecked on Apply.".into());
    }
    Ok(plan)
}
fn push_index(plan: &mut DdlPlan, table: &TableRef, index: &IndexDesign) -> Result<()> {
    plan.push(
        format!(
            "CREATE {}INDEX {}.{} ON {} ({})",
            if index.unique { "UNIQUE " } else { "" },
            ident(&table.schema)?,
            ident(&index.name)?,
            ident(&table.name)?,
            names(ENGINE, &index.columns)?
        ),
        false,
        "Create SQLite index",
    );
    Ok(())
}

async fn query(db: &impl QuerySession, sql: &str) -> Result<Vec<QueryResult>> {
    let results = db.query(sql, LIMIT).await?;
    ensure!(
        results.len() == 1 && !results[0].truncated,
        "SQLite schema inspection exceeded bounds; no partial review is allowed"
    );
    Ok(results)
}
async fn rows(db: &impl QuerySession, sql: &str) -> Result<Vec<Vec<Cell>>> {
    Ok(query(db, sql).await?.remove(0).rows)
}

/// Native catalog display, including generated columns and full index terms.
pub async fn inspect(db: &impl QuerySession, table: &TableRef) -> Result<Vec<QueryResult>> {
    ensure!(
        db.engine() == ENGINE,
        "SQLite schema inspection requires SQLite"
    );
    validate_table(table)?;
    ensure!(
        !table.schema.is_empty(),
        "SQLite inspection requires an explicit namespace"
    );
    let schema = ident(&table.schema)?;
    let name = val(&table.name);
    let namespace = val(&table.schema);
    let mut results = Vec::new();
    for sql in [
        format!(
            "SELECT type,name,tbl_name,rootpage,sql FROM {schema}.sqlite_schema WHERE name={name} COLLATE NOCASE OR tbl_name={name} COLLATE NOCASE ORDER BY type,name"
        ),
        format!(
            "SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo({name},{namespace}) ORDER BY cid"
        ),
        format!(
            "SELECT seq,name,\"unique\",origin,partial FROM pragma_index_list({name},{namespace}) ORDER BY seq"
        ),
        format!(
            "SELECT il.name AS index_name,ix.seqno,ix.cid,ix.name,ix.desc,ix.coll,ix.key FROM pragma_index_list({name},{namespace}) il JOIN pragma_index_xinfo(il.name,{namespace}) ix ORDER BY il.name,ix.seqno"
        ),
        format!(
            "SELECT id,seq,\"table\",\"from\",\"to\",on_update,on_delete,match FROM pragma_foreign_key_list({name},{namespace}) ORDER BY id,seq"
        ),
    ] {
        results.extend(query(db, &sql).await?);
    }
    ensure!(!results[0].rows.is_empty(), "SQLite object does not exist");
    Ok(results)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    identity: Vec<Cell>,
    catalog: Vec<Vec<Cell>>,
    temp: Vec<Vec<Cell>>,
}
#[derive(Debug, Clone)]
pub struct Review {
    plan: DdlPlan,
    schema: String,
    snapshot: Snapshot,
    operations: Vec<DdlOperation>,
}
impl Review {
    pub fn plan(&self) -> &DdlPlan {
        &self.plan
    }
}

async fn snapshot(db: &impl QuerySession, schema: &str) -> Result<Snapshot> {
    let identity = db.sqlite_schema_identity(schema).await?;
    let catalog = rows(
        db,
        &format!(
            "SELECT type,name,tbl_name,rootpage,sql FROM {}.sqlite_schema ORDER BY type,name",
            ident(schema)?
        ),
    )
    .await?;
    let temp = rows(
        db,
        "SELECT type,name,tbl_name,rootpage,sql FROM temp.sqlite_schema ORDER BY type,name",
    )
    .await?;
    ensure!(
        identity == db.sqlite_schema_identity(schema).await?,
        "SQLite namespace/schema changed during inspection; review again"
    );
    Ok(Snapshot {
        identity,
        catalog,
        temp,
    })
}

async fn validate_live(db: &impl QuerySession, operations: &[DdlOperation]) -> Result<()> {
    let version = rows(db, "SELECT sqlite_version()").await?;
    let version = text(&version[0][0])?
        .split('.')
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        version.as_slice() >= [3, 37, 0].as_slice(),
        "SQLite schema tools require SQLite 3.37 or newer (native DROP COLUMN and reliable table-kind inspection)"
    );
    let mut created = std::collections::HashSet::new();
    let mut columns: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for op in operations {
        let table = target(op);
        let key = table.name.to_ascii_lowercase();
        if let DdlOperation::CreateTable(design) = op {
            columns.insert(
                key,
                design
                    .columns
                    .iter()
                    .map(|c| c.name.to_ascii_lowercase())
                    .collect(),
            );
            created.insert(table.name.to_ascii_lowercase());
            continue;
        }
        if !columns.contains_key(&key) {
            let native_columns = rows(
                db,
                &format!(
                    "SELECT name FROM pragma_table_xinfo({},{})",
                    val(&table.name),
                    val(&table.schema)
                ),
            )
            .await?;
            columns.insert(
                key.clone(),
                native_columns
                    .iter()
                    .map(|row| text(&row[0]).map(str::to_ascii_lowercase))
                    .collect::<Result<_>>()?,
            );
        }
        let available = columns.get_mut(&key).expect("column snapshot inserted");
        match op {
            DdlOperation::CreateIndex { index, .. } => {
                // SQLite's historical DQS fallback accepts "missing" as a string
                // expression in CREATE INDEX. Typed column names must never turn
                // into such constant indexes. Include earlier native column edits.
                ensure!(
                    index
                        .columns
                        .iter()
                        .all(|name| available.contains(&name.to_ascii_lowercase())),
                    "SQLite index refers to an unknown column; quoted names are never treated as string expressions"
                );
            }
            DdlOperation::AddColumn { column, .. } => {
                available.insert(column.name.to_ascii_lowercase());
            }
            DdlOperation::DropColumn { name, .. } => {
                available.remove(&name.to_ascii_lowercase());
            }
            DdlOperation::RenameColumn { name, new_name, .. } => {
                ensure!(
                    available.remove(&name.to_ascii_lowercase()),
                    "SQLite column to rename does not exist"
                );
                available.insert(new_name.to_ascii_lowercase());
            }
            _ => {}
        }
        if created.contains(&table.name.to_ascii_lowercase()) {
            continue;
        }
        let kind = rows(
            db,
            &format!(
                "SELECT type FROM pragma_table_list WHERE schema={} AND name={} COLLATE NOCASE",
                val(&table.schema),
                val(&table.name)
            ),
        )
        .await?;
        ensure!(
            kind.len() == 1 && text(&kind[0][0])? == "table",
            "SQLite schema mutations require an ordinary table; missing/view/virtual/shadow objects are unsupported"
        );
        if let DdlOperation::DropIndex { name, .. } = op {
            let index = rows(
                db,
                &format!(
                    "SELECT origin FROM pragma_index_list({},{}) WHERE name={} COLLATE NOCASE",
                    val(&table.name),
                    val(&table.schema),
                    val(name)
                ),
            )
            .await?;
            ensure!(
                index.len() == 1 && text(&index[0][0])? == "c",
                "Only an explicit index belonging to the reviewed table can be dropped; constraint/internal indexes are unsupported"
            );
        }
        if matches!(
            op,
            DdlOperation::DropTable { .. } | DdlOperation::DropIndex { .. }
        ) {
            // DROP TABLE can implicitly DELETE and cascade child rows. Reject all
            // inbound FKs, including empty children, regardless of foreign_keys.
            let inbound = rows(db,&format!("SELECT t.name,f.id FROM pragma_table_list t JOIN pragma_foreign_key_list(t.name,t.schema) f WHERE t.schema={} AND f.\"table\"={} COLLATE NOCASE",val(&table.schema),val(&table.name))).await?;
            ensure!(
                inbound.is_empty(),
                "SQLite drop rejected: dependent foreign keys could lose data or enforcement; remove dependencies explicitly first"
            );
        }
        if matches!(op, DdlOperation::DropTable { .. }) {
            for namespace in [table.schema.as_str(), "temp"] {
                let dependencies = rows(db,&format!("SELECT name FROM {}.sqlite_schema WHERE type='view' OR (type='trigger' AND tbl_name!={} COLLATE NOCASE)",ident(namespace)?,val(&table.name))).await?;
                ensure!(
                    dependencies.is_empty(),
                    "SQLite DROP TABLE cannot prove view/external-trigger dependencies safe; this namespace contains such objects"
                );
            }
        }
    }
    Ok(())
}

pub async fn prepare(db: &Database, operations: &[DdlOperation]) -> Result<Review> {
    ensure!(db.engine() == ENGINE, "SQLite review requires SQLite");
    ensure!(
        !db.in_transaction().await?,
        "SQLite review requires no caller-owned transaction"
    );
    ensure!(
        !operations.is_empty(),
        "No SQLite schema operations to review"
    );
    let plan = preview(operations)?;
    let schema = target(&operations[0]).schema.clone();
    ensure!(
        operations.iter().all(|op| target(op).schema == schema),
        "One SQLite namespace per review is required for atomic file changes"
    );
    let before = snapshot(db, &schema).await?;
    validate_live(db, operations).await?;
    ensure!(
        before == snapshot(db, &schema).await?,
        "SQLite schema changed during preparation; review again"
    );
    Ok(Review {
        plan,
        schema,
        snapshot: before,
        operations: operations.to_vec(),
    })
}

pub async fn apply(db: &Database, review: &Review) -> Result<AdministrativeOutcome> {
    ensure!(db.engine() == ENGINE, "SQLite review requires SQLite");
    let mut operation = db.administrative_operation(None).await?;
    let result: Result<()> = async {
        ensure!(snapshot(&operation,&review.schema).await? == review.snapshot, "SQLite review is stale or belongs to a different session/file/attachment; prepare again");
        validate_live(&operation,&review.operations).await?;
        for statement in &review.plan.statements { operation.execute(&statement.sql).await?; }
        Ok(())
    }.await;
    match result {
        Ok(()) => operation.finish().await,
        Err(error) => {
            let mut outcome = operation.close().await?;
            outcome.error = Some(format!(
                "{error:#}{}",
                outcome
                    .error
                    .as_ref()
                    .map(|e| format!("; {e}"))
                    .unwrap_or_default()
            ));
            Ok(outcome)
        }
    }
}
