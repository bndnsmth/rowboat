//! Regressions for native evidence ambiguity and edit ownership, using only
//! disposable repository-local files. No network fixtures are needed.
use anyhow::{Result, ensure};
use rowboat_core::*;
use std::{path::PathBuf, time::Duration};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .join(format!("pass12_core_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
    async fn connect(&self) -> Result<Database> {
        Database::connect(
            &ConnectionProfile {
                engine: Engine::Sqlite,
                file: Some(SqliteFileOptions {
                    path: self.0.join("main.sqlite"),
                    mode: SqliteOpenMode::CreateIfMissing,
                }),
                ..Default::default()
            },
            "",
        )
        .await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn table(name: &str) -> TableRef {
    TableRef {
        schema: "main".into(),
        name: name.into(),
    }
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 10).await?[0].rows[0][0].display())
}
async fn plan(db: &Database, sql: &str) -> Result<QueryEditPlan> {
    let (mut stream, capture) = db
        .query_stream_with_edit_source(sql, Default::default())
        .await?;
    let mut result = QueryResult::default();
    while let Some(event) = stream.recv().await {
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
            QueryEvent::Error { message, .. } => anyhow::bail!(message),
            _ => {}
        }
    }
    stream.close().await?;
    capture.finish().await.plan(&result)
}
fn update(result: &QueryResult, row: usize, column: &str, value: &str) -> RowChange {
    RowChange::Update {
        original: result
            .columns
            .iter()
            .zip(&result.rows[row])
            .map(|(c, v)| (c.name.clone(), v.clone()))
            .collect(),
        values: vec![(column.into(), Cell::Text(value.into()))],
    }
}

#[tokio::test]
async fn attachment_incarnations_invalidate_both_query_and_browse_proofs() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    for memory in [true, false] {
        let file = fixture.0.join("attachment.sqlite");
        let source = if memory {
            ":memory:".into()
        } else {
            file.to_string_lossy().replace('\'', "''")
        };
        let attach = format!("ATTACH '{source}' AS aux");
        let ddl = "CREATE TABLE aux.t(id INTEGER PRIMARY KEY,note TEXT); INSERT INTO aux.t VALUES(1,'old')";
        db.query(&attach, 10).await?;
        db.query(ddl, 10).await?;
        let captured = plan(&db, "SELECT id,note FROM aux.t").await?;
        let t = TableRef {
            schema: "aux".into(),
            name: "t".into(),
        };
        let rows = db.browse(&t, &Default::default()).await?;
        db.query("DETACH aux", 10).await?;
        if !memory {
            std::fs::rename(&file, file.with_extension("old"))?;
        }
        db.query(&attach, 10).await?;
        db.query(ddl, 10).await?;
        assert!(
            db.apply_query_changes(&captured, &[update(captured.result(), 0, "note", "wrong")])
                .await
                .is_err()
        );
        assert!(
            db.apply_changes(&t, &[update(&rows, 0, "note", "wrong")])
                .await
                .is_err()
        );
        assert_eq!(scalar(&db, "SELECT note FROM aux.t").await?, "old");
        assert!(!db.in_transaction().await?);
        let fresh = plan(&db, "SELECT id,note FROM aux.t").await?;
        assert_eq!(
            db.apply_query_changes(&fresh, &[update(fresh.result(), 0, "note", "fresh")])
                .await?,
            1
        );
        db.query("DETACH aux", 10).await?;
    }
    Ok(())
}

#[tokio::test]
async fn display_equivalent_browse_originals_never_choose_first_native_key() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id PRIMARY KEY NOT NULL,note TEXT); INSERT INTO t VALUES(1,'old'),('1','old')",10).await?;
    let t = table("t");
    let rows = db
        .browse(
            &t,
            &BrowseRequest {
                sort: Some(("id".into(), true)),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(rows.rows[0], rows.rows[1]);
    let error = db
        .apply_changes(&t, &[update(&rows, 1, "note", "wrong")])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Ambiguous"));
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM t WHERE note='old'").await?,
        "2"
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn deferred_commit_failure_rolls_back_query_and_table_apply() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1)",10).await?;
    let captured = plan(&db, "SELECT id,parent_id FROM child").await?;
    assert!(
        db.apply_query_changes(
            &captured,
            &[update(captured.result(), 0, "parent_id", "999")]
        )
        .await
        .is_err()
    );
    assert!(!db.in_transaction().await?);
    assert_eq!(scalar(&db, "SELECT parent_id FROM child").await?, "1");
    let t = table("child");
    let rows = db.browse(&t, &Default::default()).await?;
    assert!(
        db.apply_changes(&t, &[update(&rows, 0, "parent_id", "999")])
            .await
            .is_err()
    );
    assert!(!db.in_transaction().await?);
    assert_eq!(scalar(&db, "SELECT parent_id FROM child").await?, "1");
    // A caller transaction is never committed or fully rolled back by table Apply.
    db.begin().await?;
    db.query("INSERT INTO parent VALUES(2)", 10).await?;
    let rows = db.browse(&t, &Default::default()).await?;
    assert_eq!(
        db.apply_changes(&t, &[update(&rows, 0, "parent_id", "2")])
            .await?,
        1
    );
    assert!(db.in_transaction().await?);
    db.rollback().await?;
    assert_eq!(scalar(&db, "SELECT parent_id FROM child").await?, "1");
    Ok(())
}

