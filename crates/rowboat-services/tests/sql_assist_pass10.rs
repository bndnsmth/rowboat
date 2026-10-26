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
        columns: columns.iter().map(|name| (*name).into()).collect(),
    }
}

fn complete(sql: &str, catalog: &CompletionCatalog, limit: usize) -> SqlCompletions {
    let caret = sql.find('|').unwrap();
    complete_sql(&sql.replacen('|', "", 1), caret, catalog, limit).unwrap()
}

fn labels(result: &SqlCompletions) -> Vec<&str> {
    result
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect()
}

#[test]
fn exact_then_shortest_relation_name_ignores_schema_and_catalog_order() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let mut tables = vec![
            table("a", "user_activity_log", &[]),
            table("zz_schema_with_a_much_longer_name", "users", &[]),
            table("zzz", "user", &[]),
        ];
        for _ in 0..2 {
            let catalog = CompletionCatalog::new(engine, tables.clone()).unwrap();
            let result = complete("SELECT * FROM user|", &catalog, 100);
            assert_eq!(result.context, CompletionContext::Relation);
            assert_eq!(
                labels(&result),
                [
                    "zzz.user",
                    "zz_schema_with_a_much_longer_name.users",
                    "a.user_activity_log",
                ]
            );
            assert!(!result.truncated);
            let one = complete("SELECT * FROM user|", &catalog, 1);
            assert_eq!(one.items, result.items[..1]);
            assert!(one.truncated);
            tables.reverse();
        }
        // The motivating draft also works when there is no exact-name table.
        tables.retain(|table| table.table.name != "user");
        let catalog = CompletionCatalog::new(engine, tables).unwrap();
        assert_eq!(
            labels(&complete("SELECT * FROM user|", &catalog, 1)),
            ["zz_schema_with_a_much_longer_name.users"]
        );
    }
}

#[test]
fn late_best_candidates_survive_bounded_top_k_under_catalog_reordering() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let mut tables: Vec<_> = (0..300)
            .map(|index| table("a", &format!("user_{index:03}"), &[]))
            .collect();
        tables.push(table("z", "users", &[]));
        tables.push(table("z", "user", &[]));
        let catalog = CompletionCatalog::new(engine, tables.clone()).unwrap();
        let baseline = complete("SELECT * FROM user|", &catalog, 200);
        assert_eq!(&labels(&baseline)[..3], ["z.user", "z.users", "a.user_000"]);
        assert!(baseline.truncated);
        for _ in 0..3 {
            tables.reverse();
            tables.rotate_left(17);
            let catalog = CompletionCatalog::new(engine, tables.clone()).unwrap();
            for limit in [1, 2, 7, 200] {
                let result = complete("SELECT * FROM user|", &catalog, limit);
                assert_eq!(result.items, baseline.items[..limit]);
                assert!(result.truncated);
            }
        }
    }
}

#[test]
fn table_alias_and_multi_source_columns_rank_the_unqualified_name() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let catalog = CompletionCatalog::new(
            engine,
            vec![
                table("s", "records", &["user_activity_log", "users", "user"]),
                table("s", "logs", &["user_activity_log"]),
                table("s", "people", &["users", "user"]),
            ],
        )
        .unwrap();
        for sql in [
            "SELECT records.user| FROM s.records",
            "SELECT r.user| FROM s.records r",
            "SELECT user| FROM s.records r",
        ] {
            let result = complete(sql, &catalog, 100);
            assert_eq!(labels(&result), ["user", "users", "user_activity_log"]);
            assert!(
                result
                    .items
                    .iter()
                    .all(|i| i.kind == CompletionKind::Column)
            );
            assert_eq!(complete(sql, &catalog, 1).items, result.items[..1]);
        }
        let result = complete(
            "SELECT user| FROM s.logs a JOIN s.people zz_very_long_alias ON 1=1",
            &catalog,
            100,
        );
        assert_eq!(
            labels(&result),
            [
                "zz_very_long_alias.user",
                "zz_very_long_alias.users",
                "a.user_activity_log",
            ]
        );
        let q = if engine == Engine::Postgres { '"' } else { '`' };
        assert_eq!(
            result.items[0].insert_text,
            format!("{q}zz_very_long_alias{q}.{q}user{q}")
        );
    }
}

#[test]
fn exact_matches_precede_context_tiers_and_metadata_precedes_generic_keywords() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let catalog = CompletionCatalog::new(
            engine,
            vec![
                table("s", "records", &["and_value"]),
                table("user", "other", &[]),
                table("s", "users", &[]),
            ],
        )
        .unwrap();
        let exact_keyword = complete("SELECT and| FROM s.records an", &catalog, 100);
        assert_eq!(labels(&exact_keyword), ["AND", "and_value"]);
        assert_eq!(exact_keyword.items[0].kind, CompletionKind::Keyword);
        let prefix = complete("SELECT a| FROM s.records an", &catalog, 100);
        assert_eq!(labels(&prefix), ["and_value", "an", "AS", "AND"]);
        let exact_alias = complete("SELECT an| FROM s.records an", &catalog, 100);
        assert_eq!(labels(&exact_alias), ["an", "and_value", "AND"]);
        assert_eq!(exact_alias.items[0].kind, CompletionKind::Alias);
        assert_eq!(
            labels(&complete("SELECT * FROM user|", &catalog, 100)),
            ["user", "s.users"]
        );
        assert_eq!(
            labels(&complete("SELECT * FROM u|", &catalog, 100)),
            ["s.users", "user"]
        );
    }
}

