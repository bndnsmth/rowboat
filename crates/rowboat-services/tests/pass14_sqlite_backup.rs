use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, QueryEditPlan, QueryEvent, QueryResult, RowChange,
    SqliteFileOptions, SqliteOpenMode,
};
use rowboat_services::{
    ClientRuntime, ServiceOperation, UnsupportedServiceOperation, backup, restore,
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    time::{Duration, Instant},
};

fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pass14_backup_")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}
fn profile(path: &Path) -> ConnectionProfile {
    ConnectionProfile {
        name: "Pass14 backup fixture".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.into(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    }
}
async fn connect(profile: &ConnectionProfile) -> Database {
    Database::connect(profile, "").await.unwrap()
}
async fn sql(database: &Database, statement: &str) {
    database.query(statement, 10).await.unwrap();
}
async fn rows(database: &Database, statement: &str) -> Vec<Vec<Cell>> {
    database.query(statement, 10).await.unwrap().remove(0).rows
}
fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

async fn edit_plan(db: &Database) -> QueryEditPlan {
    let (mut stream, capture) = db
        .query_stream_with_edit_source("SELECT id,note FROM data", Default::default())
        .await
        .unwrap();
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
            QueryEvent::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    stream.close().await.unwrap();
    capture.finish().await.plan(&result).unwrap()
}
fn change(plan: &QueryEditPlan) -> RowChange {
    RowChange::Update {
        original: plan
            .result()
            .columns
            .iter()
            .zip(&plan.result().rows[0])
            .map(|(column, value)| (column.name.clone(), value.clone()))
            .collect(),
        values: vec![("note".into(), Cell::Text("stale update".into()))],
    }
}

#[tokio::test]
async fn identical_restore_revokes_query_proofs_on_independent_and_symlink_handles() {
    let directory = directory();
    let path = directory.path().join("target.sqlite");
    let target = profile(&path);
    let db = connect(&target).await;
    sql(&db, "PRAGMA journal_mode=WAL; CREATE TABLE data(id INTEGER PRIMARY KEY,note TEXT); INSERT INTO data VALUES(1,'original')").await;
    let independent = connect(&target).await;
    let alias = directory.path().join("alias.sqlite");
    symlink(&path, &alias).unwrap();
    let alias_handle = connect(&profile(&alias)).await;
    let primary_plan = edit_plan(&db).await;
    let independent_plan = edit_plan(&independent).await;
    let alias_plan = edit_plan(&alias_handle).await;
    let cookie_before = rows(&db, "PRAGMA schema_version").await;
    let snapshot = directory.path().join("snapshot.sqlite");
    backup(&target, "", &snapshot, &ClientRuntime::Native)
        .await
        .unwrap();
    restore(&target, "", &snapshot, &ClientRuntime::Native)
        .await
        .unwrap();
    assert_ne!(rows(&db, "PRAGMA schema_version").await, cookie_before);
    for (handle, plan) in [
        (&db, primary_plan),
        (&independent, independent_plan),
        (&alias_handle, alias_plan),
    ] {
        let error = handle
            .apply_query_changes(&plan, &[change(&plan)])
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("identity") || error.to_string().contains("schema"),
            "{error:#}"
        );
        assert_eq!(
            rows(handle, "SELECT note FROM data").await,
            vec![vec![Cell::Text("original".into())]]
        );
    }
    let fresh = edit_plan(&independent).await;
    assert_eq!(
        independent
            .apply_query_changes(&fresh, &[change(&fresh)])
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn hardlinked_target_restore_rejects_before_mutation_even_with_an_alias_edit_proof() {
    let directory = directory();
    let path = directory.path().join("target.sqlite");
    let target = profile(&path);
    let db = connect(&target).await;
    sql(&db, "CREATE TABLE data(id INTEGER PRIMARY KEY,note TEXT); INSERT INTO data VALUES(1,'original')").await;
    let snapshot = directory.path().join("snapshot.sqlite");
    backup(&target, "", &snapshot, &ClientRuntime::Native)
        .await
        .unwrap();
    let alias = directory.path().join("alias.sqlite");
    fs::hard_link(&path, &alias).unwrap();
    let alias_handle = connect(&profile(&alias)).await;
    let _alias_plan = edit_plan(&alias_handle).await;
    let before = fs::read(&path).unwrap();
    let error = restore(&target, "", &snapshot, &ClientRuntime::Native)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("hard-linked"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        rows(&alias_handle, "SELECT note FROM data").await,
        vec![vec![Cell::Text("original".into())]]
    );
}

#[tokio::test]
async fn wal_backup_is_binary_consistent_private_and_does_not_modify_source() {
    let directory = directory();
    let source_path = directory.path().join("source ' 日本語.sqlite");
    let source = profile(&source_path);
    let db = connect(&source).await;
    sql(&db, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(id INTEGER PRIMARY KEY, bytes BLOB, text TEXT); INSERT INTO data VALUES(1, X'0001ff', '日本語'); CREATE VIEW items AS SELECT * FROM data; PRAGMA user_version=42").await;
    let wal = source_path.with_file_name(format!(
        "{}-wal",
        source_path.file_name().unwrap().to_string_lossy()
    ));
    assert!(fs::metadata(&wal).unwrap().len() > 0);
    let source_before = fs::read(&source_path).unwrap();
    let wal_before = fs::read(&wal).unwrap();
    let output = directory.path().join("binary backup ' 日本語.sqlite");
    backup(&source, "ignored secret\n", &output, &ClientRuntime::Native)
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(&fs::read(&output).unwrap()[..16], b"SQLite format 3\0");
    assert_eq!(fs::read(&source_path).unwrap(), source_before);
    assert_eq!(fs::read(&wal).unwrap(), wal_before);
    assert!(
        !names(directory.path())
            .iter()
            .any(|name| name.to_string_lossy().starts_with(".rowboat-backup-"))
    );
    let copied = connect(&profile(&output)).await;
    assert_eq!(
        rows(&copied, "SELECT * FROM items").await,
        rows(&db, "SELECT * FROM items").await
    );
    assert_eq!(
        rows(&copied, "PRAGMA user_version").await,
        vec![vec![Cell::Text("42".into())]]
    );
    assert_eq!(
        rows(&copied, "PRAGMA integrity_check").await,
        vec![vec![Cell::Text("ok".into())]]
    );
    sql(&db, "INSERT INTO data VALUES(2, X'', 'later')").await;
    assert_eq!(
        rows(&copied, "SELECT count(*) FROM data").await,
        vec![vec![Cell::Text("1".into())]]
    );
}

#[tokio::test]
async fn orphan_wal_from_another_database_is_preserved_and_prevents_backup_publication() {
    let directory = directory();
    let source_path = directory.path().join("source.sqlite");
    let source = profile(&source_path);
    let db = connect(&source).await;
    sql(&db, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value); INSERT INTO data VALUES('correct backup')").await;
    let other = connect(&profile(&directory.path().join("other.sqlite"))).await;
    sql(&other, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value); INSERT INTO data VALUES('unrelated stale WAL')").await;
    let source_before = fs::read(&source_path).unwrap();
    let source_wal_before = fs::read(directory.path().join("source.sqlite-wal")).unwrap();
    let output = directory.path().join("output.sqlite");
    let orphan = directory.path().join("output.sqlite-wal");
    fs::copy(directory.path().join("other.sqlite-wal"), &orphan).unwrap();
    let orphan_before = fs::read(&orphan).unwrap();
    let orphan_inode = fs::metadata(&orphan).unwrap().ino();
    let before = names(directory.path());
    assert!(!output.exists());
    let error = backup(&source, "", &output, &ClientRuntime::Native)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("-wal"), "{error:#}");
    assert!(!output.exists());
    assert_eq!(names(directory.path()), before);
    assert_eq!(fs::read(&orphan).unwrap(), orphan_before);
    assert_eq!(fs::metadata(&orphan).unwrap().ino(), orphan_inode);
    assert_eq!(fs::read(&source_path).unwrap(), source_before);
    assert_eq!(
        fs::read(directory.path().join("source.sqlite-wal")).unwrap(),
        source_wal_before
    );

    // A separate, clean output still publishes and reopens as the source data.
    let clean = directory.path().join("clean.sqlite");
    backup(&source, "", &clean, &ClientRuntime::Native)
        .await
        .unwrap();
    let copied = connect(&profile(&clean)).await;
    assert_eq!(
        rows(&copied, "SELECT value FROM data").await,
        vec![vec![Cell::Text("correct backup".into())]]
    );
    assert_eq!(
        rows(&copied, "PRAGMA integrity_check").await,
        vec![vec![Cell::Text("ok".into())]]
    );
}

#[tokio::test]
async fn every_destination_sidecar_entry_rejects_backup_without_changes() {
    let directory = directory();
    let source_path = directory.path().join("source.sqlite");
    let source = profile(&source_path);
    let db = connect(&source).await;
    sql(
        &db,
        "CREATE TABLE data(value); INSERT INTO data VALUES('source')",
    )
    .await;
    let source_before = fs::read(&source_path).unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        for kind in ["file", "dangling-link", "source-link", "directory"] {
            let output = directory
                .path()
                .join(format!("output {kind} 日本語{suffix}.sqlite"));
            let mut sidecar = output.as_os_str().to_os_string();
            sidecar.push(suffix);
            let sidecar = std::path::PathBuf::from(sidecar);
            match kind {
                "file" => fs::write(&sidecar, b"preserve sidecar bytes").unwrap(),
                "dangling-link" => symlink(directory.path().join("missing"), &sidecar).unwrap(),
                "source-link" => symlink(&source_path, &sidecar).unwrap(),
                "directory" => fs::create_dir(&sidecar).unwrap(),
                _ => unreachable!(),
            }
            let before = names(directory.path());
            let metadata = fs::symlink_metadata(&sidecar).unwrap();
            let link = metadata
                .file_type()
                .is_symlink()
                .then(|| fs::read_link(&sidecar).unwrap());
            let error = backup(&source, "", &output, &ClientRuntime::Native)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(suffix), "{error:#}");
            assert!(!output.exists());
            assert_eq!(names(directory.path()), before);
            assert_eq!(
                fs::symlink_metadata(&sidecar).unwrap().ino(),
                metadata.ino()
            );
            if let Some(link) = link {
                assert_eq!(fs::read_link(&sidecar).unwrap(), link);
            } else if metadata.is_file() {
                assert_eq!(fs::read(&sidecar).unwrap(), b"preserve sidecar bytes");
            }
            assert_eq!(fs::read(&source_path).unwrap(), source_before);
        }
    }
}

