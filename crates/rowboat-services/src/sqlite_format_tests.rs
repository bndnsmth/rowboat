use super::{Kind, format_sqlite, tokenize};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode};

async fn fixture() -> (tempfile::TempDir, Database) {
    let directory = tempfile::Builder::new()
        .prefix("pass14_formatter_")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let profile = ConnectionProfile {
        name: "SQLite formatter test".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: directory.path().join("formatter.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = Database::connect(&profile, "").await.unwrap();
    (directory, db)
}

fn formatted(sql: &str) -> String {
    let result = crate::format_sql_for_engine(Engine::Sqlite, sql).unwrap();
    assert_eq!(format_sqlite(&result).unwrap(), result, "not idempotent");
    result
}

#[test]
fn sqlite_format_ordinary_queries_have_stable_readable_layout() {
    assert_eq!(
        formatted(
            "  select  t.id,t.name  from main . things t where t.id>=?1 order by t.name limit 10;  "
        ),
        "select t.id, t.name\nfrom main.things t\nwhere t.id >= ?1\norder by t.name\nlimit 10;"
    );
    assert_eq!(formatted("\t\r\n"), "");
    assert_eq!(formatted("select 1;select 2;"), "select 1;\nselect 2;");
}

#[test]
fn sqlite_format_preserves_exact_opaque_tokens_and_sqlite_escaping() {
    let sql = "SeLeCt 'C:\\' AS Path, 'it''s\nMiXeD' AS Value, \"a\"\"b\", `c``d`, [space name], X'00aAFF', x'', :Case, @Case, $Case, ?001; -- tail  \r\n/* block\n  exact */";
    let result = formatted(sql);
    let opaque = |sql: &str| {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    Kind::Quoted
                        | Kind::Blob
                        | Kind::Parameter
                        | Kind::LineComment
                        | Kind::BlockComment
                )
            })
            .map(|t| t.text.to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        opaque(&result),
        vec![
            "'C:\\'",
            "'it''s\nMiXeD'",
            "\"a\"\"b\"",
            "`c``d`",
            "[space name]",
            "X'00aAFF'",
            "x''",
            ":Case",
            "@Case",
            "$Case",
            "?001",
            "-- tail  \r",
            "/* block\n  exact */"
        ]
    );
    assert_eq!(opaque(sql), opaque(&result));
    assert!(result.starts_with("SeLeCt "));
    assert!(result.contains("AS Path"));
}

#[test]
fn sqlite_format_lexical_failures_return_errors_without_replacement_text() {
    for sql in [
        "select 'unterminated",
        "select \"unterminated",
        "select `unterminated",
        "select [unterminated",
        "select 1 /* unfinished",
        "select X'0'",
        "select x'GG'",
        "select x'00",
        "select :",
        "select @",
        "select $",
        "select $ns::key(unclosed",
        "select $key(has space)",
        "select 1e+",
        "select 0xGG",
        "select 1__2",
        "select 1_",
        "select 1._2",
        "select 123abc",
        "select !1",
        "select 1\0; delete from t",
        "select (1",
        "select 1)",
        "select ]",
        "select \\oops",
        "select 1\u{b}",
    ] {
        assert!(
            crate::format_sql_for_engine(Engine::Sqlite, sql).is_err(),
            "accepted {sql:?}"
        );
    }
}

#[test]
fn sqlite_format_extended_parameter_names_remain_single_tokens() {
    let sql = "select :a::b(suffix), @a::b(suffix), $ns::key(any.+-*/[]'\"text), #name, ?123, ?, $日本語, a$b";
    let result = formatted(sql);
    let params: Vec<_> = tokenize(&result)
        .unwrap()
        .into_iter()
        .filter(|t| t.kind == Kind::Parameter)
        .map(|t| t.text)
        .collect();
    assert_eq!(
        params,
        [
            ":a::b(suffix)",
            "@a::b(suffix)",
            "$ns::key(any.+-*/[]'\"text)",
            "#name",
            "?123",
            "?",
            "$日本語"
        ]
    );
    assert!(result.ends_with("a$b"));
}

