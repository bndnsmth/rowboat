//! Conservative editor policy, applied to a whole batch before executing any of it.
//! Tokens distinguish keywords from comments, strings and quoted identifiers.
//! Server READ ONLY transactions remain the actual database-write backstop.
use crate::Engine;
use crate::lex::{dash_comment, ident_byte, line_comment_end, pg_quote_continuation, sql_space};
use crate::sql::quote_ident;
use anyhow::{Result, bail, ensure};

#[derive(Clone, Copy, Default)]
pub(crate) struct LexMode {
    pub mysql_no_backslash_escapes: bool,
    pub mysql_ansi_quotes: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Read,
    Begin,
    Commit { chain: bool },
    Rollback { chain: bool },
    Savepoint,
}

#[derive(Debug)]
pub(crate) struct Statement {
    pub sql: String,
    pub action: Action,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    Literal,
    Symbol(u8),
}
impl Token {
    fn word(&self, value: &str) -> bool {
        matches!(self,Self::Word(w) if w==value)
    }
    fn identifier(&self) -> bool {
        matches!(self, Self::Word(_) | Self::Quoted(_))
    }
    fn identifier_name(&self, engine: Engine) -> String {
        match self {
            Self::Word(word) if engine == Engine::Postgres => word.to_ascii_lowercase(),
            Self::Word(word) | Self::Quoted(word) => word.clone(),
            _ => unreachable!("validated identifier"),
        }
    }
}

pub(crate) fn statements(engine: Engine, sql: &str, mode: LexMode) -> Result<Vec<Statement>> {
    parse(engine, sql, mode, false)
}

