//! Saved copies, retained-history filtering and explicit file-backed SQL documents.
//! Library association is descriptive only: no connection or execution occurs here.
use crate::{AppState, HistoryEntry, Snippet};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

pub const SQL_DOCUMENT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryMetadata {
    pub folder: String,
    pub connection_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub snippet: Snippet,
    pub metadata: QueryMetadata,
}

pub fn saved_query(state: &AppState, id: &str) -> Option<SavedQuery> {
    Some(SavedQuery {
        snippet: state.snippets.iter().find(|s| s.id == id)?.clone(),
        metadata: state.query_metadata.get(id).cloned().unwrap_or_default(),
    })
}

/// Compare against the opened snapshot in the *current* shared state, then mutate
/// only this ID. A stale window cannot resurrect a deletion or replace new SQL.
pub fn save_query(
    state: &mut AppState,
    query: SavedQuery,
    expected: Option<&SavedQuery>,
) -> Result<()> {
    ensure!(
        !query.snippet.id.is_empty(),
        "Saved query identity is required"
    );
    ensure!(
        !query.snippet.name.trim().is_empty(),
        "Enter a saved query name"
    );
    ensure!(
        query.snippet.name.len() <= 512,
        "Query name exceeds 512 bytes"
    );
    ensure!(
        query.metadata.folder.len() <= 512,
        "Folder exceeds 512 bytes"
    );
    ensure!(
        query.snippet.sql.len() <= SQL_DOCUMENT_LIMIT,
        "SQL exceeds 8 MiB"
    );
    ensure!(
        expected.is_none_or(|old| old.snippet.id == query.snippet.id),
        "Saved query identity cannot change during update"
    );
    ensure!(
        saved_query(state, &query.snippet.id).as_ref() == expected,
        "Saved query changed or was removed in another window; reopen it or use Save As"
    );
    let id = query.snippet.id.clone();
    if let Some(existing) = state.snippets.iter_mut().find(|s| s.id == id) {
        *existing = query.snippet;
    } else {
        state.snippets.push(query.snippet);
    }
    state.query_metadata.insert(id, query.metadata);
    Ok(())
}

pub fn remove_query(state: &mut AppState, expected: &SavedQuery) -> Result<()> {
    ensure!(
        saved_query(state, &expected.snippet.id).as_ref() == Some(expected),
        "Saved query changed in another window; refresh before removing it"
    );
    state.snippets.retain(|s| s.id != expected.snippet.id);
    state.query_metadata.remove(&expected.snippet.id);
    Ok(())
}

pub fn query_matches(query: &SavedQuery, text: &str) -> bool {
    let haystack = format!(
        "{}\n{}\n{}",
        query.snippet.name, query.snippet.sql, query.metadata.folder
    )
    .to_lowercase();
    text.to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryStatus {
    #[default]
    All,
    Success,
    Failure,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryFilter {
    pub text: String,
    pub connection_id: Option<String>,
    /// Inclusive UTC dates. Unparseable legacy timestamps match only unbounded filters.
    pub from: Option<NaiveDate>,
    pub through: Option<NaiveDate>,
    pub status: HistoryStatus,
}
impl HistoryFilter {
    pub fn matches(&self, entry: &HistoryEntry) -> bool {
        if self
            .connection_id
            .as_ref()
            .is_some_and(|id| id != &entry.connection_id)
            || (self.status == HistoryStatus::Success && entry.error.is_some())
            || (self.status == HistoryStatus::Failure && entry.error.is_none())
        {
            return false;
        }
        if self.from.is_some() || self.through.is_some() {
            let Ok(date) = DateTime::parse_from_rfc3339(&entry.executed_at) else {
                return false;
            };
            let date = date.with_timezone(&chrono::Utc).date_naive();
            if self.from.is_some_and(|from| date < from)
                || self.through.is_some_and(|through| date > through)
            {
                return false;
            }
        }
        let haystack =
            format!("{}\n{}", entry.sql, entry.error.as_deref().unwrap_or("")).to_lowercase();
        self.text
            .to_lowercase()
            .split_whitespace()
            .all(|word| haystack.contains(word))
    }
}

pub fn history_matches<'a>(
    state: &'a AppState,
    filter: &'a HistoryFilter,
) -> impl Iterator<Item = &'a HistoryEntry> {
    state
        .history
        .iter()
        .rev()
        .filter(move |entry| filter.matches(entry))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
    pub changed_seconds: i64,
    pub changed_nanos: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SqlDocument {
    Saved { baseline: SavedQuery },
    File { path: PathBuf, baseline: FileStamp },
}
impl SqlDocument {
    pub fn dirty(&self, sql: &str) -> bool {
        match self {
            Self::Saved { baseline } => baseline.snippet.sql != sql,
            Self::File { baseline, .. } => baseline.sha256 != digest(sql.as_bytes()),
        }
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Saved { baseline } => {
                ensure!(
                    !baseline.snippet.id.is_empty(),
                    "Recovered saved query has no identity"
                );
                ensure!(
                    baseline.snippet.sql.len() <= SQL_DOCUMENT_LIMIT,
                    "Recovered saved query exceeds 8 MiB"
                );
            }
            Self::File { path, baseline } => {
                ensure!(
                    path.is_absolute(),
                    "Recovered SQL document path must be absolute"
                );
                ensure!(
                    baseline.sha256.len() == 64
                        && baseline.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                    "Invalid recovered SQL fingerprint"
                );
            }
        }
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn stamp(file: &File, bytes: &[u8]) -> Result<FileStamp> {
    let m = file.metadata()?;
    Ok(FileStamp {
        sha256: digest(bytes),
        device: m.dev(),
        inode: m.ino(),
        modified_seconds: m.mtime(),
        modified_nanos: m.mtime_nsec(),
        changed_seconds: m.ctime(),
        changed_nanos: m.ctime_nsec(),
    })
}
fn read_file(path: &Path) -> Result<(String, FileStamp)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("Open SQL document")?;
    ensure!(
        file.metadata()?.is_file(),
        "SQL document must be a regular file, not a symlink"
    );
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&file)
        .take(SQL_DOCUMENT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= SQL_DOCUMENT_LIMIT,
        "SQL editor files are limited to 8 MiB"
    );
    let stamp = stamp(&file, &bytes)?;
    ensure!(
        before.mtime() == stamp.modified_seconds
            && before.mtime_nsec() == stamp.modified_nanos
            && before.ctime() == stamp.changed_seconds
            && before.ctime_nsec() == stamp.changed_nanos,
        "SQL file changed while being read; retry opening it"
    );
    Ok((
        String::from_utf8(bytes).context("SQL file must be UTF-8")?,
        stamp,
    ))
}

