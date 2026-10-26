//! File-preview seals and bounded streaming imports through the real SQLite facade.
use anyhow::Result;
use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, SqliteFileOptions, SqliteOpenMode, TableRef,
};
use rowboat_services::{
    TransferControl, TransferFormat, TransferOptions, apply_import, preview_import,
};

async fn database(dir: &std::path::Path) -> Result<Database> {
    let db = Database::connect(
        &ConnectionProfile {
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: dir.join("target.sqlite"),
                mode: SqliteOpenMode::CreateIfMissing,
            }),
            ..Default::default()
        },
        "",
    )
    .await?;
    db.query(
        "CREATE TABLE items(id INTEGER PRIMARY KEY, label TEXT, payload BLOB)",
        0,
    )
    .await?;
    Ok(db)
}
fn table() -> TableRef {
    TableRef {
        schema: "main".into(),
        name: "items".into(),
    }
}
async fn count(db: &Database) -> Result<String> {
    Ok(db.query("SELECT count(*) FROM items", 1).await?[0].rows[0][0].display())
}

#[tokio::test]
async fn sqlite_preview_seal_rejects_changed_file_and_ignores_edited_sample() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = database(dir.path()).await?;
    let path = dir.path().join("rows.jsonl");
    let original = "{\"id\":1,\"label\":null,\"payload\":{\"$binary\":\"00ff\"}}\n";
    std::fs::write(&path, original)?;
    let mut preview = preview_import(
        &path,
        TransferFormat::JsonLines,
        &TransferOptions::default(),
        &TransferControl::default(),
    )
    .await?;
    assert_eq!(count(&db).await?, "0");
    std::fs::write(&path, original.replace("\"id\":1", "\"id\":2"))?;
    assert!(
        apply_import(&db, &table(), &preview, &TransferControl::default())
            .await
            .is_err()
    );
    assert_eq!(count(&db).await?, "0");
    assert!(!db.in_transaction().await?);
    std::fs::write(&path, original)?;
    preview = preview_import(
        &path,
        TransferFormat::JsonLines,
        &TransferOptions::default(),
        &TransferControl::default(),
    )
    .await?;
    preview.sample[0] = vec![("id".into(), Cell::Text("99".into()))];
    assert_eq!(
        apply_import(&db, &table(), &preview, &TransferControl::default())
            .await?
            .rows,
        1
    );
    let rows = db.query("SELECT id,label,payload FROM items", 2).await?;
    assert_eq!(
        rows[0].rows,
        vec![vec![
            Cell::Text("1".into()),
            Cell::Null,
            Cell::Binary(vec![0, 255])
        ]]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_large_file_is_sample_bounded_and_apply_cancellation_rolls_back() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = database(dir.path()).await?;
    let path = dir.path().join("many.csv");
    {
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        writeln!(file, "id,label")?;
        for id in 1..=20_000 {
            writeln!(file, "{id},row{id}")?;
        }
    }
    let options = TransferOptions {
        batch_rows: 8,
        sample_rows: 3,
        ..Default::default()
    };
    let preview = preview_import(
        &path,
        TransferFormat::Csv,
        &options,
        &TransferControl::default(),
    )
    .await?;
    assert_eq!(preview.summary.rows, 20_000);
    assert_eq!(preview.sample.len(), 3);
    assert_eq!(count(&db).await?, "0");
    let control = TransferControl::default();
    let mut progress = control.subscribe();
    let target = table();
    let applying = apply_import(&db, &target, &preview, &control);
    let cancel = async {
        loop {
            progress.changed().await.expect("live progress channel");
            if progress.borrow().rows >= 8 {
                control.cancel();
                break;
            }
        }
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(applying, cancel)
    })
    .await?;
    assert!(result.is_err());
    assert_eq!(count(&db).await?, "0");
    assert!(!db.in_transaction().await?);
    let result = apply_import(&db, &table(), &preview, &TransferControl::default()).await?;
    assert_eq!(result.rows, 20_000);
    assert_eq!(count(&db).await?, "20000");
    Ok(())
}
