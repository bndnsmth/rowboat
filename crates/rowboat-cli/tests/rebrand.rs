//! Compatibility tests use disposable directories and child-only environments.
use std::{
    fs,
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStringExt, fs::MetadataExt},
    },
    path::Path,
    process::{Command, Output},
};

fn cli(base: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    command
        .env_clear()
        .env("XDG_DATA_HOME", base)
        .env("HOME", base)
        .env("PATH", "/usr/bin:/bin");
    command
}
fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn default_legacy_store_and_lock_are_shared_with_cli() {
    let base = tempfile::tempdir().unwrap();
    #[cfg(target_os = "linux")]
    let data = base.path().to_owned();
    #[cfg(target_os = "macos")]
    let data = base.path().join("Library/Application Support");
    let old = data.join("keel");
    let new = data.join("rowboat");
    fs::create_dir_all(&old).unwrap();
    ok(cli(base.path())
        .env("KEEL_URL", "postgres://postgres@localhost/keel")
        .args(["profiles", "add-url", "--name", "Keel saved profile"])
        .output()
        .unwrap());
    assert!(!new.exists());
    let before = fs::read(old.join("state.json")).unwrap();
    let lock = fs::File::open(old.join("session.lock")).unwrap();
    // SAFETY: owned live descriptor; same advisory-lock contract as the old GUI.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let inode = lock.metadata().unwrap().ino();
    let blocked = cli(base.path())
        .env("ROWBOAT_URL", "mysql://u@localhost/new")
        .args(["profiles", "add-url", "--name", "Blocked"])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("open in Rowboat"));
    let listed = ok(cli(base.path()).arg("connections").output().unwrap());
    assert!(listed.contains("Keel saved profile"));
    assert_eq!(fs::read(old.join("state.json")).unwrap(), before);
    assert_eq!(fs::metadata(old.join("session.lock")).unwrap().ino(), inode);
    assert!(!new.exists());
    fs::create_dir(&new).unwrap();
    assert_eq!(
        ok(cli(base.path()).arg("connections").output().unwrap()).trim(),
        "[]"
    );
    assert_eq!(fs::read(old.join("state.json")).unwrap(), before);
    assert!(
        ok(cli(base.path())
            .env("ROWBOAT_DATA_DIR", &old)
            .arg("connections")
            .output()
            .unwrap())
        .contains("Keel saved profile")
    );
}

#[test]
fn data_override_conflicts_are_redacted_and_empty_does_not_fall_back() {
    let base = tempfile::tempdir().unwrap();
    let a = base.path().join("private-one");
    let b = base.path().join("private-two");
    let conflict = cli(base.path())
        .env("ROWBOAT_DATA_DIR", &a)
        .env("KEEL_DATA_DIR", &b)
        .arg("connections")
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    let error = String::from_utf8_lossy(&conflict.stderr);
    assert!(error.contains("conflicts"));
    assert!(!error.contains("private-"));
    assert!(!a.exists() && !b.exists());
    assert!(
        !cli(base.path())
            .env("ROWBOAT_DATA_DIR", "")
            .arg("connections")
            .output()
            .unwrap()
            .status
            .success()
    );
    ok(cli(base.path())
        .env("KEEL_DATA_DIR", &a)
        .arg("connections")
        .output()
        .unwrap());
    assert!(a.is_dir());
    ok(cli(base.path())
        .env("ROWBOAT_DATA_DIR", &a)
        .env("KEEL_DATA_DIR", &a)
        .arg("connections")
        .output()
        .unwrap());
}

