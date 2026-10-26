//! Only task-owned base Podman fixtures; no lifecycle or global setting changes.
use anyhow::{Context, Result, ensure};
use rowboat_admin::explain::{self, AnalyzeIntent, ExplainReport, RawFormat, ReportMode};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, sql::quote_ident};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
async fn connect(engine: Engine) -> Result<Database> {
    Database::connect(&profile(engine), "rowboat-test").await
}
async fn exec(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 0).await?;
    Ok(())
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let r = db.query(sql, 1).await?;
    let cell = r
        .first()
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
        .context("Missing scalar")?;
    match cell {
        Cell::Text(s) => Ok(s.clone()),
        Cell::Binary(b) => Ok(String::from_utf8(b.clone())?),
        Cell::Null => anyhow::bail!("NULL scalar"),
    }
}
fn name(engine: Engine, label: &str) -> String {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "{}.{}",
        quote_ident(
            engine,
            if engine == Engine::Postgres {
                "public"
            } else {
                "rowboat"
            }
        ),
        quote_ident(
            engine,
            &format!("pass5_profile_{label}_{}_{id}", std::process::id())
        )
    )
}
async fn analyze(db: &Database, sql: &str) -> Result<ExplainReport> {
    explain::analyze(db, sql, AnalyzeIntent::ExecuteQuery).await
}
fn actual(report: &ExplainReport, rows: &str) -> Result<()> {
    ensure!(report.mode == ReportMode::Analyze, "wrong report mode");
    ensure!(
        report.raw_format
            == if report.engine == Engine::Postgres {
                RawFormat::Json
            } else {
                RawFormat::Tree
            },
        "wrong native format"
    );
    let root = report.roots.first().context("Missing root plan")?;
    // PG18 emits integral row averages as e.g. 3.00; normalization must retain
    // that spelling, while this fixture compares only the small numeric count.
    ensure!(
        root.actual
            .get("Actual Rows")
            .is_some_and(|n| n.0.parse::<f64>().ok() == rows.parse::<f64>().ok()),
        "wrong actual rows: {:?}",
        root.actual
    );
    ensure!(
        root.actual.contains_key("Actual Total Time")
            && root.actual.contains_key("Actual Startup Time"),
        "missing actual timings"
    );
    ensure!(
        root.actual.get("Actual Loops").is_some_and(|n| n.0 == "1"),
        "wrong loop count"
    );
    ensure!(
        root.estimates.contains_key("Plan Rows"),
        "missing estimated rows"
    );
    ensure!(
        !report.raw_report.is_empty(),
        "missing original native report"
    );
    Ok(())
}

async fn actual_once_and_transactions(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    // MySQL binary logging requires SUPER for a data-writing stored function;
    // the fixture's root login is used solely for this unique routine's DDL.
    let ddl = if engine == Engine::MySql {
        Database::connect(
            &ConnectionProfile {
                username: "root".into(),
                ..profile(engine)
            },
            "rowboat-test",
        )
        .await?
    } else {
        db.clone()
    };
    let observer = connect(engine).await?;
    let data = name(engine, "data");
    let marker = name(engine, "marker");
    let function = name(engine, "bump");
    exec(&db, &format!("CREATE TABLE {data}(id integer PRIMARY KEY); INSERT INTO {data} VALUES(1),(2),(3); CREATE TABLE {marker}(n integer); INSERT INTO {marker} VALUES(0)")).await?;
    let result = async {
        let ddl_sql = if engine == Engine::Postgres {
            format!("CREATE FUNCTION {function}(x integer) RETURNS integer LANGUAGE plpgsql VOLATILE AS $$ BEGIN UPDATE {marker} SET n=n+1; RETURN x; END $$")
        } else {
            // The returned value is deterministically x; the separate UPDATE is
            // intentionally observable. Referencing id prevents constant folding.
            format!("CREATE FUNCTION {function}(x integer) RETURNS integer DETERMINISTIC MODIFIES SQL DATA BEGIN UPDATE {marker} SET n=n+1; RETURN x; END")
        };
        exec(&ddl, &ddl_sql).await?;
        let query = format!("SELECT id FROM {data} WHERE {function}(id)>0");
        let plain = explain::execute(&db, &query).await?;
        ensure!(plain.mode == ReportMode::Explain && plain.raw_format == RawFormat::Json, "plain EXPLAIN changed mode");
        ensure!(scalar(&observer, &format!("SELECT n FROM {marker}")).await? == "0", "plain plan unexpectedly executed fixture function");
        let report = analyze(&db, &query).await.context("writable actual query")?;
        actual(&report, "3")?;
        ensure!(scalar(&observer, &format!("SELECT n FROM {marker}")).await? == "3", "Analyze replayed or failed to execute each row exactly once");
        ensure!(!db.in_transaction().await?, "profiling added a transaction");
        if engine == Engine::Postgres {
            ensure!(report.timing.contains_key("Planning Time") && report.timing.contains_key("Execution Time"), "missing report timings");
        } else {
            ensure!(report.raw_json.is_empty() && report.timing.is_empty(), "invented JSON or total timing for TREE");
        }
        actual(&analyze(&db, &format!("WITH x AS (SELECT id FROM {data}) SELECT id FROM x WHERE id>1")).await.context("WITH profile")?, "2")?;

        db.begin().await?;
        exec(&db, &format!("UPDATE {marker} SET n=10")).await?;
        actual(&analyze(&db, &query).await.context("caller transaction profile")?, "3")?;
        ensure!(db.in_transaction().await?, "Analyze ended caller transaction");
        ensure!(scalar(&db, &format!("SELECT n FROM {marker}")).await? == "13", "Analyze reset or rolled back caller state");
        ensure!(scalar(&observer, &format!("SELECT n FROM {marker}")).await? == "3", "Analyze committed caller transaction");
        db.rollback().await?;
        ensure!(scalar(&observer, &format!("SELECT n FROM {marker}")).await? == "3", "caller rollback failed");

        let readonly = Database::connect(&ConnectionProfile { read_only: true, ..profile(engine) }, "rowboat-test").await?;
        actual(&analyze(&readonly, &format!("SELECT id FROM {data}")).await.context("read-only query profile")?, "3")?;
        ensure!(analyze(&readonly, &query).await.is_err(), "read-only profile executed a writing function");
        ensure!(scalar(&observer, &format!("SELECT n FROM {marker}")).await? == "3", "read-only profile changed durable data");
        ensure!(scalar(&readonly, "SELECT 42").await? == "42", "read-only session not reusable after failure");
        ensure!(!readonly.in_transaction().await?, "read-only automatic guard leaked a transaction");

        // Boundary rejection precedes execution, even with an existing transaction.
        db.begin().await?;
        exec(&db, &format!("UPDATE {marker} SET n=20")).await?;
        for ending in ["\r", "\n", "\r\n"] {
            ensure!(analyze(&db, &format!("SELECT 1 -- hidden{ending}; COMMIT; UPDATE {marker} SET n=99")).await.is_err(), "hidden statement accepted");
        }
        ensure!(db.in_transaction().await?, "boundary rejection ended transaction");
        ensure!(scalar(&db, &format!("SELECT n FROM {marker}")).await? == "20", "boundary rejection reset transaction");
        ensure!(scalar(&observer, &format!("SELECT n FROM {marker}")).await? == "3", "hidden COMMIT executed");
        db.rollback().await?;

        // A native error inside an existing PG transaction remains caller-owned,
        // including the failed state; the helper must not auto-reset it.
        db.begin().await?;
        ensure!(analyze(&db, "SELECT * FROM pass5_profile_nonexistent_relation").await.is_err(), "invalid query succeeded");
        ensure!(db.in_transaction().await?, "profile error silently rolled back caller transaction");
        if engine == Engine::Postgres {
            ensure!(db.query("SELECT 42", 1).await.is_err(), "failed transaction was silently reset");
        }
        db.rollback().await?;
        ensure!(scalar(&db, "SELECT 42").await? == "42", "session failed after explicit rollback");
        Ok(())
    }.await;
    let _ = db.rollback().await;
    let drop_function = if engine == Engine::Postgres {
        format!("DROP FUNCTION IF EXISTS {function}(integer)")
    } else {
        format!("DROP FUNCTION IF EXISTS {function}")
    };
    exec(&ddl, &drop_function).await?;
    exec(
        &db,
        &format!("DROP TABLE IF EXISTS {data}; DROP TABLE IF EXISTS {marker}"),
    )
    .await?;
    result
}

