use serde_json::Value;
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn run(directory: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
        .env("ROWBOAT_DATA_DIR", directory)
        .env_remove("ROWBOAT_URL")
        .env_remove("ROWBOAT_PASSWORD")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn add(directory: &Path, name: &str) {
    let output = run(
        directory,
        &["profiles", "add-url", "--stdin", "--name", name],
        Some("postgres://someone:never-save-this@127.0.0.1:15435/rowboat?sslmode=disable"),
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("never-save-this"));
    success(output);
}

#[test]
fn portable_cli_roundtrip_preserves_existing_connections_and_excludes_password() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    add(source.path(), "Source");
    add(target.path(), "Existing");
    let before: Value =
        serde_json::from_str(&success(run(target.path(), &["connections"], None))).unwrap();
    let state = std::fs::read_to_string(source.path().join("state.json")).unwrap();
    assert!(!state.contains("never-save-this"));
    let exported = source.path().join("connections.json");
    success(run(
        source.path(),
        &["profiles", "export", "--output", exported.to_str().unwrap()],
        None,
    ));
    let exported_text = std::fs::read_to_string(&exported).unwrap();
    assert!(!exported_text.contains("never-save-this"));
    success(run(
        target.path(),
        &[
            "profiles",
            "import",
            exported.to_str().unwrap(),
            "--preview",
        ],
        None,
    ));
    let preview_state: Value =
        serde_json::from_str(&success(run(target.path(), &["connections"], None))).unwrap();
    assert_eq!(
        before, preview_state,
        "preview cannot change persisted state"
    );
    success(run(
        target.path(),
        &["profiles", "import", exported.to_str().unwrap()],
        None,
    ));
    let after: Value =
        serde_json::from_str(&success(run(target.path(), &["connections"], None))).unwrap();
    let source_state: Value = serde_json::from_str(&state).unwrap();
    assert_eq!(after.as_array().unwrap().len(), 2);
    assert_eq!(after[0], before[0]);
    assert_ne!(after[1]["id"], source_state["connections"][0]["id"]);
    let url = success(run(target.path(), &["profiles", "url", "Source"], None));
    assert!(!url.contains("never-save-this"));
    assert!(url.starts_with("postgres"));
    assert!(
        !run(
            source.path(),
            &["profiles", "export", "--output", exported.to_str().unwrap()],
            None
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read_to_string(exported).unwrap(), exported_text);
}

#[test]
fn malformed_import_and_duplicate_names_do_not_select_the_wrong_profile() {
    let directory = tempfile::tempdir().unwrap();
    add(directory.path(), "Same name");
    add(directory.path(), "Same name");
    let result = run(directory.path(), &["profiles", "url", "Same name"], None);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("ambiguous"));
    let state_path = directory.path().join("state.json");
    let before = std::fs::read(&state_path).unwrap();
    let input = directory.path().join("invalid.json");
    std::fs::write(&input, r#"{"version":99,"connections":[]}"#).unwrap();
    assert!(
        !run(
            directory.path(),
            &["profiles", "import", input.to_str().unwrap()],
            None
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read(state_path).unwrap(), before);
}

#[test]
fn cli_mutations_refuse_a_live_gui_lock() {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let directory = tempfile::tempdir().unwrap();
    add(directory.path(), "Initial");
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .open(directory.path().join("session.lock"))
        .unwrap();
    // SAFETY: descriptor belongs to a live owned File.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let result = run(
        directory.path(),
        &["profiles", "add-url", "--stdin", "--name", "Blocked"],
        Some("mysql://rowboat@localhost/rowboat"),
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("open in Rowboat"));
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
    success(run(directory.path(), &["profiles", "export"], None));
}

#[test]
fn invalid_url_errors_do_not_repeat_secrets() {
    let directory = tempfile::tempdir().unwrap();
    for url in [
        "postgres://u:secret-never-log@[invalid/db",
        "postgres://u:secret-never-log@localhost/db?password=secret-never-log",
    ] {
        let result = run(
            directory.path(),
            &["profiles", "add-url", "--stdin", "--name", "Bad"],
            Some(url),
        );
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("secret-never-log"));
        assert!(!directory.path().join("state.json").exists());
    }
}

#[test]
fn readonly_profile_policy_survives_import_and_cannot_be_lost_in_url_export() {
    let directory = tempfile::tempdir().unwrap();
    success(run(
        directory.path(),
        &[
            "profiles",
            "add-url",
            "--stdin",
            "--name",
            "Reader",
            "--read-only",
        ],
        Some("mysql://rowboat@localhost/rowboat"),
    ));
    let state: Value =
        serde_json::from_str(&success(run(directory.path(), &["connections"], None))).unwrap();
    assert_eq!(state[0]["read_only"], true);
    let url = run(directory.path(), &["profiles", "url", "Reader"], None);
    assert!(!url.status.success());
    assert!(String::from_utf8_lossy(&url.stderr).contains("read-only"));
    let portable = success(run(directory.path(), &["profiles", "export"], None));
    let imported = rowboat_services::import_connections(&portable).unwrap();
    assert!(imported[0].read_only);
}
