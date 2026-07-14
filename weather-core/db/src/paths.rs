use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// Files owned by one SQLite database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabasePaths {
    pub data: PathBuf,
    pub lock: PathBuf,
    pub wal: PathBuf,
    pub shm: PathBuf,
}

impl DatabasePaths {
    pub fn new(data: impl Into<PathBuf>) -> Self {
        let data = data.into();
        Self {
            lock: append_suffix(&data, ".lock"),
            wal: append_suffix(&data, "-wal"),
            shm: append_suffix(&data, "-shm"),
            data,
        }
    }

    /// Resolve symbolic path aliases before deriving lock and SQLite sidecars.
    ///
    /// Existing databases are canonicalized as files. For a new database, its
    /// parent is canonicalized and the new file name is appended. Hard-link
    /// aliases cannot share suffix-derived sidecars and are intentionally not
    /// resolved here.
    pub fn canonicalized(data: impl AsRef<Path>) -> Result<Self> {
        let data = data.as_ref();
        let canonical = if data
            .try_exists()
            .with_context(|| format!("failed to inspect database path {}", data.display()))?
        {
            std::fs::canonicalize(data).with_context(|| {
                format!("failed to canonicalize database path {}", data.display())
            })?
        } else {
            let file_name = data
                .file_name()
                .with_context(|| format!("database path has no file name: {}", data.display()))?;
            let parent = data
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::canonicalize(parent)
                .with_context(|| {
                    format!(
                        "failed to canonicalize database parent {}",
                        parent.display()
                    )
                })?
                .join(file_name)
        };
        Ok(Self::new(canonical))
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(OsStr::new(suffix));
    value.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_suffixes_to_the_complete_database_path() {
        for (data, lock, wal, shm) in [
            (
                "weather.db",
                "weather.db.lock",
                "weather.db-wal",
                "weather.db-shm",
            ),
            (
                "cache.sqlite3",
                "cache.sqlite3.lock",
                "cache.sqlite3-wal",
                "cache.sqlite3-shm",
            ),
            ("weather", "weather.lock", "weather-wal", "weather-shm"),
        ] {
            let paths = DatabasePaths::new(data);
            assert_eq!(paths.data, Path::new(data));
            assert_eq!(paths.lock, Path::new(lock));
            assert_eq!(paths.wal, Path::new(wal));
            assert_eq!(paths.shm, Path::new(shm));
        }
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_database_names() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let data = PathBuf::from(std::ffi::OsString::from_vec(vec![b'd', b'b', 0xff]));
        let paths = DatabasePaths::new(data.clone());
        assert_eq!(paths.data, data);
        assert_eq!(paths.lock.as_os_str().as_bytes(), b"db\xff.lock");
        assert_eq!(paths.wal.as_os_str().as_bytes(), b"db\xff-wal");
        assert_eq!(paths.shm.as_os_str().as_bytes(), b"db\xff-shm");
    }

    #[cfg(unix)]
    #[test]
    fn existing_database_symlinks_share_data_and_sidecar_paths() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("weather.db");
        let alias = directory.path().join("database-alias");
        std::fs::write(&data, []).unwrap();
        symlink(&data, &alias).unwrap();

        assert_eq!(
            DatabasePaths::canonicalized(&data).unwrap(),
            DatabasePaths::canonicalized(&alias).unwrap()
        );
    }
}
