//! PostgreSQL/MySQL typed schema SQL compiler. Shared plans live in ddl.
use super::{Operation, operation_support};
pub use crate::columns::{
    AdvancedMySqlColumnDefinition, GeneratedStorage, MySqlColumnEdit, MySqlColumnSnapshot,
    MySqlGeneration, MySqlSpecialType, inspect_mysql_column, mysql_column_is_current,
    plan_mysql_column_edit,
};
pub use crate::columns::{MySqlColumnDefinition, mysql_column_definition};
use crate::ddl::DdlPlan;
use anyhow::{Result, bail, ensure};
use rowboat_core::{
    Cell, Engine, TableRef,
    sql::{literal, qualified_table, quote_ident},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    SmallInt,
    Integer,
    BigInt,
    Boolean,
    Text,
    VarChar(u32),
    Char(u32),
    Decimal {
        precision: u8,
        scale: u8,
    },
    Real,
    Double,
    Date,
    Time {
        precision: Option<u8>,
        timezone: bool,
    },
    Timestamp {
        precision: Option<u8>,
        timezone: bool,
    },
    Json,
    Jsonb,
    Uuid,
    Bytea,
    Binary(u32),
    VarBinary(u32),
    Blob,
    /// PostgreSQL only; identifiers are quoted, never interpolated as raw SQL.
    Custom(TableRef),
    Array(Box<DataType>),
}

impl DataType {
    pub fn parse(engine: Engine, input: &str) -> Result<Self> {
        operation_support(engine, Operation::SchemaDdl)?;
        let s = input.trim().to_ascii_lowercase();
        if let Some(base) = s.strip_suffix("[]") {
            ensure!(engine == Engine::Postgres, "Arrays require PostgreSQL");
            return Ok(Self::Array(Box::new(Self::parse(engine, base)?)));
        }
        let (base, args, suffix) = if let Some(open) = s.find('(') {
            let close = s
                .find(')')
                .ok_or_else(|| anyhow::anyhow!("Missing closing type parenthesis"))?;
            ensure!(close > open, "Invalid data type");
            let args = s[open + 1..close]
                .split(',')
                .map(|v| v.trim().parse::<u32>())
                .collect::<std::result::Result<Vec<_>, _>>()?;
            (s[..open].trim(), args, s[close + 1..].trim())
        } else {
            (s.as_str(), vec![], "")
        };
        let single = || -> Result<u32> {
            ensure!(
                args.len() == 1 && suffix.is_empty(),
                "Type requires one size"
            );
            Ok(args[0])
        };
        let simple = || -> Result<()> {
            ensure!(
                args.is_empty() && suffix.is_empty(),
                "Unexpected type parameters"
            );
            Ok(())
        };
        let ty = match base {
            "smallint" | "int2" => {
                simple()?;
                Self::SmallInt
            }
            "integer" | "int" | "int4" => {
                simple()?;
                Self::Integer
            }
            "bigint" | "int8" => {
                simple()?;
                Self::BigInt
            }
            "boolean" | "bool" => {
                simple()?;
                Self::Boolean
            }
            "text" => {
                simple()?;
                Self::Text
            }
            "varchar" | "character varying" => Self::VarChar(single()?),
            "char" | "character" => Self::Char(single()?),
            "decimal" | "numeric" => {
                ensure!(
                    (1..=2).contains(&args.len()) && suffix.is_empty(),
                    "Numeric requires precision[, scale]"
                );
                Self::Decimal {
                    precision: args[0].try_into()?,
                    scale: args.get(1).copied().unwrap_or(0).try_into()?,
                }
            }
            "real" | "float4" | "float" => {
                simple()?;
                if base == "float" && engine == Engine::Postgres {
                    Self::Double
                } else {
                    Self::Real
                }
            }
            "double" | "double precision" | "float8" => {
                simple()?;
                Self::Double
            }
            "date" => {
                simple()?;
                Self::Date
            }
            "time"
            | "time with time zone"
            | "time without time zone"
            | "timestamp"
            | "timestamptz"
            | "timestamp with time zone"
            | "timestamp without time zone" => {
                ensure!(
                    args.len() <= 1
                        && matches!(suffix, "" | "with time zone" | "without time zone"),
                    "Invalid time type"
                );
                let precision = args.first().copied().map(u8::try_from).transpose()?;
                let timezone = base == "timestamptz"
                    || base.ends_with("with time zone")
                    || suffix == "with time zone";
                if base.starts_with("time")
                    && !base.starts_with("timestamp")
                    && base != "timestamptz"
                {
                    Self::Time {
                        precision,
                        timezone,
                    }
                } else {
                    Self::Timestamp {
                        precision,
                        timezone,
                    }
                }
            }
            "json" => {
                simple()?;
                Self::Json
            }
            "jsonb" => {
                simple()?;
                Self::Jsonb
            }
            "uuid" => {
                simple()?;
                Self::Uuid
            }
            "bytea" => {
                simple()?;
                Self::Bytea
            }
            "binary" => Self::Binary(single()?),
            "varbinary" => Self::VarBinary(single()?),
            "blob" => {
                simple()?;
                Self::Blob
            }
            _ => bail!(
                "Unsupported data type: {input}; use a typed Custom identifier for PostgreSQL user types"
            ),
        };
        ty.sql(engine)?;
        Ok(ty)
    }

