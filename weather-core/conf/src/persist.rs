use std::{
    ffi::{OsStr, OsString},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tempfile::{Builder, NamedTempFile};

use crate::AppConfig;

/// A fully written and synced configuration that has not replaced its target.
/// Dropping this value removes the unique temporary file without changing the
/// destination.
pub struct PreparedConfig {
    temporary: NamedTempFile,
    destination: PathBuf,
}

impl PreparedConfig {
    /// Atomically replace the destination with the prepared configuration.
    pub fn persist(self) -> Result<()> {
        let Self {
            temporary,
            destination,
        } = self;
        temporary
            .persist(&destination)
            .map_err(|err| err.error)
            .with_context(|| format!("failed to replace config {}", destination.display()))?;
        Ok(())
    }
}

/// Write and sync a complete configuration to a unique temporary file in the
/// destination directory, without modifying the destination itself.
pub fn prepare_config_atomic(path: &Path, config: &AppConfig) -> Result<PreparedConfig> {
    let content =
        toml::to_string_pretty(config).context("failed to serialize config for persistence")?;
    let parent = path
        .parent()
        .with_context(|| format!("config path has no parent: {}", path.display()))?;
    let prefix = temp_prefix(path);
    let mut temporary = Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp config in {}", parent.display()))?;

    temporary
        .write_all(content.as_bytes())
        .with_context(|| format!("failed to write temp config {}", temporary.path().display()))?;
    temporary
        .flush()
        .with_context(|| format!("failed to flush temp config {}", temporary.path().display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temp config {}", temporary.path().display()))?;
    Ok(PreparedConfig {
        temporary,
        destination: path.to_path_buf(),
    })
}

/// Prepare and atomically persist a complete configuration.
pub fn write_config_atomic(path: &Path, config: &AppConfig) -> Result<()> {
    prepare_config_atomic(path, config)?.persist()
}

fn temp_prefix(path: &Path) -> OsString {
    let mut prefix = OsString::from(".");
    prefix.push(path.file_name().unwrap_or_else(|| OsStr::new("config")));
    prefix.push(".");
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.toml");
        let config = AppConfig::default();

        write_config_atomic(&path, &config).unwrap();

        let loaded = crate::load_from_path(&path).unwrap();
        assert_eq!(loaded, config);
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp"))
        );
    }

    #[test]
    fn dropping_prepared_config_preserves_target_without_manifest_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.toml");
        let initial = AppConfig::default();
        write_config_atomic(&path, &initial).unwrap();
        let mut candidate = initial.clone();
        candidate.updater.weather_ttl_seconds += 1;

        let prepared = prepare_config_atomic(&path, &candidate).unwrap();
        let temporary_path = prepared.temporary.path().to_path_buf();
        assert!(temporary_path.exists());
        assert_eq!(crate::load_from_path(&path).unwrap(), initial);

        drop(prepared);

        assert!(!temporary_path.exists());
        assert_eq!(crate::load_from_path(&path).unwrap(), initial);
        assert!(!directory.path().join("component-manifest.toml").exists());
        assert!(
            !directory
                .path()
                .join("component-manifest.toml.lock")
                .exists()
        );
    }
}
