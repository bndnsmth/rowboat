//! Explicit disposable Rowboat fixtures only; parent owns container lifecycle.
use anyhow::{Context, Result, ensure};
use rowboat_admin::metrics::{CustomMetricState, sample_custom};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, sql::quote_ident};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let raw = db.query(sql, 1).await?;
    match raw
        .first()
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
    {
        Some(Cell::Text(text)) => Ok(text.clone()),
        other => anyhow::bail!("Missing text scalar: {other:?}"),
    }
}

fn namespace() -> String {
    format!(
        "pass6_metrics_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn object(engine: Engine, name: &str) -> String {
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
        quote_ident(engine, name)
    )
}

async fn exact_value(db: &Database, sql: &str, expected: &str) -> Result<()> {
    let sample = sample_custom(db, sql).await?;
    ensure!(sample.sampled_at_unix_ms > 0, "missing sample timestamp");
    ensure!(
        sample.state
            == CustomMetricState::Value {
                decimal: expected.into()
            },
        "unexpected sample: {sample:?}"
    );
    Ok(())
}

async fn values_and_no_replay(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let observer = connect(engine).await?;
    exact_value(
        &db,
        "SELECT CAST(9007199254740993.000000000001 AS DECIMAL(30,12))",
        "9007199254740993.000000000001",
    )
    .await?;
    exact_value(&db, "SELECT -42", "-42").await?;
    ensure!(
        sample_custom(&db, "SELECT 1 WHERE 1=0").await?.state == CustomMetricState::Empty,
        "empty result was not explicit"
    );
    for sql in [
        "SELECT NULL",
        "SELECT 'NaN'",
        "SELECT 'not numeric'",
        "SELECT 1 UNION ALL SELECT 2",
    ] {
        ensure!(
            matches!(
                sample_custom(&db, sql).await?.state,
                CustomMetricState::Invalid { .. }
            ),
            "accepted invalid scalar: {sql}"
        );
    }
    for sql in [
        "SELECT 1,2",
        "SELECT REPEAT('1',70000)",
        "SELECT 1; SELECT 2",
        "SHOW TABLES",
        "SELECT 1 INTO outfile '/tmp/rowboat-pass6-forbidden'",
        "SELECT 1 FOR UPDATE",
    ] {
        ensure!(
            sample_custom(&db, sql).await.is_err(),
            "accepted unsafe/unbounded sample: {sql}"
        );
    }
    if engine == Engine::MySql {
        for sql in [
            "SELECT 1 /*!50000 INTO OUTFILE '/tmp/rowboat-pass6-forbidden' */",
            "SELECT 1; /*!50000 COMMIT */",
        ] {
            ensure!(
                sample_custom(&db, sql).await.is_err(),
                "executable comment accepted"
            );
        }
    }
    exact_value(&db, "SELECT 7", "7").await?;
    ensure!(!db.in_transaction().await?, "sample leaked a transaction");

    // Session advisory locks are intentionally outside transaction rollback.
    // Their reference count detects a hidden execute/retry even if the scalar
    // is identical. Unique lock IDs/names never interfere with another test.
    let lock = namespace();
    let key =
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64 & 0x3fff_ffff_ffff_ffff;
    let (acquire, release, available) = if engine == Engine::Postgres {
        (
            format!("SELECT CASE WHEN pg_try_advisory_lock({key}) THEN 1 ELSE 0 END"),
            format!("SELECT pg_advisory_unlock({key})"),
            format!("SELECT pg_try_advisory_lock({key})"),
        )
    } else {
        (
            format!("SELECT GET_LOCK('{lock}',0)"),
            format!("SELECT RELEASE_LOCK('{lock}')"),
            format!("SELECT IS_FREE_LOCK('{lock}')"),
        )
    };
    let result = async {
        exact_value(&db, &acquire, "1").await?;
        let yes = if engine == Engine::Postgres { "t" } else { "1" };
        ensure!(
            scalar(&db, &release).await? == yes,
            "sample did not acquire lock"
        );
        ensure!(
            scalar(&observer, &available).await? == yes,
            "sample was replayed (reference count >1)"
        );
        if engine == Engine::Postgres {
            scalar(&observer, &release).await?;
        }
        let invalid = if engine == Engine::Postgres {
            format!("SELECT CASE WHEN pg_try_advisory_lock({key}) THEN 'NaN' ELSE '0' END")
        } else {
            format!("SELECT IF(GET_LOCK('{lock}',0)=1,'NaN','0')")
        };
        ensure!(
            matches!(
                sample_custom(&db, &invalid).await?.state,
                CustomMetricState::Invalid { .. }
            ),
            "expected invalid value"
        );
        ensure!(
            scalar(&db, &release).await? == yes,
            "invalid sample did not execute"
        );
        ensure!(
            scalar(&observer, &available).await? == yes,
            "invalid sample was automatically replayed"
        );
        if engine == Engine::Postgres {
            scalar(&observer, &release).await?;
        }
        Ok(())
    }
    .await;
    if engine == Engine::Postgres {
        db.query("SELECT pg_advisory_unlock_all()", 0).await?;
        observer.query("SELECT pg_advisory_unlock_all()", 0).await?;
    } else {
        db.query("SELECT RELEASE_ALL_LOCKS()", 0).await?;
    }
    result
}

