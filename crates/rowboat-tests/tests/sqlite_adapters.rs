//! Independent public-facade acceptance on disposable embedded database files.
use anyhow::{Result, ensure};
use rowboat_core::{
    BrowseRequest, Cell, ConnectionProfile, Database, Engine, QueryEditPlan, QueryEditSource,
    QueryEvent, QueryResult, RowChange, SqliteFileOptions, SqliteOpenMode, TableRef,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local/pass12-independent")
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&directory)?;
        Ok(Self(directory))
    }
    fn profile(&self, mode: SqliteOpenMode) -> ConnectionProfile {
        ConnectionProfile {
            name: "Independent SQLite adapter acceptance".into(),
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: self.0.join("database with spaces.sqlite"),
                mode,
            }),
            ..Default::default()
        }
    }
    async fn create(&self) -> Result<Database> {
        Database::connect(&self.profile(SqliteOpenMode::CreateIfMissing), "").await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let results = db.query(sql, 10).await?;
    Ok(results[0].rows[0][0].display())
}

async fn capture(db: &Database, sql: &str) -> Result<(Vec<QueryResult>, QueryEditSource)> {
    let (mut stream, proof) = db
        .query_stream_with_edit_source(sql, Default::default())
        .await?;
    let mut results: Vec<QueryResult> = Vec::new();
    let mut completed = false;
    while let Some(event) = stream.recv().await {
        match event {
            QueryEvent::Schema {
                result_index,
                columns,
                ..
            } => {
                ensure!(result_index == results.len());
                results.push(QueryResult {
                    columns,
                    ..Default::default()
                });
            }
            QueryEvent::Rows { result_index, rows } => results[result_index].rows.extend(rows),
            QueryEvent::ResultEnd {
                result_index,
                rows_affected,
                elapsed_ms,
                truncated,
                ..
            } => {
                let result = &mut results[result_index];
                result.rows_affected = rows_affected;
                result.elapsed_ms = elapsed_ms;
                result.truncated = truncated;
            }
            QueryEvent::Complete { .. } => completed = true,
            QueryEvent::Error { message, .. } => anyhow::bail!("{message}"),
        }
    }
    ensure!(completed);
    stream.close().await?;
    Ok((results, proof.finish().await))
}

fn update(plan: &QueryEditPlan, row: usize, column: &str, value: &str) -> RowChange {
    RowChange::Update {
        original: plan
            .result()
            .columns
            .iter()
            .zip(&plan.result().rows[row])
            .map(|(column, value)| (column.name.clone(), value.clone()))
            .collect(),
        values: vec![(column.into(), Cell::Text(value.into()))],
    }
}

#[tokio::test]
async fn file_open_modes_and_runtime_readonly_enforcement() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut profile = fixture.profile(SqliteOpenMode::OpenExisting);
    assert!(Database::connect(&profile, "").await.is_err());
    assert!(!profile.file.as_ref().unwrap().path.exists());
    let writer = fixture.create().await?;
    writer
        .query("CREATE TABLE records (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO records VALUES (1,'before')", 10)
        .await?;
    profile.read_only = true;
    let reader = Database::connect(&profile, "").await?;
    assert_eq!(scalar(&reader, "SELECT note FROM records").await?, "before");
    assert!(reader.query("DELETE FROM records", 10).await.is_err());
    // Even if setting PRAGMA is harmlessly refused/no-op, it cannot enable writes.
    let _ = reader.query("PRAGMA query_only=OFF", 10).await;
    assert!(
        reader
            .query("UPDATE records SET note='escaped'", 10)
            .await
            .is_err()
    );
    let attached = fixture.0.join("readonly escape.sqlite");
    let attach = format!(
        "ATTACH DATABASE '{}' AS escape",
        attached.to_string_lossy().replace('\'', "''")
    );
    assert!(reader.query(&attach, 10).await.is_err());
    assert!(!attached.exists());
    assert_eq!(scalar(&writer, "SELECT note FROM records").await?, "before");
    profile.file.as_mut().unwrap().mode = SqliteOpenMode::CreateIfMissing;
    assert!(Database::connect(&profile, "").await.is_err());
    Ok(())
}

