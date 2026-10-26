//! Dialect-aware SQL rendering. Execution uses bound values; literals are for previews.
use crate::lex::{dash_comment, ident_byte, line_comment_end, pg_quote_continuation, sql_space};
use crate::model::*;
use anyhow::{Result, bail};

pub fn quote_ident(engine: Engine, name: &str) -> String {
    let q = engine.descriptor().dialect.identifier_quote;
    format!("{q}{}{q}", name.replace(q, &format!("{q}{q}")))
}

pub fn qualified_table(engine: Engine, table: &TableRef) -> String {
    if table.schema.is_empty() {
        quote_ident(engine, &table.name)
    } else {
        format!(
            "{}.{}",
            quote_ident(engine, &table.schema),
            quote_ident(engine, &table.name)
        )
    }
}

pub fn literal(engine: Engine, value: &Cell) -> String {
    match value {
        Cell::Null => "NULL".into(),
        Cell::Binary(bytes) => match engine {
            Engine::Postgres => format!("decode('{}', 'hex')", hex::encode(bytes)),
            Engine::MySql | Engine::Sqlite => format!("X'{}'", hex::encode(bytes)),
        },
        Cell::Text(text) => match engine {
            Engine::Postgres => format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''")),
            Engine::MySql => format!("CONVERT(X'{}' USING utf8mb4)", hex::encode(text.as_bytes())),
            Engine::Sqlite if text.contains('\0') => {
                format!("CAST(X'{}' AS TEXT)", hex::encode(text.as_bytes()))
            }
            Engine::Sqlite => format!("'{}'", text.replace('\'', "''")),
        },
    }
}

