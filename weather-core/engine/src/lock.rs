use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use weather_schema::EngineLockMetadata;

const ENGINE_LOCK_RETRIES: usize = 8;
const ENGINE_LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

pub struct LockGuard {
    _file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
    }
}

impl LockGuard {
    pub fn exclusive(path: PathBuf) -> Result<Self> {
        Self::acquire(path, 0, None)
    }

    pub(crate) fn engine(path: PathBuf, metadata: &EngineLockMetadata) -> Result<Self> {
        Self::acquire(path, ENGINE_LOCK_RETRIES, Some(metadata))
    }

    fn acquire(
        path: PathBuf,
        retries: usize,
        metadata: Option<&EngineLockMetadata>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for attempt in 0..=retries {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("failed to open lock file {}", path.display()))?;
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock && attempt < retries => {
                    std::thread::sleep(ENGINE_LOCK_RETRY_DELAY);
                    continue;
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to acquire lock {}", path.display()));
                }
            }

            if !file_matches_path(&file, &path)? {
                FileExt::unlock(&file).ok();
                if attempt < retries {
                    std::thread::sleep(ENGINE_LOCK_RETRY_DELAY);
                    continue;
                }
                bail!(
                    "lock path {} changed while acquiring the lock",
                    path.display()
                );
            }

            let mut guard = Self { _file: file };
            if let Some(metadata) = metadata {
                guard.write_metadata(&path, metadata)?;
                if !file_matches_path(&guard._file, &path)? {
                    drop(guard);
                    if attempt < retries {
                        std::thread::sleep(ENGINE_LOCK_RETRY_DELAY);
                        continue;
                    }
                    bail!(
                        "lock path {} changed while initializing the lock",
                        path.display()
                    );
                }
            }
            return Ok(guard);
        }
        unreachable!("lock acquisition loop always returns")
    }

    fn write_metadata(&mut self, path: &Path, metadata: &EngineLockMetadata) -> Result<()> {
        self._file
            .seek(SeekFrom::Start(0))
            .with_context(|| format!("failed to seek lock file {}", path.display()))?;
        self._file
            .set_len(0)
            .with_context(|| format!("failed to truncate lock file {}", path.display()))?;
        serde_json::to_writer(&mut self._file, metadata)
            .with_context(|| format!("failed to initialize lock file {}", path.display()))?;
        writeln!(self._file)
            .with_context(|| format!("failed to finalize lock file {}", path.display()))?;
        self._file
            .sync_data()
            .with_context(|| format!("failed to sync lock file {}", path.display()))?;
        Ok(())
    }
}

fn file_matches_path(file: &File, path: &Path) -> Result<bool> {
    let file_handle = same_file::Handle::from_file(
        file.try_clone()
            .with_context(|| format!("failed to clone open lock file {}", path.display()))?,
    )
    .with_context(|| format!("failed to identify open lock file {}", path.display()))?;
    let path_handle = match same_file::Handle::from_path(path) {
        Ok(handle) => handle,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to identify lock path {}", path.display()));
        }
    };
    Ok(file_handle == path_handle)
}

pub(crate) fn resolve_relative(base_dir: &Path, value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(instance_id: &str) -> EngineLockMetadata {
        EngineLockMetadata {
            version: weather_schema::ENGINE_LOCK_METADATA_VERSION,
            pid: 42,
            instance_id: instance_id.to_string(),
            owner_token: Some("owner-token".to_string()),
            started_at_unix_ms: 1_788_000_000_000,
            config_path: "/tmp/weather.toml".to_string(),
        }
    }

    #[test]
    fn engine_lock_keeps_one_path_identity_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.lock");

        let first = LockGuard::engine(path.clone(), &metadata("first")).unwrap();
        let first_identity =
            same_file::Handle::from_file(first._file.try_clone().unwrap()).unwrap();
        assert!(file_matches_path(&first._file, &path).unwrap());
        drop(first);
        assert!(path.exists());

        let second = LockGuard::engine(path.clone(), &metadata("second")).unwrap();
        let second_identity =
            same_file::Handle::from_file(second._file.try_clone().unwrap()).unwrap();
        assert_eq!(first_identity, second_identity);
        assert!(file_matches_path(&second._file, &path).unwrap());
        drop(second);
        assert!(path.exists());
    }

    #[test]
    fn engine_lock_retries_a_transient_shared_probe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.lock");
        let probe = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock_shared(&probe).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            FileExt::unlock(&probe).unwrap();
        });

        let guard = LockGuard::engine(path, &metadata("retry")).unwrap();
        release.join().unwrap();
        drop(guard);
    }

    #[test]
    fn engine_lock_writes_versioned_metadata_on_the_held_inode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.lock");
        let expected = metadata("instance");

        let guard = LockGuard::engine(path.clone(), &expected).unwrap();
        let actual: EngineLockMetadata =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();

        assert_eq!(actual, expected);
        assert!(file_matches_path(&guard._file, &path).unwrap());
    }
}
