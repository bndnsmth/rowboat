//! SQLite formatting is whitespace-only: never reinterpret strings with another
//! engine's escape rules, uppercase identifiers, translate SQL, or execute it.
//!
//! Token boundaries follow SQLite's sqlite3GetToken (including Tcl-style named
//! parameters). This is lexical validation, not schema-dependent SQL validation.
//! Preparing arbitrary input is unsuitable: some PRAGMAs act during preparation,
//! and valid editor SQL may depend on tables/functions unavailable to a scratch DB.
use anyhow::{Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Word,
    Quoted,
    Number,
    Parameter,
    Blob,
    Symbol,
    LineComment,
    BlockComment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Token<'a> {
    text: &'a str,
    kind: Kind,
}

impl Token<'_> {
    fn word(self, word: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(word)
    }
}

pub(super) fn format_sqlite(sql: &str) -> Result<String> {
    let tokens = tokenize(sql)?;
    let mut output = String::with_capacity(sql.len());
    let mut depth = 0usize;
    for (index, &token) in tokens.iter().enumerate() {
        if token.text == ")" && token.kind == Kind::Symbol {
            depth = depth.saturating_sub(1);
        }
        if let Some(&previous) = index.checked_sub(1).and_then(|i| tokens.get(i)) {
            let line = previous.kind == Kind::LineComment
                || previous.text == ";"
                || (depth == 0 && clause_start(&tokens, index));
            if line {
                output.push('\n');
            } else if !tight_boundary(previous, token) {
                output.push(' ');
            }
        }
        output.push_str(token.text);
        if token.text == "(" && token.kind == Kind::Symbol {
            depth += 1;
        }
    }
    // Defense in depth: whitespace edits must preserve every token byte and its
    // identity, including comments, parameter slots and adjacent operator pairs.
    ensure!(
        tokenize(&output)? == tokens,
        "SQLite formatting could not preserve token boundaries; SQL was not changed"
    );
    Ok(output)
}

fn clause_start(tokens: &[Token<'_>], index: usize) -> bool {
    let token = tokens[index];
    // These are layout hints only, not a parser. SQLite's keyword-as-identifier
    // fallback cannot change meaning because token spelling/order is untouched.
    [
        "SELECT",
        "FROM",
        "WHERE",
        "HAVING",
        "LIMIT",
        "RETURNING",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "VALUES",
        "SET",
    ]
    .iter()
    .any(|word| token.word(word))
        || (["ORDER", "GROUP"].iter().any(|word| token.word(word))
            && tokens.get(index + 1).is_some_and(|next| next.word("BY")))
        || (token.word("ON")
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.word("CONFLICT")))
}

fn tight_boundary(left: Token<'_>, right: Token<'_>) -> bool {
    if right.kind == Kind::Symbol && matches!(right.text, "," | ";" | ")") {
        return left.kind != Kind::LineComment;
    }
    if left.kind == Kind::Symbol && left.text == "(" {
        return true;
    }
    // Never attach a dot to a number: `1 . x` and `1. x` tokenize differently.
    // Named/numbered parameters are kept intact by the tokenizer itself.
    (left.text == "." && matches!(right.kind, Kind::Word | Kind::Quoted))
        || (right.text == "." && matches!(left.kind, Kind::Word | Kind::Quoted))
        || (left.text == "." && right.text == "*")
}

fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || byte >= 0x80
}

fn quoted_end(bytes: &[u8], start: usize) -> Result<usize> {
    let delimiter = bytes[start];
    let mut at = start + 1;
    while let Some(&byte) = bytes.get(at) {
        at += 1;
        if byte == delimiter {
            if bytes.get(at) == Some(&delimiter) {
                at += 1;
            } else {
                return Ok(at);
            }
        }
    }
    bail!("Unterminated SQLite quoted token at byte {start}")
}

fn digits(bytes: &[u8], at: &mut usize, hex: bool) -> Result<()> {
    let digit = |byte: u8| {
        if hex {
            byte.is_ascii_hexdigit()
        } else {
            byte.is_ascii_digit()
        }
    };
    while let Some(&byte) = bytes.get(*at) {
        if digit(byte) {
            *at += 1;
        } else if byte == b'_' {
            ensure!(
                *at > 0 && digit(bytes[*at - 1]) && bytes.get(*at + 1).is_some_and(|b| digit(*b)),
                "Invalid SQLite numeric separator at byte {}",
                *at
            );
            *at += 1;
        } else {
            break;
        }
    }
    Ok(())
}

