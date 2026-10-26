mod common;
use anyhow::Result;
use common::peak_kib;
use rowboat_core::{Engine, TableRef};
use rowboat_services::*;

fn table(schema: &str, name: &str, columns: &[&str]) -> CatalogTable {
    CatalogTable {
        table: TableRef {
            schema: schema.into(),
            name: name.into(),
        },
        columns: columns.iter().map(|s| (*s).into()).collect(),
    }
}
fn catalog(engine: Engine) -> CompletionCatalog {
    CompletionCatalog::new(
        engine,
        vec![
            table("public", "users", &["id", "name", "日本語", "Nick\"name"]),
            table("sales", "orders", &["id", "user_id", "total"]),
            table("odd schema", "Order Items", &["item id", "price"]),
        ],
    )
    .unwrap()
}
fn complete(sql: &str, engine: Engine) -> SqlCompletions {
    let caret = sql.find('|').unwrap();
    let sql = sql.replacen('|', "", 1);
    complete_sql(&sql, caret, &catalog(engine), 100).unwrap()
}
fn columns(result: &SqlCompletions) -> Vec<&str> {
    result
        .items
        .iter()
        .filter(|i| i.kind == CompletionKind::Column)
        .map(|i| i.insert_text.as_str())
        .collect()
}

#[test]
fn aliases_join_context_and_dialect_quoting() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let result = complete(
            "SELECT u.na| FROM public.users AS u JOIN sales.orders o ON o.user_id=u.id",
            engine,
        );
        assert_eq!(
            columns(&result),
            vec![if engine == Engine::Postgres {
                "\"name\""
            } else {
                "`name`"
            }]
        );
        let join = complete(
            "SELECT * FROM public.users u JOIN sales.orders o ON o.us| = u.id",
            engine,
        );
        assert_eq!(
            columns(&join),
            vec![if engine == Engine::Postgres {
                "\"user_id\""
            } else {
                "`user_id`"
            }]
        );
        let all = complete(
            "SELECT | FROM public.users u JOIN sales.orders o ON o.user_id=u.id",
            engine,
        );
        assert!(all.items.iter().any(|i| i.label == "u.id"));
        assert!(all.items.iter().any(|i| i.label == "o.id"));
    }
    let result = complete(
        "SELECT \"Case Alias\".| FROM \"odd schema\".\"Order Items\" AS \"Case Alias\"",
        Engine::Postgres,
    );
    assert_eq!(columns(&result), vec!["\"item id\"", "\"price\""]);
    let wrong = complete(
        "SELECT casealias.| FROM public.users AS \"CaseAlias\"",
        Engine::Postgres,
    );
    assert_eq!(wrong.context, CompletionContext::Unsupported);
    let mysql = complete(
        "SELECT `Case Alias`.| FROM `odd schema`.`Order Items` AS `Case Alias`",
        Engine::MySql,
    );
    assert_eq!(columns(&mysql), vec!["`item id`", "`price`"]);
}

#[test]
fn relation_context_uses_schema_and_does_not_offer_unrelated_columns() {
    let result = complete("SELECT * FROM sales.or|", Engine::Postgres);
    assert_eq!(result.context, CompletionContext::Relation);
    assert_eq!(
        result
            .items
            .iter()
            .map(|i| i.insert_text.as_str())
            .collect::<Vec<_>>(),
        vec!["\"orders\""]
    );
    let result = complete("SELECT * FROM |", Engine::Postgres);
    assert!(
        result
            .items
            .iter()
            .all(|i| matches!(i.kind, CompletionKind::Table | CompletionKind::Schema))
    );
    let result = complete("SELECT u.to| FROM public.users u", Engine::Postgres);
    assert!(result.items.is_empty());
    let result = complete("SELECT users.| FROM public.users u", Engine::Postgres);
    assert!(
        result.items.is_empty(),
        "An explicit alias must hide the original relation qualifier"
    );
}

