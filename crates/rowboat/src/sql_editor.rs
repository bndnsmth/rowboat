//! Native SQL editing with presentation-only PostgreSQL/MySQL highlighting.
//!
//! This lexer deliberately accepts a union of dialects. Never use its tokens to
//! split statements, validate SQL, enforce read-only access, or execute queries.

use std::{ops::Range, rc::Rc, sync::Arc};

use gpui_kit::base::input::{
    DiagnosticColors, Editor, EditorState, FoldRange, HighlightStyleResolver, InputEdit,
    InputEditorStyle, InputHighlighter, Rope,
};
use gpui_kit::{
    App, AppContext, Context, Entity, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement,
    ParentElement, SharedString, Styled, Window, div, px,
};
use gpui_omarchy::{ActiveTheme, Theme};

/// Create an independently editable query buffer. Does not move keyboard focus.
pub fn create(
    sql: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<EditorState> {
    let sql = sql.into();
    let style = editor_style(cx.omarchy());
    cx.new(|cx| {
        let mut state = EditorState::new(window, cx)
            .language("sql")
            .line_number(true)
            .folding(false)
            .soft_wrap(false)
            .default_value(sql);
        state.set_highlighter_factory(
            Rc::new(|language| {
                language
                    .eq_ignore_ascii_case("sql")
                    .then(|| Box::new(SqlHighlighter::default()) as Box<dyn InputHighlighter>)
            }),
            cx,
        );
        state.set_editor_style(style);
        state
    })
}

/// Freeze user edits during coordinated close, while preserving buffers and undo.
pub fn set_frozen(editor: &Entity<EditorState>, frozen: bool, cx: &mut App) {
    editor.update(cx, |editor, cx| editor.set_disabled(frozen, cx));
}

/// Replace a byte range as one undoable edit, collapsing other carets.
pub fn replace_range(
    editor: &mut EditorState,
    range: Range<usize>,
    text: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    editor.set_selected_range(range, cx);
    editor.replace(text, window, cx);
}

/// Render inside a parent with a definite height. Colors follow live theme changes.
/// The child is the actual Base editor, including its native selection, undo,
/// clipboard, search, scrolling, and multi-caret handling.
pub fn render(
    state: &Entity<EditorState>,
    font_size: f32,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let theme = cx.omarchy().clone();
    state.update(cx, |state, _| state.set_editor_style(editor_style(&theme)));
    div()
        .size_full()
        .min_w_0()
        .min_h_0()
        .flex()
        .overflow_hidden()
        .bg(theme.background)
        .text_color(theme.foreground)
        .font_family(theme.mono_font)
        .text_size(px(font_size))
        .child(Editor::new(state))
}

fn editor_style(theme: &Theme) -> InputEditorStyle {
    InputEditorStyle {
        foreground: theme.foreground,
        muted_foreground: theme.secondary.opacity(0.65),
        background: theme.background,
        border: theme.border,
        selection: theme.accent.opacity(0.25),
        caret: theme.bright,
        diagnostics: DiagnosticColors {
            error: theme.danger,
            warning: theme.warning,
            info: theme.accent,
            hint: theme.secondary,
        },
        highlight_styles: Arc::new(SqlStyles {
            keyword: theme.accent,
            string: theme.success,
            identifier: theme.chart[2],
            number: theme.warning,
            comment: theme.secondary.opacity(0.75),
            parameter: theme.chart[1],
            operator: theme.foreground,
        }),
        editor_invisible: Some(theme.secondary.opacity(0.25)),
        editor_active_line: Some(theme.foreground.opacity(0.035)),
        editor_gutter_background: Some(theme.surface),
        ..Default::default()
    }
}

struct SqlStyles {
    keyword: Hsla,
    string: Hsla,
    identifier: Hsla,
    number: Hsla,
    comment: Hsla,
    parameter: Hsla,
    operator: Hsla,
}

impl HighlightStyleResolver for SqlStyles {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        let color = match name {
            "keyword" => self.keyword,
            "string" => self.string,
            "identifier" => self.identifier,
            "number" => self.number,
            "comment" => self.comment,
            "parameter" => self.parameter,
            "operator" => self.operator,
            _ => return None,
        };
        Some(HighlightStyle {
            color: Some(color),
            font_weight: (name == "keyword").then_some(FontWeight::SEMIBOLD),
            font_style: (name == "comment").then_some(FontStyle::Italic),
            ..Default::default()
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Keyword,
    String,
    Identifier,
    Number,
    Comment,
    Parameter,
    Operator,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Identifier => "identifier",
            Self::Number => "number",
            Self::Comment => "comment",
            Self::Parameter => "parameter",
            Self::Operator => "operator",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Token {
    range: Range<usize>,
    kind: Kind,
}

#[derive(Default)]
struct SqlHighlighter {
    tokens: Vec<Token>,
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        "sql".into()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        self.tokens = lex(&text.to_string());
    }

    fn update_batch(
        &mut self,
        edits: &[(InputEdit, Rope)],
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        // One parse per multi-caret operation, using the final buffer snapshot.
        if let Some((_, text)) = edits.last() {
            self.tokens = lex(&text.to_string());
        }
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut runs = Vec::new();
        let mut cursor = range.start;
        // Painting each visible line must not scan every preceding token.
        let start = self
            .tokens
            .partition_point(|token| token.range.end <= cursor);
        for token in &self.tokens[start..] {
            if token.range.start >= range.end {
                break;
            }
            let token_start = token.range.start.max(cursor);
            let token_end = token.range.end.min(range.end);
            if cursor < token_start {
                runs.push((cursor..token_start, HighlightStyle::default()));
            }
            if token_start < token_end {
                runs.push((
                    token_start..token_end,
                    resolver.style(token.kind.name()).unwrap_or_default(),
                ));
            }
            cursor = token_end;
        }
        if cursor < range.end {
            runs.push((cursor..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

/// Every offset is a UTF-8 byte offset, as required by InputHighlighter.
fn lex(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        let rest = &sql[pos..];
        let ch = rest.chars().next().expect("nonempty remainder");
        let kind;
        if ch.is_whitespace() {
            pos += ch.len_utf8();
            continue;
        } else if rest.starts_with("--") {
            pos += 2;
            while pos < bytes.len() && !matches!(bytes[pos], b'\r' | b'\n') {
                pos += 1;
            }
            kind = Kind::Comment;
        } else if rest.starts_with("/*") {
            pos += 2;
            let mut depth = 1usize;
            while pos < bytes.len() && depth != 0 {
                if bytes[pos..].starts_with(b"/*") {
                    depth += 1;
                    pos += 2;
                } else if bytes[pos..].starts_with(b"*/") {
                    depth -= 1;
                    pos += 2;
                } else {
                    pos += 1;
                }
            }
            kind = Kind::Comment;
        } else if let Some((quote_at, quote)) = quoted_start(bytes, pos) {
            pos = quoted_end(bytes, quote_at, quote);
            kind = if quote == b'\'' {
                Kind::String
            } else {
                Kind::Identifier
            };
        } else if ch == '$'
            && let Some(delimiter_len) = dollar_delimiter(rest)
        {
            let delimiter = &rest[..delimiter_len];
            let body = &rest[delimiter_len..];
            pos += body
                .find(delimiter)
                .map_or(rest.len(), |offset| delimiter_len + offset + delimiter_len);
            kind = Kind::String;
        } else if ch == '$' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) {
            pos += 2;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            kind = Kind::Parameter;
        } else if ch.is_ascii_digit()
            || (ch == '.' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit))
        {
            pos = number_end(bytes, pos);
            kind = Kind::Number;
        } else if identifier_start(ch) {
            pos += ch.len_utf8();
            while pos < bytes.len() {
                let next = sql[pos..].chars().next().expect("nonempty remainder");
                if !identifier_continue(next) {
                    break;
                }
                pos += next.len_utf8();
            }
            if !is_keyword(&sql[start..pos]) {
                continue;
            }
            kind = Kind::Keyword;
        } else if "+-*/%=<>!~^|&:#?".contains(ch) {
            pos += ch.len_utf8();
            kind = if ch == '?' {
                Kind::Parameter
            } else {
                Kind::Operator
            };
        } else {
            pos += ch.len_utf8();
            continue;
        }
        tokens.push(Token {
            range: start..pos,
            kind,
        });
    }
    tokens
}

fn identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic() || (!ch.is_ascii() && !ch.is_whitespace())
}

fn identifier_continue(ch: char) -> bool {
    identifier_start(ch) || ch.is_ascii_digit() || ch == '$'
}

fn quoted_start(bytes: &[u8], pos: usize) -> Option<(usize, u8)> {
    let first = bytes[pos];
    if matches!(first, b'\'' | b'"' | b'`') {
        Some((pos, first))
    } else if matches!(first, b'e' | b'E' | b'n' | b'N' | b'b' | b'B' | b'x' | b'X')
        && bytes.get(pos + 1) == Some(&b'\'')
    {
        Some((pos + 1, b'\''))
    } else if matches!(first, b'u' | b'U')
        && bytes.get(pos + 1) == Some(&b'&')
        && bytes
            .get(pos + 2)
            .is_some_and(|quote| matches!(quote, b'\'' | b'"'))
    {
        Some((pos + 2, bytes[pos + 2]))
    } else {
        None
    }
}

fn quoted_end(bytes: &[u8], quote_at: usize, quote: u8) -> usize {
    let mut pos = quote_at + 1;
    while pos < bytes.len() {
        if bytes[pos] == quote {
            pos += 1;
            if bytes.get(pos) != Some(&quote) {
                return pos;
            }
            pos += 1;
        } else if bytes[pos] == b'\\' {
            // PostgreSQL E strings and MySQL escapes. Skipping one byte is
            // sufficient: a UTF-8 continuation byte cannot be a delimiter.
            pos = (pos + 2).min(bytes.len());
        } else {
            pos += 1;
        }
    }
    pos
}

fn dollar_delimiter(rest: &str) -> Option<usize> {
    debug_assert!(rest.starts_with('$'));
    for (offset, ch) in rest[1..].char_indices() {
        if ch == '$' {
            return Some(offset + 2);
        }
        if !(identifier_start(ch) || (offset > 0 && ch.is_ascii_digit())) {
            return None;
        }
    }
    None
}

fn number_end(bytes: &[u8], mut pos: usize) -> usize {
    if bytes[pos] == b'0'
        && let Some(prefix) = bytes.get(pos + 1)
    {
        let radix = match prefix {
            b'x' | b'X' => 16,
            b'b' | b'B' => 2,
            b'o' | b'O' => 8,
            _ => 0,
        };
        if radix != 0
            && bytes
                .get(pos + 2)
                .is_some_and(|b| (*b as char).is_digit(radix))
        {
            pos += 3;
            while pos < bytes.len() && (bytes[pos] == b'_' || (bytes[pos] as char).is_digit(radix))
            {
                pos += 1;
            }
            return pos;
        }
    }
    while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
        pos += 1;
    }
    if bytes.get(pos) == Some(&b'.') && bytes.get(pos + 1) != Some(&b'.') {
        pos += 1;
        while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
            pos += 1;
        }
    }
    if bytes.get(pos).is_some_and(|b| matches!(b, b'e' | b'E')) {
        let mut end = pos + 1;
        if bytes.get(end).is_some_and(|b| matches!(b, b'+' | b'-')) {
            end += 1;
        }
        // While typing an incomplete exponent, leave the unfinished suffix plain.
        if bytes.get(end).is_some_and(u8::is_ascii_digit) {
            pos = end + 1;
            while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
                pos += 1;
            }
        }
    }
    pos
}

fn is_keyword(word: &str) -> bool {
    // Presentation vocabulary shared by PostgreSQL and MySQL; intentionally
    // includes common types and builtins, without pretending to resolve names.
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ADD"
            | "ALL"
            | "ALTER"
            | "ANALYZE"
            | "AND"
            | "ANY"
            | "ARRAY"
            | "AS"
            | "ASC"
            | "AVG"
            | "BEGIN"
            | "BETWEEN"
            | "BIGINT"
            | "BINARY"
            | "BIT"
            | "BOOL"
            | "BOOLEAN"
            | "BOTH"
            | "BY"
            | "BYTEA"
            | "CALL"
            | "CASCADE"
            | "CASE"
            | "CAST"
            | "CHAR"
            | "CHARACTER"
            | "CHECK"
            | "COALESCE"
            | "COLLATE"
            | "COLUMN"
            | "COMMIT"
            | "CONCURRENTLY"
            | "CONFLICT"
            | "CONSTRAINT"
            | "COPY"
            | "COUNT"
            | "CREATE"
            | "CROSS"
            | "CURRENT_DATE"
            | "CURRENT_TIME"
            | "CURRENT_TIMESTAMP"
            | "CURRENT_USER"
            | "DATABASE"
            | "DATE"
            | "DATETIME"
            | "DECIMAL"
            | "DECLARE"
            | "DEFAULT"
            | "DELETE"
            | "DESC"
            | "DESCRIBE"
            | "DISTINCT"
            | "DO"
            | "DOUBLE"
            | "DROP"
            | "DUPLICATE"
            | "ELSE"
            | "END"
            | "ENUM"
            | "ESCAPE"
            | "EXCEPT"
            | "EXCLUDED"
            | "EXISTS"
            | "EXPLAIN"
            | "FALSE"
            | "FETCH"
            | "FILTER"
            | "FIRST"
            | "FLOAT"
            | "FOLLOWING"
            | "FOR"
            | "FOREIGN"
            | "FROM"
            | "FULL"
            | "FUNCTION"
            | "GENERATED"
            | "GRANT"
            | "GROUP"
            | "HAVING"
            | "ILIKE"
            | "IN"
            | "INDEX"
            | "INNER"
            | "INSERT"
            | "INT"
            | "INTEGER"
            | "INTERSECT"
            | "INTERVAL"
            | "INTO"
            | "IS"
            | "ISOLATION"
            | "JOIN"
            | "JSON"
            | "JSONB"
            | "KEY"
            | "LAST"
            | "LATERAL"
            | "LEADING"
            | "LEFT"
            | "LIKE"
            | "LIMIT"
            | "LOCAL"
            | "LOCK"
            | "MATERIALIZED"
            | "MAX"
            | "MIN"
            | "NATURAL"
            | "NOT"
            | "NOTHING"
            | "NOW"
            | "NULL"
            | "NULLIF"
            | "NULLS"
            | "NUMERIC"
            | "OF"
            | "OFFSET"
            | "ON"
            | "ONLY"
            | "OR"
            | "ORDER"
            | "OUTER"
            | "OVER"
            | "PARTITION"
            | "PRECEDING"
            | "PRIMARY"
            | "PROCEDURE"
            | "RANGE"
            | "READ"
            | "REAL"
            | "RECURSIVE"
            | "REFERENCES"
            | "REGEXP"
            | "RELEASE"
            | "RENAME"
            | "REPLACE"
            | "RESTRICT"
            | "RETURNING"
            | "RETURNS"
            | "REVOKE"
            | "RIGHT"
            | "ROLLBACK"
            | "ROW"
            | "ROWS"
            | "SAVEPOINT"
            | "SCHEMA"
            | "SELECT"
            | "SEQUENCE"
            | "SERIAL"
            | "SESSION"
            | "SET"
            | "SHOW"
            | "SIMILAR"
            | "SMALLINT"
            | "SOME"
            | "START"
            | "SUM"
            | "TABLE"
            | "TEMP"
            | "TEMPORARY"
            | "TEXT"
            | "THEN"
            | "TIME"
            | "TIMESTAMP"
            | "TIMESTAMPTZ"
            | "TO"
            | "TRAILING"
            | "TRANSACTION"
            | "TRIGGER"
            | "TRUE"
            | "TRUNCATE"
            | "TYPE"
            | "UNBOUNDED"
            | "UNION"
            | "UNIQUE"
            | "UNKNOWN"
            | "UNSIGNED"
            | "UPDATE"
            | "USE"
            | "USER"
            | "USING"
            | "UUID"
            | "VACUUM"
            | "VALUES"
            | "VARCHAR"
            | "VARYING"
            | "VIEW"
            | "WHEN"
            | "WHERE"
            | "WINDOW"
            | "WITH"
            | "WITHOUT"
            | "WORK"
            | "WRITE"
            | "ZONE"
    )
}