fn tokenize(sql: &str) -> Result<Vec<Token<'_>>> {
    let bytes = sql.as_bytes();
    ensure!(!bytes.contains(&0), "SQLite SQL cannot contain a NUL byte");
    let mut tokens = Vec::new();
    let mut at = 0;
    let mut depth = 0usize;
    while at < bytes.len() {
        if space(bytes[at]) {
            at += 1;
            continue;
        }
        if bytes[at..].starts_with(&[0xef, 0xbb, 0xbf]) {
            at += 3;
            continue;
        }
        let start = at;
        let kind = match bytes[at] {
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                at += 2;
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                Kind::LineComment
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at + 1 < bytes.len() && &bytes[at..at + 2] != b"*/" {
                    at += 1;
                }
                ensure!(
                    at + 1 < bytes.len(),
                    "Unterminated SQLite comment at byte {start}"
                );
                at += 2;
                Kind::BlockComment
            }
            b'\'' | b'"' | b'`' => {
                at = quoted_end(bytes, at)?;
                Kind::Quoted
            }
            b'[' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b']' {
                    at += 1;
                }
                ensure!(
                    at < bytes.len(),
                    "Unterminated SQLite bracket identifier at byte {start}"
                );
                at += 1;
                Kind::Quoted
            }
            b'x' | b'X' if bytes.get(at + 1) == Some(&b'\'') => {
                at += 2;
                let content = at;
                while at < bytes.len() && bytes[at].is_ascii_hexdigit() {
                    at += 1;
                }
                ensure!(
                    bytes.get(at) == Some(&b'\'') && (at - content).is_multiple_of(2),
                    "Invalid SQLite blob literal at byte {start}"
                );
                at += 1;
                Kind::Blob
            }
            b'?' => {
                at += 1;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                Kind::Parameter
            }
            b':' | b'$' | b'@' | b'#' => {
                at += 1;
                let mut named = false;
                while let Some(&byte) = bytes.get(at) {
                    if identifier(byte) {
                        named = true;
                        at += 1;
                    } else if byte == b':' && bytes.get(at + 1) == Some(&b':') {
                        at += 2;
                    } else if byte == b'(' && named {
                        at += 1;
                        while at < bytes.len() && !space(bytes[at]) && bytes[at] != b')' {
                            at += 1;
                        }
                        ensure!(
                            bytes.get(at) == Some(&b')'),
                            "Invalid SQLite parameter suffix at byte {start}"
                        );
                        at += 1;
                        break;
                    } else {
                        break;
                    }
                }
                ensure!(named, "Missing SQLite parameter name at byte {start}");
                Kind::Parameter
            }
            byte if byte.is_ascii_digit()
                || (byte == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                if bytes[at] == b'0'
                    && matches!(bytes.get(at + 1), Some(b'x' | b'X'))
                    && bytes.get(at + 2).is_some_and(u8::is_ascii_hexdigit)
                {
                    at += 2;
                    digits(bytes, &mut at, true)?;
                } else {
                    digits(bytes, &mut at, false)?;
                    if bytes.get(at) == Some(&b'.') {
                        at += 1;
                        digits(bytes, &mut at, false)?;
                    }
                    if matches!(bytes.get(at), Some(b'e' | b'E')) {
                        at += 1;
                        if matches!(bytes.get(at), Some(b'+' | b'-')) {
                            at += 1;
                        }
                        ensure!(
                            bytes.get(at).is_some_and(u8::is_ascii_digit),
                            "Invalid SQLite exponent at byte {start}"
                        );
                        digits(bytes, &mut at, false)?;
                    }
                }
                ensure!(
                    !bytes.get(at).is_some_and(|b| identifier(*b)),
                    "Invalid SQLite number at byte {start}"
                );
                Kind::Number
            }
            byte if identifier(byte) => {
                at += 1;
                while bytes.get(at).is_some_and(|b| identifier(*b)) {
                    at += 1;
                }
                Kind::Word
            }
            b'(' => {
                depth += 1;
                at += 1;
                Kind::Symbol
            }
            b')' => {
                ensure!(
                    depth > 0,
                    "Unmatched SQLite closing parenthesis at byte {start}"
                );
                depth -= 1;
                at += 1;
                Kind::Symbol
            }
            b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'|' | b'&' | b'~'
            | b'.' | b',' | b';' => {
                let remaining = &bytes[at..];
                let length = if remaining.starts_with(b"->>") {
                    3
                } else if [
                    b"->", b"||", b"<<", b">>", b"<=", b">=", b"!=", b"<>", b"==",
                ]
                .iter()
                .any(|op| remaining.starts_with(*op))
                {
                    2
                } else {
                    ensure!(bytes[at] != b'!', "Invalid SQLite operator at byte {start}");
                    1
                };
                at += length;
                Kind::Symbol
            }
            _ => bail!("Invalid SQLite token at byte {start}"),
        };
        tokens.push(Token {
            text: &sql[start..at],
            kind,
        });
    }
    ensure!(depth == 0, "Unclosed SQLite parenthesis");
    Ok(tokens)
}

#[cfg(test)]
#[path = "sqlite_format_tests.rs"]
mod tests;