fn parse(engine: Engine, sql: &str, mode: LexMode, sample_only: bool) -> Result<Vec<Statement>> {
    let b = sql.as_bytes();
    ensure!(!b.contains(&0), "SQL text may not contain NUL bytes");
    let mut i = 0;
    let mut start = 0;
    let mut tokens = Vec::new();
    let mut result = Vec::new();
    while i < b.len() {
        match b[i] {
            c if sql_space(c) => i += 1,
            b';' => {
                if !tokens.is_empty() {
                    result.push(classify(engine, &sql[start..i], &tokens, sample_only)?);
                }
                tokens.clear();
                i += 1;
                start = i;
            }
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
                    engine != Engine::MySql
                        || !(b.get(i + 2) == Some(&b'!')
                            || b.get(i + 2..i + 4)
                                .is_some_and(|s| s.eq_ignore_ascii_case(b"M!"))),
                    "Read-only editor does not allow executable MySQL comments"
                );
                i += 2;
                let mut depth = 1;
                while i < b.len() && depth > 0 {
                    if b[i..].starts_with(b"/*") && engine == Engine::Postgres {
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
            quote @ (b'\'' | b'"' | b'`') if quote != b'`' || engine == Engine::MySql => {
                let is_identifier = quote == b'`'
                    || (quote == b'"' && (engine == Engine::Postgres || mode.mysql_ansi_quotes));
                let escape = if engine == Engine::MySql {
                    !is_identifier && !mode.mysql_no_backslash_escapes
                } else {
                    quote == b'\''
                        && i > 0
                        && matches!(b[i - 1], b'e' | b'E')
                        && (i < 2 || !ident_byte(b[i - 2]))
                };
                let from = i + 1;
                i += 1;
                let mut end = None;
                while i < b.len() {
                    if b[i] == b'\\' && escape {
                        i = (i + 2).min(b.len());
                    } else if b[i] == quote {
                        if b.get(i + 1) == Some(&quote) {
                            i += 2;
                        } else if engine == Engine::Postgres
                            && quote == b'\''
                            && let Some(next) = pg_quote_continuation(b, i + 1)
                        {
                            i = next + 1;
                        } else {
                            end = Some(i);
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let Some(end) = end else {
                    bail!("Unterminated SQL quote")
                };
                if is_identifier {
                    let q = quote as char;
                    tokens.push(Token::Quoted(
                        sql[from..end].replace(&format!("{q}{q}"), &q.to_string()),
                    ));
                } else {
                    tokens.push(Token::Literal);
                }
            }
            b'$' if engine == Engine::Postgres => {
                let mut end = i + 1;
                while b
                    .get(end)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c >= 128)
                {
                    end += 1;
                }
                if b.get(end) == Some(&b'$') && (end == i + 1 || !b[i + 1].is_ascii_digit()) {
                    let tag = &sql[i..=end];
                    let Some(close) = sql[end + 1..].find(tag) else {
                        bail!("Unterminated dollar quote")
                    };
                    i = end + 1 + close + tag.len();
                    tokens.push(Token::Literal);
                } else {
                    tokens.push(Token::Symbol(b'$'));
                    i += 1;
                }
            }
            c if ident_byte(c) => {
                let from = i;
                i += 1;
                while b.get(i).is_some_and(|c| ident_byte(*c)) {
                    i += 1;
                }
                tokens.push(Token::Word(sql[from..i].to_ascii_uppercase()));
            }
            c => {
                tokens.push(Token::Symbol(c));
                i += 1;
            }
        }
    }
    if !tokens.is_empty() {
        result.push(classify(engine, &sql[start..], &tokens, sample_only)?);
    }
    Ok(result)
}

/// Sampling accepts SELECT only (including read-only SELECT CTEs). Reuse the
/// dialect lexer rather than trusting prefixes or stripping comments with regex.
pub(crate) fn sample_statement(engine: Engine, sql: &str, mode: LexMode) -> Result<String> {
    ensure!(sql.len() <= 64 * 1024, "Sample SQL exceeds 64 KiB");
    let parsed = parse(engine, sql, mode, true)?;
    ensure!(
        parsed.len() == 1,
        "Sampling requires exactly one SELECT statement"
    );
    let statement = parsed.into_iter().next().expect("one statement");
    ensure!(
        statement.action == Action::Read,
        "Sampling requires a SELECT statement"
    );
    Ok(statement.sql)
}

fn classify(engine: Engine, sql: &str, tokens: &[Token], sample_only: bool) -> Result<Statement> {
    let Some(Token::Word(first)) = tokens.first() else {
        bail!("Read-only editor requires an allowed SQL command")
    };
    if sample_only {
        ensure!(
            matches!(first.as_str(), "SELECT" | "WITH"),
            "Sampling requires SELECT or WITH SELECT"
        );
        if first == "WITH" {
            let mut depth = 0usize;
            let mut select = false;
            for token in tokens {
                match token {
                    Token::Symbol(b'(') => depth += 1,
                    Token::Symbol(b')') => depth = depth.saturating_sub(1),
                    token if depth == 0 && token.word("SELECT") => select = true,
                    _ => {}
                }
            }
            ensure!(select, "Sampling WITH requires a top-level SELECT");
        }
        ensure!(
            !tokens.iter().any(|token| matches!(token, Token::Word(word) if matches!(word.as_str(),
                "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "CALL" | "COPY" | "INTO" | "LOCK" | "FOR"))),
            "Sampling does not allow mutations, output, or locking SELECTs"
        );
    }
    // Encoded identifiers make inspection of session-mutating function names ambiguous.
    // Reject this optional syntax rather than trying to implement Unicode SQL escapes.
    for pair in tokens.windows(2) {
        ensure!(
            !(pair[0].word("U") && pair[1] == Token::Symbol(b'&')),
            "Unicode-escaped SQL identifiers/literals are not supported by the read-only editor"
        );
        if matches!(&pair[0],Token::Word(w)|Token::Quoted(w) if w.eq_ignore_ascii_case("SET_CONFIG"))
            && pair[1] == Token::Symbol(b'(')
        {
            bail!("Read-only editor does not allow set_config session mutation");
        }
    }
    let mut normalized = None;
    let action = match first.as_str() {
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" => {
            ensure!(
                !tokens.iter().any(|t| t.word("INTO")),
                "Read-only editor does not allow SELECT INTO or file output"
            );
            Action::Read
        }
        "BEGIN" | "START" => {
            let mut rest = &tokens[1..];
            if first == "START" {
                ensure!(
                    rest.first().is_some_and(|t| t.word("TRANSACTION")),
                    "Only START TRANSACTION is allowed"
                );
                rest = &rest[1..];
            } else if rest
                .first()
                .is_some_and(|t| t.word("WORK") || t.word("TRANSACTION"))
            {
                rest = &rest[1..];
            }
            ensure!(
                rest.is_empty()
                    || (rest.len() == 2 && rest[0].word("READ") && rest[1].word("ONLY")),
                "Read-only editor permits BEGIN/START TRANSACTION with READ ONLY, never READ WRITE or session mode overrides"
            );
            Action::Begin
        }
        "COMMIT" | "END" | "ROLLBACK" | "ABORT" => {
            let rollback = matches!(first.as_str(), "ROLLBACK" | "ABORT");
            let mut rest = &tokens[1..];
            if rest
                .first()
                .is_some_and(|t| t.word("WORK") || t.word("TRANSACTION"))
            {
                rest = &rest[1..];
            }
            if rollback && rest.first().is_some_and(|t| t.word("TO")) {
                rest = &rest[1..];
                if rest.first().is_some_and(|t| t.word("SAVEPOINT")) {
                    rest = &rest[1..];
                }
                ensure!(
                    rest.len() == 1 && rest[0].identifier(),
                    "Invalid ROLLBACK TO savepoint"
                );
                normalized = Some(format!(
                    "ROLLBACK TO SAVEPOINT {}",
                    quote_ident(engine, &rest[0].identifier_name(engine))
                ));
                Action::Savepoint
            } else {
                let chain = if rest.is_empty() {
                    false
                } else if rest.len() == 2 && rest[0].word("AND") && rest[1].word("CHAIN") {
                    true
                } else if rest.len() == 3
                    && rest[0].word("AND")
                    && rest[1].word("NO")
                    && rest[2].word("CHAIN")
                {
                    false
                } else {
                    bail!("Unsupported transaction control in read-only editor")
                };
                if rollback {
                    Action::Rollback { chain }
                } else {
                    Action::Commit { chain }
                }
            }
        }
        "SAVEPOINT" | "RELEASE" => {
            let mut rest = &tokens[1..];
            if first == "RELEASE" && rest.first().is_some_and(|t| t.word("SAVEPOINT")) {
                rest = &rest[1..];
            }
            ensure!(
                rest.len() == 1 && rest[0].identifier(),
                "Invalid savepoint command"
            );
            normalized = Some(format!(
                "{} {}",
                if first == "RELEASE" {
                    "RELEASE SAVEPOINT"
                } else {
                    "SAVEPOINT"
                },
                quote_ident(engine, &rest[0].identifier_name(engine))
            ));
            Action::Savepoint
        }
        _ => bail!(
            "Read-only editor does not allow {first}; session controls, writes and unclassified commands are blocked"
        ),
    };
    Ok(Statement {
        // MySQL does not support preparing SAVEPOINT commands. Emit only our
        // canonical command plus one dialect-quoted identifier, never raw input.
        sql: normalized.unwrap_or_else(|| sql.trim().into()),
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sampler_requires_one_select_without_hidden_controls() {
        for engine in [Engine::Postgres, Engine::MySql] {
            for sql in [
                "WITH x AS (SELECT 1) SELECT * FROM x",
                "SELECT 'COMMIT; INSERT INTO x' AS n; -- ignored",
                "/* SELECT */ SELECT 1",
            ] {
                assert!(sample_statement(engine, sql, LexMode::default()).is_ok());
            }
            for sql in [
                "SELECT 1; COMMIT",
                "SELECT 1 -- control\r\n; COMMIT",
                "WITH x AS (SELECT 1) TABLE x",
                "WITH x AS (DELETE FROM x RETURNING *) SELECT * FROM x",
                "EXPLAIN ANALYZE SELECT 1",
                "SELECT 1 INTO OUTFILE 'x'",
                "SELECT 1 FOR SHARE",
                "SELECT pg_catalog.set_config('x','y',false)",
            ] {
                assert!(
                    sample_statement(engine, sql, LexMode::default()).is_err(),
                    "{sql}"
                );
            }
            let boundary = format!("SELECT 1{}", " ".repeat(64 * 1024 - 8));
            assert!(sample_statement(engine, &boundary, LexMode::default()).is_ok());
            assert!(sample_statement(engine, &(boundary + " "), LexMode::default()).is_err());
        }
    }

    #[test]
    fn hidden_line_comment_batches_are_rejected() {
        for newline in ["\r", "\n", "\r\n"] {
            let payload = format!(
                "SELECT 1 -- hidden{newline}; COMMIT; START TRANSACTION READ WRITE; CREATE TABLE escaped(id integer)"
            );
            assert!(
                statements(Engine::Postgres, &payload, LexMode::default()).is_err(),
                "{payload:?}"
            );
        }
        for marker in [
            "# '", "-- '", "--\t'", "--\x01'", "--\x0b'", "--\x0c'", "--\x1f'", "--\x7f'",
        ] {
            for newline in ["\n", "\r\n"] {
                let payload = format!(
                    "SELECT 1 {marker}{newline}; COMMIT; START TRANSACTION READ WRITE; CREATE TABLE escaped(id integer); -- '"
                );
                assert!(
                    statements(Engine::MySql, &payload, LexMode::default()).is_err(),
                    "{payload:?}"
                );
            }
            // MySQL LF-only comments must not turn commented-out commands into
            // real commands merely because a bare CR occurs inside the comment.
            let commented = format!("SELECT 1 {marker}\r; COMMIT; START TRANSACTION READ WRITE");
            assert_eq!(
                statements(Engine::MySql, &commented, LexMode::default())
                    .unwrap()
                    .len(),
                1
            );
        }
        assert!(
            statements(
                Engine::Postgres,
                "SELECT 1 ` 2; COMMIT; START TRANSACTION READ WRITE; SELECT 3 ` 4",
                LexMode::default()
            )
            .is_err()
        );
        for engine in [Engine::Postgres, Engine::MySql] {
            assert!(
                statements(
                    engine,
                    "SELECT 1 -- hidden\0; COMMIT; START TRANSACTION READ WRITE",
                    LexMode::default()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn savepoint_controls_are_rebuilt_from_one_quoted_identifier() {
        for engine in [Engine::Postgres, Engine::MySql] {
            let name = quote_ident(engine, "Mixed; COMMIT; \"quoted`name");
            let sql =
                format!("SAVEPOINT {name} -- ignored\n; ROLLBACK WORK TO {name}; RELEASE {name}");
            let parsed = statements(engine, &sql, LexMode::default()).unwrap();
            assert_eq!(parsed[0].sql, format!("SAVEPOINT {name}"));
            assert_eq!(parsed[1].sql, format!("ROLLBACK TO SAVEPOINT {name}"));
            assert_eq!(parsed[2].sql, format!("RELEASE SAVEPOINT {name}"));
        }
    }

    #[test]
    fn continued_postgres_escape_strings_keep_their_escape_mode() {
        for gap in ["\r", "\n", "\r\n", " -- continued\r"] {
            let sql =
                format!("SELECT E'a'{gap}'\\'; COMMIT; START TRANSACTION READ WRITE;--'; SELECT 2");
            let parsed = statements(Engine::Postgres, &sql, LexMode::default()).unwrap();
            assert_eq!(parsed.len(), 2, "{sql:?}");
            assert!(parsed.iter().all(|s| s.action == Action::Read));
        }
    }
    #[test]
    fn controls_are_tokens_not_substrings() {
        for engine in [Engine::Postgres, Engine::MySql] {
            assert!(
                statements(
                    engine,
                    "/* BEGIN READ WRITE */ SELECT 'SET transaction_read_only=off; COMMIT'",
                    LexMode::default()
                )
                .is_ok()
            );
            for sql in [
                "START /* hidden */ TRANSACTION READ WRITE; SELECT 1",
                "BEGIN WORK READ WRITE",
                "SET SESSION TRANSACTION READ WRITE",
                "RESET ALL",
                "DISCARD ALL",
                "SELECT 1; SET transaction_read_only=off",
                "SELECT pg_catalog.set_config('transaction_read_only','off',false)",
            ] {
                assert!(
                    statements(engine, sql, LexMode::default()).is_err(),
                    "{engine:?}: {sql}"
                );
            }
        }
        assert!(
            statements(
                Engine::MySql,
                "/*!80000 SET SESSION TRANSACTION READ WRITE */",
                LexMode::default()
            )
            .is_err()
        );
        assert!(
            statements(
                Engine::Postgres,
                "SELECT pg_catalog.\"set_config\"('transaction_read_only','off',false)",
                LexMode::default()
            )
            .is_err()
        );
        assert!(
            statements(
                Engine::Postgres,
                "SELECT $$SET x=1;$$; BEGIN; COMMIT AND CHAIN; ROLLBACK",
                LexMode::default()
            )
            .is_ok()
        );
    }
    #[test]
    fn mysql_string_modes_cannot_hide_controls() {
        let no_escapes = LexMode {
            mysql_no_backslash_escapes: true,
            ..Default::default()
        };
        assert!(
            statements(
                Engine::MySql,
                "SELECT '\\'; SET SESSION TRANSACTION READ WRITE; -- ' ",
                no_escapes
            )
            .is_err()
        );
        assert!(statements(Engine::MySql, "SELECT 'BEGIN READ WRITE'", no_escapes).is_ok());
        assert_eq!(statements(Engine::Postgres,"BEGIN TRANSACTION READ ONLY; SAVEPOINT \"x;y\"; ROLLBACK WORK TO SAVEPOINT \"x;y\"; RELEASE SAVEPOINT \"x;y\"; END",LexMode::default()).unwrap().len(),5);
    }
}
