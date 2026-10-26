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
        columns: columns.iter().map(|c| (*c).into()).collect(),
    }
}

fn catalog() -> CompletionCatalog {
    CompletionCatalog::new(
        Engine::Sqlite,
        vec![table(
            "main",
            "records",
            &[
                "User",
                "Users",
                "user_activity_log",
                "日本語",
                "Ωname",
                "ωname",
                "nick\"name",
                "a,b (c)",
            ],
        )],
    )
    .unwrap()
}

fn complete(sql: &str, catalog: &CompletionCatalog, limit: usize) -> SqlCompletions {
    let caret = sql.find('|').unwrap();
    complete_sql(&sql.replacen('|', "", 1), caret, catalog, limit).unwrap()
}

fn columns(result: &SqlCompletions) -> Vec<&str> {
    result
        .items
        .iter()
        .filter(|i| i.kind == CompletionKind::Column)
        .map(|i| i.label.as_str())
        .collect()
}

#[test]
fn sqlite_three_identifier_delimiters_use_exact_prefix_and_canonical_insertion() {
    let catalog = catalog();
    for (open, close) in [('"', '"'), ('`', '`'), ('[', ']')] {
        let sql =
            format!("SELECT r.{open}User|{close} FROM {open}main{close}.{open}records{close} r");
        let result = complete(&sql, &catalog, 100);
        assert_eq!(columns(&result), ["User", "Users"]);
        assert_eq!(result.items[0].insert_text, "\"User\"");
        assert_eq!(
            &sql.replace('|', "")[result.replacement],
            format!("{open}User{close}")
        );
        let lower = complete(
            &format!("SELECT r.{open}user|{close} FROM main.records r"),
            &catalog,
            100,
        );
        assert_eq!(columns(&lower), ["user_activity_log"]);
        let punctuation = complete(
            &format!("SELECT r.{open}a,|b (c){close} FROM main.records r"),
            &catalog,
            100,
        );
        assert_eq!(columns(&punctuation), ["a,b (c)"]);
        assert_eq!(punctuation.items[0].insert_text, "\"a,b (c)\"");
    }
    let result = complete(
        "SELECT r.\"nick\"\"|name\" FROM main.records r",
        &catalog,
        100,
    );
    assert_eq!(columns(&result), ["nick\"name"]);
    assert_eq!(result.items[0].insert_text, "\"nick\"\"name\"");
}

#[test]
fn sqlite_ascii_resolution_preserves_spelling_and_does_not_fold_unicode() {
    let catalog = catalog();
    for sql in [
        "SELECT r.USER| FROM MAIN.RECORDS R",
        "SELECT \"alias\".USER| FROM main.records AS \"Alias\"",
        "SELECT d.USER| FROM (SELECT r.uSeR FROM main.records r) d",
    ] {
        let result = complete(sql, &catalog, 100);
        if sql.contains("(SELECT") {
            assert_eq!(columns(&result), ["User"]);
        } else {
            assert_eq!(columns(&result), ["User", "Users", "user_activity_log"]);
        }
    }
    assert_eq!(
        columns(&complete("SELECT r.ω| FROM main.records r", &catalog, 100)),
        ["ωname"]
    );
    assert_eq!(
        columns(&complete("SELECT r.Ω| FROM main.records r", &catalog, 100)),
        ["Ωname"]
    );
    assert_eq!(
        complete("SELECT ω.| FROM main.records Ω", &catalog, 100).context,
        CompletionContext::Unsupported
    );
    assert!(
        CompletionCatalog::new(Engine::Sqlite, vec![table("main", "t", &["Name", "name"])])
            .is_err()
    );
    assert!(CompletionCatalog::new(Engine::Sqlite, vec![table("main", "t", &["Ω", "ω"])]).is_ok());
}

#[test]
fn sqlite_comments_strings_and_blob_literals_do_not_borrow_server_lexing() {
    let catalog = catalog();
    for sql in [
        "SELECT 1 --without whitespace r.|\n FROM main.records r",
        "SELECT 1 --comment\r r.|\n FROM main.records r",
        "SELECT 'single '' quote r.|' FROM main.records r",
        "SELECT X'fe|ff' FROM main.records r",
        "SELECT /* block r.| */ 1 FROM main.records r",
    ] {
        let result = complete(sql, &catalog, 100);
        assert_eq!(result.context, CompletionContext::Suppressed, "{sql}");
        assert!(result.items.is_empty());
    }
    for sql in [
        "SELECT '\\', r.us| FROM main.records r",
        "SELECT 1 --comment\n, r.us| FROM main.records r",
        "SELECT /* outer /* inner */ r.us| FROM main.records r",
        "SELECT /*!99999 FROM imaginary x */ r.us| FROM main.records r",
        "SELECT X'feff', r.us| FROM main.records r",
    ] {
        assert_eq!(
            columns(&complete(sql, &catalog, 100)),
            ["User", "Users", "user_activity_log"],
            "{sql}"
        );
    }
    // These are invalid SQLite drafts, not PostgreSQL/MySQL strings/comments.
    for sql in [
        "SELECT # r.us| FROM main.records r",
        "SELECT $tag$r.us|$tag$ FROM main.records r",
    ] {
        assert_ne!(
            complete(sql, &catalog, 100).context,
            CompletionContext::Suppressed,
            "{sql}"
        );
    }
}

