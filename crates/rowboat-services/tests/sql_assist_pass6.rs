mod common;
use anyhow::Result;
use common::peak_kib;
use rowboat_core::{Engine, TableRef};
use rowboat_services::{
    CatalogTable, CompletionCatalog, CompletionContext, CompletionKind, SqlCompletions,
    complete_sql,
};

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
            table("tenant", "users", &["id", "tenant_name", "日本語"]),
            table("public", "users", &["id", "public_name"]),
            table("off_path", "only_here", &["hidden"]),
        ],
    )
    .unwrap()
}
fn complete(sql: &str, catalog: &CompletionCatalog) -> SqlCompletions {
    let caret = sql.find('|').unwrap();
    complete_sql(&sql.replacen('|', "", 1), caret, catalog, 100).unwrap()
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
fn explicit_path_order_empty_unknown_reset_and_off_path_resolution() -> Result<()> {
    let mut cat = catalog(Engine::Postgres);
    assert!(cat.search_path().is_none());
    assert!(columns(&complete("SELECT u.| FROM users u", &cat)).is_empty());
    // Legacy catalog-only behavior is unchanged; it does not assume public.
    assert_eq!(
        columns(&complete("SELECT h.| FROM only_here h", &cat)),
        ["\"hidden\""]
    );
    cat.set_search_path(Some(vec![
        "missing".into(),
        "tenant".into(),
        "public".into(),
    ]))?;
    assert!(columns(&complete("SELECT u.| FROM users u", &cat)).contains(&"\"tenant_name\""));
    assert!(!columns(&complete("SELECT u.| FROM users u", &cat)).contains(&"\"public_name\""));
    assert!(columns(&complete("SELECT h.| FROM only_here h", &cat)).is_empty());
    assert_eq!(
        columns(&complete("SELECT h.| FROM off_path.only_here h", &cat)),
        ["\"hidden\""]
    );
    cat.set_search_path(Some(vec!["public".into(), "tenant".into()]))?;
    assert_eq!(
        columns(&complete("SELECT u.| FROM users u", &cat)),
        ["\"id\"", "\"public_name\""]
    );
    cat.set_search_path(Some(vec![]))?;
    assert_eq!(cat.search_path(), Some([].as_slice()));
    assert!(columns(&complete("SELECT u.| FROM users u", &cat)).is_empty());
    let relations = complete("SELECT * FROM |", &cat);
    assert!(
        relations
            .items
            .iter()
            .any(|i| i.insert_text == "\"off_path\".\"only_here\"")
    );
    cat.set_search_path(None)?;
    assert_eq!(
        columns(&complete("SELECT h.| FROM only_here h", &cat)),
        ["\"hidden\""]
    );
    let mut unknown_first = CompletionCatalog::new(
        Engine::Postgres,
        vec![
            table("tenant", "users", &[]),
            table("public", "users", &["not_visible"]),
        ],
    )?;
    unknown_first.set_search_path(Some(vec!["tenant".into(), "public".into()]))?;
    let result = complete("SELECT u.| FROM users u", &unknown_first);
    assert!(
        columns(&result).is_empty(),
        "An empty catalog column list must not bypass a shadowing relation"
    );
    assert!(result.reason.is_some());
    Ok(())
}

#[test]
fn cte_shadowing_nested_path_and_explicit_schema_bypass() -> Result<()> {
    let mut cat = catalog(Engine::Postgres);
    cat.set_search_path(Some(vec!["tenant".into(), "public".into()]))?;
    let cte = complete(
        "WITH users AS (SELECT 1 AS local_name) SELECT u.| FROM users u",
        &cat,
    );
    assert_eq!(columns(&cte), ["\"local_name\""]);
    assert_eq!(
        columns(&complete(
            "WITH users AS (SELECT 1 AS local_name) SELECT u.| FROM public.users u",
            &cat
        )),
        ["\"id\"", "\"public_name\""]
    );
    let nested = complete(
        "WITH c AS (SELECT users.* FROM users) SELECT d.| FROM (SELECT c.tenant_name, c.日本語 FROM c) d",
        &cat,
    );
    assert_eq!(columns(&nested), ["\"tenant_name\"", "\"日本語\""]);
    assert!(nested.reason.is_none());
    assert!(columns(&complete("SELECT u.id, (SELECT u.|) FROM users u", &cat)).is_empty());
    Ok(())
}

#[test]
fn path_names_are_literal_case_preserving_and_do_not_expand_sql_macros() -> Result<()> {
    let mut cat = CompletionCatalog::new(
        Engine::Postgres,
        vec![
            table("Tenant", "Users", &["Mixed"]),
            table("tenant", "users", &["lower"]),
            table("Odd \"日本語", "users", &["世界"]),
            table("user_name", "users", &["private"]),
        ],
    )?;
    cat.set_search_path(Some(vec!["Tenant".into()]))?;
    assert!(columns(&complete("SELECT u.| FROM users u", &cat)).is_empty());
    assert_eq!(
        columns(&complete("SELECT u.| FROM \"Users\" u", &cat)),
        ["\"Mixed\""]
    );
    cat.set_search_path(Some(vec!["Odd \"日本語".into()]))?;
    assert_eq!(
        columns(&complete("SELECT u.世|界 FROM users u", &cat)),
        ["\"世界\""]
    );
    cat.set_search_path(Some(vec!["$user".into(), "pg_temp".into()]))?;
    assert!(columns(&complete("SELECT u.| FROM users u", &cat)).is_empty());
    Ok(())
}

#[test]
fn path_validation_is_atomic_and_mysql_ambiguity_does_not_pick_a_winner() -> Result<()> {
    let mut pg = catalog(Engine::Postgres);
    pg.set_search_path(Some(vec!["tenant".into()]))?;
    for names in [
        vec!["x".into(); 129],
        vec!["x".repeat(1025)],
        vec!["".into()],
        vec!["a\0b".into()],
    ] {
        assert!(pg.set_search_path(Some(names)).is_err());
        assert_eq!(pg.search_path().unwrap(), &["tenant"]);
    }
    pg.set_search_path(Some(vec!["x".repeat(1024); 128]))?;
    let mut my = CompletionCatalog::new(
        Engine::MySql,
        vec![
            table("db", "users", &["one"]),
            table("db", "Users", &["two"]),
            table("DB", "users", &["other_database"]),
        ],
    )?;
    assert!(
        my.set_search_path(Some(vec!["db".into(), "DB".into()]))
            .is_err()
    );
    assert!(my.search_path().is_none());
    my.set_search_path(Some(vec!["db".into()]))?;
    let ambiguous = complete("SELECT u.| FROM users u", &my);
    assert!(columns(&ambiguous).is_empty());
    assert!(ambiguous.reason.is_some());
    assert_eq!(
        columns(&complete("SELECT u.| FROM `Users` u", &my)),
        ["`two`"]
    );
    my.set_search_path(Some(vec!["DB".into()]))?;
    assert_eq!(
        columns(&complete("SELECT u.| FROM users u", &my)),
        ["`other_database`"]
    );
    Ok(())
}

#[test]
fn qualified_references_parentheses_stars_and_alias_hiding_are_proven_locally() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let cat = catalog(engine);
        let q = if engine == Engine::Postgres { '"' } else { '`' };
        for projection in [
            "tenant.users.tenant_name",
            "((tenant.users.tenant_name))",
            "tenant.users.tenant_name renamed",
        ] {
            let sql = format!("SELECT d.| FROM (SELECT {projection} FROM tenant.users) d");
            let result = complete(&sql, &cat);
            let name = if projection.ends_with("renamed") {
                "renamed"
            } else {
                "tenant_name"
            };
            assert_eq!(columns(&result), [format!("{q}{name}{q}")], "{sql}");
            assert!(result.reason.is_none());
        }
        let star = complete(
            "SELECT d.| FROM (SELECT tenant.users.* FROM tenant.users) d",
            &cat,
        );
        assert_eq!(columns(&star).len(), 3);
        assert!(star.reason.is_none());
        for sql in [
            "SELECT d.| FROM (SELECT tenant.users.* FROM tenant.users u) d",
            "SELECT d.| FROM (SELECT tenant.users.id FROM tenant.users u) d",
            "SELECT d.| FROM (SELECT u.* FROM tenant.users u JOIN public.users u ON TRUE) d",
            "SELECT d.| FROM (SELECT u.id FROM tenant.users u JOIN public.users u ON TRUE) d",
            "SELECT d.| FROM (SELECT nonexistent FROM tenant.users) d",
            "SELECT d.| FROM (SELECT (u).* FROM tenant.users u) d",
            "SELECT d.| FROM (SELECT id FROM tenant.users JOIN public.users ON TRUE) d",
        ] {
            let result = complete(sql, &cat);
            assert!(columns(&result).is_empty(), "{sql}");
            assert!(result.reason.is_some(), "{sql}");
        }
    }
}