#[cfg(test)]
mod tests {
    use super::{Kind, SqlHighlighter, editor_style, lex};
    use gpui_kit::HighlightStyle;
    use gpui_kit::base::input::{HighlightStyleResolver, InputHighlighter};
    use gpui_omarchy::Theme;

    fn tokens(sql: &str) -> Vec<(&str, Kind)> {
        lex(sql)
            .into_iter()
            .map(|t| (&sql[t.range], t.kind))
            .collect()
    }

    #[test]
    fn unicode_identifiers_are_not_split_into_ascii_keywords() {
        let sql = "sElEcT café, selecté, 中文, \"名\", '🙂' FROM 表 WHERE id = $12";
        assert_eq!(
            tokens(sql),
            vec![
                ("sElEcT", Kind::Keyword),
                ("\"名\"", Kind::Identifier),
                ("'🙂'", Kind::String),
                ("FROM", Kind::Keyword),
                ("WHERE", Kind::Keyword),
                ("=", Kind::Operator),
                ("$12", Kind::Parameter),
            ]
        );
    }

    #[test]
    fn quoted_content_hides_comments_and_keywords() {
        let sql = r#"'it''s -- SELECT' E'it\'s /* ok */' "a""b" `a``b` U&"名" N'你好'"#;
        assert_eq!(
            tokens(sql),
            vec![
                ("'it''s -- SELECT'", Kind::String),
                (r"E'it\'s /* ok */'", Kind::String),
                ("\"a\"\"b\"", Kind::Identifier),
                ("`a``b`", Kind::Identifier),
                ("U&\"名\"", Kind::Identifier),
                ("N'你好'", Kind::String),
            ]
        );
    }

