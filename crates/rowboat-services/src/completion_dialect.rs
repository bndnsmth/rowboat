//! Parser policy for bounded, conservative completion, not execution SQL.
//! Keep engine registration here so the shared scope analyzer uses semantics.
use rowboat_core::{DialectFamily, Engine, IdentifierCase, NamespaceBehavior};

#[derive(Clone, Copy)]
pub(crate) enum NameCase {
    Exact,
    AsciiInsensitive,
    UnicodeInsensitive,
}

impl NameCase {
    pub(crate) fn key(self, name: &str) -> String {
        match self {
            Self::Exact => name.into(),
            Self::AsciiInsensitive => name.to_ascii_lowercase(),
            Self::UnicodeInsensitive => name.to_lowercase(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct IdentifierQuote {
    pub(crate) close: char,
    pub(crate) doubled_escape: bool,
}

pub(crate) struct CompletionRules {
    pub(crate) catalog_case: NameCase,
    pub(crate) column_case: NameCase,
    pub(crate) prefix_case: NameCase,
    pub(crate) fold_unquoted: bool,
    pub(crate) quoted_case_sensitive: bool,
    primary_quote: char,
    alternate_quotes: &'static str,
    pub(crate) double_quote_strings: bool,
    pub(crate) backslash_strings: bool,
    pub(crate) dollar_strings: bool,
    pub(crate) dash_requires_space: bool,
    pub(crate) hash_comments: bool,
    pub(crate) cr_ends_comment: bool,
    pub(crate) nested_comments: bool,
    pub(crate) executable_comments: bool,
    pub(crate) single_database: bool,
    pub(crate) partial_output_aliases: bool,
    pub(crate) source_output_aliases: bool,
    pub(crate) forward_recursive_ctes: bool,
    pub(crate) distinct_on: bool,
}

impl CompletionRules {
    pub(crate) fn for_engine(engine: Engine) -> Self {
        let descriptor = engine.descriptor().dialect;
        let fold_unquoted = descriptor.unquoted_case == IdentifierCase::Lower;
        let primary_quote = descriptor.identifier_quote;
        let single_database = engine.descriptor().namespaces == NamespaceBehavior::Databases;
        match descriptor.family {
            DialectFamily::PostgreSql => Self {
                catalog_case: NameCase::Exact,
                column_case: NameCase::Exact,
                prefix_case: NameCase::UnicodeInsensitive,
                fold_unquoted,
                quoted_case_sensitive: true,
                primary_quote,
                alternate_quotes: "",
                double_quote_strings: false,
                // Preserve conservative suppression for drafts whose server
                // standard_conforming_strings setting is not known.
                backslash_strings: true,
                dollar_strings: true,
                dash_requires_space: false,
                hash_comments: false,
                cr_ends_comment: true,
                nested_comments: true,
                executable_comments: false,
                single_database,
                partial_output_aliases: true,
                source_output_aliases: true,
                forward_recursive_ctes: true,
                distinct_on: true,
            },
            DialectFamily::MySql => Self {
                catalog_case: NameCase::AsciiInsensitive,
                column_case: NameCase::UnicodeInsensitive,
                prefix_case: NameCase::UnicodeInsensitive,
                fold_unquoted,
                quoted_case_sensitive: true,
                primary_quote,
                alternate_quotes: "",
                double_quote_strings: true,
                backslash_strings: true,
                dollar_strings: false,
                dash_requires_space: true,
                hash_comments: true,
                cr_ends_comment: false,
                nested_comments: false,
                executable_comments: true,
                single_database,
                partial_output_aliases: false,
                source_output_aliases: true,
                forward_recursive_ctes: false,
                distinct_on: false,
            },
            DialectFamily::Sqlite => Self {
                catalog_case: NameCase::AsciiInsensitive,
                column_case: NameCase::AsciiInsensitive,
                prefix_case: NameCase::AsciiInsensitive,
                fold_unquoted,
                // SQLite resolves even quoted ASCII names case-insensitively.
                // Typed quoted completion prefixes remain exact-case separately.
                quoted_case_sensitive: false,
                primary_quote,
                alternate_quotes: "`[",
                double_quote_strings: false,
                backslash_strings: false,
                dollar_strings: false,
                dash_requires_space: false,
                hash_comments: false,
                cr_ends_comment: false,
                nested_comments: false,
                executable_comments: false,
                single_database,
                partial_output_aliases: false,
                source_output_aliases: false,
                forward_recursive_ctes: true,
                distinct_on: false,
            },
        }
    }

    pub(crate) fn identifier_quote(&self, open: char) -> Option<IdentifierQuote> {
        (self.primary_quote == open || self.alternate_quotes.contains(open)).then_some(
            IdentifierQuote {
                close: if open == '[' { ']' } else { open },
                doubled_escape: open != '[',
            },
        )
    }

    pub(crate) fn resolved(&self, text: &str, quoted: bool) -> String {
        if self.fold_unquoted && !quoted {
            text.to_ascii_lowercase()
        } else {
            text.into()
        }
    }

    pub(crate) fn matches(&self, text: &str, quoted: bool, name: &str) -> bool {
        if quoted && self.quoted_case_sensitive {
            text == name
        } else if self.fold_unquoted && !quoted {
            text.chars()
                .map(|c| c.to_ascii_lowercase())
                .eq(name.chars())
        } else {
            match self.catalog_case {
                NameCase::Exact => text == name,
                NameCase::AsciiInsensitive => text.eq_ignore_ascii_case(name),
                NameCase::UnicodeInsensitive => text.to_lowercase() == name.to_lowercase(),
            }
        }
    }

    pub(crate) fn column_matches(&self, text: &str, quoted: bool, name: &str) -> bool {
        match self.column_case {
            NameCase::Exact => self.matches(text, quoted, name),
            NameCase::AsciiInsensitive => text.eq_ignore_ascii_case(name),
            NameCase::UnicodeInsensitive => text.to_lowercase() == name.to_lowercase(),
        }
    }
}
