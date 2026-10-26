//! Bounded, conservative SQL completion over a caller-owned catalog snapshot.
use crate::completion_dialect::CompletionRules;
use anyhow::{Result, ensure};
use rowboat_core::{Engine, TableRef};
use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct CatalogTable {
    pub table: TableRef,
    pub columns: Vec<String>,
}
#[derive(Debug, Clone)]
struct Relation {
    table: TableRef,
    columns: Arc<[String]>,
}
#[derive(Debug, Clone)]
pub struct CompletionCatalog {
    engine: Engine,
    tables: Vec<Relation>,
    by_name: BTreeMap<String, Vec<usize>>,
    by_qualified_name: BTreeMap<(String, String), Vec<usize>>,
    search_path: Option<Vec<String>>,
}
impl CompletionCatalog {
    pub fn new(engine: Engine, tables: Vec<CatalogTable>) -> Result<Self> {
        ensure!(
            tables.len() <= 10_000,
            "Completion catalog exceeds 10000 tables"
        );
        let mut bytes = 0;
        let mut columns = 0;
        let mut seen = HashSet::new();
        for table in &tables {
            columns += table.columns.len();
            ensure!(
                columns <= 100_000,
                "Completion catalog exceeds column budget"
            );
            ensure!(
                seen.insert((&table.table.schema, &table.table.name)),
                "Duplicate catalog table"
            );
            let mut names = HashSet::new();
            for name in std::iter::once(&table.table.name).chain(table.columns.iter()) {
                ensure!(
                    !name.is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control),
                    "Invalid catalog identifier"
                );
                bytes += name.len();
                ensure!(
                    bytes <= 16 * 1024 * 1024,
                    "Completion catalog exceeds identifier byte budget"
                );
            }
            ensure!(
                table.table.schema.len() <= 1024
                    && !table.table.schema.chars().any(char::is_control),
                "Invalid catalog schema"
            );
            bytes += table.table.schema.len();
            ensure!(
                table
                    .columns
                    .iter()
                    .all(|n| names.insert(column_key(engine, n))),
                "Duplicate catalog column"
            );
            ensure!(
                bytes <= 16 * 1024 * 1024 && columns <= 100_000,
                "Completion catalog exceeds identifier/column budget"
            );
        }
        let mut by_name = BTreeMap::<String, Vec<usize>>::new();
        let mut by_qualified_name = BTreeMap::<(String, String), Vec<usize>>::new();
        for (index, table) in tables.iter().enumerate() {
            let name = catalog_key(engine, &table.table.name);
            by_name.entry(name.clone()).or_default().push(index);
            by_qualified_name
                .entry((catalog_key(engine, &table.table.schema), name))
                .or_default()
                .push(index);
        }
        Ok(Self {
            engine,
            search_path: None,
            by_name,
            by_qualified_name,
            tables: tables
                .into_iter()
                .map(|t| Relation {
                    table: t.table,
                    columns: t.columns.into(),
                })
                .collect(),
        })
    }

    /// Supply actual session-visible schema names, in resolution order, rather
    /// than raw SQL search_path text. None preserves catalog-only resolution;
    /// Some(empty) permits no unqualified catalog relations. PostgreSQL callers
    /// should supply current_schemas(true), MySQL callers the current database.
    /// Refresh this context with the session; this pure snapshot cannot detect SET.
    /// Validation finishes before replacing the previous context.
    pub fn set_search_path(&mut self, schemas: Option<Vec<String>>) -> Result<()> {
        if let Some(names) = &schemas {
            ensure!(
                names.len() <= 128,
                "Completion search path exceeds 128 schemas"
            );
            ensure!(
                !CompletionRules::for_engine(self.engine).single_database || names.len() <= 1,
                "MySQL completion accepts at most one current database"
            );
            let mut bytes = 0;
            for name in names {
                ensure!(
                    !name.is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control),
                    "Invalid completion search-path schema"
                );
                bytes += name.len();
                ensure!(
                    bytes <= 128 * 1024,
                    "Completion search path exceeds identifier byte budget"
                );
            }
        }
        self.search_path = schemas;
        Ok(())
    }

    pub fn search_path(&self) -> Option<&[String]> {
        self.search_path.as_deref()
    }
}

fn catalog_key(engine: Engine, name: &str) -> String {
    CompletionRules::for_engine(engine).catalog_case.key(name)
}

