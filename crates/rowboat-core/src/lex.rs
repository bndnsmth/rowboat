//! Shared dialect rules for policy scanning, splitting, and leading comments.
//! PostgreSQL scan.l: newline=[\n\r], space=[ \t\n\r\f\v].
//! MySQL sql_lex.cc: -- requires my_isspace OR my_iscntrl; both -- and #
//! consume through LF (a bare CR remains inside a MySQL line comment).
use crate::Engine;

pub(crate) fn sql_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

pub(crate) fn dash_comment(engine: Engine, following: Option<&u8>) -> bool {
    engine == Engine::Postgres || following.is_none_or(|b| sql_space(*b) || b.is_ascii_control())
}

pub(crate) fn line_comment_end(engine: Engine, byte: u8) -> bool {
    byte == b'\n' || (engine == Engine::Postgres && byte == b'\r')
}

pub(crate) fn ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || byte >= 128
}

/// After a PostgreSQL closing single quote, newline-separated literal segments
/// continue the original string's escape mode (including E strings). Only space
/// and -- comments belong to scan.l's quotecontinue; block comments end the token.
pub(crate) fn pg_quote_continuation(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut newline = false;
    while let Some(&b) = bytes.get(i) {
        if sql_space(b) {
            newline |= matches!(b, b'\r' | b'\n');
            i += 1;
        } else if bytes[i..].starts_with(b"--") {
            while i < bytes.len() && !line_comment_end(Engine::Postgres, bytes[i]) {
                i += 1;
            }
        } else {
            break;
        }
    }
    (newline && bytes.get(i) == Some(&b'\'')).then_some(i)
}