#[test]
fn quoted_prefix_case_and_unicode_scalar_lengths_are_preserved() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let q = if engine == Engine::Postgres { '"' } else { '`' };
        let escaped = format!("u{q}{q}");
        let catalog = CompletionCatalog::new(
            engine,
            vec![table(
                "s",
                "records",
                &[
                    "User_activity_log",
                    "Users",
                    "User",
                    "user_lower",
                    "u界",
                    "uab",
                    "uabc",
                    &escaped,
                    "Ωβγ",
                    "ωa",
                    "Ω",
                ],
            )],
        )
        .unwrap();
        let sql = format!("SELECT r.{q}User|{q} FROM s.records r");
        let result = complete(&sql, &catalog, 100);
        assert_eq!(labels(&result), ["User", "Users", "User_activity_log"]);
        assert_eq!(result.items[0].insert_text, format!("{q}User{q}"));
        assert_eq!(
            &sql.replace('|', "")[result.replacement],
            format!("{q}User{q}")
        );

        let result = complete(
            &format!("SELECT r.{q}u|{q} FROM s.records r"),
            &catalog,
            100,
        );
        assert_eq!(
            labels(&result),
            ["u界", &escaped, "uab", "uabc", "user_lower"]
        );
        assert_eq!(result.items[1].insert_text, format!("{q}u{q}{q}{q}{q}{q}"));
        // An escaped delimiter in the draft is decoded before matching/ranking.
        assert_eq!(
            labels(&complete(
                &format!("SELECT r.{q}u{q}{q}|{q} FROM s.records r"),
                &catalog,
                1,
            )),
            [escaped.as_str()]
        );
        let folded = complete("SELECT r.ω| FROM s.records r", &catalog, 100);
        assert_eq!(labels(&folded), ["Ω", "ωa", "Ωβγ"]);
        assert_eq!(
            labels(&complete(
                &format!("SELECT r.{q}Ω|{q} FROM s.records r"),
                &catalog,
                100
            )),
            ["Ω", "Ωβγ"]
        );
    }
}

#[test]
fn equal_length_ties_use_folded_name_then_label_then_insertion() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let mut tables = vec![
            table("a", "uBB", &[]),
            table("zz", "uaa", &[]),
            table("z", "uAA", &[]),
            table("x.y", "uCC", &[]),
        ];
        for _ in 0..2 {
            let catalog = CompletionCatalog::new(engine, tables.clone()).unwrap();
            assert_eq!(
                labels(&complete("SELECT * FROM u|", &catalog, 100)),
                ["z.uAA", "zz.uaa", "a.uBB", "x.y.uCC"]
            );
            tables.reverse();
        }
        // Same matched column and display label; one qualifier is a dotted
        // identifier, the other is two identifiers. Keep both SQL insertions.
        let q = if engine == Engine::Postgres { '"' } else { '`' };
        let catalog = CompletionCatalog::new(
            engine,
            vec![
                table("", "s.t", &["user"]),
                table("s", "t", &["user"]),
                table("other", "t", &["id"]),
            ],
        )
        .unwrap();
        for sources in [
            format!("s.t, other.t, {q}s.t{q}"),
            format!("{q}s.t{q}, other.t, s.t"),
        ] {
            let result = complete(&format!("SELECT user| FROM {sources}"), &catalog, 100);
            assert_eq!(labels(&result), ["s.t.user", "s.t.user"]);
            let expected = if engine == Engine::Postgres {
                ["\"s\".\"t\".\"user\"", "\"s.t\".\"user\""]
            } else {
                ["`s.t`.`user`", "`s`.`t`.`user`"]
            };
            assert_eq!(result.items[0].insert_text, expected[0]);
            assert_eq!(result.items[1].insert_text, expected[1]);
        }
    }
}

#[test]
fn empty_prefix_browses_logical_names_and_deduplicates_schemas_before_truncation() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let mut tables = vec![
            table("a", "zz", &[]),
            table("zz_long_schema", "bbbb", &[]),
            table("zz_long_schema", "aaaa", &[]),
        ];
        for _ in 0..2 {
            let catalog = CompletionCatalog::new(engine, tables.clone()).unwrap();
            let all = complete("SELECT * FROM |", &catalog, 5);
            assert_eq!(
                labels(&all),
                [
                    "zz_long_schema.aaaa",
                    "zz_long_schema.bbbb",
                    "a.zz",
                    "a",
                    "zz_long_schema"
                ]
            );
            assert!(
                !all.truncated,
                "Repeated schemas must not consume result slots"
            );
            let small = complete("SELECT * FROM |", &catalog, 1);
            assert_eq!(small.items, all.items[..1]);
            assert!(small.truncated);
            tables.reverse();
        }
        // Preserve the old identity even when a table and schema share it.
        let mut tables = vec![table("", "same", &[]), table("same", "other", &[])];
        for _ in 0..2 {
            let catalog = CompletionCatalog::new(engine, tables.clone()).unwrap();
            let result = complete("SELECT * FROM same|", &catalog, 1);
            assert_eq!(labels(&result), ["same"]);
            assert_eq!(result.items[0].kind, CompletionKind::Table);
            assert!(!result.truncated);
            tables.reverse();
        }
    }
}