#[tokio::test]
async fn actual_catalog_resolution_and_transaction_ownership() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.create().await?;
    db.query("CREATE TABLE records (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO records VALUES (1,'main'); CREATE TEMP TABLE records(id INTEGER PRIMARY KEY, note TEXT); INSERT INTO temp.records VALUES (2,'temporary'); ATTACH DATABASE ':memory:' AS extra; CREATE TABLE extra.attached_only (id INTEGER PRIMARY KEY)", 10).await?;
    let catalog = db.completion_catalog(&["extra".into()]).await?;
    assert_eq!(&catalog.search_path[..2], &["temp", "main"]);
    assert!(catalog.search_path.iter().any(|s| s == "extra"));
    for schema in ["main", "temp"] {
        assert!(
            catalog
                .tables
                .iter()
                .any(|t| t.table.schema == schema && t.table.name == "records")
        );
    }
    assert_eq!(scalar(&db, "SELECT note FROM records").await?, "temporary");
    let table = TableRef {
        schema: "extra".into(),
        name: "attached_only".into(),
    };
    assert_eq!(db.completion_column_names(&table).await?, vec!["id"]);
    assert!(!db.in_transaction().await?);
    assert!(
        !db.in_transaction().await?,
        "state inspection must not begin a transaction"
    );
    db.begin().await?;
    assert!(db.in_transaction().await?);
    db.query("INSERT INTO main.records VALUES (3,'rolled back')", 10)
        .await?;
    db.rollback().await?;
    assert!(!db.in_transaction().await?);
    assert_eq!(scalar(&db, "SELECT count(*) FROM main.records").await?, "1");
    Ok(())
}

#[tokio::test]
async fn aliased_composite_key_review_apply_and_conflict_binding() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.create().await?;
    db.query("CREATE TABLE records (tenant INTEGER NOT NULL, id INTEGER NOT NULL, note TEXT, omitted TEXT, PRIMARY KEY(tenant,id)) WITHOUT ROWID; INSERT INTO records VALUES (7,1,'before','keep'),(8,1,'other tenant','keep too')", 10).await?;
    let (results, source) = capture(
        &db,
        "SELECT tenant AS account, id AS key_value, note AS displayed FROM records ORDER BY tenant",
    )
    .await?;
    let mut changed_result = results[0].clone();
    changed_result.rows[0][0] = Cell::Text("8".into());
    assert!(
        source.plan(&changed_result).is_err(),
        "result contents are execution-bound"
    );
    let plan = source.plan(&results[0])?;
    let changes = [update(&plan, 0, "note", "O'Reilly reviewed")];
    let preview = plan.preview_changes(&changes)?;
    assert!(preview.starts_with("UPDATE ") && preview.contains("WHERE"));
    assert!(!preview.contains("displayed"));
    assert_eq!(
        scalar(&db, "SELECT note FROM records WHERE tenant=7").await?,
        "before"
    );
    let other = Database::connect(&fixture.profile(SqliteOpenMode::OpenExisting), "").await?;
    assert!(other.apply_query_changes(&plan, &changes).await.is_err());
    assert_eq!(db.apply_query_changes(&plan, &changes).await?, 1);
    assert_eq!(
        scalar(&other, "SELECT note FROM records WHERE tenant=7").await?,
        "O'Reilly reviewed"
    );
    assert_eq!(
        scalar(&db, "SELECT note FROM records WHERE tenant=8").await?,
        "other tenant"
    );
    assert_eq!(
        scalar(&db, "SELECT omitted FROM records WHERE tenant=7").await?,
        "keep"
    );
    assert!(
        db.apply_query_changes(&plan, &changes).await.is_err(),
        "stale original values must conflict"
    );
    let (rows, source) = capture(&db, "SELECT id,note FROM records").await?;
    assert!(
        source.plan(&rows[0]).is_err(),
        "partial composite keys must be read-only"
    );
    Ok(())
}