pub fn preview_changes(engine: Engine, table: &TableRef, changes: &[RowChange]) -> Result<String> {
    let table = qualified_table(engine, table);
    let mut statements = Vec::new();
    for change in changes {
        let predicate = |original: &[(String, Cell)]| -> Result<String> {
            if original.is_empty() {
                bail!("An original row is required for an update or delete");
            }
            Ok(original
                .iter()
                .map(|(name, cell)| {
                    let name = quote_ident(engine, name);
                    if *cell == Cell::Null {
                        format!("{name} IS NULL")
                    } else {
                        let value = literal(engine,cell);
                        match (engine,cell) {
                            (Engine::Postgres,Cell::Text(_)) => format!("{name}::text COLLATE pg_catalog.\"C\" IS NOT DISTINCT FROM {value}::text COLLATE pg_catalog.\"C\""),
                            (Engine::Postgres,_) => format!("{name} = {value}"),
                            (Engine::MySql,_) => format!("CAST({name} AS BINARY) <=> CAST({value} AS BINARY)"),
                            (Engine::Sqlite,_) => format!("{name} IS {value}"),
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(" AND "))
        };
        statements.push(match change {
            RowChange::Insert { values } if values.is_empty() => match engine {
                Engine::Postgres | Engine::Sqlite => format!("INSERT INTO {table} DEFAULT VALUES;"),
                Engine::MySql => format!("INSERT INTO {table} () VALUES ();"),
            },
            RowChange::Insert { values } => format!(
                "INSERT INTO {table} ({}) VALUES ({});",
                values
                    .iter()
                    .map(|(n, _)| quote_ident(engine, n))
                    .collect::<Vec<_>>()
                    .join(", "),
                values
                    .iter()
                    .map(|(_, v)| literal(engine, v))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            RowChange::Update { original, values } => {
                if values.is_empty() {
                    bail!("Update has no changed values");
                }
                format!(
                    "UPDATE {table} SET {} WHERE {};",
                    values
                        .iter()
                        .map(|(n, v)| format!(
                            "{} = {}",
                            quote_ident(engine, n),
                            literal(engine, v)
                        ))
                        .collect::<Vec<_>>()
                        .join(", "),
                    predicate(original)?
                )
            }
            RowChange::Delete { original } => {
                format!("DELETE FROM {table} WHERE {};", predicate(original)?)
            }
        });
    }
    Ok(statements.join("\n"))
}

/// Split outside strings, identifiers, line/block comments, and PostgreSQL dollar quotes.
/// MySQL DELIMITER is a client directive and is deliberately rejected.
pub fn split_statements(engine: Engine, sql: &str) -> Result<Vec<String>> {
    let bytes = sql.as_bytes();
    if bytes.contains(&0) {
        bail!("SQL text may not contain NUL bytes");
    }
    let mut result = Vec::new();
    let (mut start, mut i) = (0, 0);
    let mut substantive = false;
    while i < bytes.len() {
        match bytes[i] {
            b';' => {
                if substantive {
                    result.push(sql[start..i].trim().to_owned());
                }
                i += 1;
                start = i;
                substantive = false;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') && dash_comment(engine, bytes.get(i + 2)) => {
                i += 2;
                while i < bytes.len() && !line_comment_end(engine, bytes[i]) {
                    i += 1;
                }
            }
            b'#' if engine == Engine::MySql => {
                while i < bytes.len() && !line_comment_end(engine, bytes[i]) {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                if engine == Engine::MySql && bytes.get(i + 2) == Some(&b'!') {
                    substantive = true;
                }
                i += 2;
                let mut depth = 1;
                while i < bytes.len() && depth > 0 {
                    if bytes[i..].starts_with(b"/*") && engine == Engine::Postgres {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth != 0 {
                    bail!("Unterminated block comment");
                }
            }
            q @ (b'\'' | b'"' | b'`') if q != b'`' || engine == Engine::MySql => {
                substantive = true;
                let escape = (engine == Engine::MySql && q != b'`')
                    || (q == b'\''
                        && i > 0
                        && matches!(bytes[i - 1], b'e' | b'E')
                        && (i < 2 || !ident_byte(bytes[i - 2])));
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && escape {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == q {
                        i += 1;
                        if bytes.get(i) == Some(&q) {
                            i += 1;
                        } else if engine == Engine::Postgres
                            && q == b'\''
                            && let Some(next) = pg_quote_continuation(bytes, i)
                        {
                            i = next + 1;
                        } else {
                            closed = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                if !closed {
                    bail!("Unterminated quoted string or identifier");
                }
            }
            b'$' if engine == Engine::Postgres
                && (i == 0
                    || !(bytes[i - 1].is_ascii_alphanumeric()
                        || matches!(bytes[i - 1], b'_' | b'$')
                        || bytes[i - 1] >= 128)) =>
            {
                let mut end = i + 1;
                if bytes.get(end) == Some(&b'$')
                    || bytes
                        .get(end)
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b >= 128)
                {
                    while bytes
                        .get(end)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 128)
                    {
                        end += 1;
                    }
                    if bytes.get(end) == Some(&b'$') {
                        let delimiter = &sql[i..=end];
                        if let Some(close) = sql[end + 1..].find(delimiter) {
                            i = end + 1 + close + delimiter.len();
                            substantive = true;
                            continue;
                        }
                        bail!("Unterminated dollar-quoted string");
                    }
                }
                substantive = true;
                i += 1;
            }
            b if sql_space(b) => i += 1,
            _ => {
                substantive = true;
                i += 1;
            }
        }
    }
    if substantive {
        result.push(sql[start..].trim().to_owned());
    }
    if engine == Engine::MySql
        && result.iter().any(|s| {
            s.lines().any(|l| {
                l.trim_start()
                    .to_ascii_uppercase()
                    .starts_with("DELIMITER ")
            })
        })
    {
        bail!(
            "DELIMITER is a mysql CLI directive; submit the routine definition as one SQL statement"
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialect_line_comments_and_escape_continuations() {
        for newline in ["\r", "\n", "\r\n"] {
            let sql = format!(
                "SELECT 1 -- hidden{newline}; COMMIT; START TRANSACTION READ WRITE; SELECT 2"
            );
            assert_eq!(
                split_statements(Engine::Postgres, &sql).unwrap().len(),
                4,
                "{sql:?}"
            );
            let sql = format!("SELECT E'a'{newline}'\\'; still literal'; SELECT 2");
            assert_eq!(
                split_statements(Engine::Postgres, &sql).unwrap().len(),
                2,
                "{sql:?}"
            );
        }
        for marker in [
            "# '", "-- '", "--\t'", "--\x01'", "--\x0b'", "--\x0c'", "--\x1f'", "--\x7f'",
        ] {
            let sql = format!("SELECT 1 {marker}\n; SELECT 2");
            assert_eq!(
                split_statements(Engine::MySql, &sql).unwrap().len(),
                2,
                "{sql:?}"
            );
            let sql = format!("SELECT 1 {marker}\r; SELECT 2");
            assert_eq!(
                split_statements(Engine::MySql, &sql).unwrap().len(),
                1,
                "{sql:?}"
            );
        }
        assert_eq!(
            split_statements(Engine::MySql, "SELECT 1--2; SELECT 3")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            split_statements(Engine::Postgres, "SELECT 1--2; SELECT 3")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            split_statements(Engine::Postgres, "SELECT 1 ` 2; SELECT 3 ` 4")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            split_statements(Engine::MySql, "SELECT `a\\`; SELECT 2")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            split_statements(Engine::Postgres, "SELECT $é$semi;colon$é$; SELECT 2")
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn identifiers_and_literals_cannot_escape() {
        assert_eq!(quote_ident(Engine::Postgres, "a\";drop"), "\"a\"\";drop\"");
        assert_eq!(quote_ident(Engine::MySql, "a`b"), "`a``b`");
        assert_eq!(
            literal(Engine::Postgres, &Cell::Text("a'\\b".into())),
            "E'a''\\\\b'"
        );
        assert_eq!(
            literal(Engine::MySql, &Cell::Text("'\\".into())),
            "CONVERT(X'275c' USING utf8mb4)"
        );
        assert_ne!(
            literal(Engine::Postgres, &Cell::Null),
            literal(Engine::Postgres, &Cell::Text("NULL".into()))
        );
    }
    #[test]
    fn split_complex_postgres() {
        let statements = split_statements(Engine::Postgres, "-- hi;\n SELECT 'a;''b', E'a\\';b'; /* outer /* nested; */ end */ DO $fn$ BEGIN RAISE NOTICE ';'; END $fn$; SELECT \"semi;colon\"; -- trailing").unwrap();
        assert_eq!(statements.len(), 3);
        assert!(statements[1].contains("RAISE NOTICE"));
        assert!(split_statements(Engine::Postgres, "select $x$unclosed").is_err());
        assert!(split_statements(Engine::Postgres, "/* unclosed").is_err());
    }
    #[test]
    fn split_mysql_comments_strings() {
        assert_eq!(
            split_statements(Engine::MySql, "# ;\n SELECT 'a\\';b'; SELECT `a;b`; -- ;\n")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            split_statements(Engine::MySql, "/*!40101 SET @a = 1 */;")
                .unwrap()
                .len(),
            1
        );
        assert!(split_statements(Engine::MySql, "DELIMITER $$\nselect 1$$").is_err());
    }
    #[test]
    fn preview_rejects_unbounded_mutation() {
        let t = TableRef {
            schema: "public".into(),
            name: "x".into(),
        };
        assert!(
            preview_changes(
                Engine::Postgres,
                &t,
                &[RowChange::Delete { original: vec![] }]
            )
            .is_err()
        );
        let s = preview_changes(
            Engine::MySql,
            &t,
            &[RowChange::Delete {
                original: vec![("id".into(), Cell::Null)],
            }],
        )
        .unwrap();
        assert!(s.contains("WHERE `id` IS NULL"));
    }
}
