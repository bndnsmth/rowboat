//! Explicit edit refresh must never fall back to running rejected SQL.
//! Network tests use only the task-owned localhost fixtures, never profiles.
use anyhow::{Result, ensure};
use rowboat_core::*;
use std::{path::PathBuf, time::Duration};
use tokio::time::timeout;
#[path = "support/edit_wire_probe.rs"]
mod edit_wire_probe;

#[tokio::test]
async fn sqlite_plain_retention_exceeds_native_proof_cap_and_explicit_editing_stays_bounded()
-> Result<()> {
    let fixture = SqliteFixture::new()?;
    let db = Database::connect(&fixture.profile(), "").await?;
    const PAYLOAD: usize = 768 * 1024;
    const ROWS: usize = 96;
    db.query(&format!("CREATE TABLE large_rows(id INTEGER PRIMARY KEY,payload BLOB); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{ROWS}) INSERT INTO large_rows SELECT i,zeroblob({PAYLOAD}) FROM n"), 0).await?;
    let sql = "SELECT * FROM large_rows ORDER BY id";
    let options = QueryStreamOptions {
        row_limit: None,
        byte_limit: Some(256 * 1024 * 1024),
        ..Default::default()
    };
    for required in [false, true] {
        let (mut stream, capture) = if required {
            let (stream, capture) = db.query_stream_for_editing(sql, options.clone()).await?;
            (stream, Some(capture))
        } else {
            (db.query_stream(sql, options.clone()).await?, None)
        };
        let mut count = 0;
        let mut payload_bytes = 0;
        let mut completed = false;
        while let Some(event) = timeout(Duration::from_secs(15), stream.recv()).await? {
            match event {
                QueryEvent::Rows { rows, .. } => {
                    for row in rows {
                        count += 1;
                        ensure!(row[0].display() == count.to_string());
                        let Cell::Binary(bytes) = &row[1] else {
                            anyhow::bail!("Lost native BLOB value")
                        };
                        ensure!(bytes.len() == PAYLOAD && bytes.iter().all(|b| *b == 0));
                        payload_bytes += bytes.len();
                    }
                }
                QueryEvent::ResultEnd {
                    rows_seen,
                    rows_emitted,
                    truncated,
                    ..
                } => {
                    ensure!(rows_seen == ROWS as u64 && rows_emitted == ROWS as u64 && !truncated);
                }
                QueryEvent::Complete { result_count, .. } => completed = result_count == 1,
                QueryEvent::Error { message, .. } => anyhow::bail!(message),
                _ => {}
            }
        }
        ensure!(completed && count == ROWS && payload_bytes == ROWS * PAYLOAD);
        ensure!(payload_bytes > 64 * 1024 * 1024);
        if !required {
            ensure!(stream.timings().unwrap().source_capture_ms == 0);
        }
        stream.close().await?;
        if let Some(capture) = capture {
            // An API caller may request more rows than edit evidence retains;
            // that disables proof, never silently lowers plain result delivery.
            ensure!(capture.finish().await.read_only_reason().is_some());
        }
        ensure!(!db.in_transaction().await?);
    }
    let capped = plan_with_options(
        &db,
        sql,
        QueryStreamOptions {
            byte_limit: Some(64 * 1024 * 1024),
            ..options
        },
    )
    .await?;
    ensure!(capped.result().truncated);
    ensure!(capped.result().rows.len() == 85);
    ensure!(capped.result().rows[84][0].display() == "85");
    ensure!(!db.in_transaction().await?);
    Ok(())
}

