use rowboat_admin::query::{expand_named, explain_sql};
use rowboat_core::{Cell, Engine};
use std::collections::BTreeMap;

fn parameters() -> BTreeMap<String, Cell> {
    BTreeMap::from([("after".into(), Cell::Null)])
}

#[test]
fn postgres_parameters_resume_after_cr_lf_and_crlf() {
    for ending in ["\r", "\n", "\r\n"] {
        let sql = format!("SELECT 1 -- :hidden{ending}, :after::text");
        assert_eq!(
            expand_named(Engine::Postgres, &sql, &parameters()).unwrap(),
            format!("SELECT 1 -- :hidden{ending}, NULL::text")
        );
        let missing = format!("SELECT 1 -- hidden{ending}, :missing");
        assert!(expand_named(Engine::Postgres, &missing, &parameters()).is_err());
        assert!(
            explain_sql(
                Engine::Postgres,
                &format!("-- heading{ending}SELECT 1 -- trailing{ending}")
            )
            .is_ok()
        );
    }
}

#[test]
fn mysql_parameters_follow_native_lf_comment_termination() {
    for marker in ["-- ", "#"] {
        for ending in ["\n", "\r\n"] {
            let sql = format!("SELECT 1 {marker}:hidden{ending}, :after");
            assert_eq!(
                expand_named(Engine::MySql, &sql, &parameters()).unwrap(),
                format!("SELECT 1 {marker}:hidden{ending}, NULL")
            );
            assert!(
                explain_sql(
                    Engine::MySql,
                    &format!("{marker}heading{ending}SELECT 1; {marker}trailing{ending}")
                )
                .is_ok()
            );
        }
        // MySQL 8's lexer consumes bare CR as part of --/# comment text.
        let sql = format!("SELECT 1 {marker}:hidden\r:still_hidden\n, :after");
        assert_eq!(
            expand_named(Engine::MySql, &sql, &parameters()).unwrap(),
            format!("SELECT 1 {marker}:hidden\r:still_hidden\n, NULL")
        );
        assert!(explain_sql(Engine::MySql, &sql).is_err());
    }
}

#[test]
fn mysql_dash_comment_requires_whitespace_or_control() {
    for following in [
        ' ', '\t', '\r', '\u{b}', '\u{c}', '\u{1}', '\u{1f}', '\u{7f}',
    ] {
        let sql = format!("SELECT 1 --{following}:hidden\n, :after");
        assert_eq!(
            expand_named(Engine::MySql, &sql, &parameters()).unwrap(),
            format!("SELECT 1 --{following}:hidden\n, NULL")
        );
    }
    assert_eq!(
        expand_named(Engine::MySql, "SELECT 1--:after", &parameters()).unwrap(),
        "SELECT 1--NULL"
    );
    assert_eq!(
        expand_named(Engine::Postgres, "SELECT 1--:after", &parameters()).unwrap(),
        "SELECT 1--:after"
    );
    // NBSP is not a MySQL UTF-8 whitespace/control byte after --.
    assert!(expand_named(Engine::MySql, "SELECT 1--\u{a0}:missing", &parameters()).is_err());
}

#[test]
fn explain_rejects_trailing_statements_after_every_line_ending() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let markers: &[&str] = if engine == Engine::MySql {
            &["-- hidden", "# hidden"]
        } else {
            &["-- hidden"]
        };
        for ending in ["\r", "\n", "\r\n"] {
            for marker in markers {
                for sql in [
                    format!("SELECT 1 {marker}{ending}; DELETE FROM victims"),
                    format!("SELECT 1; {marker}{ending}DELETE FROM victims"),
                    format!("SELECT 1 {marker}{ending}; COMMIT; START TRANSACTION READ WRITE"),
                    format!("SELECT 1; {marker}{ending}'trailing quoted token'"),
                ] {
                    assert!(explain_sql(engine, &sql).is_err(), "{engine:?}: {sql:?}");
                }
            }
        }
    }
    assert!(explain_sql(Engine::Postgres, "SELECT 1;\r; DELETE FROM victims").is_err());
    assert!(
        explain_sql(
            Engine::MySql,
            "SELECT 1--notacomment\r; DELETE FROM victims"
        )
        .is_err()
    );
}

#[test]
fn executable_comments_are_rejected_not_treated_as_trivia() {
    for executable in [
        "/*!80000 :hidden */",
        "/*! :hidden */",
        "/*M! :hidden */",
        "/*m! :hidden */",
    ] {
        for sql in [
            format!("SELECT 1 {executable}"),
            format!("SELECT 1; {executable}"),
        ] {
            assert!(explain_sql(Engine::MySql, &sql).is_err());
            assert!(expand_named(Engine::MySql, &sql, &parameters()).is_err());
        }
    }
    assert!(explain_sql(Engine::Postgres, "SELECT 1 /* ! is just a comment here */;").is_ok());
}

#[test]
fn quoted_strings_identifiers_and_dollar_bodies_do_not_end_early() {
    let pg = "SELECT E'quote\\\'; -- :hidden\r', '-- :hidden\r; #', $body$; -- :hidden\r COMMIT;$body$, $日本語$:hidden;\r$日本語$, 1 AS \"semi;--\r:identifier\" /* outer /* ; :hidden */ still ; */ , :after";
    let rendered = expand_named(Engine::Postgres, pg, &parameters()).unwrap();
    assert_eq!(rendered, pg.replace(":after", "NULL"));
    assert!(explain_sql(Engine::Postgres, &rendered).is_ok());
    let mysql = "SELECT '-- :hidden\r; #', 1 AS `semi;--\r:identifier`, 2 AS `ends\\`, :after";
    let rendered = expand_named(Engine::MySql, mysql, &parameters()).unwrap();
    assert_eq!(rendered, mysql.replace(":after", "NULL"));
    assert!(explain_sql(Engine::MySql, &rendered).is_ok());
    // A dollar sign inside an unquoted PostgreSQL identifier is not a quote.
    assert!(
        explain_sql(
            Engine::Postgres,
            "SELECT identifier$tag$; DELETE FROM victims; -- $tag$"
        )
        .is_err()
    );
    // A backtick identifier has no backslash escapes on MySQL.
    assert!(
        explain_sql(
            Engine::MySql,
            "SELECT 1 AS `ends\\`; DELETE FROM victims; -- `"
        )
        .is_err()
    );
}

#[test]
fn explain_rejects_sql_mode_dependent_quote_boundaries() {
    for sql in [
        "SELECT 'safe\\'; INSERT INTO victims VALUES (1); -- '",
        "SELECT 1 AS \"safe\\\"; INSERT INTO victims VALUES (1); -- \"",
    ] {
        assert!(explain_sql(Engine::MySql, sql).is_err());
    }
    // Explicit PostgreSQL E strings are unambiguous across session modes.
    assert!(explain_sql(Engine::Postgres, "SELECT E'a\\\'b'").is_ok());
    assert!(explain_sql(Engine::Postgres, "SELECT 'a\\\'b'").is_err());
    assert!(explain_sql(Engine::MySql, "SELECT 'it''s fine'").is_ok());
}
