use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tempfile::Builder;

pub const COMPONENT_MANIFEST_FILE_NAME: &str = "component-manifest.toml";
const COMPONENT_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Bin,
    Config,
    Db,
    Lock,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComponentEntry {
    pub kind: ComponentKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ComponentManifest {
    path: PathBuf,
    lock_path: PathBuf,
}

impl ComponentManifest {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        Self {
            lock_path: Self::lock_path_for(&path),
            path,
        }
    }

    pub fn for_config_path(config_path: impl AsRef<Path>) -> Self {
        let config_path = config_path.as_ref();
        Self::open(parent_directory(config_path).join(COMPONENT_MANIFEST_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn lock_path_for(path: impl AsRef<Path>) -> PathBuf {
        let mut lock_path = path.as_ref().as_os_str().to_os_string();
        lock_path.push(OsStr::new(".lock"));
        lock_path.into()
    }

    pub fn record(&self, kind: ComponentKind, path: impl AsRef<Path>) -> Result<()> {
        let parent = parent_directory(&self.path);
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        let entry = ComponentEntry {
            kind,
            path: PathBuf::from(path.as_ref().display().to_string()),
        };
        self.with_exclusive_lock(|| {
            let mut entries =
                if self.path.try_exists().with_context(|| {
                    format!("inspect component manifest {}", self.path.display())
                })? {
                    self.load_entries_unlocked()?
                } else {
                    BTreeSet::new()
                };
            if entries.insert(entry) {
                self.write_entries_unlocked(&entries)?;
            }
            Ok(())
        })
    }

    pub fn list(&self) -> Result<Vec<ComponentEntry>> {
        if !self
            .path
            .try_exists()
            .with_context(|| format!("inspect component manifest {}", self.path.display()))?
        {
            return Ok(Vec::new());
        }
        self.with_shared_lock(|| Ok(self.load_entries_unlocked()?.into_iter().collect()))
    }

    fn load_entries_unlocked(&self) -> Result<BTreeSet<ComponentEntry>> {
        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("read component manifest {}", self.path.display()))?;
        let document: ManifestDocument = toml::from_str(&content)
            .with_context(|| format!("parse component manifest {}", self.path.display()))?;
        if document.version != COMPONENT_MANIFEST_VERSION {
            bail!(
                "component manifest version {} is unsupported; this build supports {}",
                document.version,
                COMPONENT_MANIFEST_VERSION
            );
        }
        Ok(document
            .components
            .into_iter()
            .map(|entry| ComponentEntry {
                kind: entry.kind,
                path: PathBuf::from(entry.path),
            })
            .collect())
    }

    fn write_entries_unlocked(&self, entries: &BTreeSet<ComponentEntry>) -> Result<()> {
        let document = ManifestDocument {
            version: COMPONENT_MANIFEST_VERSION,
            components: entries
                .iter()
                .map(|entry| StoredComponentEntry {
                    kind: entry.kind,
                    path: entry.path.display().to_string(),
                })
                .collect(),
        };
        let content =
            toml::to_string_pretty(&document).context("serialize component manifest as TOML")?;
        let parent = parent_directory(&self.path);
        let prefix = temporary_prefix(&self.path);
        let mut temporary = Builder::new()
            .prefix(&prefix)
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| format!("create temporary manifest in {}", parent.display()))?;
        temporary
            .write_all(content.as_bytes())
            .with_context(|| format!("write temporary manifest {}", temporary.path().display()))?;
        temporary
            .flush()
            .with_context(|| format!("flush temporary manifest {}", temporary.path().display()))?;
        temporary
            .as_file()
            .sync_all()
            .with_context(|| format!("sync temporary manifest {}", temporary.path().display()))?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace component manifest {}", self.path.display()))?;
        sync_parent_directory(parent)?;
        Ok(())
    }

    fn open_lock(&self) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .with_context(|| format!("open component manifest lock {}", self.lock_path.display()))
    }

    fn with_shared_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock)
            .with_context(|| format!("lock component manifest {}", self.path.display()))?;
        finish_locked(operation(), FileExt::unlock(&lock))
    }

    fn with_exclusive_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("lock component manifest {}", self.path.display()))?;
        finish_locked(operation(), FileExt::unlock(&lock))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    version: u32,
    components: Vec<StoredComponentEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredComponentEntry {
    kind: ComponentKind,
    path: String,
}

