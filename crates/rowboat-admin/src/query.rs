//! SQL-editor helpers. Named parameters expand to dialect-safe literal SQL,
//! not identifiers or arbitrary SQL fragments.
use anyhow::{Result, bail, ensure};
use rowboat_core::{
    Cell, Database, Engine, QueryResult, TableRef,
    sql::{literal, qualified_table, quote_ident},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanKind {
    Code,
    Quoted,
    Comment,
}
#[derive(PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: SpanKind,
}

/// Match the MySQL session's sql_mode when parsing caller-authored SQL.
/// PostgreSQL assumes standard_conforming_strings=on, as set by rowboat-core.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SqlLexOptions {
    pub mysql_no_backslash_escapes: bool,
    pub mysql_ansi_quotes: bool,
}

/// Scan UTF-8 by ASCII delimiter bytes; all produced boundaries are ASCII
/// delimiters or the input endpoints and therefore valid UTF-8 boundaries.
pub(crate) fn spans(engine: Engine, sql: &str) -> Result<Vec<Span>> {
    spans_with_options(engine, sql, SqlLexOptions::default())
}

/// A query embedded inside a generated parenthesized view definition cannot
/// close the containing statement. Reuse EXPLAIN's mode-independent boundary
/// checks, then require balanced query parentheses and no outer terminator.
pub(crate) fn view_query(engine: Engine, sql: &str) -> Result<()> {
    explain_sql(engine, sql)?;
    let mut depth = 0usize;
    let mut first = None;
    for span in spans(engine, sql)? {
        if span.kind != SpanKind::Code {
            continue;
        }
        let text = &sql[span.start..span.end];
        if first.is_none() && !text.trim().is_empty() {
            first = Some(
                text.trim_start()
                    .split(|c: char| !c.is_ascii_alphabetic())
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase(),
            );
        }
        for b in text.bytes() {
            match b {
                b'(' => depth += 1,
                b')' => {
                    ensure!(depth > 0, "View query closes its enclosing definition");
                    depth -= 1;
                }
                b';' => bail!("View query cannot contain an outer statement terminator"),
                _ => (),
            }
        }
    }
    ensure!(depth == 0, "Unbalanced view query parentheses");
    ensure!(
        matches!(
            first.as_deref(),
            Some("SELECT" | "WITH" | "VALUES" | "TABLE")
        ),
        "Expected a view query"
    );
    Ok(())
}

fn spans_with_options(engine: Engine, sql: &str, options: SqlLexOptions) -> Result<Vec<Span>> {
    scan(engine, sql, options, false)
}