#[test]
fn postgres_distinct_on_and_mysql_column_case_keep_correct_labels() -> Result<()> {
    let result = complete(
        "WITH c AS (SELECT DISTINCT ON (u.id, u.tenant_name) ((u.日本語)), u.tenant_name FROM tenant.users u ORDER BY u.id) SELECT c.| FROM c",
        &catalog(Engine::Postgres),
    );
    assert_eq!(columns(&result), ["\"tenant_name\"", "\"日本語\""]);
    assert!(result.reason.is_none());
    for sql in [
        "SELECT d.| FROM (SELECT DISTINCT ON id FROM tenant.users) d",
        "SELECT d.| FROM (SELECT DISTINCT ON (id) id FROM tenant.users) d",
    ] {
        assert!(columns(&complete(sql, &catalog(Engine::MySql))).is_empty());
    }
    let my = CompletionCatalog::new(Engine::MySql, vec![table("db", "t", &["ID", "Ä", "name"])])?;
    assert_eq!(
        columns(&complete(
            "SELECT d.| FROM (SELECT ((t.id)), t.`ä` FROM db.t t) d",
            &my
        )),
        ["`ID`", "`Ä`"]
    );
    Ok(())
}

#[test]
fn duplicate_outputs_and_unknown_star_join_order_do_not_become_addressable() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let cat = catalog(engine);
        for projection in [
            "u.id, u.id",
            "1 AS repeated, 2 AS repeated",
            "u.id AS repeated, u.tenant_name AS repeated",
        ] {
            let sql = format!(
                "SELECT d.| FROM (SELECT {projection}, 3 AS independent FROM tenant.users u) d"
            );
            let result = complete(&sql, &cat);
            assert_eq!(
                result
                    .items
                    .iter()
                    .map(|i| i.label.as_str())
                    .collect::<Vec<_>>(),
                ["independent"]
            );
            assert!(result.reason.is_some());
        }
        for join in ["JOIN public.users USING(id)", "NATURAL JOIN public.users"] {
            let sql =
                format!("WITH c(first) AS (SELECT * FROM tenant.users {join}) SELECT c.| FROM c");
            let result = complete(&sql, &cat);
            assert_eq!(
                result
                    .items
                    .iter()
                    .map(|i| i.label.as_str())
                    .collect::<Vec<_>>(),
                ["first"]
            );
            assert!(result.reason.is_some());
        }
    }
    let my = complete(
        "SELECT d.| FROM (SELECT 1 AS a, 2 AS A, 3 AS independent) d",
        &catalog(Engine::MySql),
    );
    assert_eq!(columns(&my), ["`independent`"]);
    let pg = complete(
        "SELECT d.| FROM (SELECT 1 AS a, 2 AS \"A\") d",
        &catalog(Engine::Postgres),
    );
    assert_eq!(columns(&pg), ["\"A\"", "\"a\""]);
}

