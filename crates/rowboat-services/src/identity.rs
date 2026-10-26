//! Brand compatibility without copying stores, mutating the process environment,
//! or reading credentials. Legacy names here are deliberate persistence contracts.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

fn legacy_name(name: &'static str) -> &'static str {
    match name {
        "ROWBOAT_PASSWORD" => "KEEL_PASSWORD",
        "ROWBOAT_URL" => "KEEL_URL",
        "ROWBOAT_SSH_KNOWN_HOSTS" => "KEEL_SSH_KNOWN_HOSTS",
        "ROWBOAT_SOURCE_URL" => "KEEL_SOURCE_URL",
        "ROWBOAT_SOURCE_PASSWORD" => "KEEL_SOURCE_PASSWORD",
        "ROWBOAT_SOURCE_SSH_KNOWN_HOSTS" => "KEEL_SOURCE_SSH_KNOWN_HOSTS",
        "ROWBOAT_POSTGRES_CLIENT_IMAGE" => "KEEL_POSTGRES_CLIENT_IMAGE",
        "ROWBOAT_MYSQL_CLIENT_IMAGE" => "KEEL_MYSQL_CLIENT_IMAGE",
        "ROWBOAT_TRACE_KEYS" => "KEEL_TRACE_KEYS",
        _ => name,
    }
}

/// Select the environment key for Clap before parsing, so env values participate
/// in required/conflict checks and explicit flags retain precedence. Empty new
/// values count as present. DATA_DIR has a separate, conflict-checking resolver.
pub fn environment_name(name: &'static str) -> &'static str {
    if std::env::var_os(name).is_none() && std::env::var_os(legacy_name(name)).is_some() {
        legacy_name(name)
    } else {
        name
    }
}

/// New-name presence wins, including empty/non-Unicode values. Never writes env.
pub fn environment_os(name: &'static str) -> Option<OsString> {
    environment_with(name, |key| std::env::var_os(key))
}

fn environment_with(
    name: &'static str,
    mut lookup: impl FnMut(&str) -> Option<OsString>,
) -> Option<OsString> {
    lookup(name).or_else(|| lookup(legacy_name(name)))
}

/// Text values reject invalid Unicode without displaying the supplied value.
pub fn environment_text(name: &'static str) -> Result<Option<String>> {
    environment_os(name)
        .map(|value| {
            value.into_string().map_err(|_| {
                anyhow::anyhow!("{name} (or its legacy alias) must contain valid Unicode")
            })
        })
        .transpose()
}

/// Resolve without creating directories or opening state/secret files. An
/// existing new directory is authoritative even when empty or invalid. Symlinks
/// (including dangling links) count as present and are rejected by Store::open_at.
/// Explicit directories are exact paths, never suffixed with the application name.
pub fn resolve_data_directory(
    new: Option<&OsStr>,
    legacy: Option<&OsStr>,
    base: Option<&Path>,
) -> Result<PathBuf> {
    if let (Some(new), Some(legacy)) = (new, legacy) {
        ensure!(
            new == legacy,
            "ROWBOAT_DATA_DIR conflicts with KEEL_DATA_DIR; select one directory override"
        );
    }
    if let Some(path) = new.or(legacy) {
        ensure!(
            !path.is_empty(),
            "ROWBOAT_DATA_DIR / KEEL_DATA_DIR must not be empty"
        );
        return Ok(PathBuf::from(path));
    }
    let base = base.context("Cannot determine home data directory; set ROWBOAT_DATA_DIR")?;
    let current = base.join("rowboat");
    let legacy = base.join("keel");
    if present(&current)? || !present(&legacy)? {
        Ok(current)
    } else {
        Ok(legacy)
    }
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => anyhow::bail!("Cannot inspect application data directory"),
    }
}

pub(crate) fn data_directory() -> Result<PathBuf> {
    let new = std::env::var_os("ROWBOAT_DATA_DIR");
    let legacy = std::env::var_os("KEEL_DATA_DIR");
    let base = directories::BaseDirs::new();
    resolve_data_directory(
        new.as_deref(),
        legacy.as_deref(),
        base.as_ref().map(|dirs| dirs.data_dir()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_public_aliases_preserve_new_presence() {
        for name in [
            "ROWBOAT_PASSWORD",
            "ROWBOAT_URL",
            "ROWBOAT_SSH_KNOWN_HOSTS",
            "ROWBOAT_SOURCE_URL",
            "ROWBOAT_SOURCE_PASSWORD",
            "ROWBOAT_SOURCE_SSH_KNOWN_HOSTS",
            "ROWBOAT_POSTGRES_CLIENT_IMAGE",
            "ROWBOAT_MYSQL_CLIENT_IMAGE",
            "ROWBOAT_TRACE_KEYS",
        ] {
            let legacy = legacy_name(name);
            assert_ne!(legacy, name);
            assert_eq!(
                environment_with(name, |key| (key == legacy).then(|| "old".into())),
                Some("old".into())
            );
            assert_eq!(
                environment_with(name, |key| Some(
                    if key == name { "" } else { "old" }.into()
                )),
                Some("".into())
            );
            assert_eq!(
                environment_with(name, |key| Some(
                    if key == name { "new" } else { "old" }.into()
                )),
                Some("new".into())
            );
            assert_eq!(environment_with(name, |_| None), None);
        }
    }
}
