//! Only the explicitly provisioned, task-owned disposable Podman databases.
use anyhow::Result;
use rowboat_core::*;

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
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
        ..Default::default()
    }
}
fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}
fn original(result: &QueryResult) -> Vec<(String, Cell)> {
    result
        .columns
        .iter()
        .zip(&result.rows[0])
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect()
}

#[tokio::test]
#[ignore = "Requires explicit task-owned PostgreSQL Podman fixture on port 15435 with ICU"]
async fn postgres_nondeterministic_collation_rejects_stale_edits() -> Result<()> {
    let db = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let other = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let table = TableRef {
        schema: "public".into(),
        name: "rowboat_core_ci_rows".into(),
    };
    db.query("DROP TABLE IF EXISTS public.rowboat_core_ci_rows; DROP COLLATION IF EXISTS public.rowboat_core_ci; CREATE COLLATION public.rowboat_core_ci (provider=icu,locale='und-u-ks-level2',deterministic=false); CREATE TABLE public.rowboat_core_ci_rows (id integer PRIMARY KEY, name text COLLATE public.rowboat_core_ci NOT NULL); INSERT INTO public.rowboat_core_ci_rows VALUES (1,'Alice')",10).await?;
    let snapshot = db
        .browse(
            &table,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            },
        )
        .await?;
    let stale = original(&snapshot);
    other
        .query(
            "UPDATE public.rowboat_core_ci_rows SET name='alice' WHERE id=1",
            10,
        )
        .await?;
    // Prove the fixture reproduces the original weakness, rather than merely
    // exercising an ordinary case-sensitive database collation.
    let equal = db
        .query(
            "SELECT name='Alice' AS equal FROM public.rowboat_core_ci_rows",
            10,
        )
        .await?;
    assert_eq!(equal[0].rows[0][0], text("t"));
    assert!(
        db.apply_changes(
            &table,
            &[RowChange::Update {
                original: stale.clone(),
                values: vec![("name".into(), text("wrong"))]
            }]
        )
        .await
        .is_err()
    );
    assert!(
        db.apply_changes(
            &table,
            &[RowChange::Delete {
                original: stale.clone()
            }]
        )
        .await
        .is_err()
    );
    assert!(
        db.apply_changes(
            &table,
            &[
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text("2")),
                        ("name".into(), text("rolled back"))
                    ]
                },
                RowChange::Update {
                    original: stale,
                    values: vec![("name".into(), text("wrong"))]
                },
            ]
        )
        .await
        .is_err()
    );
    let fresh = db
        .browse(
            &table,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(fresh.rows.len(), 1);
    assert_eq!(fresh.rows[0][1], text("alice"));
    assert_eq!(
        db.apply_changes(
            &table,
            &[RowChange::Update {
                original: original(&fresh),
                values: vec![("name".into(), text("current"))]
            }]
        )
        .await?,
        1
    );
    db.query(
        "DROP TABLE public.rowboat_core_ci_rows; DROP COLLATION public.rowboat_core_ci",
        10,
    )
    .await?;
    Ok(())
}

