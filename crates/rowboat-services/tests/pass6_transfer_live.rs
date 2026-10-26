//! Explicit disposable Rowboat fixtures only; unique tables are removed on error.
use anyhow::{Result, ensure};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, TableRef};
use rowboat_services::*;
use std::{io::Write, os::unix::fs::FileExt, time::Duration};

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

async fn fixture(engine: Engine) -> Result<(Database, TableRef, String)> {
    let db = database(engine, false).await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: format!("rowboat_p6_array_{}", uuid::Uuid::new_v4().simple()),
    };
    let target = rowboat_core::sql::qualified_table(engine, &table);
    let binary = if engine == Engine::Postgres {
        "BYTEA"
    } else {
        "BLOB"
    };
    db.query(&format!("CREATE TABLE {target} (id INTEGER PRIMARY KEY, value TEXT, amount DECIMAL(38,18), payload {binary})"), 1).await?;
    db.query(
        &format!("INSERT INTO {target} (id, value) VALUES (-1, 'existing')"),
        1,
    )
    .await?;
    Ok((db, table, target))
}

async fn only_sentinel(db: &Database, target: &str) -> Result<()> {
    let rows = db
        .query(&format!("SELECT id, value FROM {target} ORDER BY id"), 3)
        .await?;
    ensure!(
        rows[0].rows == vec![vec![Cell::Text("-1".into()), Cell::Text("existing".into())]],
        "Partial import or existing row overwritten"
    );
    ensure!(!db.in_transaction().await?, "Import retained a transaction");
    Ok(())
}

async fn exact_and_guards(engine: Engine) -> Result<()> {
    let (db, table, target) = fixture(engine).await?;
    let result = async {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(br#"[
          {"id":1,"value":null,"amount":12345678901234567890.123456789012345678,"payload":{"$binary":"00ff"}},
          {"payload":{"$binary":""},"amount":0,"value":"","id":2},
          {"id":3,"value":"NULL","amount":null,"payload":null}
        ]"#)?;
        let options = TransferOptions {batch_rows:1, ..Default::default()};
        let mut preview = preview_import(file.path(), TransferFormat::JsonArray, &options, &TransferControl::default()).await?;
        // Mutable presentation fields cannot replace the sealed source/schema.
        preview.columns = vec!["wrong".into()];
        preview.sample.clear();
        let summary = apply_import(&db, &table, &preview, &TransferControl::default()).await?;
        assert_eq!(summary.rows, 3);
        assert_eq!(summary.columns, 4);
        let rows = db.query(&format!("SELECT value, amount, payload FROM {target} WHERE id > 0 ORDER BY id"), 3).await?;
        assert_eq!(rows[0].rows[0], vec![Cell::Null, Cell::Text("12345678901234567890.123456789012345678".into()), Cell::Binary(vec![0,255])]);
        assert_eq!(rows[0].rows[1][0], Cell::Text("".into()));
        assert_eq!(rows[0].rows[1][2], Cell::Binary(vec![]));
        assert_eq!(rows[0].rows[2], vec![Cell::Text("NULL".into()), Cell::Null, Cell::Null]);
        db.query(&format!("DELETE FROM {target} WHERE id > 0"), 1).await?;

        db.begin().await?;
        ensure!(apply_import(&db, &table, &preview, &TransferControl::default()).await.is_err(), "Used caller transaction");
        ensure!(db.in_transaction().await?, "Closed caller transaction");
        db.rollback().await?;
        let readonly = database(engine, true).await?;
        ensure!(apply_import(&readonly, &table, &preview, &TransferControl::default()).await.is_err(), "Read-only import accepted");

        let dir = tempfile::tempdir()?;
        let destination = dir.path().join("array.json");
        let export = ExportRequest {sql:format!("DELETE FROM {target}"), destination:destination.clone(), format:TransferFormat::JsonArray, table:None, options};
        ensure!(export_query(&db, &export, &TransferControl::default()).await.is_err(), "Unsupported export accepted");
        ensure!(std::fs::read_dir(dir.path())?.next().is_none(), "Unsupported export created output");
        std::fs::write(&destination, "existing file")?;
        ensure!(export_query(&db, &export, &TransferControl::default()).await.is_err(), "Unsupported export overwrote output");
        assert_eq!(std::fs::read_to_string(destination)?, "existing file");
        only_sentinel(&db, &target).await?;

        // A valid file with a duplicate existing key late in the stream rolls back.
        std::fs::write(file.path(), "[{\"id\":1,\"value\":\"new\"},{\"id\":-1,\"value\":\"replacement\"}]")?;
        let duplicate = preview_import(file.path(), TransferFormat::JsonArray, &TransferOptions {batch_rows:1, ..Default::default()}, &TransferControl::default()).await?;
        ensure!(apply_import(&db, &table, &duplicate, &TransferControl::default()).await.is_err(), "Duplicate existing row accepted");
        only_sentinel(&db, &target).await?;

        std::fs::write(file.path(), "[{\"id\":2,\"value\":\"changed\"}]")?;
        ensure!(apply_import(&db, &table, &duplicate, &TransferControl::default()).await.is_err(), "Changed preview source accepted");
        only_sentinel(&db, &target).await
    }.await;
    let _ = db.rollback().await;
    db.query(&format!("DROP TABLE {target}"), 1).await?;
    result
}