#[test]
fn scopes_isolate_statements_nested_queries_and_union_branches() {
    for sql in [
        "SELECT * FROM public.users u; SELECT u.| FROM sales.orders o",
        "SELECT (SELECT u.| FROM sales.orders o) FROM public.users u",
        "SELECT * FROM public.users u UNION ALL SELECT u.| FROM sales.orders o",
    ] {
        assert!(
            columns(&complete(sql, Engine::Postgres)).is_empty(),
            "leaked scope: {sql}"
        );
    }
    let result = complete(
        "SELECT (SELECT o.| FROM sales.orders o) FROM public.users o",
        Engine::Postgres,
    );
    assert!(columns(&result).contains(&"\"total\""));
    assert!(!columns(&result).contains(&"\"name\""));
    let outer = complete(
        "SELECT o.|, (SELECT * FROM sales.orders o) FROM public.users o",
        Engine::Postgres,
    );
    assert!(columns(&outer).contains(&"\"name\""));
}

#[test]
fn ctes_and_derived_tables_expose_only_projected_names() {
    for sql in [
        "WITH c AS (SELECT id AS uid, name FROM public.users) SELECT c.| FROM c",
        "SELECT c.| FROM (SELECT id AS uid, name FROM public.users) c",
        "WITH first AS (SELECT id AS uid, name FROM public.users), c AS (SELECT * FROM first) SELECT c.| FROM c",
    ] {
        assert_eq!(
            columns(&complete(sql, Engine::Postgres)),
            vec!["\"name\"", "\"uid\""],
            "{sql}"
        );
    }
    let explicit = complete(
        "WITH c(\"Output ID\", label) AS (SELECT id, name FROM public.users) SELECT c.| FROM c",
        Engine::Postgres,
    );
    assert_eq!(columns(&explicit), vec!["\"label\"", "\"Output ID\""]);
    let recursive = complete(
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT c.| FROM c) SELECT * FROM c",
        Engine::Postgres,
    );
    assert_eq!(columns(&recursive), vec!["\"n\""]);
    let later = complete(
        "WITH first AS (SELECT later.| FROM later), later AS (SELECT name FROM public.users) SELECT * FROM first",
        Engine::Postgres,
    );
    assert!(
        columns(&later).is_empty(),
        "A CTE saw a later sibling declaration"
    );
}

#[test]
fn skips_comments_strings_dollar_quotes_and_comment_shaped_strings() {
    for sql in [
        "SELECT 'u.na|me' FROM public.users u",
        "SELECT E'escape \\' u.|' FROM public.users u",
        "SELECT $$u.|$$ FROM public.users u",
        "SELECT $tag$u.|$tag$ FROM public.users u",
        "SELECT 1 -- u.|\n FROM public.users u",
        "SELECT /* outer /* inner */ u.| */ 1",
        "SELECT 'unterminated u.|",
        "SELECT $tag$unterminated u.|",
        "SELECT /* unterminated u.|",
    ] {
        let result = complete(sql, Engine::Postgres);
        assert_eq!(result.context, CompletionContext::Suppressed, "{sql}");
        assert!(result.items.is_empty());
    }
    for sql in ["SELECT # u.|", "SELECT \"u.|\"", "SELECT -- u.|\n1"] {
        assert_eq!(
            complete(sql, Engine::MySql).context,
            CompletionContext::Suppressed
        );
    }
    let actual = complete(
        "SELECT '-- FROM sales.orders x', u.na| FROM public.users u /* JOIN sales.orders x */",
        Engine::Postgres,
    );
    assert_eq!(columns(&actual), vec!["\"name\""]);
}

