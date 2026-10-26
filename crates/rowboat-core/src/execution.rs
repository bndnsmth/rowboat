//! Local execution boundaries, independent of editor highlighting and DB preflight.
//! This is a conservative lexical service, not a SQL validator or script runner.
use crate::{
    Engine,
    lex::{ident_byte, pg_quote_continuation},
};
use std::{fmt, ops::Range};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunScope {
    SelectionOrAll,
    Current,
    Selection,
    All,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryError {
    pub offset: usize,
    pub message: String,
}
impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for BoundaryError {}
fn error(offset: usize, message: &str) -> BoundaryError {
    BoundaryError {
        offset,
        message: message.into(),
    }
}

/// UTF-8 byte offset in the exact SQL submitted to this execution, never a
/// server's internal function SQL or an approximate message/line-number match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorPosition {
    pub byte_offset: usize,
}
#[derive(Debug)]
pub(crate) struct LocatedError {
    pub message: String,
    pub position: ErrorPosition,
}
impl fmt::Display for LocatedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for LocatedError {}
pub(crate) fn wire_error(error: sqlx::Error, sql: &str) -> anyhow::Error {
    if let sqlx::Error::Database(db) = &error
        && let Some(pg) = db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
        && let Some(sqlx::postgres::PgErrorPosition::Original(position)) = pg.position()
        && let Some(byte_offset) = character_position(sql, position)
    {
        return LocatedError {
            message: error.to_string(),
            position: ErrorPosition { byte_offset },
        }
        .into();
    }
    error.into()
}
/// PostgreSQL's Original position is one-based in characters, not bytes.
pub fn character_position(sql: &str, one_based: usize) -> Option<usize> {
    let index = one_based.checked_sub(1)?;
    sql.char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(sql.len()))
        .nth(index)
}

pub fn execution_range(
    engine: Engine,
    sql: &str,
    selection: Range<usize>,
    caret: usize,
    scope: RunScope,
) -> Result<Range<usize>, BoundaryError> {
    if selection.start > selection.end
        || selection.end > sql.len()
        || caret > sql.len()
        || !sql.is_char_boundary(selection.start)
        || !sql.is_char_boundary(selection.end)
        || !sql.is_char_boundary(caret)
    {
        return Err(error(0, "Editor range is no longer valid"));
    }
    match scope {
        RunScope::All => Ok(0..sql.len()),
        RunScope::SelectionOrAll => Ok(if selection.is_empty() {
            0..sql.len()
        } else {
            selection
        }),
        RunScope::Selection => {
            if selection.is_empty() {
                Err(error(caret, "Select SQL before Run Selection"))
            } else {
                Ok(selection)
            }
        }
        RunScope::Current => {
            let ranges = statement_ranges(engine, sql)?;
            ranges
                .iter()
                .find(|r| caret < r.end)
                .or_else(|| ranges.last())
                .cloned()
                .ok_or_else(|| error(caret, "No SQL statement at the cursor"))
        }
    }
}