// Column equivalence is separate from platform-dependent MySQL table-name rules.
fn column_key(engine: Engine, name: &str) -> String {
    CompletionRules::for_engine(engine).column_case.key(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Schema,
    Table,
    Column,
    Alias,
    Keyword,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionContext {
    Relation,
    Expression,
    Suppressed,
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCompletion {
    pub label: String,
    pub insert_text: String,
    pub kind: CompletionKind,
    pub detail: String,
}
#[derive(Debug, Clone)]
pub struct SqlCompletions {
    pub replacement: Range<usize>,
    pub items: Vec<SqlCompletion>,
    pub context: CompletionContext,
    pub truncated: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Ident {
    text: String,
    quoted: bool,
}
impl Ident {
    fn resolved(&self, engine: Engine) -> String {
        CompletionRules::for_engine(engine).resolved(&self.text, self.quoted)
    }
    fn matches(&self, name: &str, engine: Engine) -> bool {
        CompletionRules::for_engine(engine).matches(&self.text, self.quoted, name)
    }
}
#[derive(Debug, Clone)]
enum Kind {
    Ident(Ident),
    Symbol(char),
    Literal,
}
#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    span: Range<usize>,
    depth: usize,
}
impl Token {
    fn word(&self, word: &str) -> bool {
        matches!(&self.kind, Kind::Ident(id) if !id.quoted && id.text.eq_ignore_ascii_case(word))
    }
    fn symbol(&self, c: char) -> bool {
        matches!(self.kind, Kind::Symbol(s) if s == c)
    }
    fn ident(&self) -> Option<Ident> {
        if let Kind::Ident(id) = &self.kind {
            Some(id.clone())
        } else {
            None
        }
    }
}
struct Lexed {
    tokens: Vec<Token>,
    pairs: Vec<usize>,
    suppressed: bool,
    executable_comments: Vec<Range<usize>>,
}
fn identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}
fn lex(sql: &str, caret: usize, engine: Engine) -> Result<Lexed> {
    let rules = CompletionRules::for_engine(engine);
    let mut tokens = Vec::new();
    let mut pairs = Vec::new();
    let mut stack = Vec::new();
    let mut at = 0;
    let mut suppressed = false;
    let mut executable_comments = Vec::new();
    while at < sql.len() {
        let start = at;
        let tail = &sql[at..];
        let c = tail.chars().next().unwrap();
        if c.is_whitespace() {
            at += c.len_utf8();
            continue;
        }
        let line_comment = (tail.starts_with("--")
            && (!rules.dash_requires_space
                || tail
                    .as_bytes()
                    .get(2)
                    .is_none_or(|b| b.is_ascii_whitespace() || b.is_ascii_control())))
            || (rules.hash_comments && c == '#');
        if line_comment {
            at += if rules.cr_ends_comment {
                tail.find(['\r', '\n'])
            } else {
                tail.find('\n')
            }
            .unwrap_or(tail.len());
            suppressed |= caret > start && caret <= at;
            continue;
        }
        if tail.starts_with("/*") {
            let executable = rules.executable_comments
                && (tail.starts_with("/*!")
                    || tail.starts_with("/*M!")
                    || tail.starts_with("/*m!"));
            at += 2;
            let mut depth = 1;
            while at < sql.len() && depth > 0 {
                if sql[at..].starts_with("/*") && rules.nested_comments {
                    depth += 1;
                    at += 2;
                } else if sql[at..].starts_with("*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += sql[at..].chars().next().unwrap().len_utf8();
                }
            }
            suppressed |= caret > start && (caret < at || depth > 0 && caret == at);
            if executable {
                executable_comments.push(start..at);
            }
            continue;
        }
        let identifier_quote = rules.identifier_quote(c);
        let quote_ident = identifier_quote.is_some();
        let string = c == '\'' || c == '"' && rules.double_quote_strings;
        let kind = if quote_ident || string {
            let close = identifier_quote.map_or(c, |quote| quote.close);
            let doubled_escape = identifier_quote.is_none_or(|quote| quote.doubled_escape);
            at += c.len_utf8();
            let mut text = String::new();
            let mut closed = false;
            while at < sql.len() {
                let current = sql[at..].chars().next().unwrap();
                at += current.len_utf8();
                if current == close {
                    if doubled_escape && sql[at..].starts_with(close) {
                        text.push(c);
                        at += c.len_utf8();
                    } else {
                        closed = true;
                        break;
                    }
                } else if string && rules.backslash_strings && current == '\\' {
                    // Conservative for PostgreSQL standard_conforming_strings:
                    // escaped-quote drafts are suppressed rather than guessed.
                    if let Some(next) = sql[at..].chars().next() {
                        at += next.len_utf8();
                    }
                } else {
                    text.push(current);
                }
            }
            if string {
                suppressed |= caret > start && (caret < at || !closed && caret == at);
                Kind::Literal
            } else {
                Kind::Ident(Ident { text, quoted: true })
            }
        } else if c == '$' && rules.dollar_strings && dollar_tag(tail).is_some() {
            let tag = dollar_tag(tail).unwrap();
            at += tag.len();
            let closed = sql[at..].find(tag);
            at = closed.map_or(sql.len(), |end| at + end + tag.len());
            suppressed |= caret > start && (caret < at || closed.is_none() && caret == at);
            Kind::Literal
        } else if identifier_char(c) {
            at += c.len_utf8();
            while let Some(next) = sql[at..].chars().next() {
                if !identifier_char(next) {
                    break;
                }
                at += next.len_utf8();
            }
            Kind::Ident(Ident {
                text: sql[start..at].into(),
                quoted: false,
            })
        } else {
            at += c.len_utf8();
            Kind::Symbol(c)
        };
        if let Kind::Ident(id) = &kind {
            ensure!(id.text.len() <= 1024, "SQL identifier exceeds 1024 bytes");
        }
        let index = tokens.len();
        if matches!(kind, Kind::Symbol(')'))
            && let Some(open) = stack.pop()
        {
            pairs[open] = index;
        }
        tokens.push(Token {
            kind,
            span: start..at,
            depth: stack.len(),
        });
        pairs.push(usize::MAX);
        if c == '(' {
            stack.push(index);
            ensure!(stack.len() <= 32, "SQL completion nesting exceeds 32");
        }
        ensure!(tokens.len() <= 32768, "SQL completion exceeds 32768 tokens");
    }
    for open in stack {
        pairs[open] = tokens.len();
    }
    Ok(Lexed {
        tokens,
        pairs,
        suppressed,
        executable_comments,
    })
}
fn dollar_tag(tail: &str) -> Option<&str> {
    let end = tail[1..].find('$')? + 1;
    let name = &tail[1..end];
    if name.is_empty()
        || name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        Some(&tail[..=end])
    } else {
        None
    }
}

#[derive(Clone)]
struct NamedColumns {
    name: Ident,
    columns: Arc<[String]>,
    known: bool,
}
struct CteDeclaration {
    name: Ident,
    aliases: Option<Range<usize>>,
    body: Range<usize>,
    complete: bool,
}
#[derive(Clone)]
struct Source {
    qualifier: Vec<Ident>,
    columns: Arc<[String]>,
    known: bool,
}
#[derive(Clone)]
struct Scope {
    start: usize,
    end: usize,
    depth: usize,
    sources: Vec<Source>,
    ctes: Vec<NamedColumns>,
    partial: bool,
}
struct Info {
    scope: Scope,
    columns: Arc<[String]>,
    known: bool,
    focus: Option<Scope>,
}
struct Analyzer<'a> {
    lex: &'a Lexed,
    catalog: &'a CompletionCatalog,
    caret: usize,
    work: usize,
    projected_columns: usize,
    projected_bytes: usize,
    reference_work: usize,
    reference_bytes: usize,
    projection_limited: bool,
}
fn reserved(token: &Token) -> bool {
    [
        "SELECT",
        "FROM",
        "WHERE",
        "JOIN",
        "LEFT",
        "RIGHT",
        "FULL",
        "INNER",
        "OUTER",
        "CROSS",
        "NATURAL",
        "ON",
        "USING",
        "GROUP",
        "ORDER",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "RETURNING",
        "SET",
        "VALUES",
        "WINDOW",
        "FOR",
        "AS",
        "LATERAL",
        "FETCH",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
    ]
    .iter()
    .any(|w| token.word(w))
}
impl Analyzer<'_> {
    // Count the entire extension before allocating/cloning. This is a cumulative
    // per-completion budget, including nested scopes and output-list renames.
    fn append_projection<'a>(
        &mut self,
        target: &mut Vec<String>,
        names: impl Iterator<Item = &'a String> + Clone,
    ) -> bool {
        let mut count = self.projected_columns;
        let mut bytes = self.projected_bytes;
        for name in names.clone() {
            count += 1;
            bytes += name.len();
            if count > 100_000 || bytes > 4 * 1024 * 1024 {
                self.projection_limited = true;
                return false;
            }
        }
        self.projected_columns = count;
        self.projected_bytes = bytes;
        target.extend(names.cloned());
        true
    }

    fn output_aliases(
        &mut self,
        inferred: Arc<[String]>,
        known: bool,
        names: Vec<String>,
    ) -> (Arc<[String]>, bool) {
        if names.is_empty() {
            return (Arc::from([]), false);
        }
        if known
            && (names.len() > inferred.len()
                || !CompletionRules::for_engine(self.catalog.engine).partial_output_aliases
                    && names.len() != inferred.len())
        {
            return (Arc::from([]), false);
        }
        // Unknown arity/positions cannot safely be matched to a trailing list.
        // Explicit prefix names remain useful but are visibly partial.
        let tail = if known { &inferred[names.len()..] } else { &[] };
        let mut columns = Vec::new();
        if !self.append_projection(&mut columns, names.iter().chain(tail.iter())) {
            return (Arc::from([]), false);
        }
        let unique = retain_unique_columns(&mut columns, self.catalog.engine);
        (columns.into(), known && unique)
    }
    fn contains(&self, start: usize, end: usize) -> bool {
        let t = &self.lex.tokens;
        t.get(start).is_some_and(|t| t.span.start <= self.caret)
            && t.get(end).is_none_or(|t| self.caret <= t.span.start)
    }

    // Only collect this WITH list's headers; nested lists belong to child queries.
    // Every name is reserved before any body can fall through to the catalog.
    fn recursive_declarations(
        &self,
        mut at: usize,
        end: usize,
    ) -> Result<(Vec<CteDeclaration>, usize, bool)> {
        let t = &self.lex.tokens;
        let mut declarations = Vec::new();
        let mut names = HashSet::new();
        loop {
            let Some(name) = t.get(at).filter(|_| at < end).and_then(Token::ident) else {
                return Ok((declarations, at, true));
            };
            ensure!(
                declarations.len() < 128,
                "SQL scope exceeds 128 visible CTEs"
            );
            ensure!(
                names.insert(catalog_key(
                    self.catalog.engine,
                    &name.resolved(self.catalog.engine)
                )),
                "Duplicate recursive CTE name"
            );
            declarations.push(CteDeclaration {
                name,
                aliases: None,
                body: end..end,
                complete: false,
            });
            let declaration = declarations.last_mut().unwrap();
            at += 1;
            if t.get(at).is_some_and(|t| t.symbol('(')) {
                let close = self.lex.pairs[at].min(end);
                declaration.aliases = Some(at + 1..close);
                at = close + 1;
            }
            if at >= end || !t.get(at).is_some_and(|t| t.word("AS")) {
                return Ok((declarations, at.min(end), true));
            }
            at += 1;
            if t.get(at).is_some_and(|t| t.word("NOT")) {
                at += 1;
            }
            if t.get(at).is_some_and(|t| t.word("MATERIALIZED")) {
                at += 1;
            }
            if at >= end || !t.get(at).is_some_and(|t| t.symbol('(')) {
                return Ok((declarations, at.min(end), true));
            }
            let close = self.lex.pairs[at].min(end);
            declaration.body = at + 1..close;
            declaration.complete = close < end && t[close].symbol(')');
            if !declaration.complete {
                return Ok((declarations, end, true));
            }
            at = close + 1;
            if at >= end || !t[at].symbol(',') {
                return Ok((declarations, at, false));
            }
            at += 1;
        }
    }

    fn recursive_ctes(
        &mut self,
        declarations: &[CteDeclaration],
        ctes: &mut Vec<NamedColumns>,
    ) -> Result<(Option<Scope>, bool)> {
        let engine = self.catalog.engine;
        for declaration in declarations {
            ctes.retain(|c| !declaration.name.matches(&c.name.resolved(engine), engine));
            ctes.push(NamedColumns {
                name: declaration.name.clone(),
                columns: Arc::from([]),
                known: false,
            });
            ensure!(ctes.len() <= 128, "SQL scope exceeds 128 visible CTEs");
        }
        let t = &self.lex.tokens;
        let mut dependencies = vec![Vec::new(); declarations.len()];
        let mut blocked = vec![false; declarations.len()];
        for (index, declaration) in declarations.iter().enumerate() {
            blocked[index] = !declaration.complete;
            let depth = t.get(declaration.body.start).map_or(0, |t| t.depth);
            let union = declaration
                .body
                .clone()
                .find(|&i| t[i].depth == depth && t[i].word("UNION"));
            for i in declaration.body.clone() {
                // Overapproximate relation-shaped references in all branches and
                // nested scopes. False dependencies remain unknown, never guessed.
                // Qualified catalog references deliberately bypass CTE shadowing.
                if i == declaration.body.start || t.get(i + 1).is_some_and(|t| t.symbol('.')) {
                    continue;
                }
                let previous = &t[i - 1];
                if !previous.symbol(',')
                    && !["FROM", "JOIN", "UPDATE", "INTO", "ONLY", "LATERAL"]
                        .iter()
                        .any(|w| previous.word(w))
                {
                    continue;
                }
                let Some(name) = t[i].ident() else {
                    continue;
                };
                for (dependency, other) in declarations.iter().enumerate() {
                    if !name.matches(&other.name.resolved(engine), engine) {
                        continue;
                    }
                    if !dependencies[index].contains(&dependency) {
                        dependencies[index].push(dependency);
                    }
                    // MySQL does not permit forward siblings. A self-reference
                    // must be outside the nonrecursive UNION seed in either engine.
                    blocked[index] |= !CompletionRules::for_engine(engine).forward_recursive_ctes
                        && dependency > index
                        || dependency == index && union.is_none_or(|boundary| i < boundary);
                }
            }
        }
        let mut done = vec![false; declarations.len()];
        loop {
            let mut progress = false;
            for (index, declaration) in declarations.iter().enumerate() {
                if done[index]
                    || blocked[index]
                    || dependencies[index].iter().any(|&d| d != index && !done[d])
                {
                    continue;
                }
                // Output names come from the seed/first branch, independent of
                // the caret. Focus is analyzed once after the namespace settles.
                let caret = self.caret;
                self.caret = usize::MAX;
                let child = self.query(declaration.body.start, declaration.body.end, ctes);
                self.caret = caret;
                let child = child?;
                let (columns, known) = if let Some(range) = &declaration.aliases {
                    self.output_aliases(
                        child.columns,
                        child.known,
                        identifier_list(&t[range.clone()], engine),
                    )
                } else {
                    (child.columns, child.known)
                };
                let entry = ctes
                    .iter_mut()
                    .rev()
                    .find(|c| declaration.name.matches(&c.name.resolved(engine), engine))
                    .unwrap();
                entry.columns = columns;
                entry.known = known;
                done[index] = true;
                progress = true;
            }
            if !progress {
                break;
            }
        }
        // Mutual cycles and their dependants remain the original unknown entries.
        // No recursive graph descent, retries of bodies or unbounded fixed point.
        let mut focus = None;
        for declaration in declarations {
            if self.contains(declaration.body.start, declaration.body.end) {
                let child = self.query(declaration.body.start, declaration.body.end, ctes)?;
                focus = Some(child.focus.unwrap_or(child.scope));
                break;
            }
        }
        Ok((focus, done.iter().any(|done| !done)))
    }
    fn query(&mut self, start: usize, end: usize, inherited: &[NamedColumns]) -> Result<Info> {
        self.work += end.saturating_sub(start);
        ensure!(
            self.work <= 2_000_000,
            "SQL completion analysis budget exceeded"
        );
        let t = &self.lex.tokens;
        let engine = self.catalog.engine;
        let depth = t.get(start).map_or(0, |t| t.depth);
        let mut ctes = inherited.to_vec();
        let mut focus = None;
        let mut at = start;
        let mut partial = false;
        if t.get(at).is_some_and(|t| t.word("WITH")) {
            at += 1;
            let recursive = t.get(at).is_some_and(|t| t.word("RECURSIVE"));
            if recursive {
                at += 1;
            }
            if recursive {
                let (declarations, next, incomplete) = self.recursive_declarations(at, end)?;
                let (cte_focus, unresolved) = self.recursive_ctes(&declarations, &mut ctes)?;
                focus = cte_focus;
                partial |= incomplete || unresolved;
                at = next;
            } else {
                loop {
                    let Some(name) = t.get(at).and_then(Token::ident) else {
                        partial = true;
                        break;
                    };
                    at += 1;
                    let mut explicit = None;
                    if t.get(at).is_some_and(|t| t.symbol('(')) {
                        let close = self.lex.pairs[at].min(end);
                        explicit = Some(identifier_list(&t[at + 1..close], engine));
                        at = close + 1;
                    }
                    if !t.get(at).is_some_and(|t| t.word("AS")) {
                        partial = true;
                        break;
                    }
                    at += 1;
                    if t.get(at).is_some_and(|t| t.word("NOT")) {
                        at += 1;
                    }
                    if t.get(at).is_some_and(|t| t.word("MATERIALIZED")) {
                        at += 1;
                    }
                    if !t.get(at).is_some_and(|t| t.symbol('(')) {
                        partial = true;
                        break;
                    }
                    let close = self.lex.pairs[at].min(end);
                    let child = self.query(at + 1, close, &ctes)?;
                    if self.contains(at + 1, close) {
                        focus = Some(child.focus.unwrap_or(child.scope));
                    }
                    let (columns, known) = if let Some(names) = explicit {
                        self.output_aliases(child.columns, child.known, names)
                    } else {
                        (child.columns, child.known)
                    };
                    ctes.retain(|c| !name.matches(&c.name.resolved(engine), engine));
                    ctes.push(NamedColumns {
                        name,
                        columns,
                        known,
                    });
                    ensure!(ctes.len() <= 128, "SQL scope exceeds 128 visible CTEs");
                    at = close + 1;
                    if !t.get(at).is_some_and(|t| t.symbol(',')) {
                        break;
                    }
                    at += 1;
                }
            }
        }
        // Set-operation siblings do not share relation aliases. Derived output
        // names come from the first branch; active completion uses the caret's.
        let branch_start = at;
        let boundaries: Vec<_> = (at..end)
            .filter(|&i| {
                t[i].depth == depth
                    && ["UNION", "EXCEPT", "INTERSECT"]
                        .iter()
                        .any(|w| t[i].word(w))
            })
            .collect();
        let mut branch_end = boundaries.first().copied().unwrap_or(end);
        if self.contains(at, end) {
            for (index, &boundary) in boundaries.iter().enumerate() {
                if t[boundary].span.end <= self.caret {
                    at = boundary + 1;
                    if t.get(at)
                        .is_some_and(|t| t.word("ALL") || t.word("DISTINCT"))
                    {
                        at += 1;
                    }
                    branch_end = boundaries.get(index + 1).copied().unwrap_or(end);
                }
            }
        }
        if at != branch_start && !boundaries.is_empty() {
            partial = true;
        }
        let mut sources = Vec::new();
        let mut in_from = false;
        let mut i = at;
        while i < branch_end {
            if t[i].depth != depth {
                i += 1;
                continue;
            }
            if [
                "WHERE",
                "GROUP",
                "ORDER",
                "HAVING",
                "LIMIT",
                "RETURNING",
                "WINDOW",
                "SET",
            ]
            .iter()
            .any(|w| t[i].word(w))
            {
                in_from = false;
            }
            let relation = t[i].word("FROM")
                || t[i].word("JOIN")
                || t[i].word("UPDATE")
                || t[i].word("INTO")
                || in_from && t[i].symbol(',');
            if !relation {
                i += 1;
                continue;
            }
            in_from = true;
            i += 1;
            if i >= branch_end {
                break;
            }
            if t[i].word("LATERAL") || t[i].word("ONLY") {
                partial = true;
                i += 1;
            }
            if i >= branch_end {
                break;
            }
            let (mut qualifier, mut columns, mut known);
            if t[i].symbol('(') {
                let close = self.lex.pairs[i].min(branch_end);
                let child = self.query(i + 1, close, &ctes)?;
                if self.contains(i + 1, close) {
                    focus = Some(child.focus.unwrap_or(child.scope));
                }
                qualifier = Vec::new();
                columns = child.columns;
                known = child.known;
                i = close + 1;
            } else if let Some(first) = t[i].ident() {
                qualifier = vec![first];
                i += 1;
                while i + 1 < branch_end && t[i].symbol('.') {
                    let Some(next) = t[i + 1].ident() else { break };
                    qualifier.push(next);
                    i += 2;
                }
                (columns, known) = self.resolve(&qualifier, &ctes);
                if t.get(i).is_some_and(|t| t.symbol('(')) {
                    i = self.lex.pairs[i].min(branch_end) + 1;
                    columns = Arc::from([]);
                    known = false;
                }
            } else {
                partial = true;
                i += 1;
                continue;
            }
            if t.get(i).is_some_and(|t| t.word("AS")) {
                i += 1;
            }
            if let Some(alias) = t.get(i).filter(|t| !reserved(t)).and_then(Token::ident) {
                qualifier = vec![alias];
                i += 1;
            }
            if t.get(i).is_some_and(|t| t.symbol('(')) {
                let close = self.lex.pairs[i].min(branch_end);
                (columns, known) = if CompletionRules::for_engine(engine).source_output_aliases {
                    self.output_aliases(columns, known, identifier_list(&t[i + 1..close], engine))
                } else {
                    (Arc::from([]), false)
                };
                i = close + 1;
            }
            partial |= !known;
            if !qualifier.is_empty() {
                sources.push(Source {
                    qualifier,
                    columns,
                    known,
                });
            }
            ensure!(sources.len() <= 128, "SQL scope exceeds 128 relations");
        }
        // Expression subqueries get their own scope. Deliberately do not infer
        // correlated outer columns, so local ambiguity cannot leak another scope.
        if focus.is_none() {
            let mut i = at;
            while i < branch_end {
                if t[i].symbol('(')
                    && t.get(i + 1)
                        .is_some_and(|t| t.word("SELECT") || t.word("WITH"))
                {
                    let close = self.lex.pairs[i].min(branch_end);
                    if self.contains(i + 1, close) {
                        let child = self.query(i + 1, close, &ctes)?;
                        focus = Some(child.focus.unwrap_or(child.scope));
                        break;
                    }
                    i = close;
                }
                i += 1;
            }
        }
        let scope = Scope {
            start: at,
            end: branch_end,
            depth,
            sources,
            ctes,
            partial,
        };
        let (columns, known) = self.projection(&scope);
        let known = known && !scope.partial;
        Ok(Info {
            scope,
            columns: columns.into(),
            known,
            focus,
        })
    }
    fn resolve(&self, path: &[Ident], ctes: &[NamedColumns]) -> (Arc<[String]>, bool) {
        let engine = self.catalog.engine;
        if path.len() == 1
            && let Some(cte) = ctes
                .iter()
                .rev()
                .find(|c| path[0].matches(&c.name.resolved(engine), engine))
        {
            return (cte.columns.clone(), cte.known);
        }
        if let [name] = path
            && let Some(schemas) = &self.catalog.search_path
        {
            for schema in schemas {
                let indices = self.catalog.by_qualified_name.get(&(
                    catalog_key(engine, schema),
                    catalog_key(engine, &name.resolved(engine)),
                ));
                let mut visible = indices
                    .into_iter()
                    .flatten()
                    .map(|&index| &self.catalog.tables[index])
                    .filter(|table| {
                        table.table.schema == *schema && name.matches(&table.table.name, engine)
                    });
                if let Some(table) = visible.next() {
                    return if visible.next().is_none() {
                        (table.columns.clone(), !table.columns.is_empty())
                    } else {
                        (Arc::from([]), false)
                    };
                }
            }
            return (Arc::from([]), false);
        }
        let indices = match path {
            [name] => self
                .catalog
                .by_name
                .get(&catalog_key(engine, &name.resolved(engine))),
            [schema, name] => self.catalog.by_qualified_name.get(&(
                catalog_key(engine, &schema.resolved(engine)),
                catalog_key(engine, &name.resolved(engine)),
            )),
            _ => None,
        };
        let mut matches = indices
            .into_iter()
            .flatten()
            .map(|&index| &self.catalog.tables[index])
            .filter(|table| match path {
                [name] => name.matches(&table.table.name, engine),
                [schema, name] => {
                    schema.matches(&table.table.schema, engine)
                        && name.matches(&table.table.name, engine)
                }
                _ => false,
            });
        if let Some(table) = matches.next()
            && matches.next().is_none()
        {
            return (table.columns.clone(), !table.columns.is_empty());
        }
        (Arc::from([]), false)
    }

    // Resolve only local column references. Unknown sources may hide an
    // unqualified collision; an exact qualifier can still select a partial
    // source's independently known column. Charge scans before doing any work.
    fn projected_reference(&mut self, path: &[Ident], scope: &Scope) -> Option<String> {
        let (column, qualifier) = path.split_last()?;
        let sources: Vec<_> = scope
            .sources
            .iter()
            .filter(|source| {
                qualifier.is_empty()
                    || qualifier_matches(qualifier, &source.qualifier, self.catalog.engine)
            })
            .collect();
        if !qualifier.is_empty() && sources.len() != 1
            || qualifier.is_empty() && sources.iter().any(|s| !s.known)
        {
            return None;
        }
        let cost: usize = sources.iter().map(|s| s.columns.len()).sum();
        self.reference_work = self.reference_work.saturating_add(cost);
        if self.reference_work > 2_000_000 {
            self.projection_limited = true;
            return None;
        }
        let engine = self.catalog.engine;
        let rules = CompletionRules::for_engine(engine);
        let mut found = None;
        for name in sources.iter().flat_map(|s| s.columns.iter()) {
            self.reference_bytes += name.len();
            if self.reference_bytes > 64 * 1024 * 1024 {
                self.projection_limited = true;
                return None;
            }
            let matches = rules.column_matches(&column.text, column.quoted, name);
            if matches {
                if found.is_some() {
                    return None;
                }
                found = Some(name);
            }
        }
        found.cloned()
    }
    fn projection(&mut self, scope: &Scope) -> (Vec<String>, bool) {
        let t = &self.lex.tokens;
        let engine = self.catalog.engine;
        let Some(select) =
            (scope.start..scope.end).find(|&i| t[i].depth == scope.depth && t[i].word("SELECT"))
        else {
            return (vec![], false);
        };
        let end = (select + 1..scope.end)
            .find(|&i| {
                t[i].depth == scope.depth
                    && [
                        "FROM", "WHERE", "GROUP", "HAVING", "WINDOW", "QUALIFY", "ORDER", "LIMIT",
                        "OFFSET", "FETCH", "FOR", "INTO",
                    ]
                    .iter()
                    .any(|word| t[i].word(word))
            })
            .unwrap_or(scope.end);
        let mut start = select + 1;
        if t.get(start)
            .is_some_and(|t| t.word("DISTINCT") || t.word("ALL"))
        {
            start += 1;
            if t.get(start).is_some_and(|t| t.word("ON")) {
                if !CompletionRules::for_engine(engine).distinct_on
                    || !t.get(start + 1).is_some_and(|t| t.symbol('('))
                {
                    return (vec![], false);
                }
                let close = self.lex.pairs[start + 1];
                if close >= end || !t[close].symbol(')') {
                    return (vec![], false);
                }
                start = close + 1;
            }
        }
        let mut columns = Vec::new();
        let mut known = true;
        for stop in (start..end)
            .filter(|&i| t[i].depth == scope.depth && t[i].symbol(','))
            .chain(std::iter::once(end))
        {
            let part = &t[start..stop];
            start = stop + 1;
            let alias = part
                .len()
                .checked_sub(2)
                .and_then(|index| part.get(index..))
                .filter(|pair| pair[0].depth == scope.depth && pair[0].word("AS"))
                .and_then(|pair| pair[1].ident());
            if let Some(alias) = alias {
                if !self.append_projection(&mut columns, std::iter::once(&alias.resolved(engine))) {
                    return (vec![], false);
                }
                continue;
            }
            if unambiguous_implicit_alias(part)
                && let Some(alias) = part.last().and_then(Token::ident)
            {
                if !self.append_projection(&mut columns, std::iter::once(&alias.resolved(engine))) {
                    return (vec![], false);
                }
                continue;
            }
            if part.len() == 1 && part[0].symbol('*') {
                if (scope.start..scope.end).any(|i| {
                    t[i].depth == scope.depth && (t[i].word("USING") || t[i].word("NATURAL"))
                }) {
                    // Coalesced join keys change star order/arity; do not align
                    // later output-alias lists against a fabricated expansion.
                    known = false;
                    continue;
                }
                if scope.sources.is_empty() {
                    known = false;
                }
                if !self.append_projection(
                    &mut columns,
                    scope
                        .sources
                        .iter()
                        .flat_map(|source| source.columns.iter()),
                ) {
                    return (vec![], false);
                }
                known &= scope.sources.iter().all(|source| source.known);
            } else if matches!(part.len(), 3 | 5)
                && matches!(part[0].kind, Kind::Ident(_))
                && part[part.len() - 2].symbol('.')
                && part.last().unwrap().symbol('*')
            {
                if let Some(path) = reference_path(&part[..part.len() - 2]) {
                    let mut sources = scope
                        .sources
                        .iter()
                        .filter(|s| qualifier_matches(&path, &s.qualifier, engine));
                    if let Some(source) = sources.next().filter(|_| sources.next().is_none()) {
                        if !self.append_projection(&mut columns, source.columns.iter()) {
                            return (vec![], false);
                        }
                        known &= source.known;
                    } else {
                        known = false;
                    }
                } else {
                    known = false;
                }
            } else if let Some(path) = reference_path(part) {
                if let Some(name) = self.projected_reference(&path, scope) {
                    if !self.append_projection(&mut columns, std::iter::once(&name)) {
                        return (vec![], false);
                    }
                } else {
                    known = false;
                }
            } else {
                known = false;
            }
        }
        known &= retain_unique_columns(&mut columns, engine);
        (columns, known)
    }
}

