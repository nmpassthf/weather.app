use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use fs2::FileExt;

pub struct LockGuard {
    _file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl LockGuard {
    pub fn exclusive(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open lock file {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("failed to acquire lock {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn resolve_relative(base_dir: &Path, value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    })
}