#[test]
fn same_basename_relations_get_full_qualifiers_in_expression_context() {
    let cat = catalog(Engine::Postgres);
    let result = complete("SELECT | FROM tenant.users, public.users", &cat);
    assert!(columns(&result).contains(&"\"tenant\".\"users\".\"id\""));
    assert!(columns(&result).contains(&"\"public\".\"users\".\"id\""));
    assert!(!columns(&result).contains(&"\"users\".\"id\""));
    let qualified = complete(
        "SELECT tenant.users.| FROM tenant.users, public.users",
        &cat,
    );
    assert!(columns(&qualified).contains(&"\"tenant_name\""));
    let ambiguous = complete("SELECT users.| FROM tenant.users, public.users", &cat);
    assert_eq!(ambiguous.context, CompletionContext::Unsupported);
}

#[test]
fn contextual_unicode_drafts_are_safe_at_every_byte_boundary() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let mut cat = catalog(engine);
        cat.set_search_path(Some(vec!["tenant".into()]))?;
        for sql in [
            "WITH c AS (SELECT ((u.日本語)) FROM users u) SELECT c.日本語 FROM c",
            "SELECT DISTINCT ON ((日本語)) tenant.users. FROM tenant.users",
            "SELECT d. FROM (SELECT tenant.users.*, ((日本語) FROM users) d",
        ] {
            for caret in 0..=sql.len() {
                let result = complete_sql(sql, caret, &cat, 7);
                if !sql.is_char_boundary(caret) {
                    assert!(result.is_err());
                    continue;
                }
                let result = result?;
                assert!(result.items.len() <= 7);
                assert!(sql.is_char_boundary(result.replacement.start));
                assert!(sql.is_char_boundary(result.replacement.end));
                assert!(result.replacement.end <= sql.len());
            }
        }
    }
    Ok(())
}