// Keep only independently addressable names, preserving original projection
// order. Case-equivalent MySQL duplicates are ambiguous even when quoted.
fn retain_unique_columns(columns: &mut Vec<String>, engine: Engine) -> bool {
    let mut seen = HashSet::new();
    let mut duplicates = HashSet::new();
    for name in columns.iter() {
        let key = column_key(engine, name);
        if !seen.insert(key.clone()) {
            duplicates.insert(key);
        }
    }
    if duplicates.is_empty() {
        return true;
    }
    columns.retain(|name| !duplicates.contains(&column_key(engine, name)));
    false
}

fn reference_path(mut part: &[Token]) -> Option<Vec<Ident>> {
    while part.len() >= 2
        && part[0].symbol('(')
        && part.last()?.symbol(')')
        && part.last()?.depth == part[0].depth
        && part[1..part.len() - 1]
            .iter()
            .all(|t| t.depth > part[0].depth)
    {
        part = &part[1..part.len() - 1];
    }
    if !matches!(part.len(), 1 | 3 | 5) {
        return None;
    }
    let mut path = Vec::new();
    for (i, token) in part.iter().enumerate() {
        if i % 2 == 1 {
            if !token.symbol('.') {
                return None;
            }
        } else {
            let id = token.ident()?;
            if !id.quoted
                && (id.text.chars().next().is_some_and(|c| c.is_numeric())
                    || expression_keyword(token)
                    || reserved(token))
            {
                return None;
            }
            path.push(id);
        }
    }
    Some(path)
}
fn expression_keyword(token: &Token) -> bool {
    [
        "NULL",
        "TRUE",
        "FALSE",
        "UNKNOWN",
        "IS",
        "NOT",
        "AND",
        "OR",
        "XOR",
        "IN",
        "LIKE",
        "ILIKE",
        "BETWEEN",
        "COLLATE",
        "OVER",
        "FILTER",
        "AT",
        "TIME",
        "ZONE",
        "INTERVAL",
        "DEFAULT",
        "CURRENT_DATE",
        "CURRENT_TIME",
        "CURRENT_TIMESTAMP",
    ]
    .iter()
    .any(|word| token.word(word))
}

