//! Only the explicit disposable base fixtures from scripts/test-databases.sh.
mod common;
use anyhow::{Result, ensure};
use common::peak_kib;
use rowboat_core::{ConnectionProfile, Database, Engine};
use rowboat_services::*;
use std::{path::Path, time::Duration};

async fn database(engine: Engine, read_only: bool) -> Result<Database> {
    Database::connect(
        &ConnectionProfile {
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
                "rowboat"
            }
            .into(),
            database: "rowboat".into(),
            ssl_mode: "disable".into(),
            read_only,
            ..Default::default()
        },
        "rowboat-test",
    )
    .await
}

async fn empty_directory(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read_dir(path)?.next().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

async fn export_session(engine: Engine) -> Result<()> {
    let database = database(engine, false).await?;
    let table = format!("rowboat_export_{}", uuid::Uuid::new_v4().simple());
    let binary_type = if engine == Engine::Postgres {
        "BYTEA"
    } else {
        "BLOB"
    };
    database.query(&format!("CREATE TEMPORARY TABLE {table} (id INTEGER PRIMARY KEY, value TEXT, amount DECIMAL(38,18), payload {binary_type})"), 10).await?;
    let binary = if engine == Engine::Postgres {
        "decode('00ff','hex')"
    } else {
        "X'00ff'"
    };
    database.query(&format!("INSERT INTO {table} VALUES (1, '日本語 comma,quote''', 12345678901234567890.123456789012345678, {binary})"), 10).await?;
    database.begin().await?;
    database
        .query(
            &format!("INSERT INTO {table} VALUES (2, NULL, NULL, NULL), (3, 'NULL', 0, {binary})"),
            10,
        )
        .await?;
    let directory = tempfile::tempdir()?;
    for format in [
        TransferFormat::Csv,
        TransferFormat::JsonLines,
        TransferFormat::Sql,
    ] {
        let destination = directory.path().join(format!("data-{format:?}"));
        let request = ExportRequest {
            sql: format!("SELECT id, value, amount, payload FROM {table} ORDER BY id"),
            destination: destination.clone(),
            format,
            table: Some(rowboat_core::TableRef {
                schema: String::new(),
                name: table.clone(),
            }),
            options: TransferOptions {
                batch_rows: 1,
                ..Default::default()
            },
        };
        let summary = export_query(&database, &request, &TransferControl::default()).await?;
        ensure!(
            summary.rows == 3 && summary.columns == 4 && summary.bytes > 0,
            "Export lost session-local uncommitted data"
        );
        ensure!(
            database.in_transaction().await?,
            "Export changed the caller-owned transaction"
        );
        ensure!(
            export_query(&database, &request, &TransferControl::default())
                .await
                .is_err(),
            "Export overwrote an existing file"
        );
        if format != TransferFormat::Sql {
            let preview = preview_import(
                &destination,
                format,
                &TransferOptions::default(),
                &TransferControl::default(),
            )
            .await?;
            ensure!(preview.summary.rows == 3, "Export/import count mismatch");
            let first: std::collections::BTreeMap<_, _> =
                preview.sample[0].iter().cloned().collect();
            ensure!(
                first["amount"]
                    == rowboat_core::Cell::Text("12345678901234567890.123456789012345678".into()),
                "Decimal precision changed"
            );
            ensure!(
                first["payload"] == rowboat_core::Cell::Binary(vec![0, 255]),
                "Binary changed"
            );
        }
    }
    database.rollback().await?;
    let rows = database
        .query(&format!("SELECT COUNT(*) FROM {table}"), 10)
        .await?;
    ensure!(
        rows[0].rows[0][0] == rowboat_core::Cell::Text("1".into()),
        "Export committed the user's transaction"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_progressive_export_preserves_session_and_exact_cells() -> Result<()> {
    export_session(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_progressive_export_preserves_session_and_exact_cells() -> Result<()> {
    export_session(Engine::MySql).await
}

async fn export_failures(engine: Engine) -> Result<()> {
    let db = database(engine, true).await?;
    let directory = tempfile::tempdir()?;
    let request = ExportRequest {
        sql: "SELECT 1 AS value; SELECT * FROM rowboat_deliberately_nonexistent_export_source"
            .into(),
        destination: directory.path().join("never.csv"),
        format: TransferFormat::Csv,
        table: None,
        options: TransferOptions::default(),
    };
    ensure!(
        export_query(&db, &request, &TransferControl::default())
            .await
            .is_err(),
        "Late export failure was ignored"
    );
    empty_directory(directory.path()).await?;
    let forbidden = ExportRequest {
        sql: "CREATE TABLE forbidden_stream_export (id INTEGER)".into(),
        ..request.clone()
    };
    ensure!(
        export_query(&db, &forbidden, &TransferControl::default())
            .await
            .is_err(),
        "Read-only export mutated the database"
    );
    empty_directory(directory.path()).await?;
    let slow = ExportRequest {
        sql: if engine == Engine::Postgres {
            "SELECT pg_sleep(10) AS value"
        } else {
            "SELECT SLEEP(10) AS value"
        }
        .into(),
        ..request
    };
    let control = TransferControl::default();
    let cancel = control.clone();
    let worker = db.clone();
    let task = tokio::spawn(async move { export_query(&worker, &slow, &control).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();
    ensure!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .is_err(),
        "Cancelled export reported success"
    );
    empty_directory(directory.path()).await?;
    db.query("SELECT 1", 10).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_export_error_and_cancel_leave_no_files() -> Result<()> {
    export_failures(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_export_error_and_cancel_leave_no_files() -> Result<()> {
    export_failures(Engine::MySql).await
}

async fn import_atomic(engine: Engine) -> Result<()> {
    let db = database(engine, false).await?;
    let table = rowboat_core::TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: format!("rowboat_import_{}", uuid::Uuid::new_v4().simple()),
    };
    let target = rowboat_core::sql::qualified_table(engine, &table);
    let binary = if engine == Engine::Postgres {
        "BYTEA"
    } else {
        "BLOB"
    };
    db.query(&format!("CREATE TABLE {target} (id INTEGER PRIMARY KEY, value TEXT, amount DECIMAL(38,18), payload {binary})"), 1).await?;
    let result: Result<()> = async {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("rows.csv");
        let options = TransferOptions {
            batch_rows: 16,
            batch_bytes: 4096,
            max_record_bytes: 1024,
            ..Default::default()
        };
        let mut text = String::from("id,value,amount,payload\n");
        for id in 0..2000 {
            text.push_str(&format!(
                "{id},日本語,12345678901234567890.123456789012345678,\\x00ff\n"
            ));
        }
        std::fs::write(&source, &text)?;
        let preview = preview_import(
            &source,
            TransferFormat::Csv,
            &options,
            &TransferControl::default(),
        )
        .await?;
        let summary = apply_import(&db, &table, &preview, &TransferControl::default()).await?;
        ensure!(summary.rows == 2000, "Import did not commit all rows");
        let rows = db
            .query(
                &format!("SELECT value, amount, payload FROM {target} WHERE id = 1999"),
                1,
            )
            .await?;
        ensure!(
            rows[0].rows[0]
                == vec![
                    rowboat_core::Cell::Text("日本語".into()),
                    rowboat_core::Cell::Text("12345678901234567890.123456789012345678".into()),
                    rowboat_core::Cell::Binary(vec![0, 255])
                ],
            "Import changed exact cell values"
        );
        db.query(&format!("DELETE FROM {target}"), 1).await?;

        let jsonl = dir.path().join("rows.jsonl");
        std::fs::write(&jsonl, "{\"id\":1,\"value\":null,\"amount\":12345678901234567890.123456789012345678,\"payload\":{\"$binary\":\"00ff\"}}\n{\"id\":2,\"value\":\"NULL\",\"amount\":0,\"payload\":null}\n")?;
        let json_preview = preview_import(&jsonl, TransferFormat::JsonLines, &options, &TransferControl::default()).await?;
        ensure!(apply_import(&db, &table, &json_preview, &TransferControl::default()).await?.rows == 2, "JSON Lines import lost rows");
        let rows = db.query(&format!("SELECT value, amount, payload FROM {target} ORDER BY id"), 2).await?;
        ensure!(rows[0].rows[0] == vec![rowboat_core::Cell::Null, rowboat_core::Cell::Text("12345678901234567890.123456789012345678".into()), rowboat_core::Cell::Binary(vec![0,255])], "JSON Lines import changed exact values");
        ensure!(rows[0].rows[1][0] == rowboat_core::Cell::Text("NULL".into()), "JSON Lines lost text NULL");
        db.query(&format!("DELETE FROM {target}"), 1).await?;

        // A duplicate PK in the very last chunk must roll back earlier chunks.
        std::fs::write(&source, format!("{text}0,duplicate,0,\\N\n"))?;
        let late = preview_import(
            &source,
            TransferFormat::Csv,
            &options,
            &TransferControl::default(),
        )
        .await?;
        ensure!(
            apply_import(&db, &table, &late, &TransferControl::default())
                .await
                .is_err(),
            "Late duplicate accepted"
        );
        assert_empty(&db, &target).await?;

        std::fs::write(&source, &text)?;
        let preview = preview_import(
            &source,
            TransferFormat::Csv,
            &options,
            &TransferControl::default(),
        )
        .await?;
        db.begin().await?;
        ensure!(
            apply_import(&db, &table, &preview, &TransferControl::default())
                .await
                .is_err(),
            "Import reused caller-owned transaction"
        );
        ensure!(
            db.in_transaction().await?,
            "Import closed caller transaction"
        );
        db.rollback().await?;
        let readonly = database(engine, true).await?;
        ensure!(
            apply_import(&readonly, &table, &preview, &TransferControl::default())
                .await
                .is_err(),
            "Read-only import accepted"
        );

        // Modify the file after the producer has passed several batches to core.
        let control = TransferControl::default();
        let mut progress = control.subscribe();
        let worker_db = db.clone();
        let worker_table = table.clone();
        let worker_preview = preview.clone();
        let task = tokio::spawn(async move {
            apply_import(&worker_db, &worker_table, &worker_preview, &control).await
        });
        tokio::time::timeout(
            Duration::from_secs(10),
            progress.wait_for(|p| p.phase == TransferPhase::Applying && p.rows >= 512),
        )
        .await??;
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source)?
            .write_all(b"3000,changed,0,\\N\n")?;
        ensure!(task.await?.is_err(), "Changed file committed");
        assert_empty(&db, &target).await?;

        std::fs::write(&source, &text)?;
        let preview = preview_import(
            &source,
            TransferFormat::Csv,
            &options,
            &TransferControl::default(),
        )
        .await?;
        for abort in [false, true] {
            let control = TransferControl::default();
            let cancel = control.clone();
            let mut progress = control.subscribe();
            let worker_db = db.clone();
            let worker_table = table.clone();
            let worker_preview = preview.clone();
            let task = tokio::spawn(async move {
                apply_import(&worker_db, &worker_table, &worker_preview, &control).await
            });
            tokio::time::timeout(
                Duration::from_secs(10),
                progress.wait_for(|p| p.phase == TransferPhase::Applying && p.rows >= 512),
            )
            .await??;
            if abort {
                task.abort();
                ensure!(task.await.is_err(), "Aborted import completed");
            } else {
                cancel.cancel();
                ensure!(task.await?.is_err(), "Cancelled import completed");
            }
            tokio::time::timeout(Duration::from_secs(10), assert_empty(&db, &target)).await??;
        }
        Ok(())
    }
    .await;
    db.query(&format!("DROP TABLE {target}"), 1).await?;
    result
}

async fn assert_empty(db: &Database, target: &str) -> Result<()> {
    let count = db
        .query(&format!("SELECT COUNT(*) FROM {target}"), 1)
        .await?;
    ensure!(
        count[0].rows[0][0] == rowboat_core::Cell::Text("0".into()),
        "Import left partially committed rows"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_import_is_atomic_across_late_errors_changes_and_cancel() -> Result<()> {
    import_atomic(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_import_is_atomic_across_late_errors_changes_and_cancel() -> Result<()> {
    import_atomic(Engine::MySql).await
}

async fn oversized_export(engine: Engine) -> Result<()> {
    let db = database(engine, true).await?;
    let dir = tempfile::tempdir()?;
    for format in [
        TransferFormat::Csv,
        TransferFormat::JsonLines,
        TransferFormat::Sql,
    ] {
        let request = ExportRequest {
            sql: "SELECT REPEAT('x', 180) AS a, REPEAT('y', 180) AS b".into(),
            destination: dir.path().join("oversized"),
            format,
            table: Some(rowboat_core::TableRef {
                schema: String::new(),
                name: "data".into(),
            }),
            options: TransferOptions {
                max_record_bytes: 256,
                ..Default::default()
            },
        };
        ensure!(
            export_query(&db, &request, &TransferControl::default())
                .await
                .is_err(),
            "Encoded {format:?} row exceeded the record budget without failure"
        );
        empty_directory(dir.path()).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_export_record_limit_cleans_partial_files() -> Result<()> {
    oversized_export(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_export_record_limit_cleans_partial_files() -> Result<()> {
    oversized_export(Engine::MySql).await
}

async fn large_export(engine: Engine, test_name: &str) -> Result<()> {
    if std::env::var_os("ROWBOAT_EXPORT_MEMORY_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--ignored", "--exact", test_name, "--nocapture"])
            .env("ROWBOAT_EXPORT_MEMORY_CHILD", "1")
            .status()?;
        ensure!(
            status.success(),
            "Bounded-memory live export subprocess failed"
        );
        return Ok(());
    }
    let db = database(engine, true).await?;
    let sql = if engine == Engine::Postgres {
        "SELECT n AS id, REPEAT('x',1024) AS payload FROM generate_series(1,70000) n".into()
    } else {
        let digits = "(SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
        format!(
            "SELECT a.n AS id, REPEAT('x',1024) AS payload FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c CROSS JOIN {digits} d CROSS JOIN {digits} e LIMIT 70000"
        )
    };
    let dir = tempfile::tempdir()?;
    let before = peak_kib()?;
    let request = ExportRequest {
        sql,
        destination: dir.path().join("large.csv"),
        format: TransferFormat::Csv,
        table: None,
        options: TransferOptions {
            batch_rows: 64,
            batch_bytes: 256 * 1024,
            max_record_bytes: 128 * 1024,
            ..Default::default()
        },
    };
    let summary = export_query(&db, &request, &TransferControl::default()).await?;
    ensure!(
        summary.rows == 70000 && summary.bytes > 64 * 1024 * 1024,
        "Export unexpectedly used retained-result caps"
    );
    let growth = peak_kib()?.saturating_sub(before);
    ensure!(
        growth < 48 * 1024,
        "Streaming export RSS grew {growth} KiB for a 69MiB output"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_large_export_has_measured_bounded_memory() -> Result<()> {
    large_export(
        Engine::Postgres,
        "postgres_large_export_has_measured_bounded_memory",
    )
    .await
}

#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_large_export_has_measured_bounded_memory() -> Result<()> {
    large_export(
        Engine::MySql,
        "mysql_large_export_has_measured_bounded_memory",
    )
    .await
}

async fn publication_race(engine: Engine) -> Result<()> {
    let db = database(engine, false).await?;
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("race.csv");
    let request = ExportRequest {
        sql: if engine == Engine::Postgres {
            "SELECT pg_sleep(0.5) AS pause"
        } else {
            "SELECT SLEEP(0.5) AS pause"
        }
        .into(),
        destination: destination.clone(),
        format: TransferFormat::Csv,
        table: None,
        options: TransferOptions::default(),
    };
    let task =
        tokio::spawn(async move { export_query(&db, &request, &TransferControl::default()).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read_dir(dir.path()).unwrap().next().is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?;
    // The exporter has passed its destination check and created its temp file.
    std::fs::write(&destination, "another writer's output")?;
    let error = task
        .await?
        .expect_err("Publication race overwrote the destination");
    ensure!(
        error.chain().any(|e| e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)),
        "Publication error lost its I/O cause: {error:#}"
    );
    ensure!(
        std::fs::read_dir(dir.path())?.count() == 1,
        "Retained error owns an export temp file"
    );
    ensure!(
        std::fs::read_to_string(&destination)? == "another writer's output",
        "Existing output was replaced"
    );
    drop(error); // Intentionally retained through the cleanup assertions.
    Ok(())
}

async fn failed_writer_interrupts_slow_query(engine: Engine) -> Result<()> {
    let db = database(engine, false).await?;
    let table = format!("rowboat_writer_failure_{}", uuid::Uuid::new_v4().simple());
    db.query(&format!("CREATE TEMPORARY TABLE {table} (id INTEGER)"), 1)
        .await?;
    db.begin().await?;
    db.query(
        &format!("INSERT INTO {table} VALUES (1); SAVEPOINT before_export"),
        1,
    )
    .await?;
    let dir = tempfile::tempdir()?;
    // Two fast 10KB rows flush the first complete row through the wire buffer,
    // while the next row/batch remains behind ten seconds of server-side work.
    let sql = match engine {
        Engine::Sqlite => anyhow::bail!("This regression uses a server-side sleep fixture"),
        Engine::Postgres => {
            "SELECT repeat('x',5000) AS a, repeat('y',5000) AS b, pg_sleep(CASE WHEN g <= 2 THEN 0 ELSE 10 END) AS pause FROM generate_series(1,3) AS g"
        }
        Engine::MySql => {
            "SELECT repeat('x',5000) AS a, repeat('y',5000) AS b, SLEEP(IF(g <= 2,0,10)) AS pause FROM (SELECT 1 AS g UNION ALL SELECT 2 UNION ALL SELECT 3) AS numbers"
        }
    };
    let request = ExportRequest {
        sql: sql.into(),
        destination: dir.path().join("failed.csv"),
        format: TransferFormat::Csv,
        table: None,
        options: TransferOptions {
            batch_rows: 1,
            batch_bytes: 64 * 1024,
            max_record_bytes: 8192,
            ..Default::default()
        },
    };
    // Verify the regression's timing precondition against the actual server:
    // first batch is available, and there is no next event to reveal writer EOF.
    let mut probe = db
        .query_stream(
            sql,
            rowboat_core::QueryStreamOptions {
                row_limit: None,
                byte_limit: None,
                batch_rows: 1,
                batch_bytes: 64 * 1024,
                max_cell_bytes: 8192,
                channel_capacity: 2,
                ..Default::default()
            },
        )
        .await?;
    loop {
        match tokio::time::timeout(Duration::from_secs(3), probe.recv()).await? {
            Some(rowboat_core::QueryEvent::Schema { .. }) => {}
            Some(rowboat_core::QueryEvent::Rows { .. }) => break,
            event => anyhow::bail!("Unexpected regression probe event: {event:?}"),
        }
    }
    let next = tokio::time::timeout(Duration::from_millis(200), probe.recv()).await;
    probe.close().await?;
    ensure!(
        next.is_err(),
        "Regression query did not stall between first and next batches"
    );
    db.query("ROLLBACK TO SAVEPOINT before_export", 1).await?;
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        export_query(&db, &request, &TransferControl::default()),
    )
    .await?
    .expect_err("Oversized encoded row was published");
    ensure!(
        format!("{error:#}").contains("Encoded export record"),
        "Lost original writer error: {error:#}"
    );
    ensure!(
        std::fs::read_dir(dir.path())?.next().is_none(),
        "Failed writer left output/temp files"
    );
    ensure!(
        db.in_transaction().await?,
        "Writer failure ended the caller transaction"
    );
    // PostgreSQL cancellation leaves the original transaction failed, as normal.
    // Its caller-owned savepoint recovers it without losing the preceding insert.
    db.query("ROLLBACK TO SAVEPOINT before_export", 1).await?;
    let rows = tokio::time::timeout(
        Duration::from_secs(3),
        db.query(&format!("SELECT id FROM {table}"), 1),
    )
    .await??;
    ensure!(
        rows[0].rows == vec![vec![rowboat_core::Cell::Text("1".into())]],
        "Cleanup lost the original session or uncommitted write"
    );
    db.rollback().await?;
    assert_empty(&db, &table).await?;
    Ok(())
}

async fn bom_export_roundtrip(engine: Engine) -> Result<()> {
    let db = database(engine, false).await?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("bom.csv");
    let request = ExportRequest {
        sql: "SELECT '\u{feff}Alice' AS name UNION ALL SELECT '\u{feff}' AS name".into(),
        destination: path.clone(),
        format: TransferFormat::Csv,
        table: None,
        options: TransferOptions::default(),
    };
    ensure!(
        export_query(&db, &request, &TransferControl::default())
            .await?
            .rows
            == 2,
        "BOM export lost a row"
    );
    let preview = preview_import(
        &path,
        TransferFormat::Csv,
        &TransferOptions::default(),
        &TransferControl::default(),
    )
    .await?;
    ensure!(
        preview.sample
            == vec![
                vec![(
                    "name".into(),
                    rowboat_core::Cell::Text("\u{feff}Alice".into())
                )],
                vec![("name".into(), rowboat_core::Cell::Text("\u{feff}".into()))]
            ],
        "Streaming round trip changed BOM data or dropped the BOM-only row"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_export_publication_race_cleans_temp_while_error_retained() -> Result<()> {
    publication_race(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_export_publication_race_cleans_temp_while_error_retained() -> Result<()> {
    publication_race(Engine::MySql).await
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_writer_failure_cancels_slow_query_and_preserves_transaction() -> Result<()> {
    failed_writer_interrupts_slow_query(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_writer_failure_cancels_slow_query_and_preserves_transaction() -> Result<()> {
    failed_writer_interrupts_slow_query(Engine::MySql).await
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL at 15435"]
async fn postgres_streaming_csv_preserves_bom_data() -> Result<()> {
    bom_export_roundtrip(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires disposable MySQL at 13309"]
async fn mysql_streaming_csv_preserves_bom_data() -> Result<()> {
    bom_export_roundtrip(Engine::MySql).await
}
