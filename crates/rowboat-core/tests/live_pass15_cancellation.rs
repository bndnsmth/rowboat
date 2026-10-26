//! Explicit disposable Podman fixtures only; parent owns server lifecycle.
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, QueryCancellation, QueryEvent, QueryResult,
    QueryStream, QueryStreamOptions, RowChange,
};
use std::time::Duration;
use tokio::time::{sleep, timeout};

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::MySql {
            13309
        } else {
            15435
        },
        username: if engine == Engine::MySql {
            "rowboat"
        } else {
            "postgres"
        }
        .into(),
        database: "rowboat".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
async fn connect(engine: Engine) -> Result<Database> {
    Database::connect(&profile(engine), "rowboat-test").await
}
fn disposable() -> QueryStreamOptions {
    QueryStreamOptions {
        cancellation: QueryCancellation::RetireSession,
        channel_capacity: 8,
        ..Default::default()
    }
}
fn text(value: impl ToString) -> Cell {
    Cell::Text(value.to_string())
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let results = db.query(sql, 1).await?;
    match &results[0].rows[0][0] {
        Cell::Text(value) => Ok(value.clone()),
        other => bail!("Unexpected scalar {other:?}"),
    }
}
async fn backend(db: &Database) -> Result<String> {
    scalar(
        db,
        if db.engine() == Engine::MySql {
            "SELECT CONNECTION_ID()"
        } else {
            "SELECT pg_backend_pid()"
        },
    )
    .await
}
async fn wait_active(observer: &Database, id: &str, marker: &str) -> Result<()> {
    let sql = if observer.engine() == Engine::MySql {
        format!(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID={id} AND STATE='User sleep' AND INFO LIKE '%{marker}%'"
        )
    } else {
        format!(
            "SELECT COUNT(*) FROM pg_stat_activity WHERE pid={id} AND state='active' AND wait_event='PgSleep' AND query LIKE '%{marker}%'"
        )
    };
    timeout(Duration::from_secs(5), async {
        loop {
            if scalar(observer, &sql).await? == "1" {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("target statement never became active")??;
    Ok(())
}
async fn cancelled_terminal(stream: &mut QueryStream) -> Result<()> {
    timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.recv().await {
            match event {
                QueryEvent::Error { message, cancelled } => {
                    ensure!(
                        cancelled && message.contains("session retired"),
                        "{message}"
                    );
                    ensure!(!message.contains("could not be confirmed"), "{message}");
                    return Ok(());
                }
                QueryEvent::Complete { .. } => bail!("cancelled query reported success"),
                _ => {}
            }
        }
        bail!("missing terminal error")
    })
    .await?
}

async fn batch_cancellation(engine: Engine) -> Result<()> {
    let observer = connect(engine).await?;
    let table = format!("p15_cancel_{}", uuid::Uuid::new_v4().simple());
    observer
        .query(
            &format!("CREATE TABLE {table} (id INT PRIMARY KEY, value INT)"),
            0,
        )
        .await?;
    let result = async {
        // Exercise explicit stream cancellation, Database cancellation (which
        // must not issue KILL QUERY first), and receiver loss.
        for mode in 0..3 {
            observer.query(&format!("DELETE FROM {table}; INSERT INTO {table} VALUES (0,0)"), 0).await?;
            let db = connect(engine).await?;
            let id = backend(&db).await?;
            let (mut capture_stream, capture) = db.query_stream_with_edit_source(
                &format!("SELECT id, value FROM {table}"), disposable(),
            ).await?;
            let mut captured = QueryResult::default();
            while let Some(event) = capture_stream.recv().await {
                match event {
                    QueryEvent::Schema { columns, .. } => captured.columns = columns,
                    QueryEvent::Rows { rows, .. } => captured.rows.extend(rows),
                    QueryEvent::ResultEnd { rows_affected, elapsed_ms, truncated, .. } => {
                        captured.rows_affected = rows_affected;
                        captured.elapsed_ms = elapsed_ms;
                        captured.truncated = truncated;
                    }
                    QueryEvent::Error { message, .. } => bail!(message),
                    _ => {}
                }
            }
            let source = capture.finish().await;
            let plan = if engine == Engine::MySql && source.read_only_reason().is_some_and(|reason| reason.contains("PROCESS privilege")) {
                // The ordinary fixture account can kill its own connections but
                // cannot obtain the adapter's privileged editing evidence.
                None
            } else {
                Some(source.plan(&captured)?)
            };
            let marker = format!("{table}_sleep_{mode}");
            let delay = if engine == Engine::MySql { "SLEEP(30)" } else { "pg_sleep(30)" };
            // Mode zero preserves the independent reproducer's exact autocommit
            // boundary. Other modes additionally prove rollback on retirement.
            let open_transaction = if mode == 0 { String::new() } else {
                format!("BEGIN; INSERT INTO {table} VALUES (2,2);")
            };
            let end_transaction = if mode == 0 { "" } else { "; COMMIT" };
            let mut stream = db.query_stream(&format!(
                "BEGIN; INSERT INTO {table} VALUES (1,1); COMMIT; {open_transaction} SELECT {delay} /* {marker} */; INSERT INTO {table} VALUES (3,3){end_transaction}"
            ), disposable()).await?;
            wait_active(&observer, &id, &marker).await?;
            ensure!(scalar(&observer, &format!("SELECT COUNT(*) FROM {table} WHERE id=1")).await? == "1", "earlier commit not independently visible");

            // This command queues behind the exact session being terminated.
            let mut queued = db.query_stream(&format!("INSERT INTO {table} VALUES (4,4)"), disposable()).await?;
            match mode {
                0 => { stream.cancel(); cancelled_terminal(&mut stream).await?; }
                1 => { db.cancel().await?; cancelled_terminal(&mut stream).await?; }
                _ => drop(stream),
            }
            timeout(Duration::from_secs(5), async {
                match queued.recv().await {
                    Some(QueryEvent::Error { message, .. }) => ensure!(message.contains("Connection closed"), "{message}"),
                    other => bail!("queued peer must safely fail, got {other:?}"),
                }
                Ok::<_, anyhow::Error>(())
            }).await??;
            ensure!(db.in_transaction().await.is_err(), "retired session advertised transaction state");
            ensure!(db.query("SELECT 1", 1).await.is_err(), "retired session reused");
            if let Some(plan) = plan {
            ensure!(db.apply_query_changes(&plan, &[RowChange::Update {
                original: vec![("id".into(), text(0)), ("value".into(), text(0))],
                values: vec![("value".into(), text(99))],
            }]).await.is_err(), "edit proof survived session retirement");
            }
            // Wait for native rollback/session disappearance, independently of
            // the worker terminal notification.
            let absent = if engine == Engine::MySql {
                format!("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID={id}")
            } else {
                format!("SELECT COUNT(*) FROM pg_stat_activity WHERE pid={id}")
            };
            timeout(Duration::from_secs(5), async {
                while scalar(&observer, &absent).await? != "0" {
                    sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            }).await??;
            let rows = observer.query(&format!("SELECT id,value FROM {table} ORDER BY id"), 10).await?;
            ensure!(rows[0].rows == vec![vec![text(0),text(0)],vec![text(1),text(1)]], "following/queued write executed or transaction survived: {:?}", rows[0].rows);
            ensure!(scalar(&observer, "SELECT 42").await? == "42", "independent session affected");
        }
        Ok(())
    }.await;
    observer.query(&format!("DROP TABLE {table}"), 0).await?;
    result
}

#[tokio::test]
#[ignore = "approved MySQL13309 rowboat Podman fixture"]
async fn mysql_disposable_batch_stops_following_writes_and_retires_exact_session() -> Result<()> {
    batch_cancellation(Engine::MySql).await
}

#[tokio::test]
#[ignore = "approved PostgreSQL15435 rowboat Podman fixture"]
async fn postgres_disposable_batch_compatibility() -> Result<()> {
    batch_cancellation(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "approved MySQL13309/PostgreSQL15435 Podman fixtures"]
async fn cancelled_queued_disposable_stream_never_terminates_active_peer() -> Result<()> {
    for engine in [Engine::MySql, Engine::Postgres] {
        let db = connect(engine).await?;
        let observer = connect(engine).await?;
        let id = backend(&db).await?;
        let marker = format!("p15_queued_{}", uuid::Uuid::new_v4().simple());
        let delay = if engine == Engine::MySql {
            "SLEEP(0.5)"
        } else {
            "pg_sleep(0.5)"
        };
        let mut active = db
            .query_stream(&format!("SELECT {delay} /* {marker} */"), disposable())
            .await?;
        wait_active(&observer, &id, &marker).await?;
        let queued = db.query_stream("SELECT 9", disposable()).await?;
        timeout(Duration::from_secs(2), queued.close()).await??;
        let mut completed = false;
        while let Some(event) = timeout(Duration::from_secs(3), active.recv()).await? {
            match event {
                QueryEvent::Complete { .. } => completed = true,
                QueryEvent::Error { message, .. } => bail!(message),
                _ => {}
            }
        }
        ensure!(completed);
        ensure!(
            backend(&db).await? == id,
            "successful session unexpectedly retired"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "approved MySQL13309/PostgreSQL15435 Podman fixtures"]
async fn default_cancellation_preserves_session_and_disposable_close_reports_retirement()
-> Result<()> {
    for engine in [Engine::MySql, Engine::Postgres] {
        let db = connect(engine).await?;
        let observer = connect(engine).await?;
        let id = backend(&db).await?;
        for retire in [false, true] {
            let marker = format!("p15_close_{}", uuid::Uuid::new_v4().simple());
            let delay = if engine == Engine::MySql {
                "SLEEP(30)"
            } else {
                "pg_sleep(30)"
            };
            let stream = db
                .query_stream(
                    &format!("SELECT {delay} /* {marker} */"),
                    if retire {
                        disposable()
                    } else {
                        QueryStreamOptions::default()
                    },
                )
                .await?;
            wait_active(&observer, &id, &marker).await?;
            let result = timeout(Duration::from_secs(5), stream.close()).await?;
            if retire {
                ensure!(result.is_err(), "close concealed non-reusable session");
                ensure!(db.in_transaction().await.is_err());
            } else {
                result?;
                ensure!(backend(&db).await? == id, "default session not reusable");
                ensure!(!db.in_transaction().await?);
            }
        }
    }
    Ok(())
}
