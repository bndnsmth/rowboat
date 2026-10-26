//! Real streaming Apply against disposable repository-local SQLite databases.
use anyhow::{Result, ensure};
use rowboat_core::*;
use std::{path::PathBuf, time::Duration};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .join(format!("pass14_import_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
    async fn connect(&self) -> Result<Database> {
        Database::connect(&self.profile(false), "").await
    }
    fn profile(&self, read_only: bool) -> ConnectionProfile {
        ConnectionProfile {
            engine: Engine::Sqlite,
            read_only,
            file: Some(SqliteFileOptions {
                path: self.0.join("main.sqlite"),
                mode: if read_only {
                    SqliteOpenMode::OpenExisting
                } else {
                    SqliteOpenMode::CreateIfMissing
                },
            }),
            ..Default::default()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn table(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}
fn text(value: impl ToString) -> Cell {
    Cell::Text(value.to_string())
}
fn row(id: i64) -> ImportRow {
    vec![("id".into(), text(id))]
}
fn options() -> RowImportOptions {
    RowImportOptions {
        batch_rows: 2,
        batch_bytes: 4096,
        max_cell_bytes: 2048,
        channel_capacity: 1,
        ..Default::default()
    }
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 1).await?[0].rows[0][0].display())
}
async fn empty_reusable(db: &Database) -> Result<()> {
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(10),
            scalar(db, "SELECT count(*) FROM t")
        )
        .await??,
        "0"
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn explicit_finish_atomic_batches_native_values_and_session_lease() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    let observer = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,txt TEXT,n NUMERIC,raw,blob BLOB,optional TEXT DEFAULT 'default',generated TEXT GENERATED ALWAYS AS (txt || '!') STORED)", 0).await?;
    let mut import = db.import_rows(&table("main", "t"), options()).await?;
    import
        .send(vec![
            vec![
                ("id".into(), text(i64::MAX)),
                ("txt".into(), text("a\0b'; DROP TABLE t; --")),
                ("n".into(), text("9007199254740993")),
                ("raw".into(), text("00042")),
                ("blob".into(), Cell::Binary(vec![0, 255, 128])),
            ],
            vec![
                ("id".into(), text(i64::MIN)),
                ("optional".into(), Cell::Null),
                ("blob".into(), Cell::Binary(vec![])),
            ],
        ])
        .await?;
    assert_eq!(scalar(&observer, "SELECT count(*) FROM t").await?, "0");
    let queued_db = db.clone();
    let mut queued =
        tokio::spawn(async move { scalar(&queued_db, "SELECT count(*) FROM t").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut queued)
            .await
            .is_err()
    );
    import.send(vec![vec![]]).await?;
    assert_eq!(import.finish().await?, 3);
    assert_eq!(queued.await??, "3");
    let check = observer.query("SELECT id,typeof(id),txt,n,typeof(n),raw,typeof(raw),blob,typeof(blob),optional,generated FROM t WHERE id=9223372036854775807", 1).await?;
    assert_eq!(
        check[0].rows[0],
        vec![
            text(i64::MAX),
            text("integer"),
            text("a\0b'; DROP TABLE t; --"),
            text("9007199254740993"),
            text("integer"),
            text("00042"),
            text("text"),
            Cell::Binary(vec![0, 255, 128]),
            text("blob"),
            text("default"),
            text("a\0b'; DROP TABLE t; --!")
        ]
    );
    let check = observer
        .query(
            "SELECT optional,blob,typeof(blob) FROM t WHERE id=-9223372036854775808",
            1,
        )
        .await?;
    assert_eq!(
        check[0].rows[0],
        vec![Cell::Null, Cell::Binary(vec![]), text("blob")]
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn abandonment_idle_timeout_and_close_rollback_then_reuse() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY)", 0)
        .await?;
    let mut abandoned = db.import_rows(&table("main", "t"), options()).await?;
    abandoned.send(vec![row(1), row(2)]).await?;
    // Backpressure ensures earlier data reached the worker before abandonment.
    abandoned.send(vec![row(3)]).await?;
    abandoned.send(vec![row(4)]).await?;
    drop(abandoned);
    empty_reusable(&db).await?;
    let mut idle = db
        .import_rows(
            &table("main", "t"),
            RowImportOptions {
                idle_timeout_ms: 100,
                ..options()
            },
        )
        .await?;
    idle.send(vec![row(1)]).await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        idle.finish()
            .await
            .unwrap_err()
            .to_string()
            .contains("idle timeout")
    );
    empty_reusable(&db).await?;
    let mut closed = db.import_rows(&table("main", "t"), options()).await?;
    closed.send(vec![row(1)]).await?;
    closed.close().await?;
    empty_reusable(&db).await?;
    let mut fresh = db.import_rows(&table("main", "t"), options()).await?;
    fresh.send(vec![row(5)]).await?;
    assert_eq!(fresh.finish().await?, 1);
    Ok(())
}