    #[test]
    fn dollar_strings_require_matching_case_sensitive_tags() {
        let sql = "$$'--'$$ $body$SELECT $other$ '$BODY$' $body$ $名2$🙂$名2$ $1 foo$tag$";
        assert_eq!(
            tokens(sql),
            vec![
                ("$$'--'$$", Kind::String),
                ("$body$SELECT $other$ '$BODY$' $body$", Kind::String),
                ("$名2$🙂$名2$", Kind::String),
                ("$1", Kind::Parameter),
            ]
        );
    }

    #[test]
    fn comments_end_at_cr_or_lf_and_block_comments_nest() {
        let sql = "-- α\rSELECT -- β\nFROM -- γ\r\nWHERE /* 外 /* inner */ tail */ TRUE";
        assert_eq!(
            tokens(sql),
            vec![
                ("-- α", Kind::Comment),
                ("SELECT", Kind::Keyword),
                ("-- β", Kind::Comment),
                ("FROM", Kind::Keyword),
                ("-- γ", Kind::Comment),
                ("WHERE", Kind::Keyword),
                ("/* 外 /* inner */ tail */", Kind::Comment),
                ("TRUE", Kind::Keyword),
            ]
        );
    }

    #[test]
    fn numeric_formats_do_not_consume_neighboring_operators() {
        let sql = "42 .5 1. 2.3e-4 1E+2 0xCA_FE 0b101 0o77 1_000 1+2 3e+";
        assert_eq!(
            tokens(sql),
            vec![
                ("42", Kind::Number),
                (".5", Kind::Number),
                ("1.", Kind::Number),
                ("2.3e-4", Kind::Number),
                ("1E+2", Kind::Number),
                ("0xCA_FE", Kind::Number),
                ("0b101", Kind::Number),
                ("0o77", Kind::Number),
                ("1_000", Kind::Number),
                ("1", Kind::Number),
                ("+", Kind::Operator),
                ("2", Kind::Number),
                ("3", Kind::Number),
                ("+", Kind::Operator),
            ]
        );
    }

