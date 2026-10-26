use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{Cell, Engine, QueryResult, TableRef};
use serde::{
    Deserialize,
    de::{MapAccess, Visitor},
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};

pub fn format_sql(sql: &str) -> String {
    sqlformat::format(
        sql,
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions::default(),
    )
}

/// Format through the engine's lexical provider before replacing editor text.
pub fn format_sql_for_engine(engine: Engine, sql: &str) -> Result<String> {
    crate::service_support(engine, crate::ServiceOperation::FormatSql)?;
    match engine.descriptor().dialect.family {
        rowboat_core::DialectFamily::Sqlite => crate::sqlite_format::format_sqlite(sql),
        _ => Ok(format_sql(sql)),
    }
}

fn validate(result: &QueryResult) -> Result<()> {
    headers(result.columns.iter().map(|c| c.name.as_str()))?;
    for (index, row) in result.rows.iter().enumerate() {
        ensure!(
            row.len() == result.columns.len(),
            "Row {} has {} cells, expected {}",
            index + 1,
            row.len(),
            result.columns.len()
        );
    }
    Ok(())
}

fn headers<'a>(names: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for name in names {
        ensure!(!name.is_empty(), "Column names must not be empty");
        ensure!(seen.insert(name), "Duplicate column name: {name}");
    }
    Ok(())
}

pub fn export_csv(result: &QueryResult) -> Result<String> {
    validate(result)?;
    if result.columns.is_empty() {
        ensure!(
            result.rows.is_empty(),
            "CSV cannot represent rows without columns"
        );
        return Ok(String::new());
    }
    let mut writer = csv::WriterBuilder::new().from_writer(Vec::new());
    writer.write_record(result.columns.iter().map(|c| &c.name))?;
    for row in &result.rows {
        writer.write_record(row.iter().map(|cell| match cell {
            Cell::Null => "\\N".to_owned(),
            Cell::Binary(bytes) => format!("\\x{}", hex::encode(bytes)),
            Cell::Text(text) if text.starts_with('\\') => format!("\\{text}"),
            Cell::Text(text) => text.clone(),
        }))?;
    }
    Ok(String::from_utf8(writer.into_inner()?)?)
}

pub fn import_csv(input: &str) -> Result<Vec<Vec<(String, Cell)>>> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(input.as_bytes());
    let names = reader.headers()?.clone();
    headers(names.iter())?;
    reader
        .records()
        .enumerate()
        .map(|(index, row)| {
            let row = row.with_context(|| format!("Invalid CSV row {}", index + 2))?;
            names
                .iter()
                .zip(row.iter())
                .map(|(name, text)| {
                    let cell =
                        if text == "\\N" {
                            Cell::Null
                        } else if let Some(text) = text.strip_prefix("\\\\") {
                            Cell::Text(format!("\\{text}"))
                        } else if let Some(hex) = text.strip_prefix("\\x") {
                            Cell::Binary(hex::decode(hex).with_context(|| {
                                format!("Invalid binary in CSV row {}", index + 2)
                            })?)
                        } else {
                            Cell::Text(text.to_owned())
                        };
                    Ok((name.to_owned(), cell))
                })
                .collect()
        })
        .collect()
}

pub fn export_json(result: &QueryResult) -> Result<String> {
    validate(result)?;
    let rows: Vec<Value> = result
        .rows
        .iter()
        .map(|row| {
            let object: Map<String, Value> = result
                .columns
                .iter()
                .zip(row)
                .map(|(column, cell)| {
                    (
                        column.name.clone(),
                        match cell {
                            Cell::Null => Value::Null,
                            Cell::Text(text) => Value::String(text.clone()),
                            Cell::Binary(bytes) => {
                                serde_json::json!({"$binary": hex::encode(bytes)})
                            }
                        },
                    )
                })
                .collect();
            Value::Object(object)
        })
        .collect();
    Ok(serde_json::to_string_pretty(&rows)?)
}

struct UniqueRow(BTreeMap<String, Value>);