#[tokio::test]
async fn row_failures_generated_columns_and_bounds_roll_back_all_batches() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,generated INTEGER GENERATED ALWAYS AS (id+1), stored INTEGER GENERATED ALWAYS AS (id+2) STORED)", 0).await?;
    for bad in [
        row(1),
        vec![("generated".into(), text(3))],
        vec![("stored".into(), text(4))],
        vec![("missing".into(), Cell::Null)],
        vec![("id".into(), text(8)), ("id".into(), text(9))],
    ] {
        let mut import = db.import_rows(&table("main", "t"), options()).await?;
        import.send(vec![row(1)]).await?;
        let _ = import.send(vec![row(2), bad]).await;
        assert!(import.finish().await.is_err());
        empty_reusable(&db).await?;
    }
    for batch in [
        vec![row(2), row(3), row(4)],
        vec![vec![("id".into(), text("x".repeat(2049)))]],
        vec![vec![("x".repeat(4096), Cell::Null)]],
    ] {
        let mut import = db.import_rows(&table("main", "t"), options()).await?;
        import.send(vec![row(1)]).await?;
        assert!(import.send(batch).await.is_err());
        assert!(import.finish().await.is_err());
        empty_reusable(&db).await?;
    }
    // Import overrides the schema's ROLLBACK policy with statement-level ABORT;
    // the worker still rolls back the entire import and preserves connection reuse.
    db.query(
        "DROP TABLE t; CREATE TABLE t(id INTEGER UNIQUE ON CONFLICT ROLLBACK)",
        0,
    )
    .await?;
    let mut import = db.import_rows(&table("main", "t"), options()).await?;
    import.send(vec![row(1), row(1)]).await?;
    assert!(import.finish().await.is_err());
    empty_reusable(&db).await
}

#[tokio::test]
async fn deferred_commit_failure_rolls_back_and_reuses_connection() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE t(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)", 0).await?;
    let mut import = db.import_rows(&table("main", "t"), options()).await?;
    import.send(vec![row(1)]).await?;
    let error = import.finish().await.unwrap_err().to_string();
    assert!(error.contains("COMMIT failed"), "{error}");
    empty_reusable(&db).await?;
    db.query("INSERT INTO parent VALUES(1)", 0).await?;
    let mut import = db.import_rows(&table("main", "t"), options()).await?;
    import.send(vec![row(1)]).await?;
    assert_eq!(import.finish().await?, 1);
    Ok(())
}

#[tokio::test]
async fn producer_and_database_cancellation_during_batches_preserve_reuse() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY)", 0)
        .await?;
    let mut import = db.import_rows(&table("main", "t"), options()).await?;
    import.send(vec![row(1)]).await?;
    import.cancel();
    assert!(import.finish().await.is_err());
    empty_reusable(&db).await?;
    let mut import = db.import_rows(&table("main", "t"), options()).await?;
    let (ready, started) = tokio::sync::oneshot::channel();
    let producer = tokio::spawn(async move {
        for id in 0..64 {
            import.send(vec![row(id)]).await?;
        }
        let _ = ready.send(());
        for id in 64..100_000 {
            import.send(vec![row(id)]).await?;
        }
        import.finish().await
    });
    started.await?;
    db.cancel().await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(10), producer)
            .await??
            .is_err()
    );
    empty_reusable(&db).await?;
    let mut fresh = db.import_rows(&table("main", "t"), options()).await?;
    fresh.send(vec![row(7)]).await?;
    assert_eq!(fresh.finish().await?, 1);
    assert_eq!(scalar(&db, "SELECT id FROM t").await?, "7");
    Ok(())
}

#[tokio::test]
async fn caller_transactions_and_readonly_modes_are_rejected_without_side_effects() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY); BEGIN; INSERT INTO t VALUES(99)",
        0,
    )
    .await?;
    assert!(
        db.import_rows(&table("main", "t"), options())
            .await
            .is_err()
    );
    assert!(db.in_transaction().await?);
    assert_eq!(scalar(&db, "SELECT id FROM t").await?, "99");
    db.rollback().await?;
    empty_reusable(&db).await?;
    let readonly = Database::connect(&fixture.profile(true), "").await?;
    assert!(
        readonly
            .import_rows(&table("main", "t"), options())
            .await
            .is_err()
    );
    db.query("PRAGMA query_only=ON", 0).await?;
    assert!(
        db.import_rows(&table("main", "t"), options())
            .await
            .is_err()
    );
    db.query("PRAGMA query_only=OFF", 0).await?;
    empty_reusable(&db).await?;
    db.query("PRAGMA journal_mode=OFF", 1).await?;
    assert!(
        db.import_rows(&table("main", "t"), options())
            .await
            .is_err()
    );
    empty_reusable(&db).await
}