fn unambiguous_implicit_alias(part: &[Token]) -> bool {
    let Some(last) = part.last() else {
        return false;
    };
    let Some(id) = last.ident() else {
        return false;
    };
    if reserved(last)
        || expression_keyword(last)
        || !id.quoted && id.text.chars().next().is_some_and(|c| c.is_numeric())
    {
        return false;
    }
    match part {
        [first, _] => {
            !expression_keyword(first) && matches!(first.kind, Kind::Ident(_) | Kind::Literal)
        }
        [qualifier, dot, column, _] if dot.symbol('.') => {
            qualifier.ident().is_some() && column.ident().is_some()
        }
        _ => {
            part.len() > 2
                && (part[part.len() - 2].symbol(')')
                    || reference_path(&part[..part.len() - 1]).is_some())
        }
    }
}
fn identifier_list(tokens: &[Token], engine: Engine) -> Vec<String> {
    if tokens.is_empty()
        || tokens.len().is_multiple_of(2)
        || tokens.iter().enumerate().any(|(i, t)| {
            if i % 2 == 0 {
                t.ident().is_none()
            } else {
                !t.symbol(',')
            }
        })
    {
        return Vec::new();
    }
    let names: Vec<_> = tokens
        .iter()
        .filter_map(Token::ident)
        .map(|id| id.resolved(engine))
        .collect();
    let mut seen = HashSet::new();
    if !names
        .iter()
        .all(|name| seen.insert(column_key(engine, name)))
    {
        return Vec::new();
    }
    names
}
fn qualifier_matches(query: &[Ident], actual: &[Ident], engine: Engine) -> bool {
    query.len() <= actual.len()
        && query
            .iter()
            .zip(&actual[actual.len() - query.len()..])
            .all(|(q, a)| q.matches(&a.resolved(engine), engine))
}
fn quote(engine: Engine, value: &str) -> String {
    rowboat_core::sql::quote_ident(engine, value)
}