fn finish_locked<T>(operation: Result<T>, unlock: std::io::Result<()>) -> Result<T> {
    match operation {
        Err(error) => Err(error),
        Ok(value) => {
            unlock.context("unlock component manifest")?;
            Ok(value)
        }
    }
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn temporary_prefix(path: &Path) -> OsString {
    let mut prefix = OsString::from(".");
    prefix.push(
        path.file_name()
            .unwrap_or_else(|| OsStr::new("component-manifest")),
    );
    prefix.push(".");
    prefix
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)
        .with_context(|| format!("open manifest directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync manifest directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn records_unique_components_in_stable_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(COMPONENT_MANIFEST_FILE_NAME);
        let manifest = ComponentManifest::open(&path);

        manifest.record(ComponentKind::Bin, "/tmp/bin/z").unwrap();
        manifest
            .record(ComponentKind::Config, "/tmp/config/weather.toml")
            .unwrap();
        manifest.record(ComponentKind::Bin, "/tmp/bin/a").unwrap();
        manifest.record(ComponentKind::Bin, "/tmp/bin/a").unwrap();

        assert_eq!(
            manifest.list().unwrap(),
            vec![
                ComponentEntry {
                    kind: ComponentKind::Bin,
                    path: PathBuf::from("/tmp/bin/a"),
                },
                ComponentEntry {
                    kind: ComponentKind::Bin,
                    path: PathBuf::from("/tmp/bin/z"),
                },
                ComponentEntry {
                    kind: ComponentKind::Config,
                    path: PathBuf::from("/tmp/config/weather.toml"),
                },
            ]
        );
        assert!(manifest.lock_path().is_file());
        let persisted = fs::read_to_string(path).unwrap();
        assert!(persisted.starts_with("version = 1\n"));
        assert_eq!(persisted.matches("path = \"/tmp/bin/a\"").count(), 1);
    }

    #[test]
    fn concurrent_records_do_not_lose_entries() {
        const THREADS: usize = 24;
        let directory = tempfile::tempdir().unwrap();
        let path = Arc::new(directory.path().join(COMPONENT_MANIFEST_FILE_NAME));
        let barrier = Arc::new(Barrier::new(THREADS));
        let workers = (0..THREADS)
            .map(|index| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let manifest = ComponentManifest::open(path.as_ref());
                    barrier.wait();
                    manifest
                        .record(ComponentKind::Db, format!("/tmp/database-{index}"))
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }

        let entries = ComponentManifest::open(path.as_ref()).list().unwrap();

        assert_eq!(entries.len(), THREADS);
        for index in 0..THREADS {
            assert!(entries.contains(&ComponentEntry {
                kind: ComponentKind::Db,
                path: PathBuf::from(format!("/tmp/database-{index}")),
            }));
        }
    }

    #[test]
    fn rejects_invalid_documents_without_overwriting_them() {
        let directory = tempfile::tempdir().unwrap();
        let future = directory.path().join("future.toml");
        let future_content = "version = 2\ncomponents = []\n";
        fs::write(&future, future_content).unwrap();
        let manifest = ComponentManifest::open(&future);
        let error = manifest.list().unwrap_err().to_string();
        assert!(error.contains("version 2 is unsupported"), "{error}");
        assert!(manifest.record(ComponentKind::Db, "/tmp/new").is_err());
        assert_eq!(fs::read_to_string(&future).unwrap(), future_content);

        let unknown = directory.path().join("unknown.toml");
        let unknown_content = "version = 1\n[[components]]\nkind = \"temp\"\npath = \"/tmp/x\"\n";
        fs::write(&unknown, unknown_content).unwrap();
        let manifest = ComponentManifest::open(&unknown);
        let error = format!("{:#}", manifest.list().unwrap_err());
        assert!(error.contains("parse component manifest"), "{error}");
        assert!(error.contains("unknown variant"), "{error}");
        assert!(manifest.record(ComponentKind::Db, "/tmp/new").is_err());
        assert_eq!(fs::read_to_string(&unknown).unwrap(), unknown_content);

        let corrupt = directory.path().join("corrupt.toml");
        let corrupt_content = "version = 1\n[[components]\n";
        fs::write(&corrupt, corrupt_content).unwrap();
        let manifest = ComponentManifest::open(&corrupt);
        assert!(manifest.list().is_err());
        assert!(manifest.record(ComponentKind::Db, "/tmp/new").is_err());
        assert_eq!(fs::read_to_string(&corrupt).unwrap(), corrupt_content);
    }

    #[test]
    fn missing_manifest_list_has_no_filesystem_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("missing/config/weather.toml");

        let manifest = ComponentManifest::for_config_path(&config);

        assert!(manifest.list().unwrap().is_empty());
        assert!(!directory.path().join("missing").exists());
        assert!(!manifest.path().exists());
        assert!(!manifest.lock_path().exists());
    }

    #[test]
    fn config_relative_manifest_path_and_lock_are_stable() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("weather.toml");

        let manifest = ComponentManifest::for_config_path(&config);

        assert_eq!(
            manifest.path(),
            directory.path().join(COMPONENT_MANIFEST_FILE_NAME)
        );
        assert_eq!(
            manifest.lock_path(),
            directory.path().join("component-manifest.toml.lock")
        );
    }
}
