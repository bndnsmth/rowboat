//! Deliberately small SELECT grammar, not a presentation lexer or SQL executor.
use crate::{
    Engine,
    lex::{dash_comment, ident_byte, line_comment_end, sql_space},
    readonly::LexMode,
};
use anyhow::{Result, bail, ensure};

#[derive(Debug, Clone)]
pub(super) struct Projection {
    pub column: String,
    pub label: String,
}
#[derive(Debug)]
pub(super) struct Select {
    pub schema: Option<String>,
    pub table: String,
    /// None means the sole projection is * or alias.*.
    pub projections: Option<Vec<Projection>>,
    pub table_range: std::ops::Range<usize>,
}
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Ident(String),
    Literal,
    Number(String),
    Symbol(u8),
}
impl Token {
    fn word(&self, s: &str) -> bool {
        matches!(self, Self::Word(w) if w.eq_ignore_ascii_case(s))
    }
}

fn tokens(
    engine: Engine,
    sql: &str,
    mode: LexMode,
) -> Result<(Vec<Token>, Vec<std::ops::Range<usize>>)> {
    ensure!(sql.len() <= 256 * 1024, "Query edit SQL exceeds 256 KiB");
    let b = sql.as_bytes();
    ensure!(!b.contains(&0), "NUL in SQL is unsupported for editing");
    let mut out = Vec::new();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let count = out.len();
        match b[i] {
            c if sql_space(c) => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') && dash_comment(engine, b.get(i + 2)) => {
                while i < b.len() && !line_comment_end(engine, b[i]) {
                    i += 1;
                }
            }
            b'#' if engine == Engine::MySql => {
                while i < b.len() && !line_comment_end(engine, b[i]) {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                ensure!(
                    !matches!(b.get(i + 2), Some(b'!' | b'+'))
                        && !b
                            .get(i + 2..i + 4)
                            .is_some_and(|s| s.eq_ignore_ascii_case(b"M!")),
                    "Executable comments and optimizer hints are unsupported for editing"
                );
                i += 2;
                let mut depth = 1;
                while i < b.len() && depth > 0 {
                    if engine == Engine::Postgres && b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                ensure!(depth == 0, "Unterminated SQL comment");
            }
            q @ (b'\'' | b'"' | b'`') => {
                ensure!(
                    q != b'`' || engine != Engine::Postgres,
                    "Unsupported SQL quote"
                );
                let identifier =
                    q == b'`' || q == b'"' && (engine != Engine::MySql || mode.mysql_ansi_quotes);
                let escapes =
                    engine == Engine::MySql && !identifier && !mode.mysql_no_backslash_escapes;
                i += 1;
                let from = i;
                let mut end = None;
                while i < b.len() {
                    if b[i] == b'\\' && escapes {
                        i = (i + 2).min(b.len());
                    } else if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                        } else {
                            end = Some(i);
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let end = end.ok_or_else(|| anyhow::anyhow!("Unterminated SQL quote"))?;
                out.push(if identifier {
                    let q = q as char;
                    Token::Ident(sql[from..end].replace(&format!("{q}{q}"), &q.to_string()))
                } else {
                    Token::Literal
                });
            }
            c if c.is_ascii_digit() => {
                let from = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                    while b.get(i).is_some_and(u8::is_ascii_digit) {
                        i += 1;
                    }
                }
                out.push(Token::Number(sql[from..i].into()));
            }
            c if ident_byte(c) && c != b'$' => {
                let from = i;
                i += 1;
                while b.get(i).is_some_and(|c| ident_byte(*c)) {
                    i += 1;
                }
                out.push(Token::Word(sql[from..i].into()));
            }
            c @ (b'.' | b',' | b'*' | b'(' | b')' | b'=' | b'<' | b'>' | b'!' | b'-' | b'+'
            | b';') => {
                out.push(Token::Symbol(c));
                i += 1;
            }
            _ => bail!("Unsupported SQL token for editable SELECT"),
        }
        if out.len() > count {
            spans.push(start..i);
        }
    }
    if out.last() == Some(&Token::Symbol(b';')) {
        out.pop();
        spans.pop();
    }
    ensure!(
        !out.contains(&Token::Symbol(b';')),
        "Only one SELECT statement can be edited"
    );
    Ok((out, spans))
}