async fn server_readonly_and_transactions(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let observer = connect(engine).await?;
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
    let ns = namespace();
    let table = object(engine, &ns);
    let function = object(engine, &format!("{ns}_f"));
    db.query(
        &format!("CREATE TABLE {table}(n integer); INSERT INTO {table} VALUES(0)"),
        0,
    )
    .await?;
    let result = async {
        let create = if engine == Engine::Postgres {
            format!("CREATE FUNCTION {function}() RETURNS integer LANGUAGE plpgsql VOLATILE AS $$ BEGIN UPDATE {table} SET n=n+1; RETURN 1; END $$")
        } else {
            format!("CREATE FUNCTION {function}() RETURNS integer DETERMINISTIC MODIFIES SQL DATA BEGIN UPDATE {table} SET n=n+1; RETURN 1; END")
        };
        ddl.query(&create, 0).await?;
        let error = sample_custom(&db, &format!("SELECT {function}()")).await.unwrap_err();
        ensure!(format!("{error:#}").to_ascii_lowercase().contains("read"), "not a server readonly failure: {error:#}");
        ensure!(scalar(&observer, &format!("SELECT n FROM {table}")).await? == "0", "server readonly backstop failed");
        ensure!(!db.in_transaction().await?, "failed function leaked owned transaction");
        exact_value(&db, "SELECT 2", "2").await?;
        ensure!(sample_custom(&db, "SELECT no_such_pass6_column").await.is_err(), "invalid SQL accepted");
        exact_value(&db, "SELECT 3", "3").await?;

        db.begin().await?;
        db.query(&format!("UPDATE {table} SET n=9"), 0).await?;
        ensure!(sample_custom(&db, "SELECT 1").await.is_err(), "sample adopted caller transaction");
        ensure!(db.in_transaction().await?, "sample ended caller transaction");
        ensure!(scalar(&db, &format!("SELECT n FROM {table}")).await? == "9", "caller writes rolled back");
        ensure!(scalar(&observer, &format!("SELECT n FROM {table}")).await? == "0", "caller writes committed");
        if engine == Engine::Postgres {
            ensure!(db.query("SELECT 1/0", 1).await.is_err());
            ensure!(sample_custom(&db, "SELECT 1").await.is_err(), "sample adopted failed transaction");
            ensure!(db.in_transaction().await?, "sample ended failed transaction");
        }
        db.rollback().await?;
        if engine == Engine::MySql {
            db.query("SET autocommit=0", 0).await?;
            ensure!(sample_custom(&db, "SELECT 1").await.is_err(), "sample allowed autocommit=0");
            db.query("ROLLBACK; SET autocommit=1", 0).await?;
        }
        let readonly = Database::connect(&ConnectionProfile { read_only: true, ..profile(engine) }, "rowboat-test").await?;
        exact_value(&readonly, "SELECT 4", "4").await?;
        // A sampling guard must restore the writable profile's original defaults.
        db.query(&format!("UPDATE {table} SET n=5"), 0).await?;
        ensure!(scalar(&observer, &format!("SELECT n FROM {table}")).await? == "5", "sampling changed session defaults");
        Ok(())
    }.await;
    let _ = db.rollback().await;
    if engine == Engine::MySql {
        let _ = db.query("SET autocommit=1", 0).await;
    }
    ddl.query(
        &format!(
            "DROP FUNCTION IF EXISTS {function}{}",
            if engine == Engine::Postgres { "()" } else { "" }
        ),
        0,
    )
    .await?;
    db.query(&format!("DROP TABLE {table}"), 0).await?;
    result
}