// Rank the matched logical name, never its qualified label or quoted SQL.
// Empty-prefix browsing has no match to shorten: use name-alphabetical order
// within each context tier. Field order is the deterministic sort contract.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CompletionRank {
    non_exact: bool,
    context_tier: u8,
    identifier_len: usize,
    folded_name: String,
    label: String,
    insert_text: String,
}

pub fn complete_sql(
    sql: &str,
    caret: usize,
    catalog: &CompletionCatalog,
    limit: usize,
) -> Result<SqlCompletions> {
    ensure!(
        sql.len() <= 256 * 1024,
        "SQL completion input exceeds 256KiB"
    );
    ensure!(
        caret <= sql.len() && sql.is_char_boundary(caret),
        "Caret must be a UTF-8 byte boundary in SQL"
    );
    ensure!((1..=200).contains(&limit), "Completion limit must be 1–200");
    let lexed = lex(sql, caret, catalog.engine)?;
    let t = &lexed.tokens;
    let mut result = SqlCompletions {
        replacement: caret..caret,
        items: vec![],
        context: CompletionContext::Expression,
        truncated: false,
        reason: None,
    };
    if lexed.suppressed {
        result.context = CompletionContext::Suppressed;
        result.reason = Some("Caret is inside a comment or string literal".into());
        return Ok(result);
    }
    // Versioned/vendor executable comments can add aliases, delimiters or whole
    // clauses. We cannot discard their SQL and claim the remaining scope is known.
    // Ordinary comments and optimizer hints have no such parser semantics.
    let statement_start = t
        .iter()
        .rev()
        .find(|t| t.depth == 0 && t.symbol(';') && t.span.end <= caret)
        .map_or(0, |t| t.span.end);
    let statement_end = t
        .iter()
        .find(|t| t.depth == 0 && t.symbol(';') && t.span.start >= caret)
        .map_or(sql.len(), |t| t.span.start);
    if lexed
        .executable_comments
        .iter()
        .any(|span| span.start < statement_end && span.end > statement_start)
    {
        result.context = CompletionContext::Unsupported;
        result.reason = Some("MySQL executable comments make this statement's scope unknown; versioned SQL is not interpreted".into());
        return Ok(result);
    }
    let active = t.iter().position(|t| {
        matches!(t.kind, Kind::Ident(_)) && t.span.start <= caret && caret <= t.span.end
    });
    let mut prefix = String::new();
    let mut quoted = false;
    let before = if let Some(index) = active {
        let token = &t[index];
        let id = token.ident().unwrap();
        quoted = id.quoted;
        result.replacement = token.span.clone();
        prefix = sql[token.span.start..caret].to_owned();
        if quoted {
            let q = sql[token.span.start..].chars().next().unwrap();
            let quote = CompletionRules::for_engine(catalog.engine)
                .identifier_quote(q)
                .unwrap();
            prefix = prefix.strip_prefix(q).unwrap_or(&prefix).to_owned();
            if caret == token.span.end
                && sql[token.span.start..token.span.end].ends_with(quote.close)
            {
                prefix.pop();
            }
            if quote.doubled_escape {
                prefix = prefix.replace(&format!("{q}{q}"), &q.to_string());
            }
        }
        index
    } else {
        t.partition_point(|t| t.span.end <= caret)
    };
    let mut qualifier = Vec::new();
    let mut pos = before;
    while pos >= 2 && t[pos - 1].symbol('.') {
        let Some(id) = t[pos - 2].ident() else { break };
        qualifier.insert(0, id);
        pos -= 2;
    }
    let start = (0..before)
        .rev()
        .find(|&i| t[i].depth == 0 && t[i].symbol(';'))
        .map_or(0, |i| i + 1);
    let end = (before..t.len())
        .find(|&i| t[i].depth == 0 && t[i].symbol(';'))
        .unwrap_or(t.len());
    if end > start + 1
        && !["SELECT", "WITH", "UPDATE", "DELETE", "INSERT"]
            .iter()
            .any(|word| t[start].word(word))
    {
        result.context = CompletionContext::Unsupported;
        result.reason = Some("Completion supports SELECT/WITH and basic DML relation contexts, not this statement type".into());
        return Ok(result);
    }
    if t.get(start).is_some_and(|t| t.word("INSERT"))
        && t.iter().enumerate().any(|(i, token)| {
            token.symbol('(')
                && token.span.start < caret
                && t.get(lexed.pairs[i])
                    .is_none_or(|close| caret <= close.span.start)
        })
    {
        result.context = CompletionContext::Unsupported;
        result.reason = Some("INSERT column/value lists are not inferred".into());
        return Ok(result);
    }
    let mut analyzer = Analyzer {
        lex: &lexed,
        catalog,
        caret,
        work: 0,
        projected_columns: 0,
        projected_bytes: 0,
        reference_work: 0,
        reference_bytes: 0,
        projection_limited: false,
    };
    let info = analyzer.query(start, end, &[])?;
    let scope = info.focus.unwrap_or(info.scope);
    let relation = (scope.start..pos.min(scope.end))
        .rev()
        .filter(|&i| t[i].depth == scope.depth)
        .find_map(|i| {
            if ["FROM", "JOIN", "UPDATE", "INTO"]
                .iter()
                .any(|w| t[i].word(w))
            {
                Some(true)
            } else if [
                "SELECT",
                "ON",
                "USING",
                "WHERE",
                "GROUP",
                "ORDER",
                "HAVING",
                "SET",
                "RETURNING",
                "VALUES",
            ]
            .iter()
            .any(|w| t[i].word(w))
            {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(false);
    result.context = if relation {
        CompletionContext::Relation
    } else {
        CompletionContext::Expression
    };
    if analyzer.projection_limited {
        result.reason = Some("Projection inference budget exceeded; only independently resolved columns are available".into());
    } else if scope.partial {
        result.reason = Some(
            "Some relations or projection columns could not be resolved from this catalog/scope"
                .into(),
        );
    }
    let mut candidates = BTreeMap::<CompletionRank, SqlCompletion>::new();
    // Keep the existing (keyword/nonkeyword, label, insertion) dedup identity,
    // including collisions between kinds. Both maps retain at most limit items.
    let mut identities = BTreeMap::<(bool, String, String), CompletionRank>::new();
    let prefix_case = CompletionRules::for_engine(catalog.engine).prefix_case;
    let folded_prefix = prefix_case.key(&prefix);
    let mut add =
        |name: &str, label: String, insert_text: String, kind: CompletionKind, detail: String| {
            let matched_name = prefix_case.key(name);
            let matches = if quoted {
                name.starts_with(&prefix)
            } else {
                matched_name.starts_with(&folded_prefix)
            };
            if !matches {
                return;
            }
            let exact = if quoted {
                name == prefix
            } else {
                matched_name == folded_prefix
            };
            let rank = CompletionRank {
                non_exact: !exact,
                context_tier: match kind {
                    CompletionKind::Table | CompletionKind::Column => 0,
                    CompletionKind::Schema | CompletionKind::Alias => 1,
                    CompletionKind::Keyword => 2,
                },
                identifier_len: if prefix.is_empty() {
                    0
                } else {
                    name.chars().count()
                },
                folded_name: name.to_lowercase(),
                label: label.clone(),
                insert_text: insert_text.clone(),
            };
            let identity = (
                kind == CompletionKind::Keyword,
                label.clone(),
                insert_text.clone(),
            );
            if let Some(previous) = identities.get(&identity) {
                if previous < &rank {
                    return;
                }
                candidates.remove(previous);
            }
            identities.insert(identity, rank.clone());
            candidates.insert(
                rank,
                SqlCompletion {
                    label,
                    insert_text,
                    kind,
                    detail,
                },
            );
            if candidates.len() > limit {
                let (_, removed) = candidates.pop_last().unwrap();
                identities.remove(&(
                    removed.kind == CompletionKind::Keyword,
                    removed.label,
                    removed.insert_text,
                ));
                result.truncated = true;
            }
        };
    let engine = catalog.engine;
    if relation {
        if qualifier.is_empty() {
            for cte in &scope.ctes {
                let name = cte.name.resolved(engine);
                add(
                    &name,
                    name.clone(),
                    quote(engine, &name),
                    CompletionKind::Table,
                    "CTE".into(),
                );
            }
            for table in &catalog.tables {
                let name = &table.table.name;
                let schema = &table.table.schema;
                let insert = if schema.is_empty() {
                    quote(engine, name)
                } else {
                    format!("{}.{}", quote(engine, schema), quote(engine, name))
                };
                add(
                    name,
                    if schema.is_empty() {
                        name.clone()
                    } else {
                        format!("{schema}.{name}")
                    },
                    insert,
                    CompletionKind::Table,
                    "catalog relation".into(),
                );
                if !schema.is_empty() {
                    add(
                        schema,
                        schema.clone(),
                        quote(engine, schema),
                        CompletionKind::Schema,
                        "schema".into(),
                    );
                }
            }
        } else if qualifier.len() == 1 {
            for table in &catalog.tables {
                if qualifier[0].matches(&table.table.schema, engine) {
                    add(
                        &table.table.name,
                        table.table.name.clone(),
                        quote(engine, &table.table.name),
                        CompletionKind::Table,
                        table.table.schema.clone(),
                    );
                }
            }
        }
    } else if !qualifier.is_empty() {
        let sources: Vec<_> = scope
            .sources
            .iter()
            .filter(|s| qualifier_matches(&qualifier, &s.qualifier, engine))
            .collect();
        if sources.len() == 1 {
            for name in sources[0].columns.iter() {
                add(
                    name,
                    name.clone(),
                    quote(engine, name),
                    CompletionKind::Column,
                    "qualified column".into(),
                );
            }
        } else {
            result.context = CompletionContext::Unsupported;
            result.reason =
                Some("Qualifier is unknown or ambiguous in the local query scope".into());
        }
    } else {
        for source in &scope.sources {
            let alias = source.qualifier.last().unwrap().resolved(engine);
            let short_unique = scope
                .sources
                .iter()
                .filter(|s| {
                    s.qualifier
                        .last()
                        .is_some_and(|id| id.matches(&alias, engine))
                })
                .count()
                == 1;
            let qualifier = if short_unique {
                vec![alias.clone()]
            } else if source.qualifier.len() > 1
                && scope
                    .sources
                    .iter()
                    .filter(|s| qualifier_matches(&source.qualifier, &s.qualifier, engine))
                    .count()
                    == 1
            {
                source
                    .qualifier
                    .iter()
                    .map(|id| id.resolved(engine))
                    .collect()
            } else {
                continue;
            };
            let qualifier_label = qualifier.join(".");
            let qualifier_sql = qualifier
                .iter()
                .map(|part| quote(engine, part))
                .collect::<Vec<_>>()
                .join(".");
            add(
                &alias,
                qualifier_label.clone(),
                qualifier_sql.clone(),
                CompletionKind::Alias,
                "local relation".into(),
            );
            for name in source.columns.iter() {
                let qualified = scope.sources.len() > 1;
                add(
                    name,
                    if qualified {
                        format!("{qualifier_label}.{name}")
                    } else {
                        name.clone()
                    },
                    if qualified {
                        format!("{}.{}", qualifier_sql, quote(engine, name))
                    } else {
                        quote(engine, name)
                    },
                    CompletionKind::Column,
                    qualifier_label.clone(),
                );
            }
        }
        if !quoted {
            for word in [
                "SELECT", "FROM", "WHERE", "JOIN", "ON", "GROUP BY", "ORDER BY", "HAVING", "LIMIT",
                "AS", "AND", "OR", "IS NULL", "DISTINCT", "WITH",
            ] {
                add(
                    word,
                    word.into(),
                    word.into(),
                    CompletionKind::Keyword,
                    "SQL keyword".into(),
                );
            }
        }
    }
    result.items = candidates.into_values().collect();
    Ok(result)
}