struct Parser {
    engine: Engine,
    tokens: Vec<Token>,
    at: usize,
    qualifier: Option<String>,
    table: String,
}
impl Parser {
    fn word(&mut self, s: &str) -> bool {
        if self.tokens.get(self.at).is_some_and(|t| t.word(s)) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn symbol(&mut self, s: u8) -> bool {
        if self.tokens.get(self.at) == Some(&Token::Symbol(s)) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn ident(&mut self) -> Result<String> {
        let result = match self.tokens.get(self.at) {
            Some(Token::Word(s)) if !reserved(s) => {
                if self.engine == Engine::Postgres {
                    s.to_ascii_lowercase()
                } else {
                    s.clone()
                }
            }
            Some(Token::Ident(s)) if !s.is_empty() => s.clone(),
            _ => bail!("Expected a direct column or table identifier"),
        };
        self.at += 1;
        Ok(result)
    }
    fn has_ident(&self) -> bool {
        matches!(self.tokens.get(self.at), Some(Token::Word(s)) if !reserved(s))
            || matches!(self.tokens.get(self.at), Some(Token::Ident(_)))
    }
    fn same(&self, a: &str, b: &str) -> bool {
        if self.engine != Engine::Postgres {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }
    fn reference(&mut self) -> Result<String> {
        let first = self.ident()?;
        if self.symbol(b'.') {
            ensure!(
                self.same(&first, self.qualifier.as_deref().unwrap_or(&self.table)),
                "Column qualifier does not identify the single source table"
            );
            self.ident()
        } else {
            Ok(first)
        }
    }
    fn scalar(&mut self) -> Result<()> {
        let signed = self.symbol(b'-') || self.symbol(b'+');
        if matches!(self.tokens.get(self.at), Some(Token::Number(_))) {
            self.at += 1;
            return Ok(());
        }
        ensure!(!signed, "Unsupported signed value");
        if matches!(self.tokens.get(self.at), Some(Token::Literal)) {
            self.at += 1;
            return Ok(());
        }
        if self.word("NULL") || self.word("TRUE") || self.word("FALSE") {
            return Ok(());
        }
        self.reference()?;
        Ok(())
    }
    fn predicate(&mut self, depth: usize) -> Result<()> {
        ensure!(depth <= 32, "WHERE nesting exceeds edit parser limit");
        self.word("NOT");
        if self.symbol(b'(') {
            self.boolean(depth + 1)?;
            ensure!(self.symbol(b')'), "Unbalanced WHERE parentheses");
            return Ok(());
        }
        self.scalar()?;
        if self.word("IS") {
            self.word("NOT");
            ensure!(
                self.word("NULL") || self.word("TRUE") || self.word("FALSE"),
                "Unsupported IS predicate"
            );
        } else {
            let not = self.word("NOT");
            if self.word("IN") {
                ensure!(self.symbol(b'('), "Expected IN value list");
                self.scalar()?;
                while self.symbol(b',') {
                    self.scalar()?;
                }
                ensure!(self.symbol(b')'), "Only scalar IN lists are supported");
            } else if self.word("LIKE") {
                self.scalar()?;
            } else if self.word("BETWEEN") {
                self.scalar()?;
                ensure!(self.word("AND"), "Expected BETWEEN bounds");
                self.scalar()?;
            } else {
                ensure!(!not, "Unsupported NOT predicate");
                if self.symbol(b'=') {
                } else if self.symbol(b'<') {
                    if !self.symbol(b'=') {
                        self.symbol(b'>');
                    }
                } else if self.symbol(b'>') {
                    self.symbol(b'=');
                } else if self.symbol(b'!') {
                    ensure!(self.symbol(b'='), "Unsupported comparison");
                } else {
                    bail!(
                        "WHERE requires direct comparisons; functions and expressions are unsupported"
                    );
                }
                self.scalar()?;
            }
        }
        Ok(())
    }
    fn boolean(&mut self, depth: usize) -> Result<()> {
        self.predicate(depth)?;
        while self.word("AND") || self.word("OR") {
            self.predicate(depth)?;
        }
        Ok(())
    }
    fn count(&mut self) -> Result<()> {
        ensure!(
            matches!(self.tokens.get(self.at), Some(Token::Number(s)) if s.bytes().all(|b| b.is_ascii_digit())),
            "LIMIT/OFFSET requires a nonnegative integer"
        );
        self.at += 1;
        Ok(())
    }
}

fn reserved(s: &str) -> bool {
    if matches!(
        s.to_ascii_uppercase().as_str(),
        "USER"
            | "SESSION_USER"
            | "SYSTEM_USER"
            | "CURRENT_USER"
            | "CURRENT_ROLE"
            | "CURRENT_SCHEMA"
            | "CURRENT_CATALOG"
            | "CURRENT_DATE"
            | "CURRENT_TIME"
            | "CURRENT_TIMESTAMP"
            | "LOCALTIME"
            | "LOCALTIMESTAMP"
            | "UTC_DATE"
            | "UTC_TIME"
            | "UTC_TIMESTAMP"
            | "HIGH_PRIORITY"
            | "SQL_SMALL_RESULT"
            | "SQL_BIG_RESULT"
            | "SQL_BUFFER_RESULT"
            | "SQL_NO_CACHE"
            | "SQL_CACHE"
            | "DISTINCTROW"
    ) {
        return true;
    }
    matches!(
        s.to_ascii_uppercase().as_str(),
        "SELECT"
            | "FROM"
            | "WHERE"
            | "AS"
            | "ORDER"
            | "BY"
            | "LIMIT"
            | "OFFSET"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "NATURAL"
            | "ON"
            | "USING"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "WITH"
            | "DISTINCT"
            | "ALL"
            | "GROUP"
            | "HAVING"
            | "WINDOW"
            | "INTO"
            | "FOR"
            | "LOCK"
            | "PROCEDURE"
            | "AND"
            | "OR"
            | "NOT"
            | "NULL"
            | "TRUE"
            | "FALSE"
            | "IS"
            | "IN"
            | "LIKE"
            | "BETWEEN"
            | "ASC"
            | "DESC"
            | "NULLS"
            | "FIRST"
            | "LAST"
            | "FETCH"
            | "ONLY"
            | "TABLESAMPLE"
            | "PARTITION"
            | "SQL_CALC_FOUND_ROWS"
            | "STRAIGHT_JOIN"
    )
}

pub(super) fn parse(engine: Engine, sql: &str, mode: LexMode) -> Result<Select> {
    let (tokens, spans) = tokens(engine, sql, mode)?;
    let mut p = Parser {
        engine,
        tokens,
        at: 0,
        qualifier: None,
        table: String::new(),
    };
    ensure!(
        p.word("SELECT"),
        "Editing supports a single base-table SELECT, not CTEs or other statements"
    );
    let projection_start = p.at;
    let from = p
        .tokens
        .iter()
        .position(|t| t.word("FROM"))
        .ok_or_else(|| anyhow::anyhow!("SELECT must name one base table"))?;
    p.at = from + 1;
    let table_start = p.at;
    let first = p.ident()?;
    let (schema, table) = if p.symbol(b'.') {
        (Some(first), p.ident()?)
    } else {
        (None, first)
    };
    let table_range = spans[table_start].start..spans[p.at - 1].end;
    p.table = table.clone();
    if p.word("AS") || p.has_ident() {
        p.qualifier = Some(p.ident()?);
    }
    let tail_start = p.at;
    p.at = projection_start;
    let mut projections = Vec::new();
    let mut wildcard = false;
    loop {
        if p.symbol(b'*') {
            wildcard = true;
        } else {
            let first = p.ident()?;
            let column = if p.symbol(b'.') {
                ensure!(
                    p.same(&first, p.qualifier.as_deref().unwrap_or(&table)),
                    "Projection qualifier does not identify the source table"
                );
                if p.symbol(b'*') {
                    wildcard = true;
                    String::new()
                } else {
                    p.ident()?
                }
            } else {
                first
            };
            if !wildcard {
                let label = if p.word("AS") || p.has_ident() {
                    p.ident()?
                } else {
                    column.clone()
                };
                projections.push(Projection { column, label });
            }
        }
        if !p.symbol(b',') {
            break;
        }
        ensure!(!wildcard, "Mixed wildcard projections are unsupported");
    }
    ensure!(
        p.at == from,
        "Only direct column projections and optional aliases are editable"
    );
    ensure!(
        !wildcard || projections.is_empty(),
        "Mixed wildcard projections are unsupported"
    );
    p.at = tail_start;
    if p.word("WHERE") {
        p.boolean(0)?;
    }
    if p.word("ORDER") {
        ensure!(p.word("BY"), "Expected ORDER BY");
        loop {
            if matches!(p.tokens.get(p.at), Some(Token::Number(_))) {
                p.count()?;
            } else {
                p.reference()?;
            }
            if !p.word("ASC") {
                p.word("DESC");
            }
            if engine == Engine::Postgres && p.word("NULLS") {
                ensure!(
                    p.word("FIRST") || p.word("LAST"),
                    "Expected NULLS FIRST/LAST"
                );
            }
            if !p.symbol(b',') {
                break;
            }
        }
    }
    if p.word("LIMIT") {
        p.count()?;
        if engine == Engine::MySql && p.symbol(b',') {
            p.count()?;
        }
    }
    if p.word("OFFSET") {
        p.count()?;
    }
    ensure!(
        p.at == p.tokens.len(),
        "Joins, expressions, functions, aggregation and other SELECT forms are read-only"
    );
    Ok(Select {
        schema,
        table,
        table_range,
        projections: if wildcard { None } else { Some(projections) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_projection_grammar_and_dialect_quotes() {
        for engine in [Engine::Postgres, Engine::MySql] {
            for sql in [
                "SELECT * FROM records",
                "select t.* from app.records AS t WHERE t.id >= 2 AND (flag = 'x' OR flag IS NULL) ORDER BY t.id DESC LIMIT 10 OFFSET 2",
                "SELECT t.id AS key_id, t.part second_key, payload FROM app.records t WHERE id IN (1,2) AND part BETWEEN 3 AND 8",
            ] {
                assert!(parse(engine, sql, LexMode::default()).is_ok(), "{sql}");
            }
        }
        let p = parse(
            Engine::Postgres,
            "SELECT \"MiX\" AS \"Alias\" FROM \"Odd\".\"T\"",
            LexMode::default(),
        )
        .unwrap();
        assert_eq!(p.schema.as_deref(), Some("Odd"));
        assert_eq!(p.projections.unwrap()[0].column, "MiX");
        let p = parse(
            Engine::MySql,
            "SELECT `a``b` AS `label` FROM `TableCase`",
            LexMode::default(),
        )
        .unwrap();
        assert_eq!(p.table, "TableCase");
        assert_eq!(p.projections.unwrap()[0].column, "a`b");
    }
    #[test]
    fn rejects_ambiguous_and_executable_forms() {
        for engine in [Engine::Postgres, Engine::MySql] {
            for sql in [
                "SELECT id+1 AS id FROM t",
                "SELECT count(*) FROM t",
                "SELECT DISTINCT * FROM t",
                "SELECT * FROM t JOIN u ON t.id=u.id",
                "SELECT * FROM t,u",
                "SELECT * FROM t UNION SELECT * FROM u",
                "WITH x AS (SELECT * FROM t) SELECT * FROM x",
                "SELECT * FROM f()",
                "SELECT f(id) FROM t",
                "SELECT * FROM t WHERE f(id)=1",
                "SELECT * FROM t WHERE id IN (SELECT id FROM u)",
                "SELECT * FROM t; UPDATE t SET id=1",
                "SELECT * FROM t /*! WHERE id=1 */",
                "SELECT * FROM t /*+ hint */",
                "SELECT * FROM t /*",
                "SELECT x.id FROM t",
                "SELECT *,id FROM t",
                "SELECT * FROM t FOR UPDATE",
                "SELECT * INTO other FROM t",
                "SELECT @x FROM t",
                "SELECT * FROM t WHERE id=E'escaped\\'text'",
                "SELECT * FROM t WHERE id=$$dollar$$",
                "SELECT * FROM t WHERE id='continued'\n'literal'",
            ] {
                assert!(
                    parse(engine, sql, LexMode::default()).is_err(),
                    "{engine:?}: {sql}"
                );
            }
        }
        for sql in [
            "SELECT CURRENT_USER AS id FROM t",
            "SELECT CURRENT_DATE FROM t",
            "SELECT SQL_NO_CACHE id FROM t",
            "SELECT HIGH_PRIORITY id FROM t",
        ] {
            for engine in [Engine::Postgres, Engine::MySql] {
                assert!(parse(engine, sql, LexMode::default()).is_err(), "{sql}");
            }
        }
    }
    #[test]
    fn dialect_comments_cannot_hide_projection_or_extra_statements() {
        assert!(
            parse(
                Engine::Postgres,
                "SELECT id /*outer /*inner*/ done*/ FROM t --x\r; DELETE FROM t",
                LexMode::default()
            )
            .is_err()
        );
        assert!(
            parse(
                Engine::Postgres,
                "SELECT id /*outer /*inner*/ done*/ FROM t",
                LexMode::default()
            )
            .is_ok()
        );
        assert!(
            parse(
                Engine::MySql,
                "SELECT id --not_comment\n FROM t",
                LexMode::default()
            )
            .is_err()
        );
        assert!(
            parse(
                Engine::MySql,
                "SELECT id FROM t #comment\n; DELETE FROM t",
                LexMode::default()
            )
            .is_err()
        );
        assert!(
            parse(
                Engine::MySql,
                "SELECT id FROM t /*M! UNION SELECT id FROM u */",
                LexMode::default()
            )
            .is_err()
        );
        assert!(parse(Engine::MySql, "SELECT \"id\" FROM t", LexMode::default()).is_err());
        assert!(
            parse(
                Engine::MySql,
                "SELECT \"id\" FROM t",
                LexMode {
                    mysql_ansi_quotes: true,
                    ..Default::default()
                }
            )
            .is_ok()
        );
    }
}