#[test]
fn unicode_and_mid_identifier_edits_replace_the_whole_identifier() {
    let sql = "SELECT u.日|本語 FROM public.users u";
    let result = complete(sql, Engine::Postgres);
    let original = sql.replace('|', "");
    assert_eq!(&original[result.replacement.clone()], "日本語");
    assert_eq!(columns(&result), vec!["\"日本語\""]);
    let sql = "SELECT u.\"Ni|ck\"\"name\" FROM public.users u";
    let result = complete(sql, Engine::Postgres);
    assert_eq!(
        &sql.replace('|', "")[result.replacement.clone()],
        "\"Nick\"\"name\""
    );
    assert_eq!(columns(&result), vec!["\"Nick\"\"name\""]);
    assert!(complete_sql("日本語", 1, &catalog(Engine::Postgres), 20).is_err());
    assert!(complete_sql("x", 2, &catalog(Engine::Postgres), 20).is_err());
    let unicode =
        CompletionCatalog::new(Engine::Postgres, vec![table("public", "Ωusers", &["Name"])])
            .unwrap();
    let sql = "SELECT Ω. FROM public.Ωusers Ω";
    assert_eq!(
        columns(&complete_sql(sql, "SELECT Ω.".len(), &unicode, 20).unwrap()),
        vec!["\"Name\""]
    );
}

#[test]
fn unknown_ambiguous_and_complex_sources_do_not_invent_catalog_columns() {
    let duplicate = CompletionCatalog::new(
        Engine::Postgres,
        vec![
            table("one", "users", &["secret_one"]),
            table("two", "users", &["secret_two"]),
        ],
    )
    .unwrap();
    let sql = "SELECT u. FROM users u";
    let result = complete_sql(sql, 9, &duplicate, 10).unwrap();
    assert!(columns(&result).is_empty());
    assert!(result.reason.is_some());
    let complex = complete(
        "SELECT f.| FROM generate_series(1, 10) AS f",
        Engine::Postgres,
    );
    assert!(columns(&complex).is_empty());
    assert!(complex.reason.is_some());
    let partial = complete(
        "SELECT d.| FROM (SELECT count(*), name AS label FROM public.users GROUP BY name) d",
        Engine::Postgres,
    );
    assert_eq!(columns(&partial), vec!["\"label\""]);
    assert!(partial.reason.is_some());
}

#[test]
fn invalid_and_large_catalogs_are_bounded_and_deterministic() -> Result<()> {
    let mut tables = Vec::new();
    for i in 0..10_000 {
        tables.push(table("large", &format!("table_{i:05}"), &["id", "value"]));
    }
    let catalog = CompletionCatalog::new(Engine::Postgres, tables)?;
    let a = complete_sql("SELECT * FROM large.table_", 26, &catalog, 7)?;
    assert_eq!(a.items.len(), 7);
    assert!(a.truncated);
    assert_eq!(
        a.items,
        complete_sql("SELECT * FROM large.table_", 26, &catalog, 7)?.items
    );
    assert!(CompletionCatalog::new(Engine::Postgres, vec![table("", "t", &["id", "id"])]).is_err());
    assert!(CompletionCatalog::new(Engine::Postgres, vec![table("", "t", &[]); 10_001]).is_err());
    assert!(complete_sql(&"x".repeat(256 * 1024 + 1), 0, &catalog, 10).is_err());
    assert!(complete_sql(&"(".repeat(33), 0, &catalog, 10).is_err());
    assert!(complete_sql("", 0, &catalog, 201).is_err());
    Ok(())
}