async fn readonly_and_state(engine: Engine) -> Result<()> {
    let profile = profile(engine);
    let writer = Database::connect(&profile, "rowboat-test").await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: "rowboat_core_ro_rows".into(),
    };
    let name = sql::qualified_table(engine, &table);
    writer.query(&format!("DROP TABLE IF EXISTS {name}; CREATE TABLE {name}(id integer PRIMARY KEY, note varchar(40)); INSERT INTO {name} VALUES (1,'unchanged')"),10).await?;
    let db = Database::connect(
        &ConnectionProfile {
            read_only: true,
            ..profile
        },
        "rowboat-test",
    )
    .await?;
    let insert = format!("INSERT INTO {name} VALUES (2,'bypass')");
    let attempts = match engine {
        Engine::Sqlite => unreachable!("network fixture test"),
        Engine::Postgres => vec![
            "START TRANSACTION READ WRITE; CREATE TABLE public.rowboat_core_ro_escaped(id integer)"
                .to_string(),
            format!("BEGIN TRANSACTION READ WRITE; {insert}"),
            format!("SET default_transaction_read_only = off; {insert}"),
            format!("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE; {insert}"),
            format!("SET transaction_read_only = off; {insert}"),
            format!("RESET default_transaction_read_only; {insert}"),
            format!("RESET ALL; {insert}"),
            format!("DISCARD ALL; {insert}"),
            format!(
                "SELECT pg_catalog.\"set_config\"('default_transaction_read_only','off',false); {insert}"
            ),
            format!("WITH written AS ({insert} RETURNING *) SELECT * FROM written"),
            format!("EXPLAIN ANALYZE {insert}"),
        ],
        Engine::MySql => vec![
            "START TRANSACTION READ WRITE; CREATE TABLE rowboat.rowboat_core_ro_escaped(id integer)"
                .into(),
            format!("BEGIN READ WRITE; {insert}"),
            format!("BEGIN; SET SESSION TRANSACTION READ WRITE; COMMIT; {insert}"),
            format!("SET SESSION TRANSACTION READ WRITE; {insert}"),
            format!("SET @@session.transaction_read_only = OFF; {insert}"),
            format!("SET SESSION transaction_read_only=0; {insert}"),
            format!("SET autocommit=1; {insert}"),
            format!("/*!80000 SET SESSION TRANSACTION READ WRITE */; {insert}"),
            format!("RESET CONNECTION; {insert}"),
        ],
    };
    for attempt in attempts {
        assert!(
            db.query(&attempt, 10).await.is_err(),
            "read-only bypass accepted: {attempt}"
        );
        assert!(
            !db.in_transaction().await?,
            "failed standalone read must clean up: {attempt}"
        );
        assert!(db.query(&insert, 10).await.is_err());
        let setting = if engine == Engine::Postgres {
            "SHOW transaction_read_only"
        } else {
            "SELECT @@session.transaction_read_only"
        };
        let result = db.query(setting, 10).await?;
        assert_eq!(
            result[0].rows[0][0],
            text(if engine == Engine::Postgres {
                "on"
            } else {
                "1"
            })
        );
        assert!(!db.in_transaction().await?);
    }
    // Forbidden words in literal/comment tokens are ordinary read-only SQL.
    db.query(
        "/* SET transaction_read_only=off */ SELECT 'BEGIN READ WRITE; RESET ALL'",
        10,
    )
    .await?;
    for begin in ["BEGIN", "START TRANSACTION", "START TRANSACTION READ ONLY"] {
        db.query(begin, 10).await?;
        assert!(db.in_transaction().await?);
        db.query("SAVEPOINT rowboat_readonly_point; ROLLBACK TO SAVEPOINT rowboat_readonly_point; RELEASE SAVEPOINT rowboat_readonly_point",10).await?;
        let point = sql::quote_ident(engine, "Mixed; COMMIT; \"quoted`name");
        db.query(
            &format!("SAVEPOINT {point}; ROLLBACK TO {point}; RELEASE SAVEPOINT {point}"),
            10,
        )
        .await?;
        assert!(db.in_transaction().await?);
        assert!(db.query("SET TRANSACTION READ WRITE", 10).await.is_err());
        assert!(db.in_transaction().await?);
        assert!(db.query(&insert, 10).await.is_err());
        db.query("COMMIT AND CHAIN", 10).await?;
        assert!(db.in_transaction().await?);
        db.query("ROLLBACK", 10).await?;
        assert!(!db.in_transaction().await?);
    }
    db.begin().await?;
    assert!(db.in_transaction().await?);
    assert!(
        db.query("SELECT * FROM rowboat_core_deliberately_missing", 10)
            .await
            .is_err()
    );
    assert!(
        db.in_transaction().await?,
        "failed PG transactions are still active"
    );
    db.rollback().await?;
    assert!(!db.in_transaction().await?);
    db.query("SELECT 1", 10).await?;
    let worker = db.clone();
    let slow = tokio::spawn(async move {
        worker
            .query(
                if engine == Engine::Postgres {
                    "SELECT pg_sleep(30)"
                } else {
                    "SELECT SLEEP(30)"
                },
                10,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    db.cancel().await?;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), slow)
            .await??
            .is_err()
    );
    assert!(
        !db.in_transaction().await?,
        "cancelled owned read-only scope must rollback"
    );
    db.query("SELECT 1", 10).await?;

    // Test server-side enforcement even when the outer statement is a permitted
    // SELECT and a function attempts to change transaction mode internally.
    if engine == Engine::Postgres {
        writer.query(&format!("CREATE OR REPLACE FUNCTION public.rowboat_core_ro_write() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN PERFORM set_config('transaction_read_only','off',true); {insert}; RETURN 1; END $$"),10).await?;
        assert!(
            db.query("SELECT public.rowboat_core_ro_write()", 10)
                .await
                .is_err()
        );
        assert!(!db.in_transaction().await?);
        writer
            .query("DROP FUNCTION public.rowboat_core_ro_write()", 10)
            .await?;
        writer.query("CREATE OR REPLACE FUNCTION public.rowboat_core_ro_settings() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN PERFORM set_config('default_transaction_read_only','off',false); PERFORM set_config('standard_conforming_strings','off',false); RETURN 1; END $$",10).await?;
        db.query(
            "BEGIN; SELECT public.rowboat_core_ro_settings(); SELECT 'one\\'; SELECT 2; COMMIT",
            10,
        )
        .await?;
        assert!(!db.in_transaction().await?);
        let result = db
            .query(
                "SHOW transaction_read_only; SHOW standard_conforming_strings",
                10,
            )
            .await?;
        assert_eq!(result[0].rows[0][0], text("on"));
        assert_eq!(result[1].rows[0][0], text("on"));
        db.begin().await?;
        db.query("SELECT public.rowboat_core_ro_settings()", 10)
            .await?;
        db.commit().await?;
        assert_eq!(
            db.query("SHOW transaction_read_only", 10).await?[0].rows[0][0],
            text("on")
        );
        writer
            .query("DROP FUNCTION public.rowboat_core_ro_settings()", 10)
            .await?;
    }
    let rows = writer
        .browse(
            &table,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(rows.rows, vec![vec![text("1"), text("unchanged")]]);
    assert!(
        !writer
            .catalog()
            .await?
            .iter()
            .any(|t| t.table.name == "rowboat_core_ro_escaped")
    );

    // The GUI can also resync writable editor SQL, errors, and MySQL implicit DDL commits.
    assert!(!writer.in_transaction().await?);
    writer.query("BEGIN", 10).await?;
    assert!(writer.in_transaction().await?);
    assert!(
        writer
            .query("SELECT * FROM rowboat_core_deliberately_missing", 10)
            .await
            .is_err()
    );
    assert!(writer.in_transaction().await?);
    writer.query("ROLLBACK", 10).await?;
    assert!(!writer.in_transaction().await?);
    writer.query("BEGIN", 10).await?;
    assert!(
        writer
            .query("SELECT * FROM rowboat_core_deliberately_missing", 10)
            .await
            .is_err()
    );
    writer.query("COMMIT", 10).await?;
    assert!(!writer.in_transaction().await?);
    writer.query("START TRANSACTION", 10).await?;
    writer
        .query(&format!("ALTER TABLE {name} ADD COLUMN extra integer"), 10)
        .await?;
    assert_eq!(writer.in_transaction().await?, engine == Engine::Postgres);
    writer.rollback().await?;
    writer.query(&format!("DROP TABLE {name}"), 10).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires explicit task-owned PostgreSQL Podman fixture on port 15435"]
async fn postgres_readonly_controls_and_transaction_state() -> Result<()> {
    readonly_and_state(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires explicit task-owned MySQL Podman fixture on port 13309"]
async fn mysql_readonly_controls_and_transaction_state() -> Result<()> {
    readonly_and_state(Engine::MySql).await
}

async fn hidden_comment_batches(engine: Engine) -> Result<()> {
    let profile = profile(engine);
    let writer = Database::connect(&profile, "rowboat-test").await?;
    let db = Database::connect(
        &ConnectionProfile {
            read_only: true,
            ..profile
        },
        "rowboat-test",
    )
    .await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: "rowboat_core_comment_escape".into(),
    };
    let name = sql::qualified_table(engine, &table);
    writer
        .query(&format!("DROP TABLE IF EXISTS {name}"), 10)
        .await?;
    let markers = if engine == Engine::Postgres {
        vec!["-- hidden", "--nospace '"]
    } else {
        vec![
            "# hidden '",
            "-- hidden '",
            "--\t'",
            "--\x01'",
            "--\x0b'",
            "--\x0c'",
            "--\x1f'",
            "--\x7f'",
        ]
    };
    for marker in markers {
        for newline in ["\r", "\n", "\r\n"] {
            // Independently confirm the native server's comment boundaries using
            // harmless raw SQL, then compare the policy path with that behavior.
            let harmless = format!("SELECT 1 {marker}{newline}; SELECT 2");
            let expected = if engine == Engine::MySql && newline == "\r" {
                1
            } else {
                2
            };
            assert_eq!(
                writer.query(&harmless, 10).await?.len(),
                expected,
                "server: {harmless:?}"
            );
            assert_eq!(
                db.query(&harmless, 10).await?.len(),
                expected,
                "policy: {harmless:?}"
            );
            let payload = format!(
                "SELECT 1 {marker}{newline}; COMMIT; START TRANSACTION READ WRITE; CREATE TABLE {name}(id integer); COMMIT"
            );
            db.begin().await?;
            let outcome = db.query(&payload, 10).await;
            let active = db.in_transaction().await?;
            let escaped = writer.catalog().await?.iter().any(|t| t.table == table);
            // Clean up our disposable test object even when a regression exists.
            db.rollback().await?;
            writer
                .query(&format!("DROP TABLE IF EXISTS {name}"), 10)
                .await?;
            if expected == 2 {
                let error = outcome.expect_err(&format!("hidden batch accepted: {payload:?}"));
                assert!(
                    format!("{error:#}").contains("Read-only editor"),
                    "must reject the whole batch during policy validation: {error:#}"
                );
            } else {
                // A bare CR is still inside a MySQL line comment. It must not
                // cause commented-out commands to execute as new statements.
                assert_eq!(outcome?.len(), 1);
            }
            assert!(active, "hidden COMMIT must never execute: {payload:?}");
            assert!(!escaped, "hidden CREATE must never execute: {payload:?}");
        }
    }
    if engine == Engine::Postgres {
        for gap in ["\r", "\n", "\r\n", " -- continuation\r"] {
            let sql =
                format!("SELECT E'a'{gap}'\\'; COMMIT; START TRANSACTION READ WRITE;--'; SELECT 2");
            let native = writer.query(&sql, 10).await?;
            let guarded = db.query(&sql, 10).await?;
            assert_eq!(native.len(), 2);
            assert_eq!(guarded[0].rows, native[0].rows, "{sql:?}");
            assert_eq!(guarded.len(), 2);
        }
    } else {
        let sql = "SELECT 4--2; SELECT 7";
        assert_eq!(
            db.query(sql, 10).await?[0].rows,
            writer.query(sql, 10).await?[0].rows
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Requires explicit task-owned PostgreSQL Podman fixture on port 15435"]
async fn postgres_hidden_cr_batch_is_rejected_before_execution() -> Result<()> {
    hidden_comment_batches(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires explicit task-owned MySQL Podman fixture on port 13309"]
async fn mysql_line_comment_boundaries_match_server() -> Result<()> {
    hidden_comment_batches(Engine::MySql).await
}