#[tokio::test]
async fn abandoned_table_apply_retires_and_releases_native_write_lock() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,note TEXT); INSERT INTO t VALUES(1,'old'); CREATE TRIGGER slow BEFORE UPDATE ON t BEGIN SELECT (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n); END",10).await?;
    let t = table("t");
    let rows = db.browse(&t, &Default::default()).await?;
    let worker = db.clone();
    let task = tokio::spawn(async move {
        worker
            .apply_changes(&t, &[update(&rows, 0, "note", "abandoned")])
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    ensure!(!task.is_finished(), "slow mutation did not remain active");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(db.in_transaction().await.is_err());
    let other = fixture.connect().await?;
    tokio::time::timeout(
        Duration::from_secs(10),
        other.query("BEGIN IMMEDIATE; ROLLBACK", 10),
    )
    .await??;
    assert_eq!(scalar(&other, "SELECT note FROM t").await?, "old");
    Ok(())
}

#[tokio::test]
async fn native_empty_multiple_and_bounded_results_keep_schema_and_reuse() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,note TEXT); INSERT INTO t VALUES(1,'one'),(2,'two'),(3,'three')",10).await?;
    let results=db.query("SELECT id,note FROM t WHERE 0; SELECT id FROM t ORDER BY id; SELECT x'00ff' AS blob,NULL AS empty",1).await?;
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].columns.len(), 2);
    assert!(results[0].rows.is_empty());
    assert!(results[1].truncated);
    assert_eq!(results[1].rows.len(), 1);
    assert_eq!(
        results[2].rows[0],
        vec![Cell::Binary(vec![0, 255]), Cell::Null]
    );
    assert!(!db.server_version().await?.is_empty());
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn explicit_apply_cancellation_never_commits_an_earlier_change() -> Result<()> {
    for query_edit in [false, true] {
        let fixture = Fixture::new()?;
        let db = fixture.connect().await?;
        db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,note TEXT); INSERT INTO t VALUES(1,'old'),(2,'old'); CREATE TRIGGER slow BEFORE UPDATE ON t WHEN OLD.id=2 BEGIN SELECT (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n); END",10).await?;
        let captured = plan(&db, "SELECT id,note FROM t ORDER BY id").await?;
        let rows = db
            .browse(
                &table("t"),
                &BrowseRequest {
                    sort: Some(("id".into(), true)),
                    ..Default::default()
                },
            )
            .await?;
        let worker = db.clone();
        let task = tokio::spawn(async move {
            let result = if query_edit { captured.result() } else { &rows };
            let changes = [
                update(result, 0, "note", "cancelled"),
                update(result, 1, "note", "cancelled"),
            ];
            if query_edit {
                worker.apply_query_changes(&captured, &changes).await
            } else {
                worker.apply_changes(&table("t"), &changes).await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        ensure!(!task.is_finished(), "Slow batch did not remain active");
        tokio::time::timeout(Duration::from_secs(10), db.cancel()).await??;
        ensure!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await??
                .is_err()
        );
        let observer = fixture.connect().await?;
        tokio::time::timeout(
            Duration::from_secs(10),
            observer.query("BEGIN IMMEDIATE; ROLLBACK", 0),
        )
        .await??;
        assert_eq!(
            scalar(&observer, "SELECT count(*) FROM t WHERE note='old'").await?,
            "2"
        );
        if let Ok(active) = db.in_transaction().await {
            assert!(!active);
        }
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_preview_literals_are_readable_and_round_trip() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    for text in [
        "Ada native12",
        "O'Reilly",
        "back\\slash",
        "line\nbreak",
        "nul\0tail",
    ] {
        let rendered = sql::literal(Engine::Sqlite, &Cell::Text(text.into()));
        if !text.contains('\0') {
            ensure!(
                rendered.starts_with('\''),
                "Ordinary review text must remain readable"
            );
        }
        let result = db.query(&format!("SELECT {rendered}"), 1).await?;
        assert_eq!(result[0].rows[0][0], Cell::Text(text.into()));
    }
    Ok(())
}

#[tokio::test]
async fn query_preview_preserves_native_integer_key_and_readable_text() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query(
        "CREATE TABLE t(id PRIMARY KEY NOT NULL,note TEXT); INSERT INTO t VALUES(1,'before')",
        10,
    )
    .await?;
    let captured = plan(&db, "SELECT id,note FROM t").await?;
    let changes = [update(captured.result(), 0, "note", "Ada O'Reilly")];
    let preview = captured.preview_changes(&changes)?;
    assert!(
        preview.contains("'Ada O''Reilly'")
            && preview.contains("'integer'")
            && preview.contains("IS 1")
    );
    db.begin().await?;
    db.query(&preview, 10).await?;
    assert_eq!(scalar(&db, "SELECT note FROM t").await?, "Ada O'Reilly");
    db.rollback().await?;
    assert_eq!(scalar(&db, "SELECT note FROM t").await?, "before");
    assert_eq!(db.apply_query_changes(&captured, &changes).await?, 1);
    Ok(())
}