#[test]
fn search_path_and_reference_budget_have_measured_peak_bound() -> Result<()> {
    let Some(case) = std::env::var_os("ROWBOAT_PASS6_SQL_MEMORY_CASE") else {
        for case in ["path", "references", "reference_count"] {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "search_path_and_reference_budget_have_measured_peak_bound",
                    "--nocapture",
                ])
                .env("ROWBOAT_PASS6_SQL_MEMORY_CASE", case)
                .status()?;
            anyhow::ensure!(
                status.success(),
                "Isolated completion memory case failed: {case}"
            );
        }
        return Ok(());
    };
    let count_case = case == "reference_count";
    let names: Vec<_> = (0..10_000)
        .map(|n| {
            format!(
                "col_{n:06}_{}",
                "x".repeat(if count_case { 0 } else { 940 })
            )
        })
        .collect();
    let references = names
        .iter()
        .take(if count_case { 256 } else { 128 })
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let mut cat = CompletionCatalog::new(
        Engine::Postgres,
        vec![CatalogTable {
            table: TableRef {
                schema: "s127".into(),
                name: "t".into(),
            },
            columns: names,
        }],
    )?;
    cat.set_search_path(Some((0..128).map(|n| format!("s{n}")).collect()))?;
    let sql = if case == "path" {
        format!(
            "SELECT * FROM {} WHERE a0.col_|",
            (0..128)
                .map(|n| format!("t a{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if count_case {
        format!("SELECT d.ind| FROM (SELECT {references}, 1 AS independent FROM t) d")
    } else {
        format!("SELECT d.| FROM (SELECT {references}, 1 AS independent FROM t) d")
    };
    let before = peak_kib()?;
    let result = complete(&sql, &cat);
    let growth = peak_kib()?.saturating_sub(before);
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("budget"))
    );
    if case == "path" {
        assert_eq!(result.items.len(), 100);
    } else {
        assert!(columns(&result).contains(&"\"independent\""));
    }
    anyhow::ensure!(growth < 16 * 1024, "Peak RSS grew {growth} KiB");
    eprintln!("pass6 case={case:?} peak_growth_kib={growth}");
    Ok(())
}