pub fn open_sql_document(path: &Path) -> Result<(String, SqlDocument)> {
    // Resolve the parent, not the filename: selecting a symlink is deliberately rejected.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let path = parent
        .canonicalize()?
        .join(path.file_name().context("SQL path needs a filename")?);
    let (sql, baseline) = read_file(&path)?;
    Ok((sql, SqlDocument::File { path, baseline }))
}

/// Save As never overwrites an existing path (including symlinks). Use Save on
/// an opened document for replacement with a known baseline.
pub fn create_sql_document(path: &Path, sql: &str) -> Result<SqlDocument> {
    ensure!(sql.len() <= SQL_DOCUMENT_LIMIT, "SQL exceeds 8 MiB");
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let path = parent
        .canonicalize()?
        .join(path.file_name().context("SQL path needs a filename")?);
    let parent = path.parent().unwrap();
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(sql.as_bytes())?;
    temporary.as_file().sync_all()?;
    let written = temporary
        .persist_noclobber(&path)
        .context("Destination already exists or cannot be created; choose a new path")?;
    File::open(parent)?.sync_all()?;
    // Record our write, not a reread that might silently adopt another writer's
    // bytes as the baseline after publication.
    let baseline = stamp(&written, sql.as_bytes())?;
    Ok(SqlDocument::File { path, baseline })
}

/// Optimistic, content-and-identity checked atomic replacement. Advisory locking
/// serializes cooperating writers. The fingerprint is rechecked immediately before
/// rename; uncooperative filesystem writers cannot be made transactional by POSIX.
pub fn save_sql_document(document: &SqlDocument, sql: &str) -> Result<SqlDocument> {
    ensure!(sql.len() <= SQL_DOCUMENT_LIMIT, "SQL exceeds 8 MiB");
    let SqlDocument::File { path, baseline } = document else {
        anyhow::bail!("This is a saved copy, not a file-backed document");
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "SQL document is no longer a regular file"
    );
    // SAFETY: owned valid file descriptor; flock takes no pointers.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "SQL file is being saved by another writer"
    );
    ensure!(
        &read_file(path)?.1 == baseline,
        "SQL file changed externally; draft preserved. Open disk version in a new tab or Save As a new path"
    );
    let parent = path
        .parent()
        .context("SQL document needs an absolute path")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(file.metadata()?.permissions())?;
    temporary.write_all(sql.as_bytes())?;
    temporary.as_file().sync_all()?;
    ensure!(
        &read_file(path)?.1 == baseline,
        "SQL file changed externally while saving; draft preserved"
    );
    let written = temporary
        .persist(path)
        .context("Atomically save SQL document")?;
    File::open(parent)?.sync_all()?;
    let baseline = stamp(&written, sql.as_bytes())?;
    Ok(SqlDocument::File {
        path: path.clone(),
        baseline,
    })
}