#[tokio::test]
async fn view_virtual_shadow_and_trigger_targets_are_rejected() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY); CREATE VIEW v AS SELECT * FROM t; CREATE VIRTUAL TABLE search USING fts5(body)", 0).await?;
    for name in ["v", "search", "search_data", "sqlite_schema", "missing"] {
        assert!(
            db.import_rows(&table("main", name), options())
                .await
                .is_err(),
            "accepted {name}"
        );
        assert!(!db.in_transaction().await?);
    }
    db.query(
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO t VALUES(new.id+1); END",
        0,
    )
    .await?;
    assert!(
        db.import_rows(&table("main", "t"), options())
            .await
            .is_err()
    );
    db.query("DROP TRIGGER tr; CREATE TEMP TRIGGER tr AFTER INSERT ON main.t BEGIN INSERT INTO t VALUES(new.id+1); END", 0).await?;
    assert!(
        db.import_rows(&table("main", "t"), options())
            .await
            .is_err()
    );
    empty_reusable(&db).await
}

#[tokio::test]
async fn resolved_temp_and_attached_names_stay_pinned_until_finish() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY); CREATE TEMP TABLE t(id INTEGER PRIMARY KEY); ATTACH ':memory:' AS aux; CREATE TABLE aux.t(id INTEGER PRIMARY KEY)", 0).await?;
    for (schema, id) in [("", 1), ("main", 2), ("aux", 3)] {
        let mut import = db.import_rows(&table(schema, "t"), options()).await?;
        import.send(vec![row(id)]).await?;
        assert_eq!(import.finish().await?, 1);
    }
    assert_eq!(scalar(&db, "SELECT id FROM temp.t").await?, "1");
    assert_eq!(scalar(&db, "SELECT id FROM main.t").await?, "2");
    assert_eq!(scalar(&db, "SELECT id FROM aux.t").await?, "3");
    let mut import = db.import_rows(&table("aux", "t"), options()).await?;
    import.send(vec![row(4)]).await?;
    let queued_db = db.clone();
    let mut replacement = tokio::spawn(async move {
        // Replacement cannot interleave, even when its catalog would look equal.
        assert_eq!(
            scalar(&queued_db, "SELECT group_concat(id) FROM aux.t").await?,
            "3,4"
        );
        queued_db
            .query(
                "DETACH aux; ATTACH ':memory:' AS aux; CREATE TABLE aux.t(id INTEGER PRIMARY KEY)",
                0,
            )
            .await?;
        Ok::<_, anyhow::Error>(())
    });
    ensure!(
        tokio::time::timeout(Duration::from_millis(50), &mut replacement)
            .await
            .is_err()
    );
    assert_eq!(import.finish().await?, 1);
    replacement.await??;
    let mut fresh = db.import_rows(&table("aux", "t"), options()).await?;
    fresh.send(vec![row(5)]).await?;
    assert_eq!(fresh.finish().await?, 1);
    assert_eq!(scalar(&db, "SELECT id FROM aux.t").await?, "5");
    Ok(())
}