#[test]
fn sqlite_format_never_merges_operators_comments_blobs_or_numbers() {
    for sql in [
        "select 1- -2, 1+ +2, 1/ *2, x 'ab', 1 . name",
        "select 1 -- CR does not end SQLite line comments\r still comment\n+ 2",
        "select 1/*a*/+/*b*/2 -- trailing spaces  ",
        "select .5, 1., 1.e2, 1_234, 0xFF_FF, 12.3_4e+5_6, .5e-2",
        "\u{feff}select 1, 'not\u{feff}whitespace', 日本語.列",
        "select json -> '$.x', json ->> '$.x', a != b, a <> b, a || b, a << 2, ~a",
        "select 'not /* a comment */ -- or parameter ?1'; -- end",
    ] {
        let result = formatted(sql);
        assert_eq!(tokenize(sql).unwrap(), tokenize(&result).unwrap());
    }
}

#[tokio::test]
async fn sqlite_format_real_engine_literals_identifiers_numbers_and_json_are_equivalent() {
    let (_directory, db) = fixture().await;
    db.query("CREATE TABLE [quoted table] (\"a\"\"b\", `c``d`, [日本語]); INSERT INTO [quoted table] VALUES (7, 8, 9)", 100).await.unwrap();
    for sql in [
        "select 'C:\\' as path, 'it''s\nMiXeD' as value, X'00aAFF' as bytes, x'' as empty",
        "select q.\"a\"\"b\" as a,q.`c``d` as b,q.[日本語] as c from main.[quoted table] q",
        "select .5 as a,1. as b,1.e2 as c,1_234 as d,0xFF_FF as e,12.3_4e+2 as f,.5e-2 as g",
        "select 1- -2 as a,1+ +2 as b,4 / 2 as c,1/*comment\n  exact*/+2 as d",
        "select '{\"x\":\"MiXeD\"}'->'$.x' as a,'{\"x\":\"MiXeD\"}'->>'$.x' as b",
        "select 1 -- comment\r SELECT 999; still comment\n+2 as a",
    ] {
        let before = db.query(sql, 100).await.unwrap();
        let after = db.query(&formatted(sql), 100).await.unwrap();
        assert_eq!(before[0].rows, after[0].rows, "{sql}");
        assert_eq!(
            before[0]
                .columns
                .iter()
                .map(|c| &c.name)
                .collect::<Vec<_>>(),
            after[0].columns.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "{sql}"
        );
    }
}

#[test]
fn sqlite_format_real_engine_parameter_slots_and_names_are_identical() {
    use libsqlite3_sys as ffi;
    use std::ffi::{CStr, CString};

    struct Connection(*mut ffi::sqlite3);
    impl Drop for Connection {
        fn drop(&mut self) {
            // This test owns the handle; statements are finalized first.
            unsafe { ffi::sqlite3_close(self.0) };
        }
    }
    struct Statement(*mut ffi::sqlite3_stmt);
    impl Drop for Statement {
        fn drop(&mut self) {
            unsafe { ffi::sqlite3_finalize(self.0) };
        }
    }

    fn evidence(sql: &str) -> (Vec<Option<String>>, Vec<i64>) {
        let mut db = Connection(std::ptr::null_mut());
        assert_eq!(
            unsafe { ffi::sqlite3_open(c":memory:".as_ptr(), &mut db.0) },
            ffi::SQLITE_OK
        );
        let sql = CString::new(sql).unwrap();
        let mut statement = Statement(std::ptr::null_mut());
        assert_eq!(
            unsafe {
                ffi::sqlite3_prepare_v2(
                    db.0,
                    sql.as_ptr(),
                    -1,
                    &mut statement.0,
                    std::ptr::null_mut(),
                )
            },
            ffi::SQLITE_OK
        );
        let count = unsafe { ffi::sqlite3_bind_parameter_count(statement.0) };
        let mut names = Vec::new();
        for slot in 1..=count {
            let name = unsafe { ffi::sqlite3_bind_parameter_name(statement.0, slot) };
            names.push(if name.is_null() {
                None
            } else {
                Some(unsafe { CStr::from_ptr(name) }.to_str().unwrap().to_owned())
            });
            assert_eq!(
                unsafe { ffi::sqlite3_bind_int64(statement.0, slot, i64::from(slot) * 100) },
                ffi::SQLITE_OK
            );
        }
        assert_eq!(unsafe { ffi::sqlite3_step(statement.0) }, ffi::SQLITE_ROW);
        let columns = unsafe { ffi::sqlite3_column_count(statement.0) };
        let values = (0..columns)
            .map(|column| unsafe { ffi::sqlite3_column_int64(statement.0, column) })
            .collect();
        assert_eq!(unsafe { ffi::sqlite3_step(statement.0) }, ffi::SQLITE_DONE);
        (names, values)
    }

    let sql = "SELECT :Case,:case,:Case,@Case,$Case,?12,?,?001,$ns::key(suffix),:a::b(suffix),@a::b(suffix),#name,$日本語";
    let before = evidence(sql);
    let after = evidence(&formatted(sql));
    // Compare SQLite's slot/name map AND values bound to distinct slots. Merely
    // comparing unbound NULL result cells would miss parameter identity changes.
    assert_eq!(before, after);
    assert_eq!(after.0.len(), 18);
    assert_eq!(after.0[13].as_deref(), Some("$ns::key(suffix)"));
    assert_eq!(
        after.1,
        [
            100, 200, 100, 300, 400, 1200, 1300, 100, 1400, 1500, 1600, 1700, 1800
        ]
    );
}