struct SqliteFixture(PathBuf);
impl SqliteFixture {
    fn new() -> Result<Self> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .join(format!("required_edit_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
    fn profile(&self) -> ConnectionProfile {
        ConnectionProfile {
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: self.0.join("main.sqlite"),
                mode: SqliteOpenMode::CreateIfMissing,
            }),
            ..Default::default()
        }
    }
}
impl Drop for SqliteFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn network_profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        database: "rowboat".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 1).await?[0].rows[0][0].display())
}
async fn reject(db: &Database, sql: &str) -> Result<String> {
    reject_with_options(db, sql, Default::default()).await
}
async fn reject_with_options(
    db: &Database,
    sql: &str,
    options: QueryStreamOptions,
) -> Result<String> {
    let (mut stream, capture) = db.query_stream_for_editing(sql, options).await?;
    let mut error = None;
    while let Some(event) = timeout(Duration::from_secs(10), stream.recv()).await? {
        match event {
            QueryEvent::Error { message, .. } => error = Some(message),
            other => anyhow::bail!("Rejected SQL emitted an execution event: {other:?}"),
        }
    }
    let reason = error.ok_or_else(|| anyhow::anyhow!("Missing rejection"))?;
    ensure!(stream.timings().is_some());
    stream.close().await?;
    ensure!(capture.finish().await.read_only_reason().is_some());
    Ok(reason)
}
async fn plan(db: &Database, sql: &str) -> Result<QueryEditPlan> {
    plan_with_options(db, sql, Default::default()).await
}
async fn plan_with_options(
    db: &Database,
    sql: &str,
    options: QueryStreamOptions,
) -> Result<QueryEditPlan> {
    let (mut stream, capture) = db.query_stream_for_editing(sql, options).await?;
    let mut result = QueryResult::default();
    let mut completed = false;
    while let Some(event) = timeout(Duration::from_secs(10), stream.recv()).await? {
        match event {
            QueryEvent::Schema { columns, .. } => result.columns = columns,
            QueryEvent::Rows { rows, .. } => result.rows.extend(rows),
            QueryEvent::ResultEnd {
                rows_affected,
                elapsed_ms,
                truncated,
                ..
            } => {
                result.rows_affected = rows_affected;
                result.elapsed_ms = elapsed_ms;
                result.truncated = truncated;
            }
            QueryEvent::Complete { result_count, .. } => {
                ensure!(result_count == 1);
                completed = true;
            }
            QueryEvent::Error { message, .. } => anyhow::bail!(message),
        }
    }
    ensure!(completed);
    stream.close().await?;
    capture.finish().await.plan(&result)
}
fn update(plan: &QueryEditPlan, value: &str) -> RowChange {
    RowChange::Update {
        original: plan
            .result()
            .columns
            .iter()
            .zip(&plan.result().rows[0])
            .map(|(column, value)| (column.name.clone(), value.clone()))
            .collect(),
        values: vec![("note".into(), Cell::Text(value.into()))],
    }
}
async fn setup(db: &Database) -> Result<()> {
    db.query("CREATE TABLE records(id INTEGER PRIMARY KEY,note VARCHAR(40)); INSERT INTO records VALUES(1,'original'); CREATE TABLE composite(a INTEGER NOT NULL,b INTEGER NOT NULL,note VARCHAR(40),PRIMARY KEY(a,b)); CREATE TABLE no_key(note VARCHAR(40))", 0).await?;
    Ok(())
}
async fn rejections_and_transactions(db: &Database) -> Result<()> {
    for sql in [
        "UPDATE records SET note='wrong'",
        "INSERT INTO records VALUES(2,'wrong')",
        "SELECT * FROM records; DELETE FROM records",
        "WITH x AS (SELECT * FROM records) SELECT * FROM x",
        "SELECT id+1 AS id,note FROM records",
        "SELECT * FROM nonexistent",
        "SELECT note FROM records",
        "SELECT a,note FROM composite",
        "SELECT * FROM no_key",
        "SELECT id AS duplicate,note AS duplicate FROM records",
        "SELECT id,id,note FROM records",
    ] {
        reject(db, sql).await?;
        ensure!(
            !db.in_transaction().await?,
            "Leaked capture transaction for {sql}"
        );
        ensure!(scalar(db, "SELECT COUNT(*) FROM records").await? == "1");
        ensure!(scalar(db, "SELECT note FROM records").await? == "original");
    }
    let captured = plan(db, "SELECT * FROM records").await?;
    ensure!(captured.result().rows.len() == 1);
    ensure!(
        db.apply_query_changes(&captured, &[update(&captured, "edited")])
            .await?
            == 1
    );
    ensure!(
        db.apply_query_changes(&captured, &[update(&captured, "stale")])
            .await
            .is_err()
    );
    ensure!(scalar(db, "SELECT note FROM records").await? == "edited");
    db.query("BEGIN", 0).await?;
    db.query("INSERT INTO records VALUES(3,'caller')", 0)
        .await?;
    for policy in [
        QueryCancellation::PreserveSession,
        QueryCancellation::RetireSession,
    ] {
        let error = reject_with_options(
            db,
            "SELECT * FROM records",
            QueryStreamOptions {
                cancellation: policy,
                ..Default::default()
            },
        )
        .await?;
        ensure!(error.contains("existing transaction"), "{error}");
    }
    ensure!(db.in_transaction().await?);
    ensure!(scalar(db, "SELECT COUNT(*) FROM records").await? == "2");
    db.query("ROLLBACK", 0).await?;
    ensure!(scalar(db, "SELECT COUNT(*) FROM records").await? == "1");
    // Compatibility: the existing best-effort API still executes unsupported SQL.
    let (mut stream, capture) = db
        .query_stream_with_edit_source(
            "UPDATE records SET note='compatibility'",
            Default::default(),
        )
        .await?;
    while let Some(event) = stream.recv().await {
        if let QueryEvent::Error { message, .. } = event {
            anyhow::bail!(message);
        }
    }
    stream.close().await?;
    ensure!(capture.finish().await.read_only_reason().is_some());
    ensure!(scalar(db, "SELECT note FROM records").await? == "compatibility");
    Ok(())
}
async fn cancellation(db: &Database) -> Result<()> {
    for policy in [
        QueryCancellation::PreserveSession,
        QueryCancellation::RetireSession,
    ] {
        let lease = db.administrative_operation(None).await?;
        let (mut stream, capture) = db
            .query_stream_for_editing(
                "SELECT * FROM records",
                QueryStreamOptions {
                    cancellation: policy,
                    ..Default::default()
                },
            )
            .await?;
        stream.cancel();
        ensure!(matches!(
            timeout(Duration::from_secs(10), stream.recv()).await?,
            Some(QueryEvent::Error {
                cancelled: true,
                ..
            })
        ));
        stream.close().await?;
        ensure!(capture.finish().await.read_only_reason().is_some());
        lease.finish().await?;
        ensure!(!db.in_transaction().await?);
        ensure!(scalar(db, "SELECT COUNT(*) FROM records").await? == "1");
    }
    let values = (2..=500)
        .map(|i| format!("({i},'cancel')"))
        .collect::<Vec<_>>()
        .join(",");
    db.query(&format!("INSERT INTO records VALUES {values}"), 0)
        .await?;
    let (mut stream, capture) = db
        .query_stream_for_editing(
            "SELECT * FROM records",
            QueryStreamOptions {
                batch_rows: 1,
                channel_capacity: 1,
                ..Default::default()
            },
        )
        .await?;
    ensure!(matches!(
        stream.recv().await,
        Some(QueryEvent::Schema { .. })
    ));
    stream.cancel();
    let mut cancelled = false;
    while let Some(event) = timeout(Duration::from_secs(15), stream.recv()).await? {
        if let QueryEvent::Error {
            cancelled: flag, ..
        } = event
        {
            cancelled = flag;
        }
    }
    ensure!(cancelled);
    stream.close().await?;
    ensure!(capture.finish().await.read_only_reason().is_some());
    ensure!(!db.in_transaction().await?);
    ensure!(scalar(db, "SELECT COUNT(*) FROM records").await? == "500");
    let (mut stream, capture) = db
        .query_stream_for_editing(
            "SELECT * FROM records",
            QueryStreamOptions {
                batch_rows: 1,
                channel_capacity: 1,
                cancellation: QueryCancellation::RetireSession,
                ..Default::default()
            },
        )
        .await?;
    ensure!(matches!(
        stream.recv().await,
        Some(QueryEvent::Schema { .. })
    ));
    stream.cancel();
    let mut retired = false;
    while let Some(event) = timeout(Duration::from_secs(15), stream.recv()).await? {
        if let QueryEvent::Error { cancelled, message } = event {
            retired = cancelled && message.contains("retired");
        }
    }
    ensure!(retired);
    stream.close().await?;
    ensure!(capture.finish().await.read_only_reason().is_some());
    ensure!(db.query("SELECT 1", 1).await.is_err());
    Ok(())
}