fn review_catalog(engine: Engine) -> CompletionCatalog {
    let mut cat = CompletionCatalog::new(
        engine,
        vec![
            table("public", "a", &["id", "a_value"]),
            table("public", "b", &["id", "database_only"]),
            table("public", "c", &["catalog_c"]),
        ],
    )
    .unwrap();
    cat.set_search_path(Some(vec!["public".into()])).unwrap();
    cat
}

#[test]
fn recursive_forward_ctes_shadow_catalog_and_preserve_output_aliases() {
    let cat = review_catalog(Engine::Postgres);
    for sql in [
        "WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
        "WITH RECURSIVE a AS (SELECT b.cte_only FROM b), b AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
        "WITH RECURSIVE a AS (SELECT * FROM b), b(cte_only) AS (SELECT 1 AS original) SELECT a.| FROM a",
        "WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT * FROM c), c AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
    ] {
        let result = complete(sql, &cat);
        assert_eq!(columns(&result), ["\"cte_only\""], "{sql}");
        assert!(result.reason.is_none(), "{sql}");
    }
    let invalid_reference = complete(
        "WITH RECURSIVE a AS (SELECT b.database_only FROM b), b AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
        &cat,
    );
    assert!(columns(&invalid_reference).is_empty());
    assert!(invalid_reference.reason.is_some());
    let partial_list = complete(
        "WITH RECURSIVE a AS (SELECT * FROM b), b(renamed) AS (SELECT 1 AS first, 2 AS second) SELECT a.| FROM a",
        &cat,
    );
    assert_eq!(columns(&partial_list), ["\"renamed\"", "\"second\""]);
    let quoted = complete(
        "WITH RECURSIVE a AS (SELECT * FROM \"B\"), \"B\"(\"世界\") AS (SELECT 1 AS original) SELECT a.| FROM a",
        &cat,
    );
    assert_eq!(columns(&quoted), ["\"世界\""]);
}

#[test]
fn recursive_visibility_is_engine_specific_without_changing_nonrecursive_order() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let cat = review_catalog(engine);
        let lexical = complete(
            "WITH a AS (SELECT * FROM b), b AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
            &cat,
        );
        assert!(lexical.items.iter().any(|i| i.label == "database_only"));
        let earlier = complete(
            "WITH b AS (SELECT 1 AS cte_only), a AS (SELECT * FROM b) SELECT a.| FROM a",
            &cat,
        );
        assert_eq!(earlier.items[0].label, "cte_only");
        let qualified = complete(
            "WITH RECURSIVE a AS (SELECT * FROM public.b), b AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
            &cat,
        );
        assert!(qualified.items.iter().any(|i| i.label == "database_only"));
        let forward = complete(
            "WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT 1 AS cte_only) SELECT a.| FROM a",
            &cat,
        );
        if engine == Engine::MySql {
            assert!(columns(&forward).is_empty());
            assert!(forward.reason.is_some());
        } else {
            assert_eq!(columns(&forward), ["\"cte_only\""]);
        }
    }
}

