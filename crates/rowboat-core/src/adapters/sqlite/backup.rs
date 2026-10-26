//! SQLite's online-backup API. Handles and FFI remain inside the engine adapter.
use anyhow::{Context, Result, bail, ensure};
use libsqlite3_sys as ffi;
use std::{
    ffi::{CStr, CString, c_int, c_void},
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const BUSY_LIMIT: Duration = Duration::from_secs(5);
const OPERATION_LIMIT: Duration = Duration::from_secs(60);

/// Copy one consistent, integrity-checked `main` snapshot into an existing file.
///
/// This blocking primitive is intended for tool providers on a blocking worker.
/// It never creates either path. The destination is changed by one SQLite backup
/// transaction, so existing WAL readers retain their snapshots and failures roll
/// back. Source and destination must be regular, non-symlink, single-link files.
/// Callers publish a private destination separately for a no-overwrite backup.
/// Cancellation is cooperative; never kill the worker during a native operation.
/// Lock contention is bounded to five seconds, total work to sixty seconds (at
/// SQLite progress / 128-page boundaries; filesystem calls are not interruptible).
pub fn copy_sqlite_snapshot(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<()> {
    let progress = Progress {
        cancelled,
        started: Instant::now(),
    };
    progress.check()?;
    let source_metadata = regular_file(source)?;
    let destination_metadata = regular_file(destination)?;
    ensure!(
        (source_metadata.dev(), source_metadata.ino())
            != (destination_metadata.dev(), destination_metadata.ino()),
        "SQLite source and destination refer to the same file"
    );
    // WAL sidecar names are path-based. A second hard-link name cannot safely
    // identify which journal belongs to the database, even for a read-only open.
    ensure!(
        source_metadata.nlink() == 1 && destination_metadata.nlink() == 1,
        "SQLite backup/restore does not support hard-linked database files"
    );
    let mut header = [0_u8; 16];
    fs::File::open(source)
        .context("Read SQLite source header")?
        .read_exact(&mut header)
        .context("Source is not an initialized SQLite database")?;
    ensure!(
        &header == b"SQLite format 3\0",
        "Source is not a SQLite database"
    );
    let input = Connection::open(source, true, &progress)?;
    input.exec("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; BEGIN")?;
    // integrity_check establishes and validates the SAME read transaction used
    // by sqlite3_backup_step, including committed pages still only in the WAL.
    input.integrity_check()?;
    progress.check()?;
    let output = Connection::open(destination, false, &progress)?;
    output.exec("PRAGMA synchronous=FULL")?;
    ensure!(
        output.scalar("PRAGMA main.journal_mode")? != "wal"
            || input.scalar("PRAGMA main.page_size")? == output.scalar("PRAGMA main.page_size")?,
        "SQLite restore into a WAL-mode target requires matching source and target page sizes"
    );
    copy_pages(&input, &output, &progress, || {})
}

fn copy_pages(
    input: &Connection,
    output: &Connection,
    progress: &Progress<'_>,
    mut after_step: impl FnMut(),
) -> Result<()> {
    // Both connections and the progress context outlive this backup guard.
    let pointer =
        unsafe { ffi::sqlite3_backup_init(output.0, c"main".as_ptr(), input.0, c"main".as_ptr()) };
    ensure!(
        !pointer.is_null(),
        "Initialize SQLite backup: {}",
        output.error()
    );
    let mut backup = Backup(pointer);
    loop {
        progress.check()?;
        let rc = unsafe { ffi::sqlite3_backup_step(backup.0, 128) };
        after_step();
        match rc {
            ffi::SQLITE_DONE => {
                // DONE is the atomic commit boundary. Do not report cancellation
                // after this point: the whole database has already committed.
                let rc = backup.finish();
                ensure!(
                    rc == ffi::SQLITE_OK,
                    "Finish SQLite backup: {}",
                    output.error()
                );
                return Ok(());
            }
            ffi::SQLITE_OK => {}
            ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => {
                ensure!(
                    progress.started.elapsed() < BUSY_LIMIT,
                    "SQLite backup/restore is busy; close active write transactions and retry (5 second limit)"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => bail!("SQLite backup/restore failed ({rc}): {}", output.error()),
        }
    }
}

fn regular_file(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).context("SQLite file must already exist")?;
    ensure!(
        metadata.file_type().is_file(),
        "SQLite backup/restore requires regular files, not symlinks or directories"
    );
    Ok(metadata)
}

struct Progress<'a> {
    cancelled: &'a AtomicBool,
    started: Instant,
}
impl Progress<'_> {
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.started.elapsed() >= OPERATION_LIMIT
    }
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "SQLite backup/restore cancelled"
        );
        ensure!(
            self.started.elapsed() < OPERATION_LIMIT,
            "SQLite backup/restore exceeded its 60 second limit"
        );
        Ok(())
    }
}

