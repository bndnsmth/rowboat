//! Explicit disposable fixtures only. Temporary tables are connection-local.
use anyhow::{Result, ensure};
use rowboat_admin::query::{expand_named, explain};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine};
use std::collections::BTreeMap;

async fn assert_rejected_without_write(db: &Database, sql: &str) -> Result<()> {
    let result = explain(db, sql).await;
    // Check the marker even if EXPLAIN returned an error: a native batch can
    // commit an earlier mutation before reporting a later statement failure.
    let counts = db
        .query("SELECT COUNT(*) FROM rowboat_admin_explain_marker", 10)
        .await?;
    ensure!(
        counts[0].rows[0][0] == Cell::Text("0".into()),
        "EXPLAIN executed hidden INSERT: {sql:?}"
    );
    ensure!(
        result.is_err(),
        "EXPLAIN accepted a hidden statement: {sql:?}"
    );
    Ok(())
}

async fn regression(engine: Engine) -> Result<()> {
    let profile = ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "rowboat"
        }
        .into(),
        ssl_mode: "disable".into(),
        read_only: false, // Prove EXPLAIN validation, not a server read-only guard.
        ..Default::default()
    };
    let db = Database::connect(&profile, "rowboat-test").await?;
    db.query(
        "CREATE TEMPORARY TABLE rowboat_admin_explain_marker (id INTEGER)",
        10,
    )
    .await?;
    ensure!(
        !explain(&db, "SELECT 1").await?.is_empty(),
        "Control EXPLAIN failed"
    );
    let markers = if engine == Engine::MySql {
        vec!["-- hidden", "# hidden"]
    } else {
        vec!["-- hidden"]
    };
    for ending in ["\r", "\n", "\r\n"] {
        for marker in &markers {
            let sql = format!(
                "SELECT 1 {marker}{ending}; INSERT INTO rowboat_admin_explain_marker VALUES (1)"
            );
            assert_rejected_without_write(&db, &sql).await?;

            // Verify parameter expansion against the native server lexer, not
            // merely against our own expected string or core's SQL splitter.
            let named = format!("SELECT 1 {marker} :unbound_in_comment{ending}, :after");
            let rendered = expand_named(
                engine,
                &named,
                &BTreeMap::from([("after".into(), Cell::Text("second".into()))]),
            )?;
            let results = db.query(&rendered, 10).await?;
            let expected = if engine == Engine::MySql && ending == "\r" {
                1
            } else {
                2
            };
            ensure!(
                results[0].columns.len() == expected,
                "Parameter lexer differs from server: {named:?}"
            );
            if expected == 2 {
                ensure!(
                    results[0].rows[0][1] == Cell::Text("second".into()),
                    "Parameter after comment was not expanded"
                );
            }
        }
    }
    if engine == Engine::MySql {
        for mode in [
            "NO_BACKSLASH_ESCAPES",
            "ANSI_QUOTES",
            "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
        ] {
            db.query(&format!("SET SESSION sql_mode='{mode}'"), 10)
                .await?;
            assert_rejected_without_write(
                &db,
                "SELECT 'safe\\'; INSERT INTO rowboat_admin_explain_marker VALUES (1); -- '",
            )
            .await?;
            assert_rejected_without_write(
                &db,
                "SELECT 1 AS \"safe\\\"; INSERT INTO rowboat_admin_explain_marker VALUES (1); -- \"",
            )
            .await?;
            ensure!(
                !explain(&db, "SELECT 'it''s fine', 1 AS `ends\\`")
                    .await?
                    .is_empty(),
                "Unambiguous EXPLAIN rejected in {mode}"
            );
        }
        for control in ['\u{1}', '\u{b}', '\u{c}', '\u{1f}', '\u{7f}'] {
            let sql = format!("SELECT 1 --{control}:hidden\n, :after");
            let rendered = expand_named(
                engine,
                &sql,
                &BTreeMap::from([("after".into(), Cell::Null)]),
            )?;
            let results = db.query(&rendered, 10).await?;
            ensure!(
                results[0].rows[0] == vec![Cell::Text("1".into()), Cell::Null],
                "MySQL control-character comment differs from server"
            );
        }
    } else {
        db.query("SET standard_conforming_strings = off", 10)
            .await?;
        assert_rejected_without_write(
            &db,
            "SELECT 'safe\\'; INSERT INTO rowboat_admin_explain_marker VALUES (1); -- '",
        )
        .await?;
        ensure!(!explain(&db, "SELECT E'quote\\\'; -- hidden\r', $body$; INSERT INTO rowboat_admin_explain_marker VALUES (1); --\r$body$").await?.is_empty(), "Explicit E/dollar strings rejected");
    }
    db.query("DROP TABLE rowboat_admin_explain_marker", 10)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires parent-owned disposable PostgreSQL at 127.0.0.1:15435"]
async fn postgres_explain_rejects_hidden_writes() -> Result<()> {
    regression(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires parent-owned disposable MySQL at 127.0.0.1:13309"]
async fn mysql_explain_rejects_hidden_writes() -> Result<()> {
    regression(Engine::MySql).await
}