async fn cancellation(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let observer = connect(engine).await?;
    let data = name(engine, "cancel");
    exec(
        &db,
        &format!("CREATE TABLE {data}(id integer PRIMARY KEY); INSERT INTO {data} VALUES(1)"),
    )
    .await?;
    let result = async {
        let id = scalar(&db, if engine == Engine::Postgres { "SELECT pg_backend_pid()" } else { "SELECT CONNECTION_ID()" }).await?;
        for caller_transaction in [false, true] {
            if caller_transaction { db.begin().await?; }
            let sql = if engine == Engine::Postgres {
                format!("SELECT id, pg_sleep(20) FROM {data}")
            } else { format!("SELECT id FROM {data} WHERE sleep(20)=0") };
            let worker = db.clone();
            let task = tokio::spawn(async move { analyze(&worker, &sql).await });
            let probe = if engine == Engine::Postgres {
                format!("SELECT count(*) FROM pg_stat_activity WHERE pid={id} AND state='active' AND query LIKE 'EXPLAIN (ANALYZE%'")
            } else { format!("SELECT count(*) FROM information_schema.PROCESSLIST WHERE ID={id} AND INFO LIKE 'EXPLAIN ANALYZE%'") };
            let mut running = false;
            for _ in 0..100 {
                if scalar(&observer, &probe).await? == "1" { running = true; break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Always cancel/join our worker before any assertion/fixture cleanup.
            db.cancel().await?;
            let outcome = tokio::time::timeout(Duration::from_secs(5), task).await??;
            ensure!(running, "did not observe executing profile before cancellation");
            ensure!(outcome.is_err(), "cancelled profile reported success");
            ensure!(db.in_transaction().await? == caller_transaction, "cancellation changed transaction ownership");
            if caller_transaction { db.rollback().await?; }
            ensure!(scalar(&db, "SELECT 42").await? == "42", "cancelled profile left protocol unusable");
        }
        Ok(())
    }.await;
    let _ = db.rollback().await;
    exec(&db, &format!("DROP TABLE IF EXISTS {data}")).await?;
    result
}

#[tokio::test]
#[ignore = "Task-owned PostgreSQL 18 on 15435"]
async fn postgres_profile_actual_once_readonly_and_transaction_guards() -> Result<()> {
    actual_once_and_transactions(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Task-owned MySQL 8.4 on 13309"]
async fn mysql_profile_actual_once_readonly_and_transaction_guards() -> Result<()> {
    actual_once_and_transactions(Engine::MySql).await
}
#[tokio::test]
#[ignore = "Task-owned PostgreSQL 18 on 15435"]
async fn postgres_profile_cancel_and_reuse() -> Result<()> {
    cancellation(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Task-owned MySQL 8.4 on 13309"]
async fn mysql_profile_cancel_and_reuse() -> Result<()> {
    cancellation(Engine::MySql).await
}
