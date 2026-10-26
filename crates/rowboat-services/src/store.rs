use anyhow::{Context, Result, ensure};
use rowboat_core::ConnectionProfile;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const HISTORY_LIMIT: usize = 1000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    pub connections: Vec<ConnectionProfile>,
    pub history: Vec<HistoryEntry>,
    pub snippets: Vec<Snippet>,
    /// Additive metadata: legacy saved SQL keeps its exact ID, name and contents.
    pub query_metadata: BTreeMap<String, crate::query_library::QueryMetadata>,
    pub snippet_templates: Vec<crate::SnippetTemplate>,
    pub last_connection_id: Option<String>,
    pub preferences: crate::Preferences,
    pub saved_filters: Vec<crate::SavedFilter>,
    pub dashboards: Vec<crate::Dashboard>,
    pub custom_metrics: Vec<crate::CustomMetricDefinition>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    pub id: String,
    pub connection_id: String,
    pub sql: String,
    pub executed_at: String,
    pub elapsed_ms: u64,
    pub row_count: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub sql: String,
}

#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self> {
        Self::open_at(crate::identity::data_directory()?)
    }

    pub fn open_at(path: impl AsRef<Path>) -> Result<Self> {
        let directory = path.as_ref().to_path_buf();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .context("Create private Rowboat data directory")?;
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Rowboat data directory must be a real directory, not a symlink"
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(Self { directory })
    }

    pub fn path(&self) -> &Path {
        &self.directory
    }

    pub fn load(&self) -> Result<AppState> {
        let path = self.directory.join("state.json");
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppState::default());
            }
            Err(error) => return Err(error).context("Open Rowboat state"),
        };
        ensure!(
            file.metadata()?.is_file(),
            "Rowboat state is not a regular file"
        );
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let mut state: AppState = serde_json::from_reader(file)
            .context("Rowboat state is invalid JSON; original file was preserved")?;
        cap_history(&mut state);
        crate::validate_app_state(&state)
            .context("Invalid persisted preferences or saved metadata")?;
        Ok(state)
    }

    pub fn save(&self, state: &AppState) -> Result<()> {
        crate::validate_app_state(state)?;
        let mut state = state.clone();
        cap_history(&mut state);
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut temporary, &state)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.directory.join("state.json"))
            .context("Atomically save Rowboat state")?;
        fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

fn cap_history(state: &mut AppState) {
    if state.history.len() > HISTORY_LIMIT {
        state.history.drain(..state.history.len() - HISTORY_LIMIT);
    }
}