    #[test]
    fn unfinished_constructs_extend_to_eof_during_typing() {
        for sql in [
            "'你好",
            "\"名",
            "`名",
            "E'escaped\\",
            "$tag$body",
            "/* nested /* x */",
        ] {
            let parsed = lex(sql);
            assert_eq!(parsed.len(), 1, "{sql}");
            assert_eq!(parsed[0].range, 0..sql.len());
        }
    }

    #[test]
    fn every_typed_prefix_has_ordered_utf8_token_boundaries() {
        let sql = "SELECT é, U&\"名\", E'🙂\\\'a', $名$雪$名$, 1e-3 /* 中 /* -- */ */\r\nFROM café";
        for end in (0..=sql.len()).filter(|&end| sql.is_char_boundary(end)) {
            let prefix = &sql[..end];
            let mut previous = 0;
            for token in lex(prefix) {
                assert!(previous <= token.range.start && token.range.start < token.range.end);
                assert!(prefix.is_char_boundary(token.range.start));
                assert!(prefix.is_char_boundary(token.range.end));
                previous = token.range.end;
            }
        }
    }

    struct NoStyles;
    impl HighlightStyleResolver for NoStyles {
        fn style(&self, _: &str) -> Option<HighlightStyle> {
            None
        }
    }

    #[test]
    fn style_runs_cover_each_requested_window_without_gaps_or_overlaps() {
        let sql = "  SELECT café, '雪' -- hello\r\nFROM 表  ";
        let highlighter = SqlHighlighter { tokens: lex(sql) };
        let palette = editor_style(&Theme::tokyo_night());
        let boundaries: Vec<_> = (0..=sql.len())
            .filter(|&i| sql.is_char_boundary(i))
            .collect();
        for &start in &boundaries {
            for &end in boundaries.iter().filter(|&&end| end >= start) {
                let range = start..end;
                let runs = highlighter.styles(&range, palette.highlight_styles.as_ref());
                let mut cursor = start;
                for (run, _) in runs {
                    assert_eq!(run.start, cursor, "{range:?}");
                    assert!(run.start < run.end && run.end <= end);
                    assert!(sql.is_char_boundary(run.start) && sql.is_char_boundary(run.end));
                    cursor = run.end;
                }
                assert_eq!(cursor, end, "{range:?}");
            }
        }
        let plain = highlighter.styles(&(0..sql.len()), &NoStyles);
        assert!(
            plain
                .iter()
                .all(|(_, style)| *style == HighlightStyle::default())
        );
    }

    #[test]
    fn styles_use_the_current_palette_without_relexing() {
        let highlighter = SqlHighlighter {
            tokens: lex("SELECT 'x' 42 -- comment"),
        };
        for theme in [Theme::tokyo_night(), Theme::flexoki_light()] {
            let style = editor_style(&theme);
            let runs = highlighter.styles(&(0..23), style.highlight_styles.as_ref());
            assert_eq!(runs[0].1.color, Some(theme.accent));
            assert_eq!(runs[2].1.color, Some(theme.success));
            assert_eq!(runs[4].1.color, Some(theme.warning));
            assert_eq!(runs[6].1.color, Some(theme.secondary.opacity(0.75)));
            assert_eq!(style.background, theme.background);
            assert!(style.selection.a > 0. && style.selection.a < 1.);
        }
    }
}