/// Decode one already byte-bounded row. Find scalar token boundaries before
/// calling serde, so it never buffers a document or an unsupported nested tree.
/// Binary wrappers use a derived struct to reject duplicate/unknown fields.
pub(crate) fn import_json_row(raw: &[u8], max_columns: usize) -> Result<Vec<(String, Cell)>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Binary {
        #[serde(rename = "$binary")]
        hex: String,
    }
    struct Cursor<'a> {
        raw: &'a [u8],
        at: usize,
    }
    impl Cursor<'_> {
        fn whitespace(&mut self) {
            while self
                .raw
                .get(self.at)
                .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            {
                self.at += 1;
            }
        }
        fn take(&mut self, byte: u8) -> Result<()> {
            self.whitespace();
            ensure!(
                self.raw.get(self.at) == Some(&byte),
                "Invalid JSON row syntax"
            );
            self.at += 1;
            Ok(())
        }
        fn string(&mut self) -> Result<()> {
            self.take(b'"')?;
            while let Some(byte) = self.raw.get(self.at) {
                self.at += 1;
                match byte {
                    b'"' => return Ok(()),
                    b'\\' => self.at += 1,
                    _ => {}
                }
            }
            bail!("Unterminated JSON string")
        }
        fn cell(&mut self) -> Result<Cell> {
            self.whitespace();
            let start = self.at;
            match self.raw.get(self.at) {
                Some(b'"') => self.string()?,
                Some(b'{') => {
                    self.at += 1;
                    loop {
                        match self.raw.get(self.at) {
                            Some(b'"') => self.string()?,
                            Some(b'}') => {
                                self.at += 1;
                                break;
                            }
                            Some(b'{' | b'[') => bail!("Unsupported nested JSON value"),
                            Some(_) => self.at += 1,
                            None => bail!("Unterminated JSON binary object"),
                        }
                    }
                    let binary: Binary = serde_json::from_slice(&self.raw[start..self.at])
                        .context("Expected a unique $binary hex string wrapper")?;
                    return Ok(Cell::Binary(
                        hex::decode(binary.hex).context("Invalid $binary hex string")?,
                    ));
                }
                Some(b'[') => bail!("Unsupported nested JSON array"),
                _ => {
                    while self
                        .raw
                        .get(self.at)
                        .is_some_and(|b| !matches!(b, b',' | b'}' | b' ' | b'\t' | b'\r' | b'\n'))
                    {
                        self.at += 1;
                    }
                }
            }
            Ok(
                match serde_json::from_slice(&self.raw[start..self.at])
                    .context("Invalid JSON cell")?
                {
                    Value::Null => Cell::Null,
                    Value::String(value) => Cell::Text(value),
                    Value::Number(value) => Cell::Text(value.to_string()),
                    Value::Bool(value) => Cell::Text(value.to_string()),
                    _ => bail!("Unsupported nested JSON value"),
                },
            )
        }
    }
    let mut cursor = Cursor { raw, at: 0 };
    cursor.take(b'{')?;
    cursor.whitespace();
    let mut row = BTreeMap::new();
    if cursor.raw.get(cursor.at) != Some(&b'}') {
        loop {
            // Enforce the column budget before decoding another key or cell.
            ensure!(row.len() < max_columns, "Excessive import columns");
            cursor.whitespace();
            let start = cursor.at;
            cursor.string()?;
            let name: String = serde_json::from_slice(&raw[start..cursor.at])
                .context("Invalid JSON column name")?;
            ensure!(!row.contains_key(&name), "Duplicate JSON column name");
            ensure!(
                !name.is_empty() && name.len() <= 256 && !name.contains('\0'),
                "Invalid import column name"
            );
            cursor.take(b':')?;
            row.insert(name, cursor.cell()?);
            cursor.whitespace();
            if cursor.raw.get(cursor.at) != Some(&b',') {
                break;
            }
            cursor.at += 1;
        }
    }
    cursor.take(b'}')?;
    cursor.whitespace();
    ensure!(cursor.at == raw.len(), "Unexpected content after JSON row");
    Ok(row.into_iter().collect())
}

impl<'de> Deserialize<'de> for UniqueRow {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct RowVisitor;
        impl<'de> Visitor<'de> for RowVisitor {
            type Value = UniqueRow;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a row object with unique column names")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut row = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if row.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "Duplicate column name: {key}"
                        )));
                    }
                }
                Ok(UniqueRow(row))
            }
        }
        deserializer.deserialize_map(RowVisitor)
    }
}

pub fn import_json(input: &str) -> Result<Vec<Vec<(String, Cell)>>> {
    let rows: Vec<UniqueRow> =
        serde_json::from_str(input).context("Expected a JSON array of row objects")?;
    let names: Vec<String> = rows
        .first()
        .map(|r| r.0.keys().cloned().collect())
        .unwrap_or_default();
    headers(names.iter().map(String::as_str))?;
    rows.into_iter()
        .enumerate()
        .map(|(index, UniqueRow(row))| {
            ensure!(
                row.keys().eq(names.iter()),
                "JSON row {} has inconsistent column names",
                index + 1
            );
            row.into_iter()
                .map(|(name, value)| {
                    let cell = match value {
                        Value::Null => Cell::Null,
                        Value::String(text) => Cell::Text(text),
                        Value::Number(number) => Cell::Text(number.to_string()),
                        Value::Bool(value) => Cell::Text(value.to_string()),
                        Value::Object(object)
                            if object.len() == 1 && object.contains_key("$binary") =>
                        {
                            let value = object["$binary"]
                                .as_str()
                                .context("$binary must be a hex string")?;
                            Cell::Binary(hex::decode(value).context("Invalid $binary hex string")?)
                        }
                        _ => bail!(
                            "Unsupported nested JSON value in row {}, column {name}",
                            index + 1
                        ),
                    };
                    Ok((name, cell))
                })
                .collect()
        })
        .collect()
}

fn identifier(engine: Engine, name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && !name.contains('\0'),
        "SQL identifiers must be nonempty and contain no NUL"
    );
    Ok(rowboat_core::sql::quote_ident(engine, name))
}

pub fn export_sql(engine: Engine, table: &TableRef, result: &QueryResult) -> Result<String> {
    validate(result)?;
    ensure!(
        !result.columns.is_empty() || result.rows.is_empty(),
        "Cannot export rows without columns"
    );
    let table_name = if table.schema.is_empty() {
        identifier(engine, &table.name)?
    } else {
        format!(
            "{}.{}",
            identifier(engine, &table.schema)?,
            identifier(engine, &table.name)?
        )
    };
    let columns = result
        .columns
        .iter()
        .map(|c| identifier(engine, &c.name))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let mut output = String::new();
    for row in &result.rows {
        let values = row
            .iter()
            .map(|cell| -> Result<String> {
                if let Cell::Text(text) = cell {
                    ensure!(
                        crate::providers::text_allows_nul(engine) || !text.contains('\0'),
                        "PostgreSQL text cannot contain NUL"
                    );
                }
                Ok(rowboat_core::sql::literal(engine, cell))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        output.push_str(&format!(
            "INSERT INTO {table_name} ({columns}) VALUES ({values});\n"
        ));
    }
    Ok(output)
}