async fn capture_cancellation(
    admin: &Database,
    profile: &ConnectionProfile,
    table: &str,
) -> Result<()> {
    for policy in [
        QueryCancellation::PreserveSession,
        QueryCancellation::RetireSession,
    ] {
        let probe = edit_wire_probe::WireProbe::new(profile.engine).await?;
        let mut profile = profile.clone();
        profile.port = probe.port;
        let db = Database::connect(&profile, "rowboat-test").await?;
        let selected = format!("SELECT * FROM {table}");
        if profile.engine == Engine::Postgres {
            admin
                .query(
                    &format!("BEGIN; LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"),
                    0,
                )
                .await?;
        } else {
            admin
                .query(&format!("LOCK TABLES {table} WRITE"), 0)
                .await?;
        }
        let outcome: Result<()> = async {
            let (mut stream, capture) = db
                .query_stream_for_editing(
                    &selected,
                    QueryStreamOptions {
                        cancellation: policy,
                        ..Default::default()
                    },
                )
                .await?;
            // The owner-held relation/table lock prevents source capture from
            // completing. No result may arrive, nor may original SQL dispatch.
            ensure!(
                timeout(Duration::from_millis(80), stream.recv())
                    .await
                    .is_err()
            );
            ensure!(probe.count(&selected) == 0);
            stream.cancel();
            let mut cancelled = false;
            while let Some(event) = timeout(Duration::from_secs(15), stream.recv()).await? {
                match event {
                    QueryEvent::Error {
                        cancelled: flag, ..
                    } => cancelled = flag,
                    other => anyhow::bail!("Cancelled capture emitted {other:?}"),
                }
            }
            ensure!(cancelled && probe.count(&selected) == 0);
            stream.close().await?;
            ensure!(capture.finish().await.read_only_reason().is_some());
            if policy == QueryCancellation::RetireSession {
                ensure!(db.query("SELECT 1", 1).await.is_err());
            } else {
                ensure!(!db.in_transaction().await?);
                ensure!(scalar(&db, "SELECT 1").await? == "1");
            }
            Ok(())
        }
        .await;
        let cleanup = admin
            .query(
                if profile.engine == Engine::Postgres {
                    "ROLLBACK"
                } else {
                    "UNLOCK TABLES"
                },
                0,
            )
            .await;
        outcome?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_required_capture_rejects_without_execution_and_preserves_transactions() -> Result<()>
{
    let fixture = SqliteFixture::new()?;
    let db = Database::connect(&fixture.profile(), "").await?;
    setup(&db).await?;
    rejections_and_transactions(&db).await?;
    let mut profile = fixture.profile();
    profile.read_only = true;
    profile.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    let readonly = Database::connect(&profile, "").await?;
    ensure!(
        reject(&readonly, "SELECT * FROM records")
            .await?
            .contains("read-only")
    );
    ensure!(scalar(&readonly, "SELECT COUNT(*) FROM records").await? == "1");
    cancellation(&db).await
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 disposable fixtures"]
async fn network_required_capture_guards_lexical_mode_shadowing_and_cancellation() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let profile = network_profile(engine);
        let db = Database::connect(&profile, "rowboat-test").await?;
        let name = format!("required_edit_{}", uuid::Uuid::new_v4().simple());
        let quoted = sql::quote_ident(engine, &name);
        let kind = if engine == Engine::Postgres {
            "SCHEMA"
        } else {
            "DATABASE"
        };
        db.query(&format!("CREATE {kind} {quoted}"), 0).await?;
        let outcome: Result<()> = async {
            if engine == Engine::Postgres {
                db.query(&format!("SET search_path TO {quoted}"), 0).await?;
            } else {
                db.select_database(&name).await?;
            }
            setup(&db).await?;
            let probe = edit_wire_probe::WireProbe::new(engine).await?;
            let mut observed_profile = profile.clone();
            observed_profile.port = probe.port;
            let observed = Database::connect(&observed_profile, "rowboat-test").await?;
            let table = sql::qualified_table(
                engine,
                &TableRef {
                    schema: name.clone(),
                    name: "records".into(),
                },
            );
            let selected = format!("SELECT * FROM {table}");
            for count in 1..=2 {
                ensure!(plan(&observed, &selected).await?.result().rows.len() == 1);
                ensure!(
                    probe.count(&selected) == count,
                    "Explicit refresh executed more than once"
                );
            }
            for rejected in [
                format!("UPDATE {table} SET note='wire_wrong'"),
                format!("SELECT * FROM {table}; DELETE FROM {table}"),
                format!("SELECT id+1,note FROM {table}"),
                format!("SELECT note FROM {table}"),
                format!("WITH x AS (SELECT * FROM {table}) SELECT * FROM x"),
            ] {
                reject(&observed, &rejected).await?;
                ensure!(
                    probe.count(&rejected) == 0,
                    "Rejected SQL reached the server"
                );
            }
            let mut readonly_profile = profile.clone();
            readonly_profile.read_only = true;
            let readonly = Database::connect(&readonly_profile, "rowboat-test").await?;
            ensure!(reject(&readonly, &selected).await?.contains("read-only"));
            ensure!(scalar(&readonly, &format!("SELECT COUNT(*) FROM {table}")).await? == "1");
            capture_cancellation(&db, &profile, &table).await?;
            rejections_and_transactions(&db).await?;
            if engine == Engine::MySql {
                let sql = "SELECT \"id\",note FROM records";
                db.query("SET SESSION sql_mode=''", 0).await?;
                reject(&db, sql).await?;
                db.query("SET SESSION sql_mode='ANSI_QUOTES,NO_BACKSLASH_ESCAPES'", 0)
                    .await?;
                ensure!(plan(&db, sql).await?.result().rows.len() == 1);
                db.query("SET SESSION sql_mode=''", 0).await?;
                reject(&db, sql).await?;
                db.query("SET @calls=0", 0).await?;
                reject(&db, "SELECT (@calls:=@calls+1),id FROM records").await?;
                ensure!(scalar(&db, "SELECT @calls").await? == "0");
                db.query("SET autocommit=0", 0).await?;
                ensure!(
                    reject(&db, "SELECT * FROM records")
                        .await?
                        .contains("autocommit")
                );
                ensure!(scalar(&db, "SELECT @@session.autocommit").await? == "0");
                db.query("ROLLBACK; SET autocommit=1", 0).await?;
            }
            db.query(
                "CREATE TEMPORARY TABLE records(id INTEGER PRIMARY KEY,note VARCHAR(40))",
                0,
            )
            .await?;
            db.query("INSERT INTO records VALUES(9,'shadow')", 0)
                .await?;
            reject(&db, "SELECT * FROM records").await?;
            ensure!(!db.in_transaction().await?);
            ensure!(scalar(&db, "SELECT note FROM records").await? == "shadow");
            db.query("DROP TABLE records", 0).await?;
            cancellation(&db).await?;
            Ok(())
        }
        .await;
        let admin = Database::connect(&profile, "rowboat-test").await?;
        let cleanup = admin
            .query(
                &format!(
                    "DROP {kind} {quoted}{}",
                    if engine == Engine::Postgres {
                        " CASCADE"
                    } else {
                        ""
                    }
                ),
                0,
            )
            .await;
        outcome?;
        cleanup?;
    }
    Ok(())
}