async fn tail_and_cancellation(engine: Engine) -> Result<()> {
    let (db, table, target) = fixture(engine).await?;
    let result = async {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(b"[")?;
        let payload = "日本語".repeat(60);
        for id in 1..=2000 {
            write!(
                file,
                "{}{{\"id\":{id},\"value\":\"{payload}\"}}",
                if id == 1 { "" } else { "," }
            )?;
        }
        file.write_all(b"]")?;
        let options = TransferOptions {
            batch_rows: 8,
            batch_bytes: 8192,
            max_record_bytes: 1024,
            ..Default::default()
        };
        let length = file.as_file().metadata()?.len();
        ensure!(
            length > 1024 * 1024,
            "Mutation must be beyond scanner read-ahead"
        );
        for mode in ["malformed-tail", "fingerprint", "cancel", "abort"] {
            // Restore the original file and obtain fresh metadata/fingerprint.
            file.as_file().write_all_at(b"]", length - 1)?;
            file.as_file().write_all_at("語".as_bytes(), length - 6)?;
            let preview = preview_import(
                file.path(),
                TransferFormat::JsonArray,
                &options,
                &TransferControl::default(),
            )
            .await?;
            let control = TransferControl::default();
            let mut progress = control.subscribe();
            let cancel = control.clone();
            let worker = db.clone();
            let worker_table = table.clone();
            let task = tokio::spawn(async move {
                apply_import(&worker, &worker_table, &preview, &control).await
            });
            tokio::time::timeout(
                Duration::from_secs(10),
                progress.wait_for(|p| p.phase == TransferPhase::Applying && p.rows >= 64),
            )
            .await??;
            match mode {
                "malformed-tail" => file.as_file().write_all_at(b",", length - 1)?,
                "fingerprint" => {
                    // Valid JSON with identical size still cannot bypass the sealed file.
                    file.as_file().write_all_at("本".as_bytes(), length - 6)?;
                }
                "cancel" => cancel.cancel(),
                "abort" => task.abort(),
                _ => unreachable!(),
            }
            if mode == "abort" {
                ensure!(task.await.is_err(), "Aborted apply completed");
            } else {
                let error = task.await?.expect_err("Late failure committed import");
                if mode == "malformed-tail" {
                    ensure!(
                        format!("{error:#}").contains("JSON array requires row objects"),
                        "Tail was not parsed after prior batches: {error:#}"
                    );
                } else if mode == "fingerprint" {
                    ensure!(
                        format!("{error:#}").contains("file changed while being scanned"),
                        "Expected integrity failure: {error:#}"
                    );
                }
            }
            tokio::time::timeout(Duration::from_secs(10), only_sentinel(&db, &target)).await??;
        }
        Ok(())
    }
    .await;
    db.query(&format!("DROP TABLE {target}"), 1).await?;
    result
}

#[tokio::test]
#[ignore = "requires disposable Rowboat PostgreSQL at 15435"]
async fn postgres_array_exact_values_and_guards() -> Result<()> {
    exact_and_guards(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires disposable Rowboat MySQL at 13309"]
async fn mysql_array_exact_values_and_guards() -> Result<()> {
    exact_and_guards(Engine::MySql).await
}
#[tokio::test]
#[ignore = "requires disposable Rowboat PostgreSQL at 15435"]
async fn postgres_array_late_tail_and_cancel_are_atomic() -> Result<()> {
    tail_and_cancellation(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires disposable Rowboat MySQL at 13309"]
async fn mysql_array_late_tail_and_cancel_are_atomic() -> Result<()> {
    tail_and_cancellation(Engine::MySql).await
}