fn scan(
    engine: Engine,
    sql: &str,
    options: SqlLexOptions,
    legacy_pg_strings: bool,
) -> Result<Vec<Span>> {
    let dialect = crate::providers::lexical::rules(engine);
    ensure!(!sql.contains('\0'), "SQL cannot contain NUL");
    let b = sql.as_bytes();
    let (mut i, mut code_start) = (0, 0);
    let mut out = Vec::new();
    while i < b.len() {
        let start = i;
        let kind;
        if (b[i] == b'-'
            && b.get(i + 1) == Some(&b'-')
            && (!dialect.dash_comment_requires_space
                || b.get(i + 2)
                    .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control())))
            || (dialect.mysql && b[i] == b'#')
        {
            kind = SpanKind::Comment;
            // PostgreSQL terminates on CR or LF. MySQL's native lexer ends
            // -- and # comments on LF only, including in the text protocol.
            while i < b.len() && b[i] != b'\n' && !(dialect.comment_ends_at_cr && b[i] == b'\r') {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            ensure!(
                !dialect.mysql
                    || !(b[i..].starts_with(b"/*!")
                        || b[i..].starts_with(b"/*M!")
                        || b[i..].starts_with(b"/*m!")),
                "Executable MySQL comments are not supported by SQL helpers"
            );
            kind = SpanKind::Comment;
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if dialect.nested_comments && b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            ensure!(depth == 0, "Unterminated block comment");
        } else if matches!(b[i], b'\'' | b'"')
            || (dialect.backtick_identifiers && b[i] == b'`')
            || (dialect.bracket_identifiers && b[i] == b'[')
        {
            kind = SpanKind::Quoted;
            let bracket = b[i] == b'[';
            let q = if bracket { b']' } else { b[i] };
            let backslash = (dialect.mysql
                && q != b'`'
                && !options.mysql_no_backslash_escapes
                && !(q == b'"' && options.mysql_ansi_quotes))
                || (dialect.postgres
                    && q == b'\''
                    && (legacy_pg_strings
                        || (i > 0
                            && matches!(b[i - 1], b'e' | b'E')
                            && (i < 2 || !(is_ident(b[i - 2]) || b[i - 2] == b'$')))));
            i += 1;
            let mut closed = false;
            while i < b.len() {
                if backslash && b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == q {
                    i += 1;
                    if !bracket && b.get(i) == Some(&q) {
                        i += 1;
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            ensure!(closed, "Unterminated quoted string or identifier");
        } else if dialect.postgres
            && b[i] == b'$'
            && (i == 0 || !(is_ident(b[i - 1]) || b[i - 1] == b'$'))
        {
            let mut end = i + 1;
            if b.get(end) == Some(&b'$')
                || b.get(end).is_some_and(|c| is_ident_start(*c) || *c >= 128)
            {
                while b.get(end).is_some_and(|c| is_ident(*c)) {
                    end += 1;
                }
            }
            if b.get(end) == Some(&b'$') {
                let tag = &sql[i..=end];
                let close = sql[end + 1..]
                    .find(tag)
                    .ok_or_else(|| anyhow::anyhow!("Unterminated dollar-quoted string"))?;
                i = end + 1 + close + tag.len();
                kind = SpanKind::Quoted;
            } else {
                i += 1;
                continue;
            }
        } else {
            i += 1;
            continue;
        }
        if code_start < start {
            out.push(Span {
                start: code_start,
                end: start,
                kind: SpanKind::Code,
            });
        }
        out.push(Span {
            start,
            end: i,
            kind,
        });
        code_start = i;
    }
    if code_start < b.len() {
        out.push(Span {
            start: code_start,
            end: b.len(),
            kind: SpanKind::Code,
        });
    }
    Ok(out)
}
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_ident(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c >= 128
}

pub fn expand_named(engine: Engine, sql: &str, values: &BTreeMap<String, Cell>) -> Result<String> {
    expand_named_with_options(engine, sql, values, SqlLexOptions::default())
}

pub fn expand_named_with_options(
    engine: Engine,
    sql: &str,
    values: &BTreeMap<String, Cell>,
    options: SqlLexOptions,
) -> Result<String> {
    let mut result = String::with_capacity(sql.len());
    for span in spans_with_options(engine, sql, options)? {
        if span.kind != SpanKind::Code {
            result.push_str(&sql[span.start..span.end]);
            continue;
        }
        let b = sql.as_bytes();
        let (mut i, mut copied) = (span.start, span.start);
        while i < span.end {
            if b[i] == b':'
                && (i == 0 || b[i - 1] != b':')
                && b.get(i + 1).is_some_and(|c| is_ident_start(*c))
            {
                let start = i + 1;
                i = start + 1;
                while i < span.end && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let name = &sql[start..i];
                let value = values
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("Missing named parameter :{name}"))?;
                result.push_str(&sql[copied..start - 1]);
                result.push_str(&literal(engine, value));
                copied = i;
            } else {
                i += 1;
            }
        }
        result.push_str(&sql[copied..span.end]);
    }
    Ok(result)
}

pub(crate) fn validate_expression(engine: Engine, sql: &str) -> Result<()> {
    ensure!(!sql.trim().is_empty(), "Default expression cannot be empty");
    let mut stack = Vec::new();
    for span in spans(engine, sql)? {
        ensure!(
            span.kind != SpanKind::Comment,
            "Comments are not allowed in default expressions"
        );
        if span.kind != SpanKind::Code {
            continue;
        }
        for c in sql[span.start..span.end].bytes() {
            match c {
                b';' => bail!("Statement separators are not allowed in default expressions"),
                b'(' | b'[' => stack.push(c),
                b')' => ensure!(stack.pop() == Some(b'('), "Unbalanced default expression"),
                b']' => ensure!(stack.pop() == Some(b'['), "Unbalanced default expression"),
                _ => (),
            }
        }
    }
    ensure!(stack.is_empty(), "Unbalanced default expression");
    Ok(())
}

/// Validate the spans actually forwarded, independently of core's splitter.
fn explain_statement<'a>(sql: &'a str, tokens: &[Span]) -> Result<&'a str> {
    let mut terminator = None;
    let mut substantive = false;
    for span in tokens {
        match span.kind {
            SpanKind::Comment => (),
            SpanKind::Quoted => {
                ensure!(
                    terminator.is_none(),
                    "EXPLAIN requires exactly one statement"
                );
                substantive = true;
            }
            SpanKind::Code => {
                for (offset, byte) in sql[span.start..span.end].bytes().enumerate() {
                    if byte.is_ascii_whitespace() {
                        continue;
                    }
                    ensure!(
                        terminator.is_none(),
                        "EXPLAIN requires exactly one statement"
                    );
                    if byte == b';' {
                        terminator = Some(span.start + offset);
                    } else {
                        substantive = true;
                    }
                }
            }
        }
    }
    ensure!(substantive, "EXPLAIN requires exactly one statement");
    Ok(sql[..terminator.unwrap_or(sql.len())].trim_matches(|c: char| c.is_ascii_whitespace()))
}

