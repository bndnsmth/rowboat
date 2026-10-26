use rowboat_admin::{
    explain::{self, RawFormat, ReportMode},
    query,
};
use rowboat_core::Engine;

#[test]
fn postgres_profile_keeps_actual_estimate_and_report_timings_separate() {
    let raw = r#"[{"Plan":{"Node Type":"Seq Scan","Plan Rows":9007199254740993,"Total Cost":123.0000000000000001,"Actual Startup Time":0.001,"Actual Total Time":0.017,"Actual Rows":3,"Actual Loops":2,"Plans":[{"Node Type":"Result","Actual Rows":0,"Actual Loops":0}]},"Planning Time":0.1234567890123456789,"Execution Time":0.029}]"#;
    let r = explain::decode_analyze(Engine::Postgres, raw).unwrap();
    assert_eq!(r.mode, ReportMode::Analyze);
    assert_eq!(r.raw_format, RawFormat::Json);
    assert_eq!(r.raw_report, raw);
    assert_eq!(r.raw_json, raw);
    assert_eq!(r.timing["Planning Time"].0, "0.1234567890123456789");
    assert_eq!(r.timing["Execution Time"].0, "0.029");
    assert_eq!(r.roots[0].estimates["Plan Rows"].0, "9007199254740993");
    assert_eq!(r.roots[0].actual["Actual Rows"].0, "3");
    assert_eq!(r.roots[0].actual["Actual Loops"].0, "2");
    assert_eq!(r.roots[0].children[0].actual["Actual Loops"].0, "0");
    assert_eq!(
        explain::decode(Engine::Postgres, raw).unwrap().mode,
        ReportMode::Explain
    );
}

#[test]
fn mysql_native_indentation_scientific_values_and_never_executed() {
    let raw = "-> Nested loop inner join  (cost=1.46..2.73 rows=2) (actual time=0.0177..0.0181 rows=2 loops=1)\n    -> Table scan on `odd (cost=9 rows=9)`  (cost=1 rows=9007199254740993) (actual time=50e-6..60e-6 rows=1.25 loops=2)\n        -> Materialize  (cost=0..0 rows=1) (actual time=+0.5...75 rows=1 loops=1)\n    -> Index lookup on b  (cost=1 rows=1) (never executed)\n-> Second root (actual time=1E+3..1.234567890123456789e+3 rows=2 loops=1)\n";
    let r = explain::decode_analyze(Engine::MySql, raw).unwrap();
    assert_eq!(r.mode, ReportMode::Analyze);
    assert_eq!(r.raw_format, RawFormat::Tree);
    assert_eq!(r.raw_report, raw);
    assert!(r.raw_json.is_empty());
    assert!(r.timing.is_empty());
    assert_eq!(r.roots.len(), 2);
    let root = &r.roots[0];
    assert_eq!(root.operation, "Nested loop inner join");
    assert_eq!(root.children.len(), 2);
    assert_eq!(root.estimates["Startup Cost"].0, "1.46");
    assert_eq!(root.estimates["Total Cost"].0, "2.73");
    let scan = &root.children[0];
    assert_eq!(scan.operation, "Table scan on `odd (cost=9 rows=9)`");
    assert_eq!(scan.actual["Actual Startup Time"].0, "50e-6");
    assert_eq!(scan.actual["Actual Rows"].0, "1.25");
    assert_eq!(scan.estimates["Plan Rows"].0, "9007199254740993");
    assert_eq!(scan.children[0].actual["Actual Total Time"].0, ".75");
    assert!(root.children[1].actual.is_empty());
    assert_eq!(root.children[1].predicates["Execution"], "never executed");
    assert_eq!(
        r.roots[1].actual["Actual Total Time"].0,
        "1.234567890123456789e+3"
    );
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

#[test]
fn mysql_unknown_or_invalid_metrics_are_not_invented_and_decoder_is_bounded() {
    let raw = "future heading\n-> Future node (cost=NaN rows=?) (actual time=inf..1e rows=garbage loops=1 extra=7)\n    unknown detail\n";
    let r = explain::decode_analyze(Engine::MySql, raw).unwrap();
    assert_eq!(r.raw_report, raw);
    assert!(r.roots[0].estimates.is_empty());
    assert_eq!(r.roots[0].actual.len(), 1);
    assert_eq!(r.roots[0].actual["Actual Loops"].0, "1");
    assert!(r.warnings.len() >= 7);
    assert!(explain::decode_analyze(Engine::MySql, &"x".repeat(1024 * 1024 + 1)).is_err());
    assert!(explain::decode_analyze(Engine::MySql, &"-> x\n".repeat(4097)).is_err());
    let deep = (0..129)
        .map(|i| format!("{}-> x\n", " ".repeat(i)))
        .collect::<String>();
    assert!(explain::decode_analyze(Engine::MySql, &deep).is_err());
}

#[test]
fn analyze_is_explicit_and_reuses_the_full_mode_independent_boundary_guard() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let plain = query::explain_sql(engine, "SELECT 1").unwrap();
        assert!(!plain.contains("ANALYZE"));
        let analyzed = query::analyze_sql(
            engine,
            "/* lead */ WITH x AS (SELECT ';' AS v) SELECT * FROM x; -- tail",
        )
        .unwrap();
        assert!(analyzed.contains("ANALYZE"));
        assert_eq!(analyzed.contains("FORMAT=TREE"), engine == Engine::MySql);
        for sql in [
            "SELECT 1; SELECT 2",
            "SELECT 1; COMMIT",
            "SELECT 1; 'second'",
            "EXPLAIN ANALYZE SELECT 1",
            "INSERT INTO x VALUES(1)",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
            "WITH x AS (SELECT 1) UPDATE t SET id=1",
            "SELECT 1 INTO x",
            "SELECT * FROM t FOR UPDATE",
            "SELECT 1\0",
            "-- nothing",
            "SELECT 'safe\\'; DELETE FROM t; -- '",
        ] {
            assert!(
                query::analyze_sql(engine, sql).is_err(),
                "{engine:?} accepted {sql:?}"
            );
        }
        for ending in ["\r", "\n", "\r\n"] {
            assert!(
                query::analyze_sql(
                    engine,
                    &format!("SELECT 1 -- hidden{ending}; DELETE FROM t")
                )
                .is_err()
            );
        }
        assert!(
            query::analyze_sql(engine, "SELECT 'UPDATE', 'it''s fine' -- a final comment").is_ok()
        );
        assert!(
            query::analyze_sql(engine, "SELECT 1 -- a final comment")
                .unwrap()
                .ends_with("\n;")
        );
    }
    assert!(query::analyze_sql(Engine::MySql, "SELECT 1 /*!; DELETE FROM t */").is_err());
    assert!(query::analyze_sql(Engine::Postgres, "SELECT $x$; DELETE FROM t$x$").is_ok());
}