async fn wait_sleep(observer: &Database, id: &str) -> Result<()> {
    let sql = if observer.engine() == Engine::Postgres {
        format!(
            "SELECT count(*) FROM pg_stat_activity WHERE pid={id} AND state='active' AND query LIKE '%pass6_metrics_sleep%'"
        )
    } else {
        format!(
            "SELECT count(*) FROM information_schema.processlist WHERE id={id} AND info LIKE '%pass6_metrics_sleep%'"
        )
    };
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if scalar(observer, &sql).await? == "1" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("Sleep query did not start")
}

async fn busy_cancel_drop_timeout(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let observer = connect(engine).await?;
    let id = scalar(
        &db,
        if engine == Engine::Postgres {
            "SELECT pg_backend_pid()"
        } else {
            "SELECT CONNECTION_ID()"
        },
    )
    .await?;
    let sleep = if engine == Engine::Postgres {
        "SELECT 7 FROM pg_sleep(30) /* pass6_metrics_sleep */"
    } else {
        "SELECT IF(SLEEP(30)=0,7,7) /* pass6_metrics_sleep */"
    };
    let busy_db = db.clone();
    let busy = tokio::spawn(async move { busy_db.query(sleep, 1).await });
    wait_sleep(&observer, &id).await?;
    let start = Instant::now();
    let error = sample_custom(&db, "SELECT 1").await.unwrap_err();
    ensure!(
        start.elapsed() < Duration::from_secs(1),
        "sample queued behind busy work"
    );
    ensure!(
        format!("{error:#}").to_ascii_lowercase().contains("busy"),
        "unexpected rejection: {error:#}"
    );
    db.cancel().await?;
    ensure!(busy.await?.is_err());

    let sampling_db = db.clone();
    let sampling = tokio::spawn(async move { sample_custom(&sampling_db, sleep).await });
    wait_sleep(&observer, &id).await?;
    db.cancel().await?;
    ensure!(
        tokio::time::timeout(Duration::from_secs(15), sampling)
            .await??
            .is_err(),
        "cancelled sample succeeded"
    );
    ensure!(
        !db.in_transaction().await?,
        "cancellation leaked transaction"
    );
    exact_value(&db, "SELECT 8", "8").await?;

    let sampling_db = db.clone();
    let sampling = tokio::spawn(async move { sample_custom(&sampling_db, sleep).await });
    wait_sleep(&observer, &id).await?;
    sampling.abort();
    ensure!(sampling.await.unwrap_err().is_cancelled());
    ensure!(
        !tokio::time::timeout(Duration::from_secs(15), db.in_transaction()).await??,
        "dropped future leaked transaction"
    );
    exact_value(&db, "SELECT 9", "9").await?;

    let start = Instant::now();
    let error = sample_custom(&db, sleep).await.unwrap_err();
    ensure!(
        start.elapsed() < Duration::from_secs(15),
        "deadline did not bound sampling"
    );
    let message = format!("{error:#}").to_ascii_lowercase();
    ensure!(
        message.contains("tim") || message.contains("five seconds"),
        "unexpected timeout error: {error:#}"
    );
    ensure!(!db.in_transaction().await?, "timeout leaked transaction");
    exact_value(&db, "SELECT 10", "10")
        .await
        .context("reuse after deadline")?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit Rowboat PostgreSQL fixture on 15435"]
async fn postgres_values_bounds_and_exactly_once() -> Result<()> {
    values_and_no_replay(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires explicit Rowboat MySQL fixture on 13309"]
async fn mysql_values_bounds_and_exactly_once() -> Result<()> {
    values_and_no_replay(Engine::MySql).await
}
#[tokio::test]
#[ignore = "requires explicit Rowboat PostgreSQL fixture on 15435"]
async fn postgres_server_readonly_and_caller_transactions() -> Result<()> {
    server_readonly_and_transactions(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires explicit Rowboat MySQL fixture on 13309"]
async fn mysql_server_readonly_and_caller_transactions() -> Result<()> {
    server_readonly_and_transactions(Engine::MySql).await
}
#[tokio::test]
#[ignore = "requires explicit Rowboat PostgreSQL fixture on 15435"]
async fn postgres_busy_cancel_drop_and_deadline() -> Result<()> {
    busy_cancel_drop_timeout(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires explicit Rowboat MySQL fixture on 13309"]
async fn mysql_busy_cancel_drop_and_deadline() -> Result<()> {
    busy_cancel_drop_timeout(Engine::MySql).await
}