#[tokio::test]
async fn abandoned_preflight_waiting_for_write_lock_cleans_up_before_reuse() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    let blocker = fixture.connect().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY)", 0)
        .await?;
    blocker
        .query("BEGIN IMMEDIATE; INSERT INTO t VALUES(99)", 0)
        .await?;
    let importer = db.clone();
    let pending =
        tokio::spawn(async move { importer.import_rows(&table("main", "t"), options()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !pending.is_finished(),
        "import preflight did not wait for the write lock"
    );
    pending.abort();
    assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
    blocker.rollback().await?;
    empty_reusable(&db).await?;
    let mut fresh = db.import_rows(&table("main", "t"), options()).await?;
    fresh.send(vec![row(1)]).await?;
    assert_eq!(fresh.finish().await?, 1);
    Ok(())
}

async fn reject_schema_replace(conflicting: ImportRow, constraint: &str) -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    let observer = fixture.connect().await?;
    db.query(
        "CREATE TABLE parent(id INTEGER PRIMARY KEY ON CONFLICT REPLACE, token TEXT UNIQUE ON CONFLICT REPLACE DEFAULT 'existing', note TEXT NOT NULL ON CONFLICT REPLACE DEFAULT 'fallback');
         CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE);
         INSERT INTO parent VALUES(1,'existing','precious'); INSERT INTO child VALUES(7,1)",
        0,
    ).await?;
    let mut import = db.import_rows(&table("main", "parent"), options()).await?;
    import
        .send(vec![vec![
            ("id".into(), text(10)),
            ("token".into(), text("earlier batch")),
            ("note".into(), text("must roll back")),
        ]])
        .await?;
    import
        .send(vec![
            vec![
                ("id".into(), text(11)),
                ("token".into(), text("earlier row")),
                ("note".into(), text("must also roll back")),
            ],
            conflicting,
        ])
        .await?;
    let applied = import.finish().await;
    let parents = observer
        .query("SELECT id,token,note FROM parent ORDER BY id", 10)
        .await?[0]
        .rows
        .clone();
    let children = observer
        .query("SELECT id,parent_id FROM child ORDER BY id", 10)
        .await?[0]
        .rows
        .clone();
    assert!(
        applied.is_err(),
        "insert-only import reported {applied:?}; parents={parents:?}; children={children:?}"
    );
    let error = applied.unwrap_err().to_string();
    assert!(
        error.contains(constraint),
        "expected native {constraint}, not a rows_affected workaround: {error}"
    );
    assert_eq!(
        parents,
        vec![vec![text(1), text("existing"), text("precious")]]
    );
    assert_eq!(children, vec![vec![text(7), text(1)]]);
    assert!(!db.in_transaction().await?);
    let mut fresh = db.import_rows(&table("main", "parent"), options()).await?;
    fresh
        .send(vec![vec![
            ("id".into(), text(20)),
            ("token".into(), text("fresh")),
        ]])
        .await?;
    assert_eq!(fresh.finish().await?, 1);
    assert_eq!(
        scalar(&observer, "SELECT note FROM parent WHERE id=20").await?,
        "fallback"
    );
    Ok(())
}

#[tokio::test]
async fn schema_replace_primary_key_cannot_delete_parent_or_cascade_children() -> Result<()> {
    reject_schema_replace(
        vec![
            ("id".into(), text(1)),
            ("token".into(), text("replacement")),
            ("note".into(), text("replacement")),
        ],
        "UNIQUE constraint failed: parent.id",
    )
    .await
}

#[tokio::test]
async fn schema_replace_unique_cannot_delete_parent_or_cascade_children() -> Result<()> {
    reject_schema_replace(
        vec![
            ("id".into(), text(2)),
            ("token".into(), text("existing")),
            ("note".into(), text("replacement")),
        ],
        "UNIQUE constraint failed: parent.token",
    )
    .await
}

#[tokio::test]
async fn schema_replace_default_values_cannot_delete_parent_or_cascade_children() -> Result<()> {
    reject_schema_replace(vec![], "UNIQUE constraint failed: parent.token").await
}

#[tokio::test]
async fn schema_replace_not_null_cannot_silently_substitute_default() -> Result<()> {
    reject_schema_replace(
        vec![
            ("id".into(), text(2)),
            ("token".into(), text("new")),
            ("note".into(), Cell::Null),
        ],
        "NOT NULL constraint failed: parent.note",
    )
    .await
}

#[tokio::test]
async fn schema_ignore_fail_and_rollback_produce_native_constraint_errors_and_full_rollback()
-> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.connect().await?;
    for policy in ["IGNORE", "FAIL", "ROLLBACK"] {
        db.query(
            &format!(
                "CREATE TABLE t(id INTEGER UNIQUE ON CONFLICT {policy}); INSERT INTO t VALUES(1)"
            ),
            0,
        )
        .await?;
        let mut import = db.import_rows(&table("main", "t"), options()).await?;
        import.send(vec![row(2)]).await?;
        import.send(vec![row(3), row(1)]).await?;
        let error = import.finish().await.unwrap_err().to_string();
        assert!(
            error.contains("UNIQUE constraint failed"),
            "{policy} must be overridden with ABORT, not a rows_affected workaround: {error}"
        );
        assert_eq!(
            db.query("SELECT id FROM t ORDER BY id", 10).await?[0].rows,
            vec![vec![text(1)]]
        );
        assert!(!db.in_transaction().await?);
        db.query("DROP TABLE t", 0).await?;
    }
    Ok(())
}
