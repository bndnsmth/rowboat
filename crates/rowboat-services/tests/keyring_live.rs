//! Runs against a private D-Bus session and ephemeral GNOME keyring, never the
//! user's collection. Existing host binaries only; no installation or config.
#![cfg(target_os = "linux")]
use anyhow::{Context, Result, ensure};
use rowboat_services::{credential_delete, credential_get, credential_set};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires existing dbus-run-session, gnome-keyring-daemon and secret-tool; uses an isolated session"]
fn isolated_secret_service_roundtrip() -> Result<()> {
    if std::env::var_os("ROWBOAT_ISOLATED_KEYRING_TEST").is_none() {
        let directory = tempfile::Builder::new()
            .prefix("rowboat-keyring-test-")
            .tempdir()?;
        for name in ["data", "runtime", "config", "control", "home"] {
            let path = directory.path().join(name);
            std::fs::create_dir(&path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        let status = Command::new("dbus-run-session")
            .arg("--")
            .arg(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "isolated_secret_service_roundtrip",
                "--nocapture",
            ])
            .env("ROWBOAT_ISOLATED_KEYRING_TEST", directory.path())
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("HOME", directory.path().join("home"))
            .env_remove("GNOME_KEYRING_CONTROL")
            .env_remove("GNOME_KEYRING_PID")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("NO_AT_BRIDGE", "1")
            .status()
            .context("Isolated keyring unavailable: dbus-run-session must already be installed")?;
        ensure!(
            status.success(),
            "Isolated Secret Service verification failed; user keyring was not accessed"
        );
        return Ok(());
    }
    let directory =
        std::path::PathBuf::from(std::env::var_os("ROWBOAT_ISOLATED_KEYRING_TEST").unwrap());
    let mut daemon = Daemon(
        Command::new("gnome-keyring-daemon")
            .args([
                "--foreground",
                "--components=secrets",
                "--unlock",
                "--control-directory",
            ])
            .arg(directory.join("control"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context(
                "Isolated keyring unavailable: gnome-keyring-daemon must already be installed",
            )?,
    );
    // A nonempty stdin password avoids the daemon treating an empty stream as
    // a request to show a prompt. This collection exists only in our temp dir.
    daemon
        .0
        .stdin
        .take()
        .context("Daemon stdin unavailable")?
        .write_all(b"rowboat-isolated-keyring-unlock")?;
    // The option-looking ID also verifies the secret-tool '--' boundary.
    let id = format!("--rowboat-isolated-test-{}", uuid::Uuid::new_v4());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.freedesktop.DBus",
                "--object-path",
                "/org/freedesktop/DBus",
                "--method",
                "org.freedesktop.DBus.NameHasOwner",
                "org.freedesktop.secrets",
            ])
            .output()
            .context("gdbus is required for nonactivating keyring readiness")?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains("true") {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "Private keyring failed to acquire its D-Bus name"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    loop {
        match credential_get(&id) {
            Ok(None) => break,
            Ok(Some(_)) => anyhow::bail!("Task-unique key unexpectedly existed"),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(error).context("Private Secret Service did not become ready"),
        }
    }
    // Seed precisely the old application's attributes inside this private bus.
    // New code must find that entry without copying it to a new namespace.
    let mut old = Command::new("secret-tool")
        .args([
            "store",
            "--label=Legacy compatibility fixture",
            "--",
            "application",
            "keel",
            "connection-id",
            &id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    old.stdin
        .take()
        .context("Fixture stdin unavailable")?
        .write_all(b"legacy remembered\n")?;
    ensure!(
        old.wait()?.success(),
        "Could not seed isolated legacy credential"
    );
    ensure!(
        credential_get(&id)?.as_deref() == Some("legacy remembered\n"),
        "Legacy credential lookup failed"
    );
    for (case, password) in [
        "quote'\\:日本語",
        "line one\nline two",
        "ends with newline\n",
        "",
    ]
    .into_iter()
    .enumerate()
    {
        credential_set(&id, password)?;
        let actual = credential_get(&id)?.context("Stored credential was missing")?;
        // Do not include either value in assertion output.
        ensure!(
            actual == password,
            "Keyring password did not round-trip exactly (case {case})"
        );
    }
    let legacy = Command::new("secret-tool")
        .args(["lookup", "--", "application", "keel", "connection-id", &id])
        .output()?;
    ensure!(
        legacy.status.success(),
        "New writes lost the legacy lookup identity"
    );
    let renamed = Command::new("secret-tool")
        .args([
            "lookup",
            "--",
            "application",
            "rowboat",
            "connection-id",
            &id,
        ])
        .output()?;
    ensure!(
        !renamed.status.success() && renamed.stdout.is_empty(),
        "Credential was copied to another namespace"
    );
    credential_delete(&id)?;
    ensure!(
        credential_get(&id)?.is_none(),
        "Deleted credential remained in isolated keyring"
    );
    credential_delete(&id)?;
    Ok(())
}