unsafe extern "C" fn progress_callback(context: *mut c_void) -> c_int {
    let progress = unsafe { &*context.cast::<Progress<'_>>() };
    c_int::from(progress.stopped())
}
unsafe extern "C" fn busy_callback(context: *mut c_void, _: c_int) -> c_int {
    let progress = unsafe { &*context.cast::<Progress<'_>>() };
    if progress.stopped() || progress.started.elapsed() >= BUSY_LIMIT {
        return 0;
    }
    std::thread::sleep(Duration::from_millis(10));
    1
}

struct Connection(*mut ffi::sqlite3);
impl Connection {
    fn open(path: &Path, read_only: bool, progress: &Progress<'_>) -> Result<Self> {
        // SQLite's NOFOLLOW rejects directory symlinks too (including macOS's
        // standard /var -> /private/var). Resolve only the parent, leaving the
        // database filename subject to NOFOLLOW and the regular-file checks.
        // The absolute result also cannot become a special :memory: name or URI.
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .context("Resolve SQLite backup/restore directory")?;
        let path = parent.join(
            path.file_name()
                .context("SQLite path requires a filename")?,
        );
        let name = CString::new(path.as_os_str().as_encoded_bytes())
            .context("SQLite path contains NUL")?;
        let flags = if read_only {
            ffi::SQLITE_OPEN_READONLY
        } else {
            ffi::SQLITE_OPEN_READWRITE
        } | ffi::SQLITE_OPEN_NOFOLLOW
            | ffi::SQLITE_OPEN_PRIVATECACHE;
        let mut pointer = std::ptr::null_mut();
        let rc =
            unsafe { ffi::sqlite3_open_v2(name.as_ptr(), &mut pointer, flags, std::ptr::null()) };
        ensure!(
            !pointer.is_null(),
            "Open SQLite backup/restore file failed ({rc})"
        );
        let connection = Self(pointer);
        ensure!(
            rc == ffi::SQLITE_OK,
            "Open SQLite backup/restore file: {}",
            connection.error()
        );
        let context = (progress as *const Progress<'_>).cast_mut().cast();
        unsafe {
            ffi::sqlite3_enable_load_extension(pointer, 0);
            ffi::sqlite3_busy_handler(pointer, Some(busy_callback), context);
            ffi::sqlite3_progress_handler(pointer, 1000, Some(progress_callback), context);
        }
        Ok(connection)
    }
    fn error(&self) -> String {
        unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(self.0)) }
            .to_string_lossy()
            .into_owned()
    }
    fn exec(&self, sql: &str) -> Result<()> {
        let sql = CString::new(sql)?;
        let rc = unsafe {
            ffi::sqlite3_exec(
                self.0,
                sql.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        ensure!(
            rc == ffi::SQLITE_OK,
            "SQLite backup/restore setup: {}",
            self.error()
        );
        Ok(())
    }
    fn integrity_check(&self) -> Result<()> {
        ensure!(
            self.scalar("PRAGMA main.integrity_check")? == "ok",
            "SQLite source failed integrity_check"
        );
        Ok(())
    }
    fn scalar(&self, sql: &str) -> Result<String> {
        let sql = CString::new(sql)?;
        let mut pointer = std::ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(self.0, sql.as_ptr(), -1, &mut pointer, std::ptr::null_mut())
        };
        let statement = Statement(pointer);
        ensure!(
            rc == ffi::SQLITE_OK,
            "Inspect SQLite backup/restore database: {}",
            self.error()
        );
        ensure!(
            unsafe { ffi::sqlite3_step(statement.0) } == ffi::SQLITE_ROW,
            "Inspect SQLite backup/restore database: {}",
            self.error()
        );
        let value = unsafe { ffi::sqlite3_column_text(statement.0, 0) };
        ensure!(!value.is_null(), "SQLite inspection returned NULL");
        let value = unsafe { CStr::from_ptr(value.cast()) }
            .to_string_lossy()
            .into_owned();
        ensure!(
            unsafe { ffi::sqlite3_step(statement.0) } == ffi::SQLITE_DONE,
            "SQLite inspection failed or source failed integrity_check"
        );
        Ok(value)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_close(self.0) };
    }
}
struct Statement(*mut ffi::sqlite3_stmt);
impl Drop for Statement {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_finalize(self.0) };
    }
}
struct Backup(*mut ffi::sqlite3_backup);
impl Backup {
    fn finish(&mut self) -> c_int {
        let pointer = std::mem::replace(&mut self.0, std::ptr::null_mut());
        unsafe { ffi::sqlite3_backup_finish(pointer) }
    }
}
impl Drop for Backup {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SQLite rolls back an unfinished destination backup transaction.
            self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_after_partial_page_copy_rolls_back_destination() {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("pass14_backup_{}", uuid::Uuid::new_v4())),
        );
        fs::create_dir(&fixture.0).unwrap();
        let source = fixture.0.join("source.sqlite");
        let target = fixture.0.join("target.sqlite");
        fs::File::create(&source).unwrap();
        fs::File::create(&target).unwrap();
        let cancelled = AtomicBool::new(false);
        let progress = Progress {
            cancelled: &cancelled,
            started: Instant::now(),
        };
        let input = Connection::open(&source, false, &progress).unwrap();
        input
            .exec("CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES(zeroblob(4194304))")
            .unwrap();
        let output = Connection::open(&target, false, &progress).unwrap();
        output
            .exec("CREATE TABLE sentinel(value); INSERT INTO sentinel VALUES('preserved')")
            .unwrap();
        let before = fs::read(&target).unwrap();
        input.exec("BEGIN").unwrap();
        input.integrity_check().unwrap();
        let error = copy_pages(&input, &output, &progress, || {
            cancelled.store(true, Ordering::Release)
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        cancelled.store(false, Ordering::Release);
        output.integrity_check().unwrap();
        // This checks rollback of pages already written, not merely rejection
        // before acquiring a writer lock or before the first backup step.
        assert_eq!(fs::read(&target).unwrap(), before);
        output
            .exec("INSERT INTO sentinel VALUES('still usable')")
            .unwrap();
        assert!(!fixture.0.join("target.sqlite-journal").exists());
    }
}