#[test]
fn invalid_drafts_and_unsupported_contexts_are_safe_at_every_caret_boundary() {
    for engine in [Engine::Postgres, Engine::MySql] {
        for sql in [
            "",
            "WITH",
            "WITH c(",
            "WITH c AS (",
            "WITH c(a,) AS (SELECT 1) SELECT c.",
            "SELECT (SELECT (",
            "SELECT * FROM (",
            "SELECT * FROM public.",
            "SELECT x. FROM public.users x JOIN sales.orders x ON x.id=x.id",
            "SELECT 日 FROM \"odd schema\".\"Order Items\" AS \"列\"",
            "SELECT 'a''b', $$ body $$; -- tail",
            "SELECT /* nested /* */ */ 1",
        ] {
            for caret in sql
                .char_indices()
                .map(|(at, _)| at)
                .chain(std::iter::once(sql.len()))
            {
                let result = complete_sql(sql, caret, &catalog(engine), 8).unwrap();
                assert!(result.items.len() <= 8);
                assert!(sql.is_char_boundary(result.replacement.start));
                assert!(sql.is_char_boundary(result.replacement.end));
                assert!(
                    result.replacement.start <= result.replacement.end
                        && result.replacement.end <= sql.len()
                );
            }
        }
    }
    assert_eq!(
        complete("ALTER TABLE public.users |", Engine::Postgres).context,
        CompletionContext::Unsupported
    );
    assert_eq!(
        complete("INSERT INTO public.users (|)", Engine::Postgres).context,
        CompletionContext::Unsupported
    );
    assert!(
        columns(&complete(
            "SELECT x.| FROM public.users x JOIN sales.orders x ON x.id=x.id",
            Engine::Postgres
        ))
        .is_empty()
    );
}

#[test]
fn fromless_select_projection_ends_before_each_trailing_clause() {
    for engine in [Engine::Postgres, Engine::MySql] {
        for tail in [
            "WHERE TRUE",
            "LIMIT 1",
            "ORDER BY 1 LIMIT 1",
            "LIMIT 1 OFFSET 0",
            "GROUP BY 1 HAVING TRUE",
            "WINDOW w AS ()",
        ] {
            let sql = format!("WITH c AS (SELECT 1 AS n {tail}) SELECT c.| FROM c");
            let result = complete(&sql, engine);
            assert_eq!(
                columns(&result),
                vec![if engine == Engine::Postgres {
                    "\"n\""
                } else {
                    "`n`"
                }],
                "{sql}"
            );
            assert!(
                result.reason.is_none(),
                "A known explicit output alias became partial: {sql}"
            );
        }
    }
    let result = complete(
        "WITH c AS (SELECT 1 AS n FETCH FIRST 1 ROW ONLY) SELECT c.| FROM c",
        Engine::Postgres,
    );
    assert_eq!(columns(&result), vec!["\"n\""]);
}

#[test]
fn expression_endings_are_not_invented_implicit_aliases() {
    for engine in [Engine::Postgres, Engine::MySql] {
        for expression in [
            "id IS NULL",
            "id IS NOT NULL",
            "id IS TRUE",
            "id IS FALSE",
            "id AND name",
            "id IS DISTINCT FROM name",
            "id BETWEEN 1 AND 2",
            "NULL",
            "CURRENT_TIMESTAMP",
        ] {
            let sql = format!("SELECT d.| FROM (SELECT {expression} FROM public.users) d");
            let result = complete(&sql, engine);
            assert!(
                columns(&result).is_empty(),
                "Invented an output name for {expression}: {:?}",
                result.items
            );
            assert!(
                result.reason.is_some(),
                "Unknown expression labels must be partial"
            );
        }
        for projection in [
            "id IS NULL AS absent",
            "id absent",
            "u.id absent",
            "count(*) absent",
        ] {
            let result = complete(
                &format!("SELECT d.| FROM (SELECT {projection} FROM public.users u) d"),
                engine,
            );
            assert_eq!(
                columns(&result),
                vec![if engine == Engine::Postgres {
                    "\"absent\""
                } else {
                    "`absent`"
                }]
            );
        }
    }
}