#[tokio::test]
async fn sidecars_appearing_during_backup_reject_publication_and_clean_private_temporaries() {
    let directory = directory();
    let source_path = directory.path().join("source.sqlite");
    let source = profile(&source_path);
    let db = connect(&source).await;
    sql(
        &db,
        "CREATE TABLE data(value); INSERT INTO data VALUES('source')",
    )
    .await;
    let source_before = fs::read(&source_path).unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        let output = directory.path().join(format!("late{suffix}.sqlite"));
        let sidecar = directory
            .path()
            .join(format!("late{suffix}.sqlite{suffix}"));
        // Hold the source read lock so the worker cannot finish its snapshot
        // before this test injects a sidecar after the early destination check.
        sql(&db, "BEGIN EXCLUSIVE").await;
        let profile = source.clone();
        let destination = output.clone();
        let operation = tokio::spawn(async move {
            backup(&profile, "", &destination, &ClientRuntime::Native).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !names(directory.path())
                .iter()
                .any(|name| name.to_string_lossy().starts_with(".rowboat-backup-"))
            {
                assert!(
                    !operation.is_finished(),
                    "backup unexpectedly completed before source unlock"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        fs::write(&sidecar, b"late sidecar must survive").unwrap();
        let inode = fs::metadata(&sidecar).unwrap().ino();
        sql(&db, "ROLLBACK").await;
        let error = operation.await.unwrap().unwrap_err();
        assert!(error.to_string().contains(suffix), "{error:#}");
        assert!(!output.exists());
        assert_eq!(fs::read(&sidecar).unwrap(), b"late sidecar must survive");
        assert_eq!(fs::metadata(&sidecar).unwrap().ino(), inode);
        assert_eq!(fs::read(&source_path).unwrap(), source_before);
        assert!(
            !names(directory.path())
                .iter()
                .any(|name| name.to_string_lossy().starts_with(".rowboat-backup-"))
        );
    }
}

#[tokio::test]
async fn restore_is_in_place_wal_safe_and_preserves_active_reader_snapshot() {
    let directory = directory();
    let source = profile(&directory.path().join("source.sqlite"));
    let input = connect(&source).await;
    sql(&input, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value TEXT); INSERT INTO data VALUES('restored')").await;
    let target_path = directory.path().join("target.sqlite");
    let target = profile(&target_path);
    let writer = connect(&target).await;
    sql(&writer, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value TEXT); INSERT INTO data VALUES('old')").await;
    fs::set_permissions(&target_path, fs::Permissions::from_mode(0o640)).unwrap();
    let reader = connect(&target).await;
    sql(&reader, "BEGIN").await;
    let old = rows(&reader, "SELECT * FROM data").await;
    let inode = fs::metadata(&target_path).unwrap().ino();
    restore(
        &target,
        "",
        &source.file.as_ref().unwrap().path,
        &ClientRuntime::Native,
    )
    .await
    .unwrap();
    assert_eq!(fs::metadata(&target_path).unwrap().ino(), inode);
    assert_eq!(
        fs::metadata(&target_path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(rows(&reader, "SELECT * FROM data").await, old);
    assert_eq!(
        rows(&writer, "SELECT * FROM data").await,
        vec![vec![Cell::Text("restored".into())]]
    );
    sql(&reader, "COMMIT").await;
    assert_eq!(
        rows(&reader, "SELECT * FROM data").await,
        vec![vec![Cell::Text("restored".into())]]
    );
    let reopened = connect(&target).await;
    assert_eq!(
        rows(&reopened, "PRAGMA integrity_check").await,
        vec![vec![Cell::Text("ok".into())]]
    );
}

#[tokio::test]
async fn failures_never_overwrite_or_create_sources_or_targets_and_cleanup_private_files() {
    let directory = directory();
    let missing = directory.path().join("missing.sqlite");
    let output = directory.path().join("output.sqlite");
    let mut source = profile(&missing);
    for mode in [
        SqliteOpenMode::CreateIfMissing,
        SqliteOpenMode::OpenExisting,
    ] {
        source.file.as_mut().unwrap().mode = mode;
        assert!(
            backup(&source, "", &output, &ClientRuntime::Native)
                .await
                .is_err()
        );
        assert!(!missing.exists());
        assert!(!output.exists());
        assert!(names(directory.path()).is_empty());
    }
    let initialized = directory.path().join("initialized.sqlite");
    let valid = profile(&initialized);
    let db = connect(&valid).await;
    sql(&db, "CREATE TABLE data(value); INSERT INTO data VALUES(1)").await;
    assert!(
        restore(&source, "", &initialized, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert!(!missing.exists());
    fs::write(&output, b"existing destination").unwrap();
    assert!(
        backup(&valid, "", &output, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert_eq!(fs::read(&output).unwrap(), b"existing destination");
    let dangling = directory.path().join("dangling.sqlite");
    symlink(&missing, &dangling).unwrap();
    assert!(
        backup(&valid, "", &dangling, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert!(
        fs::symlink_metadata(&dangling)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let before = names(directory.path());
    assert!(
        backup(
            &profile(&output),
            "",
            &directory.path().join("invalid-copy"),
            &ClientRuntime::Native
        )
        .await
        .is_err()
    );
    assert_eq!(names(directory.path()), before);
}

#[tokio::test]
async fn symlinked_parent_directories_support_backup_restore_but_not_database_symlinks() {
    let directory = directory();
    let physical = directory.path().join("physical");
    fs::create_dir(&physical).unwrap();
    let alias = directory.path().join("directory alias");
    symlink(fs::canonicalize(&physical).unwrap(), &alias).unwrap();
    let source_path = alias.join("source ' 日本語.sqlite");
    let source = profile(&source_path);
    let db = connect(&source).await;
    sql(
        &db,
        "PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES('original')",
    )
    .await;
    let snapshot = alias.join("snapshot.sqlite");
    backup(&source, "", &snapshot, &ClientRuntime::Native)
        .await
        .unwrap();
    sql(&db, "UPDATE data SET value='changed'").await;
    restore(&source, "", &snapshot, &ClientRuntime::Native)
        .await
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT value FROM data").await,
        vec![vec![Cell::Text("original".into())]]
    );
    assert_eq!(
        rows(&db, "PRAGMA integrity_check").await,
        vec![vec![Cell::Text("ok".into())]]
    );

    // Resolving directory aliases must not accidentally allow leaf symlinks.
    let database_link = alias.join("database-link.sqlite");
    symlink(&source_path, &database_link).unwrap();
    let untouched = fs::read(&source_path).unwrap();
    let rejected = alias.join("rejected.sqlite");
    assert!(
        backup(
            &profile(&database_link),
            "",
            &rejected,
            &ClientRuntime::Native
        )
        .await
        .is_err()
    );
    assert!(!rejected.exists());
    assert!(
        restore(
            &profile(&database_link),
            "",
            &snapshot,
            &ClientRuntime::Native
        )
        .await
        .is_err()
    );
    assert!(
        restore(&source, "", &database_link, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert_eq!(fs::read(&source_path).unwrap(), untouched);
}

#[tokio::test]
async fn aliases_symlinks_and_hardlinks_cannot_overwrite_a_database() {
    let directory = directory();
    let path = directory.path().join("source.sqlite");
    let source = profile(&path);
    let db = connect(&source).await;
    sql(
        &db,
        "CREATE TABLE sentinel(value); INSERT INTO sentinel VALUES('original')",
    )
    .await;
    let initial = fs::read(&path).unwrap();
    for alias in [&path, &directory.path().join("./source.sqlite")] {
        assert!(
            backup(&source, "", alias, &ClientRuntime::Native)
                .await
                .is_err()
        );
        assert!(
            restore(&source, "", alias, &ClientRuntime::Native)
                .await
                .is_err()
        );
    }
    let symbolic = directory.path().join("symbolic.sqlite");
    symlink(&path, &symbolic).unwrap();
    assert!(
        restore(&source, "", &symbolic, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert!(
        backup(
            &profile(&symbolic),
            "",
            &directory.path().join("symbolic-copy"),
            &ClientRuntime::Native
        )
        .await
        .is_err()
    );
    let hard = directory.path().join("hard.sqlite");
    fs::hard_link(&path, &hard).unwrap();
    assert!(
        restore(&source, "", &hard, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert!(
        backup(&source, "", &hard, &ClientRuntime::Native)
            .await
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), initial);
}

#[tokio::test]
async fn invalid_and_corrupt_restore_sources_leave_target_untouched() {
    let directory = directory();
    let target_path = directory.path().join("target.sqlite");
    let target = profile(&target_path);
    let db = connect(&target).await;
    sql(
        &db,
        "CREATE TABLE sentinel(value); INSERT INTO sentinel VALUES('unchanged')",
    )
    .await;
    let before = fs::read(&target_path).unwrap();
    for (name, bytes) in [
        ("sql", b"DROP TABLE sentinel;".to_vec()),
        ("empty", vec![]),
        ("corrupt", {
            let mut bytes = before.clone();
            bytes[100] = 0xff;
            bytes
        }),
    ] {
        let input = directory.path().join(name);
        fs::write(&input, &bytes).unwrap();
        assert!(
            restore(&target, "", &input, &ClientRuntime::Native)
                .await
                .is_err(),
            "{name}"
        );
        assert_eq!(fs::read(&target_path).unwrap(), before);
        assert_eq!(fs::read(&input).unwrap(), bytes);
    }
    assert_eq!(
        rows(&db, "SELECT * FROM sentinel").await,
        vec![vec![Cell::Text("unchanged".into())]]
    );
}

#[tokio::test]
async fn readonly_backup_works_restore_is_rejected_and_podman_fails_before_filesystem() {
    let directory = directory();
    let path = directory.path().join("source.sqlite");
    let mut source = profile(&path);
    let db = connect(&source).await;
    sql(&db, "CREATE TABLE data(value); INSERT INTO data VALUES(1)").await;
    source.read_only = true;
    source.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    let output = directory.path().join("backup.sqlite");
    backup(&source, "", &output, &ClientRuntime::Native)
        .await
        .unwrap();
    assert!(
        restore(&source, "", &output, &ClientRuntime::Native)
            .await
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );
    let before = names(directory.path());
    let invalid = profile(&directory.path().join("missing"));
    let runtime = ClientRuntime::Podman {
        image: "never-execute".into(),
    };
    for (operation, result) in [
        (
            ServiceOperation::Backup,
            backup(
                &invalid,
                "",
                &directory.path().join("missing-parent/out"),
                &runtime,
            )
            .await,
        ),
        (
            ServiceOperation::Restore,
            restore(
                &invalid,
                "",
                &directory.path().join("missing-input"),
                &runtime,
            )
            .await,
        ),
    ] {
        let error = result.unwrap_err();
        let typed = error.downcast_ref::<UnsupportedServiceOperation>().unwrap();
        assert_eq!(typed.operation, operation);
        assert!(typed.reason.contains("Native"));
    }
    assert_eq!(names(directory.path()), before);
}

#[tokio::test]
async fn active_writer_restore_times_out_and_leaves_target_unchanged() {
    let directory = directory();
    let input_path = directory.path().join("input.sqlite");
    let input = connect(&profile(&input_path)).await;
    sql(
        &input,
        "CREATE TABLE data(value); INSERT INTO data VALUES('new')",
    )
    .await;
    let target = profile(&directory.path().join("target.sqlite"));
    let db = connect(&target).await;
    sql(&db, "PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES('old'); BEGIN IMMEDIATE; INSERT INTO data VALUES('pending')").await;
    let started = Instant::now();
    let error = restore(&target, "", &input_path, &ClientRuntime::Native)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("busy"), "{error:#}");
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert!(started.elapsed() < Duration::from_secs(8));
    sql(&db, "ROLLBACK").await;
    assert_eq!(
        rows(&db, "SELECT * FROM data").await,
        vec![vec![Cell::Text("old".into())]]
    );
}

#[tokio::test]
async fn cancelling_a_waiting_restore_finishes_worker_without_a_later_mutation() {
    let directory = directory();
    let input_path = directory.path().join("input.sqlite");
    let input = connect(&profile(&input_path)).await;
    sql(
        &input,
        "CREATE TABLE data(value); INSERT INTO data VALUES('new')",
    )
    .await;
    let target = profile(&directory.path().join("target.sqlite"));
    let db = connect(&target).await;
    sql(&db, "PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES('old'); BEGIN IMMEDIATE").await;
    let operation =
        tokio::spawn(
            async move { restore(&target, "", &input_path, &ClientRuntime::Native).await },
        );
    tokio::time::sleep(Duration::from_millis(100)).await;
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(100)).await;
    sql(&db, "ROLLBACK").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rows(&db, "SELECT * FROM data").await,
        vec![vec![Cell::Text("old".into())]]
    );
    sql(&db, "INSERT INTO data VALUES('still usable')").await;
}

#[tokio::test]
async fn backup_during_wal_writes_captures_one_committed_transaction() {
    let directory = directory();
    let source = profile(&directory.path().join("source.sqlite"));
    let db = connect(&source).await;
    sql(&db, "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(id INTEGER PRIMARY KEY, epoch INTEGER, payload BLOB); WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO data SELECT x,0,zeroblob(8192) FROM n").await;
    let writer = connect(&source).await;
    let writing = tokio::spawn(async move {
        for epoch in 1..=10 {
            sql(
                &writer,
                &format!("BEGIN IMMEDIATE; UPDATE data SET epoch={epoch}; COMMIT"),
            )
            .await;
            tokio::task::yield_now().await;
        }
    });
    let output = directory.path().join("snapshot.sqlite");
    backup(&source, "", &output, &ClientRuntime::Native)
        .await
        .unwrap();
    writing.await.unwrap();
    let snapshot = connect(&profile(&output)).await;
    let captured = rows(&snapshot, "SELECT count(*),min(epoch),max(epoch) FROM data").await;
    assert_eq!(captured[0][0], Cell::Text("1000".into()));
    assert_eq!(captured[0][1], captured[0][2]);
    assert_eq!(
        rows(&snapshot, "PRAGMA integrity_check").await,
        vec![vec![Cell::Text("ok".into())]]
    );
}

#[tokio::test]
async fn incompatible_wal_page_size_restore_fails_without_partial_replacement() {
    let directory = directory();
    let input_path = directory.path().join("input.sqlite");
    let input = connect(&profile(&input_path)).await;
    sql(
        &input,
        "PRAGMA page_size=4096; CREATE TABLE data(value); INSERT INTO data VALUES('new')",
    )
    .await;
    let target = profile(&directory.path().join("target.sqlite"));
    let db = connect(&target).await;
    sql(&db, "PRAGMA page_size=8192; PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES('old')").await;
    let error = restore(&target, "", &input_path, &ClientRuntime::Native)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("page sizes"), "{error:#}");
    assert_eq!(
        rows(&db, "SELECT * FROM data").await,
        vec![vec![Cell::Text("old".into())]]
    );
    assert_eq!(
        rows(&db, "PRAGMA page_size").await,
        vec![vec![Cell::Text("8192".into())]]
    );
    assert_eq!(
        rows(&db, "PRAGMA integrity_check").await,
        vec![vec![Cell::Text("ok".into())]]
    );
}