#[tokio::test]
async fn sqlite_format_real_engine_trigger_bodies_case_and_internal_semicolons_survive() {
    let sql = "CREATE TABLE source(id INTEGER PRIMARY KEY, value TEXT); CREATE TABLE audit(id, value); CREATE TRIGGER [Mixed trigger] AFTER INSERT ON source BEGIN INSERT INTO audit VALUES (new.id, CASE WHEN new.value = 'MiXeD' THEN 'Keep; CASE END' ELSE 'other' END); /* internal ; BEGIN END\n  exact */ UPDATE source SET value = value || ';Suffix' WHERE id = new.id; END; INSERT INTO source VALUES (1, 'MiXeD'); INSERT INTO source VALUES (2, 'lower'); SELECT * FROM source ORDER BY id; SELECT * FROM audit ORDER BY id;";
    let mut evidence = Vec::new();
    for script in [sql.to_owned(), formatted(sql)] {
        let (_directory, db) = fixture().await;
        let results = db.query(&script, 100).await.unwrap();
        evidence.push(
            results
                .into_iter()
                .map(|r| (r.rows, r.rows_affected))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(evidence[0], evidence[1]);
    let audit = &evidence[1].last().unwrap().0;
    assert_eq!(audit[0][1], Cell::Text("Keep; CASE END".into()));
    assert_eq!(audit[1][1], Cell::Text("other".into()));
}

#[tokio::test]
async fn sqlite_format_real_engine_pragma_cte_upsert_returning_and_transactions_survive() {
    let script = "PRAGMA foreign_keys=ON; CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT); BEGIN; WITH seed(id, value) AS (VALUES (1, 'KeepCase')) INSERT INTO items SELECT * FROM seed WHERE true ON CONFLICT(id) DO UPDATE SET value=excluded.value RETURNING id,value; INSERT INTO items VALUES(1,'Changed') ON CONFLICT(id) DO UPDATE SET value=excluded.value RETURNING id,value; SAVEPOINT s; INSERT OR IGNORE INTO items VALUES (1, 'ignored'); ROLLBACK TO s; RELEASE s; COMMIT; PRAGMA main.table_info('items'); PRAGMA foreign_keys; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<3) SELECT x FROM n ORDER BY x; SELECT * FROM items;";
    let mut evidence = Vec::new();
    for sql in [script.to_owned(), formatted(script)] {
        let (_directory, db) = fixture().await;
        let results = db.query(&sql, 100).await.unwrap();
        evidence.push(
            results
                .into_iter()
                .map(|r| (r.rows, r.rows_affected))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(evidence[0], evidence[1]);
    assert_eq!(
        evidence[1].last().unwrap().0[0][1],
        Cell::Text("Changed".into())
    );
}

#[tokio::test]
async fn sqlite_format_does_not_execute_mutating_sql_or_require_existing_schema() {
    let (_directory, db) = fixture().await;
    db.query(
        "CREATE TABLE sentinel(value); INSERT INTO sentinel VALUES ('kept'); PRAGMA user_version=7",
        100,
    )
    .await
    .unwrap();
    formatted("DELETE FROM sentinel; PRAGMA user_version=99; DROP TABLE sentinel;");
    formatted("SELECT unknown_function(t.column) FROM nonexistent_schema.missing AS t;");
    assert_eq!(
        db.query("SELECT value FROM sentinel", 100).await.unwrap()[0].rows[0][0],
        Cell::Text("kept".into())
    );
    assert_eq!(
        db.query("PRAGMA user_version", 100).await.unwrap()[0].rows[0][0],
        Cell::Text("7".into())
    );
}
