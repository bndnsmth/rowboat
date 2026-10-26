//! Explicit MySQL full-definition editing. Unknown attributes are a hard error.
use super::{Operation, operation_support};
use crate::{
    ddl::{ColumnDesign, DataType, DefaultValue, validate_name, validate_table},
    objects::cell_text,
};
use anyhow::{Result, ensure};
use rowboat_core::{
    Cell, Engine, QuerySession, TableRef,
    sql::{literal, quote_ident},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlColumnDefinition {
    pub column: ColumnDesign,
    pub unsigned: bool,
    pub auto_increment: bool,
    pub character_set: Option<String>,
    pub collation: Option<String>,
    pub comment: String,
    /// None removes ON UPDATE; Some(0..=6) uses CURRENT_TIMESTAMP(precision).
    pub on_update_current_timestamp_precision: Option<u8>,
    pub invisible: bool,
}
impl MySqlColumnDefinition {
    pub fn sql(&self) -> Result<String> {
        let e = Engine::MySql;
        let c = &self.column;
        validate_name(&c.name)?;
        ensure!(!self.comment.contains('\0'), "Column comment contains NUL");
        let integer = matches!(
            c.data_type,
            DataType::SmallInt | DataType::Integer | DataType::BigInt
        );
        ensure!(
            !self.unsigned
                || integer
                || matches!(
                    c.data_type,
                    DataType::Decimal { .. } | DataType::Real | DataType::Double
                ),
            "UNSIGNED requires a numeric type"
        );
        ensure!(
            !self.auto_increment || (integer && !c.nullable && c.default.is_none()),
            "AUTO_INCREMENT requires a non-null integer without a default"
        );
        ensure!(
            self.character_set.is_none() && self.collation.is_none()
                || matches!(
                    c.data_type,
                    DataType::Text | DataType::VarChar(_) | DataType::Char(_)
                ),
            "Charset/collation requires a text type"
        );
        let mut sql = format!(
            "{} {}{}",
            quote_ident(e, &c.name),
            c.data_type.sql(e)?,
            if self.unsigned { " UNSIGNED" } else { "" }
        );
        for (keyword, value) in [
            ("CHARACTER SET", &self.character_set),
            ("COLLATE", &self.collation),
        ] {
            if let Some(value) = value {
                validate_name(value)?;
                sql.push_str(&format!(" {keyword} {}", quote_ident(e, value)));
            }
        }
        sql.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if let Some(default) = &c.default {
            let rendered = match default {
                // Preserve MySQL's special temporal-default form rather than
                // converting it to a general expression with different metadata.
                DefaultValue::RawExpression(value)
                    if matches!(c.data_type, DataType::Timestamp { .. })
                        && current_timestamp_precision(value).is_some() =>
                {
                    value.to_ascii_uppercase()
                }
                _ => default.sql(e)?,
            };
            sql.push_str(&format!(" DEFAULT {rendered}"));
        }
        if let Some(precision) = self.on_update_current_timestamp_precision {
            ensure!(
                precision <= 6 && matches!(c.data_type, DataType::Timestamp { .. }),
                "ON UPDATE requires a timestamp and precision 0..6"
            );
            sql.push_str(&format!(" ON UPDATE CURRENT_TIMESTAMP({precision})"));
        }
        if self.auto_increment {
            sql.push_str(" AUTO_INCREMENT");
        }
        // COMMENT grammar requires a string literal, not a CONVERT expression.
        // Quote apostrophes, reject backslash ambiguity rather than erase/change it.
        ensure!(
            !self.comment.contains('\\'),
            "Backslash column comments require manual full-definition SQL with explicit session sql_mode"
        );
        sql.push_str(&format!(" COMMENT '{}'", self.comment.replace('\'', "''")));
        sql.push_str(if self.invisible {
            " INVISIBLE"
        } else {
            " VISIBLE"
        });
        Ok(sql)
    }
}

fn current_timestamp_precision(value: &str) -> Option<u8> {
    let lower = value.to_ascii_lowercase();
    let suffix = lower.strip_prefix("current_timestamp")?;
    if suffix.is_empty() || suffix == "()" {
        return Some(0);
    }
    let precision = suffix
        .strip_prefix('(')?
        .strip_suffix(')')?
        .parse::<u8>()
        .ok()?;
    (precision <= 6).then_some(precision)
}

pub async fn mysql_column_definition(
    db: &impl QuerySession,
    table: &TableRef,
    name: &str,
) -> Result<MySqlColumnDefinition> {
    operation_support(db.engine(), Operation::SchemaInspection)?;
    ensure!(
        db.engine() == Engine::MySql,
        "Full MySQL column definition requires MySQL"
    );
    validate_table(table)?;
    validate_name(name)?;
    ensure!(!table.schema.is_empty(), "An explicit database is required");
    mysql_permanent_table_definition(db, table).await?;
    let value = |s: &str| literal(Engine::MySql, &Cell::Text(s.into()));
    let sql = format!(
        "SELECT COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, CHARACTER_SET_NAME, COLLATION_NAME, COLUMN_COMMENT, EXTRA, GENERATION_EXPRESSION FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND COLUMN_NAME={}",
        value(&table.schema),
        value(&table.name),
        value(name)
    );
    let results = db.query(&sql, 2).await?;
    let rows: Vec<_> = results.iter().flat_map(|r| &r.rows).collect();
    ensure!(
        rows.len() == 1 && results.iter().all(|r| !r.truncated),
        "Column not found or ambiguous"
    );
    let row = rows[0];
    let required = |i| -> Result<String> {
        cell_text(row, i)?.ok_or_else(|| anyhow::anyhow!("Missing column metadata"))
    };
    ensure!(
        cell_text(row, 7)?.is_none_or(|s| s.is_empty()),
        "Generated columns require manual full-definition SQL"
    );
    let mut ty = required(0)?.to_ascii_lowercase();
    let unsigned = ty.ends_with(" unsigned");
    if unsigned {
        ty.truncate(ty.len() - 9);
    }
    let data_type = DataType::parse(Engine::MySql, &ty)?;
    let mut extra = required(6)?.to_ascii_lowercase();
    let default_generated = extra.contains("default_generated");
    let auto_increment = extra.contains("auto_increment");
    let invisible = extra.contains("invisible");
    for known in ["default_generated", "auto_increment", "invisible"] {
        extra = extra.replace(known, "");
    }
    let extra = extra.trim();
    let on_update_current_timestamp_precision = if extra.is_empty() {
        None
    } else {
        let value = extra
            .strip_prefix("on update current_timestamp")
            .ok_or_else(|| {
                anyhow::anyhow!("Unmodeled column attributes; manual full definition required")
            })?;
        Some(if value.is_empty() {
            0
        } else {
            value
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(|| anyhow::anyhow!("Unrecognized ON UPDATE expression"))?
                .parse::<u8>()?
        })
    };
    reject_native_binary_default(&data_type, row.get(2))?;
    let default = cell_text(row, 2)?.map(|s| {
        if default_generated {
            DefaultValue::RawExpression(s)
        } else {
            DefaultValue::Literal(Cell::Text(s))
        }
    });
    let definition = MySqlColumnDefinition {
        column: ColumnDesign {
            name: name.into(),
            data_type,
            nullable: required(1)? == "YES",
            default,
        },
        unsigned,
        auto_increment,
        character_set: cell_text(row, 3)?,
        collation: cell_text(row, 4)?,
        comment: required(5)?,
        on_update_current_timestamp_precision,
        invisible,
    };
    definition.sql()?;
    mysql_permanent_table_definition(db, table).await?;
    Ok(definition)
}

fn reject_native_binary_default(data_type: &DataType, value: Option<&Cell>) -> Result<()> {
    ensure!(
        !matches!(
            data_type,
            DataType::Binary(_) | DataType::VarBinary(_) | DataType::Blob
        ) || matches!(value, Some(Cell::Null)),
        "Native MySQL binary defaults cannot be recovered losslessly from COLUMN_DEFAULT; inspection is blocked rather than converting bytes to text"
    );
    Ok(())
}
/// information_schema can describe a permanent table while the same qualified
/// SQL target resolves to a temporary shadow in this physical session.
async fn mysql_permanent_table_definition(
    db: &impl QuerySession,
    table: &TableRef,
) -> Result<String> {
    let show = db
        .query(
            &format!(
                "SHOW CREATE TABLE {}",
                rowboat_core::sql::qualified_table(Engine::MySql, table)
            ),
            2,
        )
        .await?;
    ensure!(
        show.len() == 1 && !show[0].truncated && show[0].rows.len() == 1,
        "Incomplete effective MySQL table definition"
    );
    let definition = cell_text(&show[0].rows[0], 1)?
        .ok_or_else(|| anyhow::anyhow!("Missing effective MySQL table definition"))?;
    let mut prefix = definition.split_whitespace();
    ensure!(
        prefix
            .next()
            .is_some_and(|s| s.eq_ignore_ascii_case("CREATE"))
            && prefix
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("TABLE")),
        "Column inspection requires a permanent base table; temporary/shadowed MySQL targets are rejected"
    );
    Ok(definition)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MySqlSpecialType {
    Enum(Vec<String>),
    Set(Vec<String>),
    DateTime { precision: u8 },
    TinyInt { display_width: Option<u8> },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GeneratedStorage {
    Virtual,
    Stored,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlGeneration {
    pub expression: String,
    pub storage: GeneratedStorage,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvancedMySqlColumnDefinition {
    pub base: MySqlColumnDefinition,
    pub specialized_type: Option<MySqlSpecialType>,
    pub generation: Option<MySqlGeneration>,
}
fn mysql_string(value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\\', '\0']),
        "SQL-mode-dependent string cannot be represented"
    );
    Ok(format!("'{}'", value.replace('\'', "''")))
}
impl MySqlSpecialType {
    pub fn base_type(&self) -> DataType {
        match self {
            Self::Enum(_) | Self::Set(_) => DataType::Text,
            Self::DateTime { precision } => DataType::Timestamp {
                precision: Some(*precision),
                timezone: false,
            },
            Self::TinyInt { .. } => DataType::SmallInt,
        }
    }
    fn sql(&self) -> Result<String> {
        Ok(match self {
            Self::Enum(values) | Self::Set(values) => {
                ensure!(
                    !values.is_empty()
                        && values.len()
                            <= if matches!(self, Self::Set(_)) {
                                64
                            } else {
                                65535
                            },
                    "Invalid ENUM/SET member count"
                );
                let mut seen = std::collections::BTreeSet::new();
                for v in values {
                    ensure!(
                        seen.insert(v) && !v.ends_with(' '),
                        "Duplicate/trailing-space ENUM/SET member needs explicit policy"
                    );
                }
                format!(
                    "{}({})",
                    if matches!(self, Self::Enum(_)) {
                        "ENUM"
                    } else {
                        "SET"
                    },
                    values
                        .iter()
                        .map(|v| mysql_string(v))
                        .collect::<Result<Vec<_>>>()?
                        .join(", ")
                )
            }
            Self::DateTime { precision } => {
                ensure!(*precision <= 6, "DATETIME precision must be 0..6");
                format!("DATETIME({precision})")
            }
            Self::TinyInt { display_width } => {
                ensure!(
                    display_width.is_none_or(|n| n > 0),
                    "Invalid TINYINT display width"
                );
                format!(
                    "TINYINT{}",
                    display_width.map(|n| format!("({n})")).unwrap_or_default()
                )
            }
        })
    }
}
impl AdvancedMySqlColumnDefinition {
    pub fn sql(&self) -> Result<String> {
        let mut base = self.base.clone();
        let default = base.column.default.take();
        ensure!(
            !base.auto_increment || default.is_none(),
            "AUTO_INCREMENT cannot have a default"
        );
        if let Some(DefaultValue::RawExpression(value)) = &default {
            ensure!(
                !value.contains('\\'),
                "Default expression quoting depends on SQL mode"
            );
        }
        if let Some(special) = &self.specialized_type {
            ensure!(
                base.column.data_type == special.base_type(),
                "Specialized type requires its canonical base placeholder"
            );
        }
        if let Some(generation) = &self.generation {
            ensure!(
                default.is_none()
                    && !base.auto_increment
                    && base.on_update_current_timestamp_precision.is_none(),
                "Generated columns cannot carry defaults/AUTO_INCREMENT/ON UPDATE"
            );
            ensure!(
                !generation.expression.contains('\\'),
                "Generated expression quoting depends on SQL mode"
            );
            crate::query::validate_expression(Engine::MySql, &generation.expression)?;
        }
        let rendered = base.sql()?;
        let old_prefix = format!(
            "{} {}",
            quote_ident(Engine::MySql, &base.column.name),
            base.column.data_type.sql(Engine::MySql)?
        );
        let ty = if let Some(special) = &self.specialized_type {
            special.sql()?
        } else {
            base.column.data_type.sql(Engine::MySql)?
        };
        let mut tail = rendered
            .strip_prefix(&old_prefix)
            .ok_or_else(|| anyhow::anyhow!("Unexpected column renderer prefix"))?
            .to_owned();
        if let Some(generation) = &self.generation {
            // Insert before NULL/NOT NULL, after charset/collation attributes.
            let null = if base.column.nullable {
                " NULL"
            } else {
                " NOT NULL"
            };
            let position = tail
                .find(null)
                .ok_or_else(|| anyhow::anyhow!("Missing nullability clause"))?;
            tail.insert_str(
                position,
                &format!(
                    " GENERATED ALWAYS AS ({}) {}",
                    generation.expression,
                    if generation.storage == GeneratedStorage::Virtual {
                        "VIRTUAL"
                    } else {
                        "STORED"
                    }
                ),
            );
        }
        let mut sql = format!(
            "{} {ty}{tail}",
            quote_ident(Engine::MySql, &base.column.name)
        );
        if let Some(default) = default {
            let value = match default {
                DefaultValue::Literal(Cell::Text(v)) => mysql_string(&v)?,
                DefaultValue::RawExpression(ref v) if current_timestamp_precision(v).is_some() => {
                    v.to_ascii_uppercase()
                }
                other => other.sql(Engine::MySql)?,
            };
            sql.push_str(&format!(" DEFAULT {value}"));
        }
        Ok(sql)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MySqlColumnSnapshot {
    table: TableRef,
    definition: AdvancedMySqlColumnDefinition,
    catalog: serde_json::Value,
    origin: serde_json::Value,
}
impl MySqlColumnSnapshot {
    pub fn table(&self) -> &TableRef {
        &self.table
    }
    pub fn definition(&self) -> &AdvancedMySqlColumnDefinition {
        &self.definition
    }
    pub fn catalog(&self) -> &serde_json::Value {
        &self.catalog
    }
    pub fn origin(&self) -> &serde_json::Value {
        &self.origin
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct MySqlColumnEdit {
    original: MySqlColumnSnapshot,
    plan: crate::ddl::DdlPlan,
}
impl MySqlColumnEdit {
    pub fn original(&self) -> &MySqlColumnSnapshot {
        &self.original
    }
    pub fn plan(&self) -> &crate::ddl::DdlPlan {
        &self.plan
    }
}
fn special_type(native: &str) -> Result<Option<MySqlSpecialType>> {
    let lower = native.to_ascii_lowercase();
    for (prefix, is_set) in [("enum(", false), ("set(", true)] {
        if lower.starts_with(prefix) {
            ensure!(
                native.ends_with(')') && !native.contains('\\'),
                "Unmodeled ENUM/SET quoting"
            );
            let inside = &native[prefix.len()..native.len() - 1];
            let mut values = vec![];
            let mut expect_value = true;
            for span in crate::query::spans(Engine::MySql, inside)? {
                let text = &inside[span.start..span.end];
                match span.kind {
                    crate::query::SpanKind::Quoted => {
                        ensure!(
                            expect_value && text.starts_with('\''),
                            "Invalid ENUM/SET literal"
                        );
                        values.push(text[1..text.len() - 1].replace("''", "'"));
                        expect_value = false;
                    }
                    crate::query::SpanKind::Code => {
                        let code = text.trim();
                        if !code.is_empty() {
                            ensure!(code == "," && !expect_value, "Invalid ENUM/SET separator");
                            expect_value = true;
                        }
                    }
                    _ => anyhow::bail!("Unexpected ENUM/SET comment"),
                }
            }
            ensure!(!expect_value, "Incomplete ENUM/SET members");
            return Ok(Some(if is_set {
                MySqlSpecialType::Set(values)
            } else {
                MySqlSpecialType::Enum(values)
            }));
        }
    }
    if let Some(suffix) = lower.strip_prefix("datetime") {
        let precision = if suffix.is_empty() {
            0
        } else {
            suffix
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .ok_or_else(|| anyhow::anyhow!("Invalid DATETIME"))?
                .parse()?
        };
        return Ok(Some(MySqlSpecialType::DateTime { precision }));
    }
    if let Some(suffix) = lower.strip_prefix("tinyint") {
        let display_width = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix('(')
                    .and_then(|s| s.strip_suffix(')'))
                    .ok_or_else(|| anyhow::anyhow!("Invalid TINYINT"))?
                    .parse()?,
            )
        };
        return Ok(Some(MySqlSpecialType::TinyInt { display_width }));
    }
    Ok(None)
}
pub async fn inspect_mysql_column(
    db: &impl QuerySession,
    table: &TableRef,
    name: &str,
) -> Result<MySqlColumnSnapshot> {
    operation_support(db.engine(), Operation::SchemaInspection)?;
    ensure!(
        db.engine() == Engine::MySql,
        "Advanced column inspection requires MySQL"
    );
    validate_table(table)?;
    validate_name(name)?;
    ensure!(!table.schema.is_empty(), "Explicit database required");
    mysql_permanent_table_definition(db, table).await?;
    let value = |s: &str| literal(Engine::MySql, &Cell::Text(s.into()));
    let raw=db.query(&format!("SELECT COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, CHARACTER_SET_NAME, COLLATION_NAME, COLUMN_COMMENT, EXTRA, GENERATION_EXPRESSION FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND COLUMN_NAME={}",value(&table.schema),value(&table.name),value(name)),2).await?;
    ensure!(
        raw.len() == 1 && !raw[0].truncated && raw[0].rows.len() == 1,
        "Column absent/inaccessible"
    );
    let row = &raw[0].rows[0];
    let get = |i| cell_text(row, i);
    let required =
        |i| -> Result<String> { get(i)?.ok_or_else(|| anyhow::anyhow!("Missing column field")) };
    let native = required(0)?;
    let unsigned = native.to_ascii_lowercase().ends_with(" unsigned");
    let native = if unsigned {
        &native[..native.len() - 9]
    } else {
        &native
    };
    let specialized_type = special_type(native)?;
    let data_type = if let Some(s) = &specialized_type {
        s.base_type()
    } else {
        DataType::parse(Engine::MySql, native)?
    };
    let mut extra = required(6)?.to_ascii_lowercase();
    let auto_increment = extra.contains("auto_increment");
    let invisible = extra.contains("invisible");
    let default_generated = extra.contains("default_generated");
    let expression = get(7)?.unwrap_or_default();
    let generation = if expression.is_empty() {
        None
    } else {
        let storage = if extra.contains("stored generated") {
            GeneratedStorage::Stored
        } else {
            ensure!(
                extra.contains("virtual generated"),
                "Unknown generated storage"
            );
            GeneratedStorage::Virtual
        };
        Some(MySqlGeneration {
            expression,
            storage,
        })
    };
    for known in [
        "auto_increment",
        "invisible",
        "default_generated",
        "stored generated",
        "virtual generated",
    ] {
        extra = extra.replace(known, "");
    }
    let extra = extra.trim();
    let on_update_current_timestamp_precision = if extra.is_empty() {
        None
    } else {
        Some(
            extra
                .strip_prefix("on update ")
                .and_then(current_timestamp_precision)
                .ok_or_else(|| anyhow::anyhow!("Unmodeled column EXTRA"))?,
        )
    };
    reject_native_binary_default(&data_type, row.get(2))?;
    let default = get(2)?.map(|v| {
        if default_generated {
            DefaultValue::RawExpression(v)
        } else {
            DefaultValue::Literal(Cell::Text(v))
        }
    });
    let definition = AdvancedMySqlColumnDefinition {
        base: MySqlColumnDefinition {
            column: ColumnDesign {
                name: name.into(),
                data_type,
                nullable: required(1)? == "YES",
                default,
            },
            unsigned,
            auto_increment,
            character_set: get(3)?,
            collation: get(4)?,
            comment: required(5)?,
            on_update_current_timestamp_precision,
            invisible,
        },
        specialized_type,
        generation,
    };
    definition.sql()?;
    let definition_sql = mysql_permanent_table_definition(db, table).await?;
    let catalog = serde_json::json!({"column":row,"table_definition":definition_sql});
    Ok(MySqlColumnSnapshot {
        table: table.clone(),
        definition,
        catalog,
        origin: crate::object_editor::connection_origin(db).await?,
    })
}
pub fn plan_mysql_column_edit(
    original: &MySqlColumnSnapshot,
    desired: AdvancedMySqlColumnDefinition,
) -> Result<MySqlColumnEdit> {
    ensure!(
        desired.base.column.name == original.definition.base.column.name,
        "Column renaming requires an explicit dependency migration"
    );
    ensure!(
        desired.generation.as_ref().map(|g| g.storage)
            == original.definition.generation.as_ref().map(|g| g.storage),
        "Generated/base or VIRTUAL/STORED conversion requires an explicit migration; no drop/add fallback"
    );
    ensure!(
        desired.base.auto_increment == original.definition.base.auto_increment,
        "AUTO_INCREMENT membership changes require atomic key migration"
    );
    let sql = desired.sql()?;
    let mut plan = crate::ddl::DdlPlan::default();
    if desired != original.definition {
        plan.push(
            format!(
                "ALTER TABLE {} MODIFY COLUMN {sql}",
                rowboat_core::sql::qualified_table(Engine::MySql, &original.table)
            ),
            true,
            "Modify advanced column while preserving all explicitly inspected attributes",
        );
    }
    plan.warnings.push("MySQL ALTER implicitly commits and may rebuild the table; existing data and dependencies are validated by the server. Recheck the immutable table/column snapshot immediately before apply.".into());
    Ok(MySqlColumnEdit {
        original: original.clone(),
        plan,
    })
}
pub async fn mysql_column_is_current(
    db: &impl QuerySession,
    edit: &MySqlColumnEdit,
) -> Result<bool> {
    operation_support(db.engine(), Operation::SchemaInspection)?;
    if db.engine() != Engine::MySql {
        return Ok(false);
    }
    let old = &edit.original;
    Ok(inspect_mysql_column(db, &old.table, &old.definition.base.column.name).await? == *old)
}
