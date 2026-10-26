//! Filesystem-only provider; no credentials, subprocesses, SSH or network tools.
use crate::ClientRuntime;
use anyhow::{Context, Result, ensure};
use rowboat_core::{ConnectionProfile, copy_sqlite_snapshot};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn profile_path(
    profile: &ConnectionProfile,
    runtime: &ClientRuntime,
    operation: crate::ServiceOperation,
) -> Result<PathBuf> {
    // This check must precede even filesystem metadata / temporary creation.
    if !matches!(runtime, ClientRuntime::Native) {
        return Err(crate::UnsupportedServiceOperation {
            engine: profile.engine,
            operation,
            reason: "SQLite backup/restore uses the built-in Native runtime; Podman is unsupported",
        }
        .into());
    }
    crate::validate_profile(profile)?;
    Ok(profile
        .file
        .as_ref()
        .context("SQLite requires file options")?
        .path
        .clone())
}

pub(crate) async fn backup(
    profile: &ConnectionProfile,
    destination: &Path,
    runtime: &ClientRuntime,
) -> Result<()> {
    let source = profile_path(profile, runtime, crate::ServiceOperation::Backup)?;
    let destination = destination.to_path_buf();
    run(move |cancelled| {
        unused_destination(&destination)?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // A private directory also owns any SQLite journal/SHM/WAL sidecars on
        // every error/cancellation path, not just the main temporary file.
        let directory = tempfile::Builder::new()
            .prefix(".rowboat-backup-")
            .tempdir_in(parent)
            .context("Create private SQLite backup directory")?;
        let output = tempfile::NamedTempFile::new_in(directory.path())?;
        output
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        copy_sqlite_snapshot(&source, output.path(), cancelled)?;
        // A WAL-mode source copies that journal mode into the new destination.
        // Native close checkpoints the private destination before publication.
        output.as_file().sync_all()?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "SQLite backup cancelled"
        );
        // A stale WAL can override the completed main image when it is opened.
        // Recheck all destination names immediately before publication; preserve
        // any sidecar that appeared while we were preparing the snapshot.
        unused_destination(&destination)?;
        output
            .persist_noclobber(&destination)
            .context("Publish SQLite backup (destination must not exist)")?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })
    .await
}

fn unused_destination(destination: &Path) -> Result<()> {
    // Append to the full OS filename, rather than replacing its extension or
    // lossily converting it to text. lstat also detects dangling symlinks.
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = destination.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(Path::new(&name)) {
            Ok(_) if suffix.is_empty() => {
                anyhow::bail!("Backup destination already exists; choose a new path")
            }
            Ok(_) => anyhow::bail!(
                "SQLite backup destination has an existing {suffix} sidecar; choose a new path"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("Inspect SQLite backup destination and sidecars");
            }
        }
    }
    Ok(())
}

pub(crate) async fn restore(
    profile: &ConnectionProfile,
    source: &Path,
    runtime: &ClientRuntime,
) -> Result<()> {
    let destination = profile_path(profile, runtime, crate::ServiceOperation::Restore)?;
    ensure!(
        !profile.read_only,
        "Restore is disabled for read-only connections"
    );
    let source = source.to_path_buf();
    run(move |cancelled| copy_sqlite_snapshot(&source, &destination, cancelled)).await
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
async fn run(work: impl FnOnce(&AtomicBool) -> Result<()> + Send + 'static) -> Result<()> {
    let guard = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = guard.0.clone();
    // Aborting the async caller requests cancellation; the blocking worker runs
    // its native rollback/close and private-temp cleanup to completion. Dropping
    // a JoinHandle never kills a thread in the middle of a restore transaction.
    tokio::task::spawn_blocking(move || work(&cancelled))
        .await
        .context("SQLite backup/restore worker failed")?
}
