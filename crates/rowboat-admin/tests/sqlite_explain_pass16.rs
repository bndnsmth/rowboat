//! Real disposable SQLite sessions plus hostile native-result decoder contracts.
use anyhow::Result;
use rowboat_admin::{
    explain::{self, AnalyzeIntent, RawFormat, SqlitePlanRow},
    providers::{self, Atomicity, Operation, UnsupportedOperation},
    query,
};
use rowboat_core::{
    Cell, Column, ConnectionProfile, Database, Engine, QueryResult, QuerySession,
    SqliteFileOptions, SqliteOpenMode,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    async fn new() -> Result<(Self, Database)> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local/pass16-explain")
            .join(format!(
                "test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&directory)?;
        let fixture = Self(directory);
        let db = Database::connect(&fixture.profile(false, true), "").await?;
        db.query("CREATE TABLE items(id INTEGER PRIMARY KEY, category TEXT, value INTEGER); CREATE INDEX category_index ON items(category); INSERT INTO items VALUES (1, 'a', 10), (2, 'b', 20); CREATE TABLE witness(id INTEGER)", 10).await?;
        Ok((fixture, db))
    }
    fn profile(&self, read_only: bool, create: bool) -> ConnectionProfile {
        ConnectionProfile {
            engine: Engine::Sqlite,
            read_only,
            file: Some(SqliteFileOptions {
                path: self.0.join("fixture.sqlite"),
                mode: if create {
                    SqliteOpenMode::CreateIfMissing
                } else {
                    SqliteOpenMode::OpenExisting
                },
            }),
            ..Default::default()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn native_rows(report: &explain::ExplainReport) -> Vec<SqlitePlanRow> {
    serde_json::from_str(&report.raw_report).unwrap()
}

#[tokio::test]
async fn actual_sqlite_plan_shapes_match_every_native_row_without_fabricated_metrics() -> Result<()>
{
    let (_fixture, db) = Fixture::new().await?;
    for (sql, detail) in [
        (
            "SELECT * FROM items WHERE category='a'",
            "SEARCH items USING INDEX category_index",
        ),
        ("SELECT * FROM items", "SCAN items"),
        (
            "SELECT * FROM items a JOIN items b ON b.id=a.value",
            "SEARCH b USING INTEGER PRIMARY KEY",
        ),
        (
            "SELECT * FROM items a WHERE EXISTS (SELECT 1 FROM items b WHERE b.value=a.id)",
            "CORRELATED SCALAR SUBQUERY",
        ),
        (
            "SELECT id FROM items UNION SELECT value FROM items",
            "COMPOUND QUERY",
        ),
        (
            "SELECT * FROM items ORDER BY value",
            "USE TEMP B-TREE FOR ORDER BY",
        ),
        (
            "WITH x AS MATERIALIZED (SELECT * FROM items) SELECT * FROM x",
            "MATERIALIZE x",
        ),
    ] {
        let actual = db.query(&format!("EXPLAIN QUERY PLAN {sql}"), 1000).await?;
        let report = explain::execute(&db, sql).await?;
        assert_eq!(report.raw_format, RawFormat::SqliteQueryPlan);
        assert!(report.raw_json.is_empty());
        assert!(report.timing.is_empty());
        let native = native_rows(&report);
        assert_eq!(native.len(), actual[0].rows.len());
        for (row, actual) in native.iter().zip(&actual[0].rows) {
            assert_eq!(
                actual,
                &vec![
                    Cell::Text(row.id.to_string()),
                    Cell::Text(row.parent.to_string()),
                    Cell::Text(row.aux.to_string()),
                    Cell::Text(row.detail.clone())
                ]
            );
        }
        assert!(
            native.iter().any(|r| r.detail.contains(detail)),
            "{sql}: {native:?}"
        );
        let mut stack: Vec<_> = report.roots.iter().collect();
        let mut count = 0;
        while let Some(node) = stack.pop() {
            count += 1;
            assert!(
                node.estimates.is_empty() && node.actual.is_empty() && node.predicates.is_empty()
            );
            assert!(node.relation.is_none());
            let row: SqlitePlanRow = serde_json::from_value(node.raw.clone())?;
            assert_eq!(node.operation, row.detail);
            assert!(native.contains(&row));
            assert!(
                node.children
                    .iter()
                    .all(|child| child.raw["parent"] == row.id)
            );
            stack.extend(node.children.iter());
        }
        assert_eq!(count, native.len());
        if sql.contains("JOIN") {
            assert_eq!(report.roots.len(), 2);
        }
    }
    Ok(())
}

#[tokio::test]
async fn dml_ctes_and_functions_are_only_planned_even_on_read_only_session() -> Result<()> {
    let (fixture, db) = Fixture::new().await?;
    // This native function throws when evaluated, proving its body is not called.
    assert!(
        db.query("SELECT abs(-9223372036854775808)", 10)
            .await
            .is_err()
    );
    let read_only = Database::connect(&fixture.profile(true, false), "").await?;
    for session in [&db, &read_only] {
        for sql in [
            "INSERT INTO items VALUES (3, 'c', abs(-9223372036854775808))",
            "UPDATE items SET value=abs(-9223372036854775808)",
            "DELETE FROM items WHERE value=10",
            "WITH x AS (SELECT 3) INSERT INTO witness SELECT * FROM x",
            "WITH x AS (SELECT 1) DELETE FROM items WHERE id IN (SELECT * FROM x)",
            "WITH x AS (SELECT abs(-9223372036854775808)) SELECT * FROM x",
            "SELECT abs(-9223372036854775808)",
        ] {
            explain::execute(session, sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e:#}"));
        }
    }
    assert_eq!(
        db.query("SELECT id, category, value FROM items ORDER BY id", 10)
            .await?[0]
            .rows,
        vec![
            vec![
                Cell::Text("1".into()),
                Cell::Text("a".into()),
                Cell::Text("10".into())
            ],
            vec![
                Cell::Text("2".into()),
                Cell::Text("b".into()),
                Cell::Text("20".into())
            ]
        ]
    );
    assert_eq!(
        db.query("SELECT count(*) FROM witness", 10).await?[0].rows[0][0],
        Cell::Text("0".into())
    );
    Ok(())
}

#[tokio::test]
async fn empty_valid_dml_plan_is_truthful_and_never_executes_delete() -> Result<()> {
    let (_fixture, db) = Fixture::new().await?;
    let raw = db
        .query("EXPLAIN QUERY PLAN DELETE FROM items", 1000)
        .await?;
    assert!(
        raw[0].rows.is_empty(),
        "Native truncate optimization should have no query-plan rows"
    );
    let report = explain::execute(&db, "DELETE FROM items").await?;
    assert!(report.roots.is_empty());
    assert_eq!(report.raw_report, "[]");
    assert!(report.warnings[0].contains("no query-plan rows"));
    assert_eq!(
        db.query("SELECT count(*) FROM items", 10).await?[0].rows[0][0],
        Cell::Text("2".into())
    );
    Ok(())
}

struct NoQueries;
impl QuerySession for NoQueries {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }
    async fn query(&self, _: &str, _: usize) -> Result<Vec<QueryResult>> {
        panic!("Invalid SQL/unsupported Analyze reached execution")
    }
}

#[tokio::test]
async fn hidden_statements_and_ambiguous_boundaries_reject_before_dispatch_or_effects() -> Result<()>
{
    let (_fixture, db) = Fixture::new().await?;
    for sql in [
        "SELECT 1; INSERT INTO witness VALUES (1)",
        "SELECT 1; -- hidden\r\nINSERT INTO witness VALUES (1)",
        "SELECT 1; -- hidden\nINSERT INTO witness VALUES (1)",
        "SELECT 1; /* first /* nested? */ INSERT INTO witness VALUES (1) -- */",
        "SELECT 'safe\\'; INSERT INTO witness VALUES (1); -- '",
        "SELECT 1 AS [safe]; INSERT INTO witness VALUES (1); -- ]",
        "SELECT 1 AS `safe`; INSERT INTO witness VALUES (1); -- `",
        "SELECT 1 AS \"safe\"; INSERT INTO witness VALUES (1); -- \"",
        "SELECT 1; 'hidden'",
        "SELECT 1;;",
        "SELECT 1 /* unterminated",
        "SELECT 1\0; INSERT INTO witness VALUES (1)",
        "-- only comment\rINSERT INTO witness VALUES (1)",
        "ANALYZE",
        "EXPLAIN QUERY PLAN SELECT 1",
    ] {
        assert!(explain::execute(&NoQueries, sql).await.is_err(), "{sql:?}");
        assert!(explain::execute(&db, sql).await.is_err(), "{sql:?}");
        assert_eq!(
            db.query("SELECT count(*) FROM witness", 10).await?[0].rows[0][0],
            Cell::Text("0".into())
        );
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_native_quotes_comments_and_exact_names_survive() -> Result<()> {
    let (_fixture, db) = Fixture::new().await?;
    db.query("CREATE TABLE \"table USING INDEX odd.name\" (\"key;column\" INTEGER); CREATE INDEX \"index (cost=10)\" ON \"table USING INDEX odd.name\"(\"key;column\")", 10).await?;
    for sql in [
        "SELECT 'ends\\', 'it''s; fine', 1 AS [bracket;name], 2 AS `back``tick;name`, 3 AS \"double\"\"quote;name\"",
        "SELECT 1 -- no space needed\r; INSERT INTO witness VALUES (1)",
        "SELECT 1; -- trailing\rINSERT INTO witness VALUES (1)",
        "SELECT 1 -- comment\r\n; /* trailing */ -- end",
        "SELECT 1 /* nonnested /* comment */; -- end",
    ] {
        explain::execute(&db, sql)
            .await
            .unwrap_or_else(|e| panic!("{sql:?}: {e:#}"));
    }
    let report = explain::execute(
        &db,
        "SELECT * FROM \"table USING INDEX odd.name\" WHERE \"key;column\"=1",
    )
    .await?;
    let row = &native_rows(&report)[0];
    assert!(row.detail.contains("table USING INDEX odd.name"));
    assert!(row.detail.contains("index (cost=10)"));
    assert!(report.roots[0].relation.is_none());
    assert_eq!(report.roots[0].access.as_deref(), Some("SEARCH"));
    assert!(report.roots[0].estimates.is_empty());
    assert_eq!(
        db.query("SELECT count(*) FROM witness", 10).await?[0].rows[0][0],
        Cell::Text("0".into())
    );
    Ok(())
}

#[tokio::test]
async fn capability_and_analyze_rejection_are_explicit_before_any_query() -> Result<()> {
    let support = providers::operation_support(Engine::Sqlite, Operation::Explain)?;
    assert_eq!(support.atomicity, Atomicity::ReadOnly);
    assert!(!support.confirmation_required);
    for error in [
        query::analyze_sql(Engine::Sqlite, "SELECT 1").unwrap_err(),
        explain::analyze(&NoQueries, "SELECT 1", AnalyzeIntent::ExecuteQuery)
            .await
            .unwrap_err(),
        explain::decode_analyze(Engine::Sqlite, "").unwrap_err(),
    ] {
        let unsupported = error.downcast_ref::<UnsupportedOperation>().unwrap();
        assert_eq!(unsupported.operation, Operation::Analyze);
        assert!(!unsupported.reason.is_empty());
    }
    Ok(())
}

fn row(id: i64, parent: i64, detail: &str) -> SqlitePlanRow {
    SqlitePlanRow {
        id,
        parent,
        aux: 0,
        detail: detail.into(),
    }
}

#[test]
fn unknown_native_details_auxiliary_fields_ids_and_source_order_are_lossless() -> Result<()> {
    let mut rows = vec![
        row(9, 50, "new detail (cost=12) actual time=9"),
        row(50, 0, "unknown parent"),
        row(2, 50, "SCAN table"),
        row(1, 0, "SEARCHING something"),
        row(20, 2, "future child"),
    ];
    rows[0].aux = i64::MAX;
    let report = explain::decode_sqlite_rows(&rows)?;
    assert_eq!(native_rows(&report), rows);
    assert_eq!(
        report
            .roots
            .iter()
            .map(|r| r.raw["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [50, 1]
    );
    assert_eq!(
        report.roots[0]
            .children
            .iter()
            .map(|r| r.raw["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [9, 2]
    );
    assert_eq!(report.roots[0].children[1].children[0].raw["id"], 20);
    assert!(report.roots[0].children[0].estimates.is_empty());
    assert!(report.roots[0].children[0].actual.is_empty());
    assert!(report.roots[1].access.is_none());
    assert_eq!(
        serde_json::to_value(explain::decode(Engine::Sqlite, &report.raw_report)?)?,
        serde_json::to_value(&report)?
    );
    Ok(())
}

#[test]
fn malformed_graphs_and_excessive_sizes_fail_with_bounded_work() {
    for rows in [
        vec![row(1, 0, "x"), row(1, 0, "duplicate")],
        vec![row(1, 1, "self cycle")],
        vec![row(1, 2, "cycle"), row(2, 1, "cycle")],
        vec![row(1, 999, "dangling")],
        vec![row(-1, 0, "negative")],
        vec![row(1, -1, "negative parent")],
        (1..=129).map(|i| row(i, i - 1, "deep")).collect(),
        (1..=1001).map(|i| row(i, 0, "wide")).collect(),
        vec![row(1, 0, &"x".repeat(1024 * 1024 + 1))],
    ] {
        assert!(explain::decode_sqlite_rows(&rows).is_err());
    }
    let boundary: Vec<_> = (1..=128).map(|i| row(i, i - 1, "deep")).collect();
    let report = explain::decode_sqlite_rows(&boundary).unwrap();
    assert_eq!(native_rows(&report).len(), 128);
    // Exercise recursive report serialization and Drop at the accepted boundary.
    assert!(serde_json::to_string(&report).is_ok());
    let wide: Vec<_> = (1..=1000).map(|i| row(i, 0, "wide")).collect();
    assert_eq!(
        explain::decode_sqlite_rows(&wide).unwrap().roots.len(),
        1000
    );
}

fn result() -> QueryResult {
    QueryResult {
        columns: ["id", "parent", "notused", "detail"]
            .into_iter()
            .map(|name| Column {
                name: name.into(),
                data_type: String::new(),
                nullable: false,
                primary_key: false,
                default_value: None,
            })
            .collect(),
        rows: vec![vec![
            Cell::Text("1".into()),
            Cell::Text("0".into()),
            Cell::Text("0".into()),
            Cell::Text("SCAN items".into()),
        ]],
        ..Default::default()
    }
}

struct NativeResult(QueryResult);
impl QuerySession for NativeResult {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }
    async fn query(&self, sql: &str, limit: usize) -> Result<Vec<QueryResult>> {
        assert!(sql.starts_with("EXPLAIN QUERY PLAN\n"));
        assert_eq!(limit, 1000);
        Ok(vec![self.0.clone()])
    }
}

#[tokio::test]
async fn truncated_or_malformed_native_results_never_become_complete_reports() -> Result<()> {
    assert!(explain::decode_sqlite(&result()).is_ok());
    let mut truncated = result();
    truncated.truncated = true;
    assert!(
        explain::execute(&NativeResult(truncated.clone()), "SELECT 1")
            .await
            .is_err()
    );
    assert!(
        query::explain(&NativeResult(truncated.clone()), "SELECT 1")
            .await
            .is_err()
    );
    let mut malformed = vec![truncated];
    let mut r = result();
    r.columns.pop();
    malformed.push(r);
    let mut r = result();
    r.columns[0].name = "detail".into();
    malformed.push(r);
    let mut r = result();
    r.rows[0].pop();
    malformed.push(r);
    for cell in [
        Cell::Null,
        Cell::Binary(vec![1]),
        Cell::Text("1.0".into()),
        Cell::Text("9223372036854775808".into()),
    ] {
        let mut r = result();
        r.rows[0][0] = cell;
        malformed.push(r);
    }
    let mut r = result();
    r.rows[0][3] = Cell::Null;
    malformed.push(r);
    for r in malformed {
        assert!(explain::decode_sqlite(&r).is_err());
        assert!(
            explain::execute(&NativeResult(r), "SELECT 1")
                .await
                .is_err()
        );
    }
    Ok(())
}