#[test]
fn sqlite_search_path_scopes_ctes_and_unsupported_syntax_stay_conservative() {
    let mut catalog = CompletionCatalog::new(
        Engine::Sqlite,
        vec![
            table("main", "records", &["main_column"]),
            table("temp", "records", &["temp_column"]),
            table("attached", "records", &["attached_column"]),
        ],
    )
    .unwrap();
    assert!(columns(&complete("SELECT r.| FROM records r", &catalog, 100)).is_empty());
    catalog
        .set_search_path(Some(vec!["temp".into(), "main".into(), "attached".into()]))
        .unwrap();
    assert_eq!(
        columns(&complete("SELECT r.| FROM records r", &catalog, 100)),
        ["temp_column"]
    );
    assert_eq!(
        columns(&complete("SELECT r.| FROM MAIN.RECORDS r", &catalog, 100)),
        ["main_column"]
    );
    assert_eq!(
        columns(&complete(
            "WITH RECURSIVE first AS (SELECT * FROM later), later(value) AS (SELECT 1) SELECT f.| FROM first f",
            &catalog,
            100
        )),
        ["value"]
    );
    for sql in [
        "SELECT r.| FROM main.records r(renamed)",
        "WITH c(a,b) AS (SELECT main_column FROM main.records) SELECT c.| FROM c",
        "SELECT r.| FROM main.records r JOIN attached.records r ON 1=1",
    ] {
        assert!(columns(&complete(sql, &catalog, 100)).is_empty(), "{sql}");
    }
    assert_eq!(
        complete("PRAGMA table_info(re|)", &catalog, 100).context,
        CompletionContext::Unsupported
    );
    let keywords = complete("SELECT SEL|", &catalog, 100);
    assert_eq!(keywords.items[0].label, "SELECT");
    assert_eq!(keywords.items[0].kind, CompletionKind::Keyword);
}

#[test]
fn sqlite_short_logical_prefix_order_precedes_top_k_and_empty_prefix_ignores_schema() {
    let mut tables: Vec<_> = (0..250)
        .map(|n| table("a", &format!("user_activity_{n:03}"), &[]))
        .collect();
    tables.push(table("zzz_schema_with_a_long_name", "users", &[]));
    tables.push(table("zz", "user", &[]));
    for _ in 0..2 {
        let catalog = CompletionCatalog::new(Engine::Sqlite, tables.clone()).unwrap();
        for limit in [1, 2, 200] {
            let result = complete("SELECT * FROM USER|", &catalog, limit);
            assert_eq!(result.items[0].label, "zz.user");
            if limit > 1 {
                assert_eq!(result.items[1].label, "zzz_schema_with_a_long_name.users");
            }
            assert_eq!(result.items.len(), limit);
            assert!(result.truncated);
        }
        assert_eq!(
            complete("SELECT * FROM |", &catalog, 1).items[0].label,
            "zz.user"
        );
        tables.reverse();
    }
}

#[test]
fn sqlite_draft_byte_boundaries_and_existing_input_catalog_caps_remain_bounded() {
    let catalog = catalog();
    for sql in [
        "SELECT r.[日本語] FROM main.records r",
        "SELECT r.`User`, 'text\\' FROM main.records r --line\r\n",
        "SELECT r.\"nick\"\"name\" FROM [main].[records] r",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c) SELECT c.n FROM c",
    ] {
        for caret in (0..=sql.len()).filter(|caret| sql.is_char_boundary(*caret)) {
            let result = complete_sql(sql, caret, &catalog, 7).unwrap();
            assert!(result.items.len() <= 7);
            assert!(sql.is_char_boundary(result.replacement.start));
            assert!(sql.is_char_boundary(result.replacement.end));
        }
    }
    assert!(complete_sql("日本語", 1, &catalog, 7).is_err());
    assert!(complete_sql("", 0, &catalog, 201).is_err());
    assert!(complete_sql(&"x".repeat(256 * 1024 + 1), 0, &catalog, 7).is_err());
    assert!(CompletionCatalog::new(Engine::Sqlite, vec![table("", "t", &[]); 10_001]).is_err());
    assert!(
        CompletionCatalog::new(Engine::Sqlite, vec![table("", &"x".repeat(1025), &[])]).is_err()
    );
}
