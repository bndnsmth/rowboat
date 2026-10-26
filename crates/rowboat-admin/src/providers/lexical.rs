//! Lexical behavior for shared editor helpers. These are dialect rules, not
//! optional administration capabilities; SQLite still supports named literals.
use rowboat_core::{DialectFamily, Engine};

pub(crate) struct Rules {
    pub postgres: bool,
    pub mysql: bool,
    pub dash_comment_requires_space: bool,
    pub comment_ends_at_cr: bool,
    pub nested_comments: bool,
    pub backtick_identifiers: bool,
    pub bracket_identifiers: bool,
}

pub(crate) fn rules(engine: Engine) -> Rules {
    match engine.descriptor().dialect.family {
        DialectFamily::PostgreSql => Rules {
            postgres: true,
            mysql: false,
            dash_comment_requires_space: false,
            comment_ends_at_cr: true,
            nested_comments: true,
            backtick_identifiers: false,
            bracket_identifiers: false,
        },
        DialectFamily::MySql => Rules {
            postgres: false,
            mysql: true,
            dash_comment_requires_space: true,
            comment_ends_at_cr: false,
            nested_comments: false,
            backtick_identifiers: true,
            bracket_identifiers: false,
        },
        DialectFamily::Sqlite => Rules {
            postgres: false,
            mysql: false,
            dash_comment_requires_space: false,
            comment_ends_at_cr: false,
            nested_comments: false,
            backtick_identifiers: true,
            bracket_identifiers: true,
        },
    }
}
