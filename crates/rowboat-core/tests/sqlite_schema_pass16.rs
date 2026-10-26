use anyhow::Result;
use rowboat_core::*;
use std::{path::PathBuf, time::Duration};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn fixture() -> Result<(Fixture, Database)> {
    let f = Fixture(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .join(format!("pass16_admin_{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&f.0)?;
    let db = Database::connect(
        &ConnectionProfile {
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: f.0.join("test.sqlite"),
                mode: SqliteOpenMode::CreateIfMissing,
            }),
            ..Default::default()
        },
        "",
    )
    .await?;
    Ok((f, db))
}

#[tokio::test]
async fn abandoned_administration_rolls_back_before_queued_query() -> Result<()> {
    let (_f, db) = fixture().await?;
    let mut op = db.administrative_operation(None).await?;
    let monitor = op.monitor();
    op.execute("CREATE TABLE never_committed(id)").await?;
    let peer_db = db.clone();
    let peer = tokio::spawn(async move {
        peer_db
            .query("CREATE TABLE peer(id); INSERT INTO peer VALUES(1)", 0)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!peer.is_finished());
    drop(op);
    let outcome = monitor.wait().await?;
    assert_eq!(
        outcome.status,
        AdministrativeStatus::RolledBack,
        "{outcome:?}"
    );
    assert!(outcome.rolled_back && outcome.reusable);
    tokio::time::timeout(Duration::from_secs(10), peer).await???;
    assert_eq!(
        db.query(
            "SELECT count(*) FROM sqlite_schema WHERE name='never_committed'",
            1
        )
        .await?[0]
            .rows[0][0]
            .display(),
        "0"
    );
    assert_eq!(
        db.query("SELECT count(*) FROM peer", 1).await?[0].rows[0][0].display(),
        "1"
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn hidden_commit_and_error_cannot_escape_owned_transaction() -> Result<()> {
    let (_f, db) = fixture().await?;
    let mut op = db.administrative_operation(None).await?;
    op.execute("CREATE TABLE first(id)").await?;
    assert!(
        op.execute("CREATE TABLE hidden(id); COMMIT; CREATE TABLE escaped(id)")
            .await
            .is_err()
    );
    let outcome = op.close().await?;
    assert_eq!(outcome.status, AdministrativeStatus::RolledBack);
    assert_eq!(
        db.query("SELECT count(*) FROM sqlite_schema WHERE type='table'", 1)
            .await?[0]
            .rows[0][0]
            .display(),
        "0"
    );
    let mut op = db.administrative_operation(None).await?;
    op.execute("CREATE TABLE successful(id)").await?;
    assert_eq!(op.finish().await?.status, AdministrativeStatus::Committed);
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn cancelled_inflight_inspection_has_actual_rollback_and_reusable_peer() -> Result<()> {
    let (_f, db) = fixture().await?;
    let mut op = db.administrative_operation(None).await?;
    op.execute("CREATE TABLE cancelled(id)").await?;
    let monitor = op.monitor();
    let task = tokio::spawn(async move {
        op.query("SELECT sum(a.value*b.value) FROM json_each('[1,2,3,4,5,6,7,8,9]') a CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') b CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') c CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') d CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') e CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') f CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') g CROSS JOIN json_each('[1,2,3,4,5,6,7,8,9]') h",1).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let peer_db = db.clone();
    let peer = tokio::spawn(async move {
        peer_db
            .query(
                "SELECT count(*) FROM sqlite_schema WHERE name='cancelled'",
                1,
            )
            .await
    });
    db.cancel().await?;
    let outcome = tokio::time::timeout(Duration::from_secs(10), monitor.wait()).await??;
    assert_eq!(
        outcome.status,
        AdministrativeStatus::RolledBack,
        "{outcome:?}"
    );
    assert!(outcome.rolled_back && outcome.reusable);
    assert!(task.await?.is_err());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), peer).await???[0].rows[0][0].display(),
        "0"
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn readonly_native_explain_compiles_dml_but_never_authorizes_writes() -> Result<()> {
    let (f, db) = fixture().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO t VALUES(1,'original'); CREATE TABLE audit(id); CREATE TRIGGER audit_t AFTER UPDATE ON t BEGIN INSERT INTO audit VALUES(new.id); END", 0).await?;
    let ro = Database::connect(
        &ConnectionProfile {
            engine: Engine::Sqlite,
            read_only: true,
            file: Some(SqliteFileOptions {
                path: f.0.join("test.sqlite"),
                mode: SqliteOpenMode::OpenExisting,
            }),
            ..Default::default()
        },
        "",
    )
    .await?;
    for sql in [
        "EXPLAIN QUERY PLAN INSERT INTO t VALUES(2,'new')",
        "EXPLAIN QUERY PLAN UPDATE t SET value='changed'",
        "EXPLAIN QUERY PLAN DELETE FROM t",
        "EXPLAIN INSERT INTO t VALUES(2,'new')",
        "EXPLAIN QUERY PLAN WITH input AS (SELECT 2 id) INSERT INTO t SELECT id,'new' FROM input",
        "/* not a lexical prefix exemption */ EXPLAIN QUERY PLAN UPDATE t SET value='changed'",
    ] {
        ro.query(sql, 1000).await?;
    }
    for sql in [
        "INSERT INTO t VALUES(2,'new')",
        "WITH input AS (SELECT 2 id) INSERT INTO t SELECT id,'new' FROM input",
        "UPDATE t SET value='changed' RETURNING *",
        "DELETE FROM t",
        "EXPLAIN QUERY PLAN SELECT * FROM t; INSERT INTO t VALUES(2,'new')",
        "EXPLAIN QUERY PLAN UPDATE t SET value='changed'; DELETE FROM t",
        "SELECT 'EXPLAIN QUERY PLAN'; UPDATE t SET value='changed'",
        "PRAGMA query_only=OFF",
        "PRAGMA writable_schema=ON",
        "ATTACH ':memory:' AS aux",
        "CREATE TEMP TABLE bypass(id)",
        "DROP TABLE t",
    ] {
        assert!(
            ro.query(sql, 1000).await.is_err(),
            "unexpected write authorization: {sql}"
        );
    }
    assert_eq!(
        db.query("SELECT value FROM t", 1).await?[0].rows[0][0].display(),
        "original"
    );
    assert_eq!(
        db.query("SELECT count(*) FROM t", 1).await?[0].rows[0][0].display(),
        "1"
    );
    assert_eq!(
        db.query("SELECT count(*) FROM audit", 1).await?[0].rows[0][0].display(),
        "0"
    );
    assert_eq!(
        ro.query("PRAGMA query_only", 1).await?[0].rows[0][0].display(),
        "1"
    );
    assert!(!ro.in_transaction().await?);
    Ok(())
}