#[test]
fn recursive_self_seed_names_and_headers_are_separate_from_illegal_self_cycles() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let cat = review_catalog(engine);
        for sql in [
            "WITH RECURSIVE c AS (SELECT 1 AS seed UNION ALL SELECT seed + 1 FROM c) SELECT c.| FROM c",
            "WITH RECURSIVE c(seed) AS (SELECT 1 UNION ALL SELECT seed + 1 FROM c) SELECT c.| FROM c",
            "WITH RECURSIVE c AS (SELECT 1 AS seed UNION ALL SELECT c.| FROM c) SELECT * FROM c",
            "WITH RECURSIVE c(seed) AS (SELECT 1 UNION ALL SELECT c.| FROM c) SELECT * FROM c",
        ] {
            let result = complete(sql, &cat);
            assert_eq!(
                result
                    .items
                    .iter()
                    .map(|i| i.label.as_str())
                    .collect::<Vec<_>>(),
                ["seed"],
                "{sql}"
            );
        }
        for sql in [
            "WITH RECURSIVE c AS (SELECT * FROM c) SELECT c.| FROM c",
            "WITH RECURSIVE c(seed) AS (SELECT * FROM c) SELECT c.| FROM c",
            "WITH RECURSIVE c(seed) AS (SELECT * FROM c UNION ALL SELECT 1) SELECT c.| FROM c",
        ] {
            let result = complete(sql, &cat);
            assert!(columns(&result).is_empty(), "{sql}");
            assert!(result.reason.is_some());
        }
    }
}

#[test]
fn mutual_recursive_cycles_and_dependants_never_leak_catalog_or_anchor_columns() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let cat = review_catalog(engine);
        for sql in [
            "WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT a.| FROM a",
            "WITH RECURSIVE a(x) AS (SELECT * FROM b), b(y) AS (SELECT * FROM a) SELECT b.| FROM b",
            "WITH RECURSIVE a AS (SELECT 1 AS anchor_a UNION ALL SELECT * FROM b), b AS (SELECT 1 AS anchor_b UNION ALL SELECT * FROM a) SELECT a.| FROM a",
            "WITH RECURSIVE c AS (SELECT * FROM a), a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT c.| FROM c",
            "WITH RECURSIVE a AS (SELECT b.| FROM b), b AS (SELECT * FROM a) SELECT * FROM a",
        ] {
            let result = complete(sql, &cat);
            assert!(columns(&result).is_empty(), "{sql}");
            assert!(result.reason.is_some(), "{sql}");
        }
    }
}

#[test]
fn recursive_namespace_limits_and_forward_chain_memory_bound() -> Result<()> {
    let cat = review_catalog(Engine::Postgres);
    for count in [128, 129] {
        let definitions = (0..count)
            .map(|n| {
                if n + 1 == count {
                    format!("x{n} AS (SELECT 1 AS leaf)")
                } else {
                    format!("x{n} AS (SELECT * FROM x{})", n + 1)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("WITH RECURSIVE {definitions} SELECT x0. FROM x0");
        let caret = sql.find("SELECT x0.").unwrap() + "SELECT x0.".len();
        let result = complete_sql(&sql, caret, &cat, 7);
        if count == 129 {
            assert!(result.is_err());
        } else {
            assert_eq!(columns(&result?), ["\"leaf\""]);
        }
    }
    if std::env::var_os("ROWBOAT_RECURSIVE_MEMORY_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "recursive_namespace_limits_and_forward_chain_memory_bound",
                "--nocapture",
            ])
            .env("ROWBOAT_RECURSIVE_MEMORY_CHILD", "1")
            .status()?;
        anyhow::ensure!(status.success(), "Recursive CTE memory subprocess failed");
        return Ok(());
    }
    let cat = CompletionCatalog::new(
        Engine::Postgres,
        vec![CatalogTable {
            table: TableRef {
                schema: "public".into(),
                name: "wide".into(),
            },
            columns: (0..10_000)
                .map(|i| format!("col_{i:05}_{}", "x".repeat(40)))
                .collect(),
        }],
    )?;
    let definitions = (0..128)
        .map(|n| {
            if n == 127 {
                format!("x{n} AS (SELECT * FROM public.wide)")
            } else {
                format!("x{n} AS (SELECT * FROM x{})", n + 1)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("WITH RECURSIVE {definitions} SELECT w.col_ FROM public.wide w");
    let before = peak_kib()?;
    let result = complete_sql(&sql, sql.find("w.col_").unwrap() + 6, &cat, 7)?;
    let growth = peak_kib()?.saturating_sub(before);
    assert_eq!(result.items.len(), 7);
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("budget"))
    );
    anyhow::ensure!(
        growth < 16 * 1024,
        "Recursive inference grew peak RSS by {growth} KiB"
    );
    eprintln!("recursive_cte_peak_growth_kib={growth}");
    Ok(())
}

#[test]
fn line_comment_terminators_follow_dialect_at_caret_and_hidden_joins() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let cat = review_catalog(engine);
        let markers = if engine == Engine::MySql {
            vec!["-- ignored", "# ignored", "--\tignored", "--\u{7f}ignored"]
        } else {
            vec!["-- ignored", "--ignored"]
        };
        for marker in markers {
            for newline in ["\r", "\n", "\r\n"] {
                let hidden = engine == Engine::MySql && newline == "\r";
                let result = complete(
                    &format!("SELECT b.| FROM a {marker}{newline} JOIN b ON a.id=b.id"),
                    &cat,
                );
                assert_eq!(
                    columns(&result).is_empty(),
                    hidden,
                    "{engine:?} {marker:?} {newline:?}"
                );
                let result = complete(&format!("SELECT a.id FROM a {marker}{newline} b.|"), &cat);
                assert_eq!(result.context == CompletionContext::Suppressed, hidden);
                let inside = complete(
                    &format!("SELECT a.id FROM a {marker} b.|{newline} JOIN b ON TRUE"),
                    &cat,
                );
                assert_eq!(inside.context, CompletionContext::Suppressed);
            }
        }
    }
    let my = review_catalog(Engine::MySql);
    assert_ne!(
        complete("SELECT 1--not_a_comment|", &my).context,
        CompletionContext::Suppressed
    );
    assert_eq!(
        complete("SELECT 1-- comment|", &my).context,
        CompletionContext::Suppressed
    );
    assert_ne!(
        complete(
            "SELECT 1 # not_a_pg_comment|",
            &review_catalog(Engine::Postgres)
        )
        .context,
        CompletionContext::Suppressed
    );
}

