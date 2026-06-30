use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Config,
    Db,
    Bin,
    Lock,
    Temp,
}

impl ComponentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Db => "db",
            Self::Bin => "bin",
            Self::Lock => "lock",
            Self::Temp => "temp",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "config" => Ok(Self::Config),
            "db" => Ok(Self::Db),
            "bin" => Ok(Self::Bin),
            "lock" => Ok(Self::Lock),
            "temp" => Ok(Self::Temp),
            other => bail!("invalid component kind `{other}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentEntry {
    pub kind: ComponentKind,
    pub path: PathBuf,
}

pub struct ComponentRegistry {
    path: PathBuf,
}

impl ComponentRegistry {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let registry = Self { path };
        registry.with_connection(|conn| {
            conn.execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA busy_timeout = 5000;
                CREATE TABLE IF NOT EXISTS components(
                    kind TEXT NOT NULL,
                    path TEXT NOT NULL,
                    PRIMARY KEY(kind, path)
                );
                "#,
            )?;
            Ok(())
        })?;
        Ok(registry)
    }

    pub fn for_config_path(config_path: impl AsRef<Path>) -> Result<Self> {
        let config_path = config_path.as_ref();
        let parent = config_path
            .parent()
            .with_context(|| format!("config path has no parent: {}", config_path.display()))?;
        Self::open(parent.join("component.list.db"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, kind: ComponentKind, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref().display().to_string();
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO components(kind, path) VALUES(?1, ?2)",
                params![kind.as_str(), path],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn list(&self) -> Result<Vec<ComponentEntry>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT kind, path FROM components ORDER BY kind, path")?;
            let rows = stmt.query_map([], |row| {
                let kind: String = row.get(0)?;
                let path: String = row.get(1)?;
                Ok((kind, path))
            })?;
            let mut entries = Vec::new();
            for row in rows {
                let (kind, path) = row?;
                entries.push(ComponentEntry {
                    kind: ComponentKind::from_str(&kind)?,
                    path: PathBuf::from(path),
                });
            }
            Ok(entries)
        })
    }

    fn with_connection<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = Connection::open(&self.path)
            .with_context(|| format!("open component registry {}", self.path.display()))?;
        f(&mut conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn registry_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "weather-component-list-{name}-{}-{}.db",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn registry_unique_sorts_components() {
        let path = registry_path("unique");
        let registry = ComponentRegistry::open(&path).unwrap();

        registry
            .record(ComponentKind::Bin, "/tmp/weather/bin/z")
            .unwrap();
        registry
            .record(ComponentKind::Config, "/tmp/weather/config/weather.toml")
            .unwrap();
        registry
            .record(ComponentKind::Bin, "/tmp/weather/bin/a")
            .unwrap();
        registry
            .record(ComponentKind::Bin, "/tmp/weather/bin/a")
            .unwrap();

        let entries = registry.list().unwrap();

        let _ = std::fs::remove_file(&path);
        assert_eq!(
            entries,
            vec![
                ComponentEntry {
                    kind: ComponentKind::Bin,
                    path: std::path::PathBuf::from("/tmp/weather/bin/a"),
                },
                ComponentEntry {
                    kind: ComponentKind::Bin,
                    path: std::path::PathBuf::from("/tmp/weather/bin/z"),
                },
                ComponentEntry {
                    kind: ComponentKind::Config,
                    path: std::path::PathBuf::from("/tmp/weather/config/weather.toml"),
                },
            ]
        );
    }
}