#[tokio::test]
async fn storage_class_changes_cannot_hide_behind_equal_display_values() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.create().await?;
    db.query("CREATE TABLE records (id INTEGER PRIMARY KEY, value, note TEXT); INSERT INTO records VALUES (1,1,'before'),(2,x'0031ff',NULL)", 10).await?;
    let table = TableRef {
        schema: "main".into(),
        name: "records".into(),
    };
    let rows = db
        .browse(
            &table,
            &BrowseRequest {
                sort: Some(("id".into(), true)),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(rows.rows[1][1], Cell::Binary(vec![0, 49, 255]));
    assert_eq!(rows.rows[1][2], Cell::Null);
    let (captured, source) = capture(&db, "SELECT id,value,note FROM records ORDER BY id").await?;
    let plan = source.plan(&captured[0])?;
    let changes = [update(&plan, 0, "note", "must not write")];
    let concurrent = Database::connect(&fixture.profile(SqliteOpenMode::OpenExisting), "").await?;
    concurrent
        .query("UPDATE records SET value='1' WHERE id=1", 10)
        .await?;
    assert_eq!(
        scalar(&concurrent, "SELECT typeof(value) FROM records WHERE id=1").await?,
        "text"
    );
    assert!(
        db.apply_query_changes(&plan, &changes).await.is_err(),
        "INTEGER 1 and TEXT '1' have different original-value evidence"
    );
    assert_eq!(
        scalar(&db, "SELECT note FROM records WHERE id=1").await?,
        "before"
    );
    Ok(())
}

#[tokio::test]
async fn replaced_source_and_new_shadow_cannot_retarget_a_captured_update() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.create().await?;
    db.query("CREATE TABLE records (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO records VALUES (1,'before')", 10).await?;
    let (rows, source) = capture(&db, "SELECT id,note FROM records").await?;
    let plan = source.plan(&rows[0])?;
    let changes = [update(&plan, 0, "note", "reviewed")];
    db.query("CREATE TEMP TABLE records (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO temp.records VALUES (1,'before')", 10).await?;
    // A conservative invalidation is acceptable; retargeting the new shadow is not.
    let _ = db.apply_query_changes(&plan, &changes).await;
    assert_eq!(
        scalar(&db, "SELECT note FROM temp.records").await?,
        "before"
    );
    db.query(
        "DROP TABLE temp.records; UPDATE main.records SET note='before'",
        10,
    )
    .await?;
    let (rows, source) = capture(&db, "SELECT id,note FROM main.records").await?;
    let plan = source.plan(&rows[0])?;
    let changes = [update(&plan, 0, "note", "must not write")];
    let other = Database::connect(&fixture.profile(SqliteOpenMode::OpenExisting), "").await?;
    other.query("DROP TABLE records; CREATE TABLE records (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO records VALUES (1,'before')", 10).await?;
    assert!(
        db.apply_query_changes(&plan, &changes).await.is_err(),
        "an identical replacement is still a different source"
    );
    assert_eq!(scalar(&other, "SELECT note FROM records").await?, "before");
    Ok(())
}

#[tokio::test]
async fn live_connection_interrupt_does_not_cancel_the_next_query() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.create().await?;
    let worker = db.clone();
    let query = tokio::spawn(async move {
        worker.query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n", 10).await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    tokio::time::timeout(Duration::from_secs(5), db.cancel()).await??;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), query)
            .await??
            .is_err()
    );
    assert_eq!(scalar(&db, "SELECT 42").await?, "42");
    assert!(!db.in_transaction().await?);
    Ok(())
}