    pub fn sql(&self, engine: Engine) -> Result<String> {
        operation_support(engine, Operation::SchemaDdl)?;
        if engine == Engine::Sqlite {
            return crate::sqlite_schema::type_sql(self);
        }
        Ok(match self {
            Self::SmallInt => "SMALLINT".into(),
            Self::Integer => "INTEGER".into(),
            Self::BigInt => "BIGINT".into(),
            Self::Boolean => "BOOLEAN".into(),
            Self::Text => "TEXT".into(),
            Self::Date => "DATE".into(),
            Self::Real => if engine == Engine::MySql {
                "FLOAT"
            } else {
                "REAL"
            }
            .into(),
            Self::Double => if engine == Engine::Postgres {
                "DOUBLE PRECISION"
            } else {
                "DOUBLE"
            }
            .into(),
            Self::VarChar(n) | Self::Char(n) | Self::Binary(n) | Self::VarBinary(n) => {
                ensure!(*n > 0, "Type size must be positive");
                let name = match self {
                    Self::VarChar(_) => "VARCHAR",
                    Self::Char(_) => "CHAR",
                    Self::Binary(_) => "BINARY",
                    _ => "VARBINARY",
                };
                if matches!(self, Self::Binary(_) | Self::VarBinary(_)) {
                    ensure!(engine == Engine::MySql, "Binary(n) types require MySQL");
                }
                if engine == Engine::MySql {
                    let max = if matches!(self, Self::Char(_) | Self::Binary(_)) {
                        255
                    } else {
                        65535
                    };
                    ensure!(*n <= max, "Type size exceeds MySQL limit");
                }
                format!("{name}({n})")
            }
            Self::Decimal { precision, scale } => {
                let max = if engine == Engine::MySql { 65 } else { 255 };
                ensure!(
                    *precision > 0 && *precision <= max && scale <= precision,
                    "Invalid numeric precision/scale"
                );
                ensure!(
                    engine != Engine::MySql || *scale <= 30,
                    "MySQL scale exceeds 30"
                );
                format!("DECIMAL({precision}, {scale})")
            }
            Self::Time {
                precision,
                timezone,
            }
            | Self::Timestamp {
                precision,
                timezone,
            } => {
                ensure!(
                    precision.is_none_or(|p| p <= 6),
                    "Time precision must be 0..6"
                );
                ensure!(
                    engine == Engine::Postgres || !timezone,
                    "MySQL has no WITH TIME ZONE type"
                );
                let name = if matches!(self, Self::Time { .. }) {
                    "TIME"
                } else {
                    "TIMESTAMP"
                };
                format!(
                    "{name}{}{}",
                    precision.map(|p| format!("({p})")).unwrap_or_default(),
                    if *timezone { " WITH TIME ZONE" } else { "" }
                )
            }
            Self::Json => "JSON".into(),
            Self::Jsonb | Self::Uuid | Self::Bytea => {
                ensure!(
                    engine == Engine::Postgres,
                    "This data type requires PostgreSQL"
                );
                match self {
                    Self::Jsonb => "JSONB",
                    Self::Uuid => "UUID",
                    _ => "BYTEA",
                }
                .into()
            }
            Self::Blob => {
                ensure!(engine == Engine::MySql, "BLOB requires MySQL");
                "BLOB".into()
            }
            Self::Custom(t) => {
                ensure!(
                    engine == Engine::Postgres,
                    "Custom types require PostgreSQL"
                );
                validate_table(t)?;
                qualified_table(engine, t)
            }
            Self::Array(t) => {
                ensure!(engine == Engine::Postgres, "Arrays require PostgreSQL");
                format!("{}[]", t.sql(engine)?)
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefaultValue {
    Literal(Cell),
    /// Explicitly trusted SQL expression, not a bound value. Lexically validated
    /// for balanced delimiters and absence of comments/statement separators.
    RawExpression(String),
}

impl DefaultValue {
    pub fn sql(&self, engine: Engine) -> Result<String> {
        operation_support(engine, Operation::SchemaDdl)?;
        match self {
            Self::Literal(v) => {
                let sql = literal(engine, v);
                // MySQL's safe text rendering is a CONVERT expression; default
                // expressions require parentheses (MySQL 8.0.13+).
                Ok(if engine == Engine::MySql && !matches!(v, Cell::Null) {
                    format!("({sql})")
                } else {
                    sql
                })
            }
            Self::RawExpression(sql) => {
                crate::query::validate_expression(engine, sql)?;
                Ok(format!("({sql})"))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDesign {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub default: Option<DefaultValue>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryKeyDesign {
    pub name: String,
    pub columns: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDesign {
    pub name: String,
    /// Trusted SQL expression; validated with the same lexical rules as raw defaults.
    pub expression: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueDesign {
    pub name: String,
    pub columns: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDesign {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferentialAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}
impl ReferentialAction {
    fn sql(self, engine: Engine) -> Result<&'static str> {
        ensure!(
            engine != Engine::MySql || self != Self::SetDefault,
            "MySQL does not support SET DEFAULT foreign keys"
        );
        Ok(match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyDesign {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_table: TableRef,
    pub referenced_columns: Vec<String>,
    pub on_update: ReferentialAction,
    pub on_delete: ReferentialAction,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDesign {
    pub table: TableRef,
    pub columns: Vec<ColumnDesign>,
    pub primary_key: Option<PrimaryKeyDesign>,
    pub indexes: Vec<IndexDesign>,
    pub foreign_keys: Vec<ForeignKeyDesign>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DdlOperation {
    AddCheck {
        table: TableRef,
        check: CheckDesign,
    },
    DropCheck {
        table: TableRef,
        name: String,
    },
    AddUnique {
        table: TableRef,
        unique: UniqueDesign,
    },
    DropUnique {
        table: TableRef,
        name: String,
    },
    SetColumnDefault {
        table: TableRef,
        name: String,
        default: Option<DefaultValue>,
    },
    SetColumnNullable {
        table: TableRef,
        name: String,
        nullable: bool,
    },
    SetColumnType {
        table: TableRef,
        name: String,
        data_type: DataType,
        using: Option<String>,
    },
    ModifyMySqlColumn {
        table: TableRef,
        definition: MySqlColumnDefinition,
    },
    CreateTable(TableDesign),
    DropTable {
        table: TableRef,
        cascade: bool,
    },
    RenameTable {
        table: TableRef,
        new_name: String,
    },
    AddColumn {
        table: TableRef,
        column: ColumnDesign,
    },
    DropColumn {
        table: TableRef,
        name: String,
    },
    RenameColumn {
        table: TableRef,
        name: String,
        new_name: String,
    },
    /// Replaces type, nullability and default. MySQL requires the complete
    /// desired definition; unsupported attributes must not be silently omitted.
    AlterColumn {
        table: TableRef,
        column: ColumnDesign,
    },
    CreateIndex {
        table: TableRef,
        index: IndexDesign,
    },
    DropIndex {
        table: TableRef,
        name: String,
    },
    RenameIndex {
        table: TableRef,
        name: String,
        new_name: String,
    },
    AddForeignKey {
        table: TableRef,
        foreign_key: ForeignKeyDesign,
    },
    DropForeignKey {
        table: TableRef,
        name: String,
    },
    AddPrimaryKey {
        table: TableRef,
        primary_key: PrimaryKeyDesign,
    },
    DropPrimaryKey {
        table: TableRef,
        name: String,
    },
}
pub(crate) fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.contains('\0'),
        "Identifier must be nonempty and contain no NUL"
    );
    Ok(())
}
pub(crate) fn validate_table(table: &TableRef) -> Result<()> {
    validate_name(&table.name)?;
    if !table.schema.is_empty() {
        validate_name(&table.schema)?;
    }
    Ok(())
}
pub(crate) fn names(engine: Engine, values: &[String]) -> Result<String> {
    ensure!(!values.is_empty(), "At least one column is required");
    let mut seen = HashSet::new();
    for n in values {
        validate_name(n)?;
        ensure!(seen.insert(n), "Duplicate column: {n}");
    }
    Ok(values
        .iter()
        .map(|n| quote_ident(engine, n))
        .collect::<Vec<_>>()
        .join(", "))
}
pub(crate) fn column_sql(engine: Engine, c: &ColumnDesign) -> Result<String> {
    validate_name(&c.name)?;
    Ok(format!(
        "{} {}{}{}",
        quote_ident(engine, &c.name),
        c.data_type.sql(engine)?,
        if c.nullable { " NULL" } else { " NOT NULL" },
        c.default
            .as_ref()
            .map(|d| d.sql(engine).map(|s| format!(" DEFAULT {s}")))
            .transpose()?
            .unwrap_or_default()
    ))
}
pub(crate) fn primary_sql(engine: Engine, p: &PrimaryKeyDesign) -> Result<String> {
    validate_name(&p.name)?;
    Ok(format!(
        "CONSTRAINT {} PRIMARY KEY ({})",
        quote_ident(engine, &p.name),
        names(engine, &p.columns)?
    ))
}
pub(crate) fn foreign_sql(engine: Engine, f: &ForeignKeyDesign) -> Result<String> {
    validate_name(&f.name)?;
    validate_table(&f.referenced_table)?;
    ensure!(
        f.columns.len() == f.referenced_columns.len(),
        "Foreign key column counts differ"
    );
    Ok(format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON UPDATE {} ON DELETE {}",
        quote_ident(engine, &f.name),
        names(engine, &f.columns)?,
        if engine == Engine::Sqlite {
            quote_ident(engine, &f.referenced_table.name)
        } else {
            qualified_table(engine, &f.referenced_table)
        },
        names(engine, &f.referenced_columns)?,
        f.on_update.sql(engine)?,
        f.on_delete.sql(engine)?
    ))
}

pub(crate) fn validate_design(engine: Engine, t: &TableDesign) -> Result<()> {
    validate_table(&t.table)?;
    ensure!(
        !t.columns.is_empty(),
        "A table must have at least one column"
    );
    let mut columns = HashSet::new();
    for c in &t.columns {
        column_sql(engine, c)?;
        ensure!(
            columns.insert(c.name.as_str()),
            "Duplicate column: {}",
            c.name
        );
    }
    let check_columns = |v: &[String]| -> Result<()> {
        names(engine, v)?;
        for c in v {
            ensure!(columns.contains(c.as_str()), "Unknown column: {c}");
        }
        Ok(())
    };
    let mut constraints = HashSet::new();
    if let Some(p) = &t.primary_key {
        primary_sql(engine, p)?;
        check_columns(&p.columns)?;
        constraints.insert(p.name.as_str());
        for c in &t.columns {
            ensure!(
                !p.columns.contains(&c.name) || !c.nullable,
                "Primary key column {} must be NOT NULL",
                c.name
            );
        }
    }
    let mut indexes = HashSet::new();
    for index in &t.indexes {
        validate_name(&index.name)?;
        ensure!(
            indexes.insert(&index.name),
            "Duplicate index: {}",
            index.name
        );
        check_columns(&index.columns)?;
    }
    for f in &t.foreign_keys {
        foreign_sql(engine, f)?;
        check_columns(&f.columns)?;
        ensure!(
            constraints.insert(&f.name),
            "Duplicate constraint: {}",
            f.name
        );
        if f.on_delete == ReferentialAction::SetNull || f.on_update == ReferentialAction::SetNull {
            for c in &t.columns {
                ensure!(
                    !f.columns.contains(&c.name) || c.nullable,
                    "SET NULL requires nullable column {}",
                    c.name
                );
            }
        }
    }
    Ok(())
}

pub fn preview(engine: Engine, operations: &[DdlOperation]) -> Result<DdlPlan> {
    operation_support(engine, Operation::SchemaDdl)?;
    if engine == Engine::Sqlite {
        return crate::sqlite_schema::preview(operations);
    }
    let mut plan = DdlPlan::default();
    for op in operations {
        use DdlOperation::*;
        if let CreateTable(t) = op {
            validate_design(engine, t)?;
            let mut parts = t
                .columns
                .iter()
                .map(|c| column_sql(engine, c))
                .collect::<Result<Vec<_>>>()?;
            if let Some(p) = &t.primary_key {
                parts.push(primary_sql(engine, p)?);
            }
            for f in &t.foreign_keys {
                parts.push(foreign_sql(engine, f)?);
            }
            plan.push(
                format!(
                    "CREATE TABLE {} (\n    {}\n)",
                    qualified_table(engine, &t.table),
                    parts.join(",\n    ")
                ),
                false,
                "Create table",
            );
            for index in &t.indexes {
                plan.statements.extend(
                    preview(
                        engine,
                        &[CreateIndex {
                            table: t.table.clone(),
                            index: index.clone(),
                        }],
                    )?
                    .statements,
                );
            }
            continue;
        }
        let table = match op {
            AddCheck { table, .. }
            | DropCheck { table, .. }
            | AddUnique { table, .. }
            | DropUnique { table, .. }
            | SetColumnDefault { table, .. }
            | SetColumnNullable { table, .. }
            | SetColumnType { table, .. }
            | ModifyMySqlColumn { table, .. } => table,
            DropTable { table, .. }
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
            CreateTable(_) => unreachable!(),
        };
        validate_table(table)?;
        let t = qualified_table(engine, table);
        let alter = format!("ALTER TABLE {t}");
        let ident = |n: &str| -> Result<String> {
            validate_name(n)?;
            Ok(quote_ident(engine, n))
        };
        match op {
            AddCheck { check, .. } => {
                crate::query::validate_expression(engine, &check.expression)?;
                plan.push(
                    format!(
                        "{alter} ADD CONSTRAINT {} CHECK ({})",
                        ident(&check.name)?,
                        check.expression
                    ),
                    false,
                    "Add CHECK constraint; existing rows must satisfy it",
                );
                if engine == Engine::MySql {
                    plan.warnings
                        .push("CHECK enforcement requires MySQL 8.0.16 or newer.".into());
                }
            }
            DropCheck { name, .. } => plan.push(
                format!(
                    "{alter} DROP {} {}",
                    if engine == Engine::MySql {
                        "CHECK"
                    } else {
                        "CONSTRAINT"
                    },
                    ident(name)?
                ),
                true,
                "Remove CHECK enforcement",
            ),
            AddUnique { unique, .. } => plan.push(
                format!(
                    "{alter} ADD CONSTRAINT {} UNIQUE ({})",
                    ident(&unique.name)?,
                    names(engine, &unique.columns)?
                ),
                false,
                "Add UNIQUE constraint; existing rows must be unique",
            ),
            DropUnique { name, .. } => plan.push(
                format!(
                    "{alter} DROP {} {}",
                    if engine == Engine::MySql {
                        "INDEX"
                    } else {
                        "CONSTRAINT"
                    },
                    ident(name)?
                ),
                true,
                "Remove UNIQUE enforcement",
            ),
            SetColumnDefault { name, default, .. } => plan.push(
                format!(
                    "{alter} ALTER COLUMN {} {}",
                    ident(name)?,
                    if let Some(value) = default {
                        format!("SET DEFAULT {}", value.sql(engine)?)
                    } else {
                        "DROP DEFAULT".into()
                    }
                ),
                false,
                "Change only the column default",
            ),
            SetColumnNullable { name, nullable, .. } => {
                ensure!(
                    engine == Engine::Postgres,
                    "MySQL nullability changes require mysql_column_definition + ModifyMySqlColumn to preserve existing attributes"
                );
                plan.push(
                    format!(
                        "{alter} ALTER COLUMN {} {} NOT NULL",
                        ident(name)?,
                        if *nullable { "DROP" } else { "SET" }
                    ),
                    false,
                    "Change only column nullability",
                );
            }
            SetColumnType {
                name,
                data_type,
                using,
                ..
            } => {
                ensure!(
                    engine == Engine::Postgres,
                    "MySQL type changes require mysql_column_definition + ModifyMySqlColumn to preserve existing attributes"
                );
                if let Some(expression) = using {
                    crate::query::validate_expression(engine, expression)?;
                }
                plan.push(
                    format!(
                        "{alter} ALTER COLUMN {} TYPE {}{}",
                        ident(name)?,
                        data_type.sql(engine)?,
                        using
                            .as_ref()
                            .map(|s| format!(" USING ({s})"))
                            .unwrap_or_default()
                    ),
                    true,
                    "Change column type; conversion can lose data",
                );
            }
            ModifyMySqlColumn { definition, .. } => {
                ensure!(engine == Engine::MySql, "ModifyMySqlColumn requires MySQL");
                plan.push(format!("{alter} MODIFY COLUMN {}",definition.sql()?),true,"Replace full MySQL column definition; review all preserved attributes and conversions");
                plan.warnings.push("Full MySQL definition replacement: load the existing definition first and recheck for concurrent schema changes. Generated columns and unmodeled attributes are rejected by the loader.".into());
            }
            DropTable { cascade, .. } => {
                ensure!(
                    engine == Engine::Postgres || !cascade,
                    "MySQL DROP TABLE has no CASCADE support"
                );
                plan.push(
                    format!("DROP TABLE {t}{}", if *cascade { " CASCADE" } else { "" }),
                    true,
                    "Drop table and its data",
                );
            }
            RenameTable { new_name, .. } => plan.push(
                format!("{alter} RENAME TO {}", {
                    ident(new_name)?;
                    if engine == Engine::MySql {
                        qualified_table(
                            engine,
                            &TableRef {
                                schema: table.schema.clone(),
                                name: new_name.clone(),
                            },
                        )
                    } else {
                        quote_ident(engine, new_name)
                    }
                }),
                false,
                "Rename table",
            ),
            AddColumn { column, .. } => plan.push(
                format!("{alter} ADD COLUMN {}", column_sql(engine, column)?),
                false,
                "Add column",
            ),
            DropColumn { name, .. } => plan.push(
                format!("{alter} DROP COLUMN {}", ident(name)?),
                true,
                "Drop column and its data",
            ),
            RenameColumn { name, new_name, .. } => plan.push(
                format!(
                    "{alter} RENAME COLUMN {} TO {}",
                    ident(name)?,
                    ident(new_name)?
                ),
                false,
                "Rename column",
            ),
            AlterColumn { column, .. } => {
                if engine == Engine::MySql {
                    plan.warnings.push("LOSSY full-definition replacement: AlterColumn omits AUTO_INCREMENT, unsigned, charset/collation, comment, ON UPDATE, generated and visibility attributes, and replaces DEFAULT. Use mysql_column_definition + ModifyMySqlColumn for existing columns, or SetColumnDefault for a targeted change.".into());
                    plan.push(
                        format!("{alter} MODIFY COLUMN {}", column_sql(engine, column)?),
                        true,
                        "Replace complete column definition; conversion may lose data",
                    );
                } else {
                    let c = ident(&column.name)?;
                    plan.push(
                        format!(
                            "{alter} ALTER COLUMN {c} TYPE {}",
                            column.data_type.sql(engine)?
                        ),
                        true,
                        "Change type; conversion may require an explicit USING expression",
                    );
                    plan.push(
                        format!(
                            "{alter} ALTER COLUMN {c} {} NOT NULL",
                            if column.nullable { "DROP" } else { "SET" }
                        ),
                        false,
                        "Change nullability",
                    );
                    plan.push(
                        format!(
                            "{alter} ALTER COLUMN {c} {}",
                            if let Some(d) = &column.default {
                                format!("SET DEFAULT {}", d.sql(engine)?)
                            } else {
                                "DROP DEFAULT".into()
                            }
                        ),
                        false,
                        "Change default",
                    );
                }
            }
            CreateIndex { index, .. } => plan.push(
                format!(
                    "CREATE {}INDEX {} ON {t} ({})",
                    if index.unique { "UNIQUE " } else { "" },
                    ident(&index.name)?,
                    names(engine, &index.columns)?
                ),
                false,
                "Create index",
            ),
            RenameIndex { name, new_name, .. } => {
                let old_name = ident(name)?;
                let new_name = ident(new_name)?;
                plan.push(
                    if engine == Engine::MySql {
                        format!("{alter} RENAME INDEX {old_name} TO {new_name}")
                    } else {
                        format!(
                            "ALTER INDEX {} RENAME TO {new_name}",
                            qualified_table(
                                engine,
                                &TableRef {
                                    schema: table.schema.clone(),
                                    name: name.clone()
                                }
                            )
                        )
                    },
                    false,
                    "Rename index",
                );
            }
            DropIndex { name, .. } => {
                let sql = if engine == Engine::MySql {
                    format!("DROP INDEX {} ON {t}", ident(name)?)
                } else {
                    ident(name)?;
                    format!(
                        "DROP INDEX {}",
                        qualified_table(
                            engine,
                            &TableRef {
                                schema: table.schema.clone(),
                                name: name.clone()
                            }
                        )
                    )
                };
                plan.push(
                    sql,
                    true,
                    "Drop index (uniqueness may no longer be enforced)",
                );
            }
            AddForeignKey { foreign_key, .. } => plan.push(
                format!("{alter} ADD {}", foreign_sql(engine, foreign_key)?),
                false,
                "Add foreign key",
            ),
            DropForeignKey { name, .. } => plan.push(
                format!(
                    "{alter} DROP {} {}",
                    if engine == Engine::MySql {
                        "FOREIGN KEY"
                    } else {
                        "CONSTRAINT"
                    },
                    ident(name)?
                ),
                true,
                "Drop foreign key enforcement",
            ),
            AddPrimaryKey { primary_key, .. } => plan.push(
                format!("{alter} ADD {}", primary_sql(engine, primary_key)?),
                false,
                "Add primary key",
            ),
            DropPrimaryKey { name, .. } => {
                ident(name)?;
                plan.push(
                    if engine == Engine::MySql {
                        format!("{alter} DROP PRIMARY KEY")
                    } else {
                        format!("{alter} DROP CONSTRAINT {}", ident(name)?)
                    },
                    true,
                    "Drop primary key enforcement",
                );
            }
            CreateTable(_) => unreachable!(),
        }
    }
    if engine == Engine::MySql && !plan.is_empty() {
        plan.warnings
            .push("MySQL DDL can implicitly commit and is not generally rollbackable.".into());
    }
    Ok(plan)
}

/// Live, non-secret constraint catalog. Definitions are display SQL, not editable
/// snapshots. Known textual metadata is strictly UTF-8 decoded even when MySQL
/// marks catalog enums/expressions as binary on the wire.
pub async fn constraints(
    db: &rowboat_core::Database,
    table: &TableRef,
) -> Result<Vec<rowboat_core::QueryResult>> {
    operation_support(db.engine(), Operation::SchemaInspection)?;
    validate_table(table)?;
    if db.engine() == Engine::Sqlite {
        return crate::sqlite_schema::inspect(db, table).await;
    }
    ensure!(
        !table.schema.is_empty(),
        "Constraint inspection requires an explicit schema/database"
    );
    let value = |s: &str| literal(db.engine(), &Cell::Text(s.into()));
    let sql = if db.engine() == Engine::Postgres {
        format!(
            "SELECT c.conname AS name, CASE c.contype WHEN 'p' THEN 'PRIMARY KEY' WHEN 'u' THEN 'UNIQUE' WHEN 'f' THEN 'FOREIGN KEY' WHEN 'c' THEN 'CHECK' END AS kind, pg_get_constraintdef(c.oid,true) AS definition FROM pg_constraint c JOIN pg_class t ON t.oid=c.conrelid JOIN pg_namespace n ON n.oid=t.relnamespace WHERE n.nspname={} AND t.relname={} AND c.contype IN ('p','u','f','c') ORDER BY c.conname",
            value(&table.schema),
            value(&table.name)
        )
    } else {
        format!(
            "SELECT t.CONSTRAINT_NAME AS name, t.CONSTRAINT_TYPE AS kind,
             CASE WHEN t.CONSTRAINT_TYPE='CHECK' THEN CONCAT('CHECK (',c.CHECK_CLAUSE,')')
             WHEN t.CONSTRAINT_TYPE='FOREIGN KEY' THEN CONCAT('FOREIGN KEY (',
               GROUP_CONCAT(CONCAT('`',REPLACE(k.COLUMN_NAME,'`','``'),'`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),
               ') REFERENCES ', MAX(CONCAT('`',REPLACE(k.REFERENCED_TABLE_SCHEMA,'`','``'),'`.`',REPLACE(k.REFERENCED_TABLE_NAME,'`','``'),'`')), ' (',
               GROUP_CONCAT(CONCAT('`',REPLACE(k.REFERENCED_COLUMN_NAME,'`','``'),'`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),
               ') ON UPDATE ',MAX(r.UPDATE_RULE),' ON DELETE ',MAX(r.DELETE_RULE))
             ELSE CONCAT(t.CONSTRAINT_TYPE,' (',GROUP_CONCAT(CONCAT('`',REPLACE(k.COLUMN_NAME,'`','``'),'`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),')') END AS definition
             FROM information_schema.TABLE_CONSTRAINTS t
             LEFT JOIN information_schema.CHECK_CONSTRAINTS c ON c.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND c.CONSTRAINT_NAME=t.CONSTRAINT_NAME AND t.CONSTRAINT_TYPE='CHECK'
             LEFT JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND k.TABLE_NAME=t.TABLE_NAME AND k.CONSTRAINT_NAME=t.CONSTRAINT_NAME
             LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND r.TABLE_NAME=t.TABLE_NAME AND r.CONSTRAINT_NAME=t.CONSTRAINT_NAME
             WHERE t.TABLE_SCHEMA={} AND t.TABLE_NAME={}
             GROUP BY t.CONSTRAINT_NAME,t.CONSTRAINT_TYPE,c.CHECK_CLAUSE ORDER BY t.CONSTRAINT_NAME",
            value(&table.schema),
            value(&table.name)
        )
    };
    crate::objects::text_results(db.query(&sql, 10_000).await?)
}
