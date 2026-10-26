use rowboat_services::{Store, resolve_data_directory};
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

fn resolve(base: &Path) -> std::path::PathBuf {
    resolve_data_directory(None, None, Some(base)).unwrap()
}

#[test]
fn default_resolution_preserves_legacy_bytes_ids_and_lock_inode() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("keel");
    let new = dir.path().join("rowboat");
    assert_eq!(resolve(dir.path()), new);
    assert!(
        !new.exists(),
        "resolving cannot create a competing directory"
    );
    fs::create_dir(&old).unwrap();
    let state = br#"{"connections":[{"id":"keel-id","name":"Keel user name","group":"Saved","color":"blue","engine":"Postgres","host":"localhost","port":15432,"database":"keel","username":"postgres","ssl_mode":"disable","read_only":false}],"history":[{"id":"h","connection_id":"keel-id","sql":"SELECT 'keel'","executed_at":"now","elapsed_ms":1,"row_count":1,"error":null}],"snippets":[{"id":"s","name":"Keel favorite","sql":"SELECT 'keel'"}],"last_connection_id":"keel-id"}"#;
    fs::write(old.join("state.json"), state).unwrap();
    fs::write(old.join("session.json"), b"legacy recovery bytes").unwrap();
    fs::write(old.join("session.lock"), b"").unwrap();
    let inode = fs::metadata(old.join("session.lock")).unwrap().ino();
    assert_eq!(resolve(dir.path()), old);
    let store = Store::open_at(resolve(dir.path())).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.connections[0].id, "keel-id");
    assert_eq!(loaded.connections[0].port, 15432);
    assert_eq!(loaded.connections[0].database, "keel");
    assert_eq!(loaded.history[0].sql, "SELECT 'keel'");
    assert_eq!(loaded.snippets[0].sql, "SELECT 'keel'");
    assert_eq!(loaded.last_connection_id.as_deref(), Some("keel-id"));
    assert_eq!(fs::read(old.join("state.json")).unwrap(), state);
    assert_eq!(
        fs::read(old.join("session.json")).unwrap(),
        b"legacy recovery bytes"
    );
    assert_eq!(fs::metadata(old.join("session.lock")).unwrap().ino(), inode);
    assert!(!new.exists());
    fs::create_dir(&new).unwrap();
    assert_eq!(
        resolve(dir.path()),
        new,
        "empty new directory is authoritative"
    );
    fs::write(new.join("state.json"), "invalid").unwrap();
    assert!(Store::open_at(resolve(dir.path())).unwrap().load().is_err());
    assert_eq!(fs::read(new.join("state.json")).unwrap(), b"invalid");
    assert_eq!(fs::read(old.join("state.json")).unwrap(), state);
    fs::remove_dir_all(&old).unwrap();
    assert_eq!(resolve(dir.path()), new);
}

#[test]
fn explicit_overrides_are_exact_and_conflicts_are_redacted() {
    let a = OsStr::new("/private/path-one");
    let b = OsStr::new("/private/path-two");
    assert_eq!(
        resolve_data_directory(Some(a), None, None).unwrap(),
        Path::new(a)
    );
    assert_eq!(
        resolve_data_directory(None, Some(a), None).unwrap(),
        Path::new(a)
    );
    assert_eq!(
        resolve_data_directory(Some(a), Some(a), None).unwrap(),
        Path::new(a)
    );
    let error = resolve_data_directory(Some(a), Some(b), None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("conflicts"));
    assert!(!error.contains("private"));
    for (new, old) in [
        (Some(OsStr::new("")), None),
        (None, Some(OsStr::new(""))),
        (Some(OsStr::new("")), Some(a)),
    ] {
        assert!(resolve_data_directory(new, old, None).is_err());
    }
    assert!(resolve_data_directory(None, None, None).is_err());
}

#[test]
fn symlinks_files_and_readonly_directories_never_redirect_selection() {
    let dir = tempfile::tempdir().unwrap();
    let new = dir.path().join("rowboat");
    let old = dir.path().join("keel");
    fs::create_dir(&old).unwrap();
    symlink(dir.path().join("missing"), &new).unwrap();
    assert_eq!(resolve(dir.path()), new);
    assert!(Store::open_at(resolve(dir.path())).is_err());
    fs::remove_file(&new).unwrap();
    symlink(&old, &new).unwrap();
    assert_eq!(resolve(dir.path()), new);
    assert!(Store::open_at(resolve(dir.path())).is_err());
    fs::remove_file(&new).unwrap();
    fs::write(&new, "not a directory").unwrap();
    assert_eq!(resolve(dir.path()), new);
    assert!(Store::open_at(resolve(dir.path())).is_err());
    fs::remove_file(&new).unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(resolve(dir.path()), old);
    assert_eq!(
        fs::metadata(&old).unwrap().permissions().mode() & 0o777,
        0o500
    );
    fs::set_permissions(&old, fs::Permissions::from_mode(0o700)).unwrap();
}