pub fn statement_ranges(engine: Engine, sql: &str) -> Result<Vec<Range<usize>>, BoundaryError> {
    if sql.len() > 1024 * 1024 {
        return Err(error(
            0,
            "Current supports buffers up to 1 MiB; select an explicit range or use All",
        ));
    }
    let bytes = sql.as_bytes();
    if let Some(i) = bytes.iter().position(|b| *b == 0) {
        return Err(error(i, "SQL contains a NUL byte"));
    }
    let mut ranges = Vec::new();
    let (mut start, mut i) = (0, 0);
    let mut substantive = false;
    let mut words: Vec<String> = Vec::new();
    let mut trigger = false;
    let mut trigger_separators = 0;
    let mut parentheses = 0usize;
    let mut previous_word = String::new();
    while i < bytes.len() {
        let at = i;
        match bytes[i] {
            b';' => {
                i += 1;
                if parentheses > 0 {
                    continue;
                }
                if trigger {
                    trigger_separators += 1;
                    if trigger_separators > 4096 {
                        return Err(error(
                            start,
                            "Trigger exceeds Current's boundary limit; select its complete SQL explicitly",
                        ));
                    }
                    let candidate = std::ffi::CString::new(&sql[start..i]).expect("NUL checked");
                    // SQLite's native completeness lexer handles trigger BEGIN/END,
                    // CASE expressions and internal semicolons without preparing SQL.
                    if unsafe { libsqlite3_sys::sqlite3_complete(candidate.as_ptr()) } == 0 {
                        continue;
                    }
                }
                if substantive {
                    push_range(sql, start, i, &mut ranges);
                }
                start = i;
                substantive = false;
                words.clear();
                previous_word.clear();
                trigger = false;
                trigger_separators = 0;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-')
                && (engine != Engine::MySql
                    || crate::lex::dash_comment(engine, bytes.get(i + 2))) =>
            {
                i += 2;
                while i < bytes.len() && !crate::lex::line_comment_end(engine, bytes[i]) {
                    i += 1;
                }
            }
            b'#' if engine == Engine::MySql => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                if engine == Engine::MySql
                    && (bytes.get(i + 2) == Some(&b'!') || bytes.get(i + 2..i + 4) == Some(b"M!"))
                {
                    return Err(error(
                        at,
                        "Current cannot infer executable-comment boundaries; select the complete SQL explicitly",
                    ));
                }
                i += 2;
                let mut depth = 1;
                while i < bytes.len() && depth > 0 {
                    if engine == Engine::Postgres && bytes[i..].starts_with(b"/*") {
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
                    return Err(error(at, "Unterminated block comment"));
                }
            }
            q @ (b'\'' | b'"' | b'`' | b'[')
                if (q != b'`' || engine != Engine::Postgres)
                    && (q != b'[' || engine == Engine::Sqlite) =>
            {
                substantive = true;
                let escaped = engine == Engine::Postgres
                    && q == b'\''
                    && i > 0
                    && matches!(bytes[i - 1], b'e' | b'E')
                    && (i < 2 || !ident_byte(bytes[i - 2]));
                let end = if q == b'[' { b']' } else { q };
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && escaped {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'\\'
                        && ((engine == Engine::MySql && q != b'`')
                            || (engine == Engine::Postgres && q == b'\''))
                    {
                        return Err(error(
                            i,
                            "Current cannot infer session-dependent backslash quoting; select the complete SQL explicitly",
                        ));
                    } else if bytes[i] == end {
                        i += 1;
                        if q != b'[' && bytes.get(i) == Some(&end) {
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
                    return Err(error(at, "Unterminated quoted string or identifier"));
                }
            }
            b'$' if engine == Engine::Postgres && (i == 0 || !ident_byte(bytes[i - 1])) => {
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
                        let rest = &sql[end + 1..];
                        let Some(close) = rest.find(delimiter) else {
                            return Err(error(at, "Unterminated PostgreSQL dollar quote"));
                        };
                        substantive = true;
                        i = end + 1 + close + delimiter.len();
                        continue;
                    }
                }
                substantive = true;
                i += 1;
            }
            b'\\' => {
                return Err(error(
                    at,
                    "Client script commands are not interpreted; use native SQL",
                ));
            }
            b'(' => {
                parentheses += 1;
                substantive = true;
                i += 1;
            }
            b')' => {
                if parentheses == 0 {
                    return Err(error(at, "Unmatched closing parenthesis"));
                }
                parentheses -= 1;
                substantive = true;
                i += 1;
            }
            b'.' if !substantive && engine == Engine::Sqlite => {
                return Err(error(
                    at,
                    "SQLite dot commands are not SQL; Current does not interpret client scripts",
                ));
            }
            b if b.is_ascii_alphabetic() || b == b'_' || b >= 128 => {
                i += 1;
                while bytes.get(i).is_some_and(|b| ident_byte(*b)) {
                    i += 1;
                }
                let word = sql[at..i].to_ascii_uppercase();
                if !substantive && word == "DELIMITER" {
                    return Err(error(
                        at,
                        "DELIMITER is a client directive, not SQL; submit routine SQL as an explicit selection without directives",
                    ));
                }
                substantive = true;
                if words.len() < 128 {
                    words.push(word.clone());
                }
                if words.first().is_some_and(|w| w == "CREATE") {
                    if engine == Engine::MySql
                        && matches!(
                            word.as_str(),
                            "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"
                        )
                    {
                        return Err(error(
                            at,
                            "Current does not split MySQL stored programs; select the entire definition without DELIMITER directives",
                        ));
                    }
                    if engine == Engine::Sqlite && word == "TRIGGER" && words.len() <= 3 {
                        trigger = true;
                    }
                }
                if engine == Engine::Postgres && word == "ATOMIC" && previous_word == "BEGIN" {
                    return Err(error(
                        at,
                        "Current does not split BEGIN ATOMIC bodies; select the entire definition explicitly",
                    ));
                }
                if engine == Engine::Postgres
                    && words.first().is_some_and(|w| w == "COPY")
                    && word == "STDIN"
                {
                    return Err(error(
                        at,
                        "COPY FROM STDIN requires a streaming import, not editor script data",
                    ));
                }
                if engine == Engine::MySql
                    && words.first().is_some_and(|w| w == "BEGIN")
                    && words.len() > 1
                    && word != "WORK"
                {
                    return Err(error(
                        at,
                        "Current cannot split a compound BEGIN body; select the entire definition explicitly",
                    ));
                }
                previous_word = word;
            }
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                substantive = true;
                i += 1;
            }
        }
    }
    if parentheses != 0 {
        return Err(error(sql.len(), "Unclosed parenthesis"));
    }
    if substantive {
        if trigger {
            let candidate =
                std::ffi::CString::new(format!("{};", &sql[start..])).expect("NUL checked");
            if unsafe { libsqlite3_sys::sqlite3_complete(candidate.as_ptr()) } == 0 {
                return Err(error(start, "Incomplete SQLite trigger body"));
            }
        }
        push_range(sql, start, sql.len(), &mut ranges);
    }
    Ok(ranges)
}
fn push_range(sql: &str, start: usize, end: usize, ranges: &mut Vec<Range<usize>>) {
    let text = &sql[start..end];
    let start = start + text.len() - text.trim_start().len();
    let end = end - (text.len() - text.trim_end().len());
    ranges.push(start..end);
}
