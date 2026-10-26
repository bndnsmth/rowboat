//! Deliberately small grammar, not a general SQL rewriter. Every emitted
//! identifier is either a verified parameter/column or the one FROM relation.
use crate::query::{SpanKind, spans};
use anyhow::{Context, Result, ensure};
use rowboat_core::{Engine, sql::quote_ident};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Ident(String),
    Literal(String),
    Number(String),
    Symbol(String),
}
impl Token {
    fn word(&self, s: &str) -> bool {
        matches!(self,Self::Word(w) if w.eq_ignore_ascii_case(s))
    }
    fn symbol(&self, s: &str) -> bool {
        matches!(self,Self::Symbol(w) if w==s)
    }
    fn ident(&self, engine: Engine) -> Option<String> {
        match self {
            Self::Ident(s) => Some(s.clone()),
            Self::Word(s) => Some(if engine == Engine::Postgres {
                s.to_ascii_lowercase()
            } else {
                s.clone()
            }),
            _ => None,
        }
    }
}
fn tokens(engine: Engine, sql: &str) -> Result<Vec<Token>> {
    ensure!(
        sql.len() <= 64 * 1024,
        "Definition exceeds bounded 64 KiB grammar"
    );
    ensure!(
        !sql.contains(['\\', '\r', '\0']),
        "Mode-dependent escapes/control characters block definition synchronization"
    );
    let mut out = vec![];
    for span in spans(engine, sql)? {
        let s = &sql[span.start..span.end];
        match span.kind {
            SpanKind::Comment => {
                ensure!(
                    !s.starts_with("/*+"),
                    "Optimizer hints are not portable definition trivia"
                );
            }
            SpanKind::Quoted => {
                if s.starts_with('\'') {
                    out.push(Token::Literal(s.into()));
                } else {
                    let q = if engine == Engine::Postgres { '"' } else { '`' };
                    ensure!(
                        s.starts_with(q) && s.ends_with(q),
                        "Unsupported quoted body/string form"
                    );
                    out.push(Token::Ident(
                        s[1..s.len() - 1].replace(&format!("{q}{q}"), &q.to_string()),
                    ));
                }
            }
            SpanKind::Code => {
                let mut chars = s.char_indices().peekable();
                while let Some((start, c)) = chars.next() {
                    if c.is_whitespace() {
                        continue;
                    }
                    if c.is_alphabetic() || c == '_' || c == '$' {
                        let mut end = start + c.len_utf8();
                        while let Some(&(i, next)) = chars.peek() {
                            if next.is_alphanumeric() || next == '_' || next == '$' {
                                end = i + next.len_utf8();
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        out.push(Token::Word(s[start..end].into()));
                    } else if c.is_ascii_digit() {
                        let mut end = start + 1;
                        while let Some(&(i, next)) = chars.peek() {
                            if next.is_ascii_digit() || next == '.' {
                                end = i + 1;
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        let value = &s[start..end];
                        ensure!(
                            value.bytes().filter(|b| *b == b'.').count() <= 1,
                            "Unsupported numeric literal"
                        );
                        out.push(Token::Number(value.into()));
                    } else {
                        let mut symbol = c.to_string();
                        if let Some(&(_, next)) = chars.peek() {
                            let pair = format!("{c}{next}");
                            if ["<=", ">=", "<>", "!=", "::"].contains(&pair.as_str()) {
                                symbol = pair;
                                chars.next();
                            }
                        }
                        ensure!(
                            [
                                "(", ")", ",", ".", "+", "-", "*", "/", "%", "=", "<", ">", "<=",
                                ">=", "<>", "!=", "::", ";"
                            ]
                            .contains(&symbol.as_str()),
                            "Unsupported SQL operator {symbol}"
                        );
                        out.push(Token::Symbol(symbol));
                    }
                }
            }
        }
    }
    ensure!(out.len() <= 8192, "Definition exceeds token bound");
    Ok(out)
}

pub(super) struct View {
    pub relation: Option<(Option<String>, String)>,
    tokens: Vec<Token>,
    projections: Vec<(usize, usize)>,
    filter: Option<(usize, usize)>,
    alias: Option<String>,
}
pub(super) fn view(engine: Engine, sql: &str) -> Result<View> {
    let mut t = tokens(engine, sql)?;
    if t.last().is_some_and(|t| t.symbol(";")) {
        t.pop();
    }
    ensure!(
        t.first().is_some_and(|t| t.word("SELECT")),
        "Only bounded SELECT views are supported"
    );
    let mut depth = 0;
    let mut from = None;
    let mut where_at = None;
    let mut cuts = vec![1];
    for (i, token) in t.iter().enumerate().skip(1) {
        if token.symbol("(") {
            depth += 1;
            ensure!(depth <= 64, "Expression nesting exceeds bound");
        }
        if token.symbol(")") {
            ensure!(depth > 0, "Unbalanced expression");
            depth -= 1;
        }
        if depth == 0 && token.word("FROM") {
            ensure!(from.is_none(), "Multiple FROM clauses unsupported");
            from = Some(i);
        }
        if depth == 0 && token.word("WHERE") {
            ensure!(where_at.is_none(), "Multiple WHERE clauses unsupported");
            where_at = Some(i);
        }
        if depth == 0 && token.symbol(",") && from.is_none() && where_at.is_none() {
            cuts.push(i + 1);
        }
    }
    ensure!(depth == 0, "Unbalanced expression");
    let projection_end = from.or(where_at).unwrap_or(t.len());
    let mut projections = vec![];
    for (i, start) in cuts.iter().enumerate() {
        projections.push((
            *start,
            if i + 1 < cuts.len() {
                cuts[i + 1] - 1
            } else {
                projection_end
            },
        ));
    }
    let mut relation = None;
    let mut alias = None;
    if let Some(at) = from {
        let end = where_at.unwrap_or(t.len());
        ensure!(end > at + 1, "Missing FROM relation");
        let mut p = at + 1;
        let first = t[p]
            .ident(engine)
            .context("Expected a base table identifier")?;
        p += 1;
        let (schema, name) = if t.get(p).is_some_and(|t| t.symbol(".")) {
            p += 1;
            let name = t
                .get(p)
                .and_then(|t| t.ident(engine))
                .context("Missing qualified relation")?;
            p += 1;
            (Some(first), name)
        } else {
            (None, first)
        };
        if p < end && t[p].word("AS") {
            p += 1;
        }
        if p < end {
            alias = Some(t[p].ident(engine).context("Invalid relation alias")?);
            p += 1;
        }
        ensure!(
            p == end,
            "Only one base table is supported; joins/subqueries/table functions block"
        );
        relation = Some((schema, name));
    }
    let filter = where_at.map(|at| (at + 1, t.len()));
    Ok(View {
        relation,
        tokens: t,
        projections,
        filter,
        alias,
    })
}
struct Expression<'a> {
    engine: Engine,
    t: &'a [Token],
    pos: usize,
    names: &'a [String],
    positional_count: u64,
    relation: Option<(&'a str, &'a str, Option<&'a str>)>,
    depth: usize,
}
impl Expression<'_> {
    fn expr(&mut self, min: u8) -> Result<String> {
        self.depth += 1;
        ensure!(self.depth <= 64, "Expression nesting exceeds bound");
        let mut left = self.atom()?;
        while let Some(token) = self.t.get(self.pos) {
            if token.symbol("::") {
                ensure!(self.engine == Engine::Postgres, "Cast syntax unsupported");
                self.pos += 1;
                let ty = self
                    .t
                    .get(self.pos)
                    .and_then(|t| t.ident(self.engine))
                    .context("Expected built-in cast type")?;
                ensure!(
                    [
                        "integer", "int4", "bigint", "int8", "smallint", "int2", "numeric", "text",
                        "boolean", "bool"
                    ]
                    .contains(&ty.as_str()),
                    "Only built-in scalar casts supported"
                );
                self.pos += 1;
                left = format!("({left})::{ty}");
                continue;
            }
            let op = match token {
                Token::Word(w)
                    if ["AND", "OR", "IS"]
                        .iter()
                        .any(|s| w.eq_ignore_ascii_case(s)) =>
                {
                    w.to_ascii_uppercase()
                }
                Token::Symbol(s)
                    if [
                        "+", "-", "*", "/", "%", "=", "<", ">", "<=", ">=", "<>", "!=",
                    ]
                    .contains(&s.as_str()) =>
                {
                    s.clone()
                }
                _ => break,
            };
            let precedence = match op.as_str() {
                "OR" => 1,
                "AND" => 2,
                "IS" | "=" | "<" | ">" | "<=" | ">=" | "<>" | "!=" => 3,
                "+" | "-" => 4,
                _ => 5,
            };
            if precedence < min {
                break;
            }
            self.pos += 1;
            if op == "IS" {
                let not = self.t.get(self.pos).is_some_and(|t| t.word("NOT"));
                if not {
                    self.pos += 1;
                }
                ensure!(
                    self.t.get(self.pos).is_some_and(|t| t.word("NULL")),
                    "Only IS [NOT] NULL supported"
                );
                self.pos += 1;
                left = format!("({left} IS {}NULL)", if not { "NOT " } else { "" });
            } else {
                let right = self.expr(precedence + 1)?;
                left = format!("({left} {op} {right})");
            }
        }
        self.depth -= 1;
        Ok(left)
    }
    fn atom(&mut self) -> Result<String> {
        let token = self
            .t
            .get(self.pos)
            .context("Missing expression operand")?
            .clone();
        self.pos += 1;
        if token.symbol("(") {
            let result = self.expr(0)?;
            ensure!(
                self.t.get(self.pos).is_some_and(|t| t.symbol(")")),
                "Expected closing parenthesis"
            );
            self.pos += 1;
            return Ok(result);
        }
        if token.symbol("+") || token.symbol("-") || token.word("NOT") {
            let op = if token.word("NOT") {
                "NOT"
            } else if token.symbol("+") {
                "+"
            } else {
                "-"
            };
            return Ok(format!(
                "({op} {})",
                self.expr(if op == "NOT" { 3 } else { 6 })?
            ));
        }
        match token {
            Token::Literal(s) | Token::Number(s) => Ok(s),
            Token::Word(ref s)
                if ["NULL", "TRUE", "FALSE"]
                    .iter()
                    .any(|w| s.eq_ignore_ascii_case(w)) =>
            {
                Ok(s.to_ascii_uppercase())
            }
            _ => {
                // A quoted "$2" is an ordinary name, not positional argument 2.
                // Keep token kind until after validation and emission; named and
                // positional namespaces must never share one allow-list.
                if self.engine == Engine::Postgres
                    && let Token::Word(word) = &token
                    && let Some(digits) = word.strip_prefix('$')
                {
                    ensure!(
                        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
                        "Unsupported positional parameter spelling"
                    );
                    let index = digits.parse::<u64>()?;
                    ensure!(
                        self.relation.is_none() && index > 0 && index <= self.positional_count,
                        "Positional parameter is outside inspected argument positions"
                    );
                    return Ok(format!("${index}"));
                }
                let mut parts = vec![
                    token
                        .ident(self.engine)
                        .context("Expected column, parameter or literal")?,
                ];
                while self.t.get(self.pos).is_some_and(|t| t.symbol(".")) {
                    self.pos += 1;
                    parts.push(
                        self.t
                            .get(self.pos)
                            .and_then(|t| t.ident(self.engine))
                            .context("Invalid qualified identifier")?,
                    );
                    self.pos += 1;
                }
                let name = parts.last().unwrap();
                ensure!(
                    self.names.contains(name),
                    "Unknown column/parameter {name}; function calls, keywords and hidden dependencies block"
                );
                if let Some((schema, table, alias)) = self.relation {
                    ensure!(
                        parts.len() == 1
                            || (parts.len() == 2 && (parts[0] == alias.unwrap_or(table)))
                            || (parts.len() == 3
                                && alias.is_none()
                                && parts[0] == schema
                                && parts[1] == table),
                        "Column qualifier does not match the parsed FROM relation"
                    );
                    Ok(format!(
                        "{}.{}",
                        quote_ident(self.engine, "rb6"),
                        quote_ident(self.engine, name)
                    ))
                } else {
                    ensure!(
                        parts.len() == 1,
                        "Routine/constant expressions cannot reference namespaces"
                    );
                    Ok(quote_ident(self.engine, name))
                }
            }
        }
    }
}
impl View {
    pub fn render(
        &self,
        engine: Engine,
        own_schema: &str,
        target_schema: &str,
        columns: &[String],
        outputs: &[String],
    ) -> Result<String> {
        ensure!(
            outputs.len() == self.projections.len(),
            "View output count differs from native columns"
        );
        if let Some((schema, _)) = &self.relation {
            ensure!(
                schema.as_deref().is_none_or(|s| s == own_schema),
                "External namespace relation is outside explicit mapping"
            );
        }
        let relation = self
            .relation
            .as_ref()
            .map(|(_, name)| (own_schema, name.as_str(), self.alias.as_deref()));
        let mut projections = vec![];
        for ((start, end), output) in self.projections.iter().zip(outputs) {
            let t = &self.tokens[*start..*end];
            let mut parser = Expression {
                engine,
                t,
                pos: 0,
                names: columns,
                positional_count: 0,
                relation,
                depth: 0,
            };
            let sql = parser.expr(0)?;
            if parser.pos < t.len() && t[parser.pos].word("AS") {
                parser.pos += 1;
                ensure!(
                    t.get(parser.pos).and_then(|t| t.ident(engine)).is_some(),
                    "Invalid projection alias"
                );
                parser.pos += 1;
            }
            ensure!(
                parser.pos == t.len(),
                "Unsupported projection syntax/functions/subqueries"
            );
            projections.push(format!("{sql} AS {}", quote_ident(engine, output)));
        }
        let mut sql = format!("SELECT {}", projections.join(", "));
        if let Some((_, name)) = &self.relation {
            sql.push_str(&format!(
                " FROM {}.{} AS {}",
                quote_ident(engine, target_schema),
                quote_ident(engine, name),
                quote_ident(engine, "rb6")
            ));
        }
        if let Some((start, end)) = self.filter {
            let t = &self.tokens[start..end];
            let mut parser = Expression {
                engine,
                t,
                pos: 0,
                names: columns,
                positional_count: 0,
                relation,
                depth: 0,
            };
            let filter = parser.expr(0)?;
            ensure!(parser.pos == t.len(), "Unsupported WHERE syntax");
            sql.push_str(&format!(" WHERE {filter}"));
        }
        Ok(sql)
    }
}
pub(super) fn routine(
    engine: Engine,
    language: &str,
    procedure: bool,
    sql: &str,
    names: &[String],
    outputs: &[String],
    positional_count: u64,
) -> Result<String> {
    let mut t = tokens(engine, sql)?;
    if t.last().is_some_and(|t| t.symbol(";")) {
        t.pop();
    }
    let mut begin = false;
    if t.first().is_some_and(|t| t.word("BEGIN")) {
        begin = true;
        t.remove(0);
        ensure!(
            t.last().is_some_and(|t| t.word("END")),
            "Expected plain BEGIN/END body"
        );
        t.pop();
        if t.last().is_some_and(|t| t.symbol(";")) {
            t.pop();
        }
    }
    if engine == Engine::Postgres {
        ensure!(
            !procedure,
            "PostgreSQL procedure bodies are outside bounded scalar scope"
        );
        ensure!(
            (language == "sql" && !begin) || (language == "plpgsql" && begin),
            "Expected SQL SELECT or PL/pgSQL BEGIN RETURN body"
        );
    }
    let head = if procedure {
        "SET"
    } else if engine == Engine::Postgres && language == "sql" {
        "SELECT"
    } else {
        "RETURN"
    };
    let mut statements = vec![];
    for statement in t.split(|t| t.symbol(";")) {
        ensure!(
            statement.first().is_some_and(|t| t.word(head)),
            "Only bounded {head} expressions supported"
        );
        let mut start = 1;
        let mut target = None;
        if procedure {
            let name = statement
                .get(1)
                .and_then(|t| t.ident(engine))
                .context("Missing OUT parameter")?;
            ensure!(
                outputs.contains(&name),
                "SET target must be an inspected OUT/INOUT parameter"
            );
            ensure!(
                statement.get(2).is_some_and(|t| t.symbol("=")),
                "Expected SET parameter = expression"
            );
            start = 3;
            target = Some(name);
        }
        let expr = &statement[start..];
        let mut p = Expression {
            engine,
            t: expr,
            pos: 0,
            names,
            positional_count,
            relation: None,
            depth: 0,
        };
        let value = p.expr(0)?;
        ensure!(
            p.pos == expr.len(),
            "Routine body has unsupported table access/calls/control syntax"
        );
        statements.push(if let Some(target) = target {
            format!("SET {} = {value}", quote_ident(engine, &target))
        } else {
            format!("{head} {value}")
        });
    }
    ensure!(
        procedure || statements.len() == 1,
        "Scalar routine requires exactly one return expression"
    );
    let body = statements.join("; ");
    if engine == Engine::Postgres && language == "sql" {
        Ok(body)
    } else {
        Ok(format!("BEGIN {body}; END"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pass6_parser_never_rewrites_literals_or_accepts_hidden_dependencies() {
        let parsed = view(
            Engine::Postgres,
            "SELECT t.x + 2 AS result, 's.t' AS label FROM s.t WHERE t.x > 0",
        )
        .unwrap();
        let sql = parsed
            .render(
                Engine::Postgres,
                "s",
                "d",
                &["x".into()],
                &["result".into(), "label".into()],
            )
            .unwrap();
        assert!(sql.contains("'s.t'"));
        assert!(sql.contains("FROM \"d\".\"t\" AS \"rb6\""));
        for sql in [
            "SELECT evil(x) FROM s.t",
            "SELECT (SELECT x FROM s.t) FROM s.t",
            "SELECT x FROM s.t JOIN s.u ON true",
            "SELECT x FROM s.t; DROP TABLE d.t",
            "SELECT x::custom_type FROM s.t",
        ] {
            assert!(
                view(Engine::Postgres, sql)
                    .and_then(|v| v.render(
                        Engine::Postgres,
                        "s",
                        "d",
                        &["x".into()],
                        &["x".into()]
                    ))
                    .is_err(),
                "{sql}"
            );
        }
        for body in [
            "RETURN other.f(x)",
            "RETURN (SELECT x FROM t)",
            "BEGIN RETURN x; DROP TABLE t; END",
            "RETURN @x",
            "RETURN x /*+ hint */",
        ] {
            assert!(
                routine(Engine::MySql, "sql", false, body, &["x".into()], &[], 0).is_err(),
                "{body}"
            );
        }
    }
}