#[test]
fn clap_aliases_participate_in_required_conflict_and_flag_precedence() {
    let base = tempfile::tempdir().unwrap();
    for variable in ["ROWBOAT_URL", "KEEL_URL"] {
        let conflict = cli(base.path())
            .env(variable, "postgres://u@localhost/db")
            .args(["--connection", "saved", "ping"])
            .output()
            .unwrap();
        assert!(!conflict.status.success());
        assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used"));
        // Explicit invalid URL is parsed instead of the environment value. No IO.
        let flag = cli(base.path())
            .env(variable, "postgres://u@localhost/db")
            .args(["--url", "bad", "ping"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&flag.stderr)
                .contains("Expected a PostgreSQL or MySQL connection URL")
        );
    }
    for variable in ["ROWBOAT_SOURCE_URL", "KEEL_SOURCE_URL"] {
        let supplied = cli(base.path())
            .env(variable, "postgres://u@localhost/db")
            .args([
                "compare-live",
                "--source-schema",
                "a",
                "--destination-schema",
                "b",
            ])
            .output()
            .unwrap();
        assert!(!supplied.status.success());
        assert!(String::from_utf8_lossy(&supplied.stderr).contains("Provide --connection"));
    }
    let empty = cli(base.path())
        .env("ROWBOAT_URL", "")
        .env("KEEL_URL", "postgres://u@localhost/db")
        .arg("ping")
        .output()
        .unwrap();
    assert!(!empty.status.success());
    assert!(!String::from_utf8_lossy(&empty.stderr).contains("Cannot connect"));
}

#[test]
fn offline_url_aliases_and_help_are_branded_and_do_not_expose_values() {
    let base = tempfile::tempdir().unwrap();
    ok(cli(base.path())
        .env("KEEL_URL", "postgres://u@localhost/legacy")
        .args(["profiles", "add-url", "--name", "old"])
        .output()
        .unwrap());
    ok(cli(base.path())
        .env("ROWBOAT_URL", "postgres://u@localhost/new")
        .env("KEEL_URL", "bad")
        .args(["profiles", "add-url", "--name", "new"])
        .output()
        .unwrap());
    let profiles = ok(cli(base.path()).arg("connections").output().unwrap());
    assert!(profiles.contains("legacy") && profiles.contains("new"));
    assert!(
        !cli(base.path())
            .env("ROWBOAT_URL", "")
            .env("KEEL_URL", "postgres://u@localhost/legacy")
            .args(["profiles", "add-url", "--name", "empty"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let help = ok(cli(base.path()).arg("--help").output().unwrap());
    assert!(
        help.contains("Rowboat") && help.contains("rowboatctl") && help.contains("ROWBOAT_URL")
    );
    assert!(!help.contains("KEEL"));
    let help = ok(cli(base.path())
        .env("KEEL_URL", "postgres://u:never-print-this@localhost/db")
        .arg("--help")
        .output()
        .unwrap());
    assert!(!help.contains("never-print-this"));
    assert!(ok(cli(base.path()).arg("--version").output().unwrap()).starts_with("rowboatctl "));
}

#[test]
fn invalid_unicode_new_password_is_redacted_and_never_uses_legacy() {
    let base = tempfile::tempdir().unwrap();
    let invalid = std::ffi::OsString::from_vec(b"never-print-this\xff".to_vec());
    let output = cli(base.path())
        .env("ROWBOAT_PASSWORD", invalid)
        .env("KEEL_PASSWORD", "old")
        .args(["--url", "postgres://u@127.0.0.1:1/db", "ping"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("valid Unicode"));
    assert!(!error.contains("never-print-this"));
}

#[test]
#[ignore = "requires new Rowboat base fixtures on 15435/13309 only"]
fn live_legacy_password_and_new_empty_precedence_on_both_engines() {
    let base = tempfile::tempdir().unwrap();
    for url in [
        "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable",
        "mysql://rowboat@127.0.0.1:13309/rowboat?sslmode=disable",
    ] {
        ok(cli(base.path())
            .env("KEEL_URL", url)
            .env("KEEL_PASSWORD", "rowboat-test")
            .arg("ping")
            .output()
            .unwrap());
        ok(cli(base.path())
            .env("ROWBOAT_URL", url)
            .env("ROWBOAT_PASSWORD", "rowboat-test")
            .env("KEEL_PASSWORD", "wrong")
            .arg("ping")
            .output()
            .unwrap());
        for new in ["", "wrong"] {
            assert!(
                !cli(base.path())
                    .env("ROWBOAT_URL", url)
                    .env("ROWBOAT_PASSWORD", new)
                    .env("KEEL_PASSWORD", "rowboat-test")
                    .arg("ping")
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
    }
}