#[test]
fn postgres_partial_output_alias_lists_rename_only_the_prefix() {
    for sql in [
        "WITH c(uid) AS (SELECT id, name FROM public.users) SELECT c.| FROM c",
        "SELECT d.| FROM (SELECT id, name FROM public.users) AS d(uid)",
        "SELECT d.| FROM public.users AS d(uid)",
    ] {
        let result = complete(sql, Engine::Postgres);
        assert!(columns(&result).contains(&"\"uid\""));
        assert!(
            columns(&result).contains(&"\"name\""),
            "Lost an unrenamed output column: {sql}"
        );
        assert!(!columns(&result).contains(&"\"id\""));
        assert!(result.reason.is_none());
    }
    let uncertain = complete(
        "WITH c(first) AS (SELECT id IS NULL, name FROM public.users) SELECT c.| FROM c",
        Engine::Postgres,
    );
    assert_eq!(columns(&uncertain), vec!["\"first\""]);
    assert!(
        uncertain.reason.is_some(),
        "Do not align omitted/unknown projection positions to inferred tail names"
    );
    let mysql = complete(
        "WITH c(uid) AS (SELECT id, name FROM public.users) SELECT c.| FROM c",
        Engine::MySql,
    );
    assert!(
        columns(&mysql).is_empty(),
        "MySQL requires equal alias and output counts"
    );
    assert!(mysql.reason.is_some());
}

#[test]
fn quoted_numeric_leading_projected_names_remain_identifiers() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let q = if engine == Engine::Postgres { '"' } else { '`' };
        let catalog =
            CompletionCatalog::new(engine, vec![table("public", "users", &["1name", "123"])])
                .unwrap();
        let sql = format!("SELECT d. FROM (SELECT {q}1name{q}, {q}123{q} FROM public.users) d");
        let result = complete_sql(&sql, "SELECT d.".len(), &catalog, 10).unwrap();
        assert_eq!(
            columns(&result),
            vec![format!("{q}123{q}"), format!("{q}1name{q}")]
        );
        assert!(result.reason.is_none());
        let literal = complete("SELECT d.| FROM (SELECT 123 FROM public.users) d", engine);
        assert!(columns(&literal).is_empty());
    }
}

#[test]
fn mysql_column_duplicate_equivalence_is_not_postgres_or_table_equivalence() {
    let input = vec![table("s", "t", &["id", "ID"])];
    assert!(CompletionCatalog::new(Engine::MySql, input.clone()).is_err());
    assert!(CompletionCatalog::new(Engine::Postgres, input).is_ok());
    // Do not impose column case rules on platform-dependent table names.
    assert!(
        CompletionCatalog::new(
            Engine::MySql,
            vec![table("s", "t", &["id"]), table("s", "T", &["id"])]
        )
        .is_ok()
    );
}

#[test]
fn wildcard_projection_has_measured_preallocation_memory_bound() -> Result<()> {
    let Some(case) = std::env::var_os("ROWBOAT_PROJECTION_MEMORY_CASE") else {
        for case in ["count", "bytes"] {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "wildcard_projection_has_measured_preallocation_memory_bound",
                    "--nocapture",
                ])
                .env("ROWBOAT_PROJECTION_MEMORY_CASE", case)
                .status()?;
            anyhow::ensure!(
                status.success(),
                "Projection memory subprocess failed for {case}"
            );
        }
        return Ok(());
    };
    let (aliases, width) = if case == "count" { (128, 40) } else { (2, 940) };
    let catalog = CompletionCatalog::new(
        Engine::Postgres,
        vec![CatalogTable {
            table: TableRef {
                schema: "public".into(),
                name: "t".into(),
            },
            columns: (0..10_000)
                .map(|n| format!("col_{n:06}_{}", "x".repeat(width)))
                .collect(),
        }],
    )?;
    let sql = format!(
        "SELECT * FROM {} WHERE a0.col_",
        (0..aliases)
            .map(|n| format!("public.t a{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let before = peak_kib()?;
    let result = complete_sql(&sql, sql.len(), &catalog, 10)?;
    let growth = peak_kib()?.saturating_sub(before);
    assert_eq!(
        result.items.len(),
        10,
        "Resolved source columns should still be usable"
    );
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("budget")),
        "Oversized projection must be reported as partial"
    );
    anyhow::ensure!(
        growth < 16 * 1024,
        "Completion grew peak RSS by {growth} KiB for 10 suggestions"
    );
    eprintln!("projection case={case:?} aliases={aliases} peak_growth_kib={growth}");
    Ok(())
}
