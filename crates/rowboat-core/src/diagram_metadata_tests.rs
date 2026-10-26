use super::*;

async fn fixture() -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "pass22_diagram_lease_{}.sqlite",
        uuid::Uuid::new_v4().simple()
    ));
    let db = Database::connect(
        &ConnectionProfile {
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: path.clone(),
                mode: SqliteOpenMode::CreateIfMissing,
            }),
            ..Default::default()
        },
        "",
    )
    .await
    .unwrap();
    (db, path)
}

#[tokio::test]
async fn diagram_aborted_owned_read_retires_instead_of_lending_uncertain_session() {
    let (db, path) = fixture().await;
    // Pause the worker after acquiring its session lease, before native I/O.
    let running = db.inner.cancel_gate.lock().await;
    let read = {
        let db = db.clone();
        tokio::spawn(async move {
            db.diagram_metadata(
                &TableRef {
                    schema: "main".into(),
                    name: "not_read".into(),
                },
                &DiagramCancellation::default(),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while db.inner.session.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    drop(running);
    assert!(matches!(db.inner.session.lock().await.conn, Conn::Closed));
    assert!(db.query("SELECT 1", 1).await.is_err());
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn diagram_cancelled_owned_read_holds_busy_lease_until_drained() {
    let (db, path) = fixture().await;
    let running = db.inner.cancel_gate.lock().await;
    let cancellation = DiagramCancellation::default();
    let read = {
        let db = db.clone();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            db.diagram_metadata(
                &TableRef {
                    schema: "main".into(),
                    name: "not_read".into(),
                },
                &cancel,
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while db.inner.session.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    tokio::task::yield_now().await;
    assert!(!read.is_finished());
    assert!(db.inner.session.try_lock().is_err());
    drop(running);
    let error = read.await.unwrap().unwrap_err();
    assert!(error.cancelled);
    assert!(!error.session_retired);
    assert!(db.query("SELECT 1", 1).await.is_ok());
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn diagram_cancelled_queue_never_touches_the_held_session() {
    let path = std::env::temp_dir().join(format!(
        "pass22_diagram_queue_{}.sqlite",
        uuid::Uuid::new_v4().simple()
    ));
    let db = Database::connect(
        &ConnectionProfile {
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: path.clone(),
                mode: SqliteOpenMode::CreateIfMissing,
            }),
            ..Default::default()
        },
        "",
    )
    .await
    .unwrap();
    let held = db.inner.session.lock().await;
    let cancel = DiagramCancellation::default();
    let read = {
        let db = db.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            db.diagram_metadata(
                &TableRef {
                    schema: "main".into(),
                    name: "not_read".into(),
                },
                &cancel,
            )
            .await
        })
    };
    tokio::task::yield_now().await;
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), read)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.cancelled);
    assert!(!error.session_retired);
    assert!(!matches!(held.conn, Conn::Closed));
    drop(held);
    assert!(db.query("SELECT 1", 1).await.is_ok());
    drop(db);
    std::fs::remove_file(path).unwrap();
}