/// Shared validation for both estimate-only EXPLAIN and explicit ANALYZE.
/// Never relax the mode-independent boundary check for an executing profile.
fn explain_input(engine: Engine, sql: &str) -> Result<(&str, String)> {
    let tokens = spans(engine, sql)?;
    if engine == Engine::MySql {
        // Bare CR is not a native MySQL terminator, but is on PostgreSQL and
        // some compatible servers. Do not forward that ambiguity in a batch.
        for span in &tokens {
            let text = &sql[span.start..span.end];
            if span.kind == SpanKind::Comment && (text.starts_with("--") || text.starts_with('#')) {
                ensure!(
                    !text.as_bytes().iter().enumerate().any(|(i, b)| *b == b'\r'
                        && sql.as_bytes().get(span.start + i + 1) != Some(&b'\n')),
                    "Use LF or CRLF instead of bare CR in MySQL EXPLAIN comments"
                );
            }
        }
        for mysql_no_backslash_escapes in [false, true] {
            for mysql_ansi_quotes in [false, true] {
                let alternate = spans_with_options(
                    engine,
                    sql,
                    SqlLexOptions {
                        mysql_no_backslash_escapes,
                        mysql_ansi_quotes,
                    },
                )?;
                ensure!(
                    alternate == tokens,
                    "EXPLAIN quote boundaries depend on MySQL sql_mode; use unambiguous quoting"
                );
            }
        }
    } else if engine == Engine::Postgres {
        let alternate = scan(engine, sql, SqlLexOptions::default(), true)?;
        ensure!(
            alternate == tokens,
            "EXPLAIN quote boundaries depend on standard_conforming_strings; use explicit E-strings"
        );
    }
    let s = explain_statement(sql, &tokens)?;
    let mut keyword = None;
    for span in spans(engine, s)? {
        if span.kind == SpanKind::Comment {
            continue;
        }
        ensure!(
            span.kind == SpanKind::Code,
            "Expected an explainable SQL statement"
        );
        let code = s[span.start..span.end].trim_start();
        if code.is_empty() {
            continue;
        }
        keyword = Some(
            code.split(|c: char| !c.is_ascii_alphabetic())
                .next()
                .unwrap_or("")
                .to_ascii_uppercase(),
        );
        break;
    }
    ensure!(
        matches!(
            keyword.as_deref(),
            Some("SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "VALUES" | "TABLE")
        ),
        "Only SELECT/WITH/INSERT/UPDATE/DELETE/VALUES/TABLE may be explained"
    );
    Ok((s, keyword.expect("validated statement keyword")))
}

/// Plain EXPLAIN only. Boundary validation is independent of the executing
/// connection's string modes: ambiguous quoting is rejected before any query.
pub fn explain_sql(engine: Engine, sql: &str) -> Result<String> {
    let prefix = crate::providers::explain::prefix(engine, false)?;
    let (s, _) = explain_input(engine, sql)?;
    Ok(format!("{}\n{}\n;", prefix, s))
}

/// Build a real executing profile; this builder does not execute it. Only
/// SELECT/WITH query shapes are initially supported. This is not a side-effect
/// sandbox: callable functions may mutate data or external state.
pub fn analyze_sql(engine: Engine, sql: &str) -> Result<String> {
    let prefix = crate::providers::explain::prefix(engine, true)?;
    let (s, keyword) = explain_input(engine, sql)?;
    ensure!(
        matches!(keyword.as_str(), "SELECT" | "WITH"),
        "Analyze supports SELECT/WITH queries only"
    );
    for span in spans(engine, s)? {
        if span.kind != SpanKind::Code {
            continue;
        }
        for word in
            s[span.start..span.end].split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
        {
            ensure!(
                !matches!(
                    word.to_ascii_uppercase().as_str(),
                    "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "INTO" | "LOCK" | "FOR"
                ),
                "Analyze supports query-only SELECT/WITH shapes; data-changing CTEs, INTO and locking clauses are unsupported"
            );
        }
    }
    Ok(format!("{}\n{s}\n;", prefix))
}
pub async fn explain(db: &impl rowboat_core::QuerySession, sql: &str) -> Result<Vec<QueryResult>> {
    let results = db.query(&explain_sql(db.engine(), sql)?, 1000).await?;
    ensure!(
        results.len() == 1 && !results[0].truncated,
        "Expected one complete EXPLAIN result; truncated or multiple results cannot represent a complete plan"
    );
    Ok(results)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionKind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionCandidate {
    pub label: String,
    pub insert_text: String,
    pub kind: CompletionKind,
    pub table: Option<TableRef>,
    pub data_type: Option<String>,
}
pub const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "INNER JOIN",
    "ON",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "AS",
    "DISTINCT",
    "UNION",
    "UNION ALL",
    "WITH",
    "RETURNING",
    "NULL",
    "IS NULL",
    "IS NOT NULL",
    "AND",
    "OR",
    "NOT",
    "EXISTS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "EXPLAIN",
];

/// Fetch catalog and per-table structure. Cache the returned candidates in the
/// UI; do not run this network scan on every keystroke. Permission errors surface.
pub async fn completion_candidates(
    db: &Database,
    prefix: &str,
) -> Result<Vec<CompletionCandidate>> {
    let mut out = Vec::new();
    let engine = db.engine();
    for word in KEYWORDS {
        if engine == Engine::MySql && *word == "RETURNING" {
            continue;
        }
        out.push(CompletionCandidate {
            label: (*word).into(),
            insert_text: (*word).into(),
            kind: CompletionKind::Keyword,
            table: None,
            data_type: None,
        });
    }
    let mut schemas = BTreeSet::new();
    for item in db.catalog().await? {
        let t = item.table;
        if !t.schema.is_empty() && schemas.insert(t.schema.clone()) {
            out.push(CompletionCandidate {
                label: t.schema.clone(),
                insert_text: quote_ident(engine, &t.schema),
                kind: CompletionKind::Schema,
                table: None,
                data_type: None,
            });
        }
        out.push(CompletionCandidate {
            label: t.name.clone(),
            insert_text: qualified_table(engine, &t),
            kind: if item.kind.to_ascii_lowercase().contains("view") {
                CompletionKind::View
            } else {
                CompletionKind::Table
            },
            table: Some(t.clone()),
            data_type: None,
        });
        for c in db.structure(&t).await?.columns {
            out.push(CompletionCandidate {
                label: c.name.clone(),
                insert_text: format!(
                    "{}.{}",
                    qualified_table(engine, &t),
                    quote_ident(engine, &c.name)
                ),
                kind: CompletionKind::Column,
                table: Some(t.clone()),
                data_type: Some(c.data_type),
            });
        }
    }
    let prefix = prefix.to_lowercase();
    out.retain(|c| {
        c.label.to_lowercase().starts_with(&prefix)
            || c.insert_text.to_lowercase().starts_with(&prefix)
            || c.table.as_ref().is_some_and(|t| {
                format!("{}.{}", t.name, c.label)
                    .to_lowercase()
                    .starts_with(&prefix)
            })
    });
    out.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then(a.insert_text.cmp(&b.insert_text))
    });
    Ok(out)
}