#[test]
fn executable_comments_block_only_the_affected_statement_not_literals_or_hints() {
    let my = review_catalog(Engine::MySql);
    for comment in [
        "/*! AS hidden_alias */",
        "/*!80000 AS hidden_alias */",
        "/*M!100100 AS hidden_alias */",
        "/*!999999 AS hidden_alias */",
        "/*! unterminated",
    ] {
        for sql in [
            format!("SELECT a.| FROM a {comment}"),
            format!("SELECT d.| FROM (SELECT a.* FROM a {comment}) d"),
            format!("{comment} SELECT a.| FROM a"),
        ] {
            let result = complete(&sql, &my);
            assert!(
                matches!(
                    result.context,
                    CompletionContext::Unsupported | CompletionContext::Suppressed
                ),
                "{sql}"
            );
            assert!(result.items.is_empty());
            assert!(result.reason.is_some());
        }
    }
    assert_eq!(
        complete("SELECT a.id FROM a /*! AS hidden| */", &my).context,
        CompletionContext::Suppressed
    );
    for sql in [
        "SELECT a.| FROM a /* AS hidden */",
        "SELECT /*+ QB_NAME(example) */ a.| FROM a",
        "SELECT '/*! AS hidden */', a.| FROM a",
        "SELECT a.| FROM a -- /*! AS hidden */",
        "SELECT a.id FROM a /*! AS hidden */; SELECT b.| FROM b",
        "SELECT b.| FROM b; SELECT a.id FROM a /*! AS hidden */",
    ] {
        let result = complete(sql, &my);
        assert!(!columns(&result).is_empty(), "{sql}");
        assert!(result.reason.is_none(), "{sql}");
    }
    let pg = complete(
        "SELECT a.| FROM a /*!80000 AS hidden */",
        &review_catalog(Engine::Postgres),
    );
    assert_eq!(columns(&pg), ["\"a_value\"", "\"id\""]);
    assert!(pg.reason.is_none());
}
