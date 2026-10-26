//! Offline profile portability. Mutations share the GUI's recovery lock so a
//! stale in-memory GUI state cannot silently overwrite an imported connection.
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use rowboat_core::{ConnectionProfile, Engine, SqliteFileOptions, SqliteOpenMode};
use rowboat_services::Store;
use std::{fs, io::Read, path::PathBuf};

#[derive(Subcommand)]
pub enum ProfileCommand {
    /// Save an existing-file SQLite profile without opening or creating the database.
    AddFile {
        path: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "Imported")]
        group: String,
        #[arg(long)]
        read_only: bool,
    },
    /// Export portable JSON, omitting passwords and machine-local key/cert paths.
    Export {
        /// Export one profile by exact ID or unique name; omit for all profiles.
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Import portable JSON with fresh IDs; existing profiles are preserved.
    Import {
        file: PathBuf,
        /// Validate and display the imported metadata without saving it.
        #[arg(long)]
        preview: bool,
    },
    /// Add a URL from stdin or ROWBOAT_URL; any embedded password is discarded.
    AddUrl {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "Imported")]
        group: String,
        /// Read URL from stdin instead of ROWBOAT_URL (avoids shell command history).
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        read_only: bool,
    },
    /// Print a password-free connection URL for a saved profile.
    Url { profile: String },
}

pub fn file_profile(path: PathBuf, create: bool, read_only: bool) -> Result<ConnectionProfile> {
    ensure!(
        !path.as_os_str().is_empty(),
        "Database file path is required"
    );
    let profile = ConnectionProfile {
        name: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
        engine: Engine::Sqlite,
        host: String::new(),
        port: 0,
        database: String::new(),
        username: String::new(),
        ssl_mode: "disable".into(),
        read_only,
        file: Some(SqliteFileOptions {
            path: std::path::absolute(path)?,
            mode: if create {
                SqliteOpenMode::CreateIfMissing
            } else {
                SqliteOpenMode::OpenExisting
            },
        }),
        ..Default::default()
    };
    rowboat_services::validate_profile(&profile)?;
    Ok(profile)
}

pub fn select<'a>(profiles: &'a [ConnectionProfile], name: &str) -> Result<&'a ConnectionProfile> {
    if let Some(profile) = profiles.iter().find(|p| p.id == name) {
        return Ok(profile);
    }
    let mut matches = profiles.iter().filter(|p| p.name == name);
    let profile = matches.next().context("Saved connection not found")?;
    ensure!(
        matches.next().is_none(),
        "Connection name is ambiguous; select its ID from `rowboatctl connections`"
    );
    Ok(profile)
}

fn exclusive(store: &Store) -> Result<fs::File> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(store.path().join("session.lock"))?;
    ensure!(
        lock.metadata()?.is_file(),
        "Profile lock must be a regular file"
    );
    // SAFETY: the owned file descriptor is live; flock does not dereference pointers.
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ensure!(
        locked == 0,
        "This data directory is open in Rowboat. Import through its connection manager or close that window first"
    );
    Ok(lock)
}

fn read_url(stdin: bool) -> Result<String> {
    if !stdin {
        return rowboat_services::environment_text("ROWBOAT_URL")?
            .context("Set ROWBOAT_URL or use --stdin to supply a database URL");
    }
    let mut raw = String::new();
    std::io::stdin().take(65537).read_to_string(&mut raw)?;
    ensure!(raw.len() <= 65536, "Connection URL exceeds 64 KiB");
    Ok(raw.trim().to_owned())
}

pub fn run(command: ProfileCommand) -> Result<()> {
    let store = Store::open()?;
    match command {
        ProfileCommand::AddFile {
            path,
            name,
            group,
            read_only,
        } => {
            let mut profile = file_profile(path, false, read_only)?;
            profile.name = name;
            profile.group = group;
            rowboat_services::validate_profile(&profile)?;
            let _lock = exclusive(&store)?;
            let mut state = store.load()?;
            let id = profile.id.clone();
            rowboat_services::merge_connections(&mut state.connections, vec![profile])?;
            store.save(&state)?;
            eprintln!("Saved connection {id}; database file was not opened");
            Ok(())
        }
        ProfileCommand::Export { profile, output } => {
            let state = store.load()?;
            let selected = if let Some(name) = profile {
                vec![select(&state.connections, &name)?.clone()]
            } else {
                state.connections
            };
            let json = rowboat_services::export_connections(&selected)?;
            super::output(&format!("{json}\n"), output)
        }
        ProfileCommand::Import { file, preview } => {
            let data = fs::read_to_string(file).context("Read connection export")?;
            let incoming = rowboat_services::import_connections(&data)?;
            if preview {
                return super::output(
                    &format!("{}\n", serde_json::to_string_pretty(&incoming)?),
                    None,
                );
            }
            let _lock = exclusive(&store)?;
            let mut state = store.load()?;
            let count = incoming.len();
            rowboat_services::merge_connections(&mut state.connections, incoming)?;
            store.save(&state)?;
            eprintln!("Imported {count} connections with fresh IDs; existing profiles preserved");
            Ok(())
        }
        ProfileCommand::AddUrl {
            name,
            group,
            stdin,
            read_only,
        } => {
            let parsed = rowboat_services::parse_connection_url(&read_url(stdin)?)?;
            let had_password = parsed.password.is_some();
            let mut profile = parsed.profile;
            profile.name = name;
            profile.group = group;
            profile.read_only |= read_only;
            rowboat_services::validate_profile(&profile)?;
            let _lock = exclusive(&store)?;
            let mut state = store.load()?;
            if state.connections.iter().any(|p| p.id == profile.id) {
                bail!("Generated profile ID already exists; retry import");
            }
            let id = profile.id.clone();
            state.connections.push(profile);
            store.save(&state)?;
            eprintln!("Saved connection {id}");
            if had_password {
                eprintln!(
                    "URL password was discarded; use ROWBOAT_PASSWORD or the GUI keyring controls when connecting"
                );
            }
            Ok(())
        }
        ProfileCommand::Url { profile } => {
            let state = store.load()?;
            let url = rowboat_services::connection_url(select(&state.connections, &profile)?)?;
            super::output(&format!("{url}\n"), None)
        }
    }
}
