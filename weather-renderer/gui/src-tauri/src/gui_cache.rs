use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use chrono::Local;
use prost::Message as _;
use rusqlite::{Connection, params};
use weather_schema::WeatherSnapshot;

#[cfg(test)]
use rusqlite::OptionalExtension as _;

const GUI_CACHE_FILE_NAME: &str = "weather-gui.db";
const GUI_CACHE_SCHEMA_VERSION: i64 = 1;
const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct GuiWeatherCache {
    path: PathBuf,
}

impl GuiWeatherCache {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) async fn load_today(
        &self,
        station_names: HashSet<String>,
    ) -> Result<Vec<WeatherSnapshot>> {
        let path = self.path.clone();
        let local_date = local_date();
        tokio::task::spawn_blocking(move || {
            let mut database = GuiCacheDatabase::open(&path)?;
            database.load_for_date(&local_date, &station_names)
        })
        .await
        .context("GUI cache read task failed")?
    }

    pub(crate) async fn store(&self, snapshot: WeatherSnapshot) -> Result<bool> {
        let path = self.path.clone();
        let local_date = local_date();
        tokio::task::spawn_blocking(move || {
            let Some((station_name, snapshot)) = prepare_for_cache(snapshot)? else {
                return Ok(false);
            };
            let mut database = GuiCacheDatabase::open(&path)?;
            database.store(&local_date, &station_name, &snapshot)?;
            Ok(true)
        })
        .await
        .context("GUI cache write task failed")?
    }
}

pub(crate) fn resolve_gui_cache_path(gui_config_path: &Path) -> Result<PathBuf> {
    let explicit = env::var_os("WEATHER_GUI_DB").map(PathBuf::from);
    let current_dir = env::current_dir().context("failed to resolve current directory")?;
    derive_gui_cache_path(explicit, gui_config_path, &current_dir)
}

fn derive_gui_cache_path(
    explicit: Option<PathBuf>,
    gui_config_path: &Path,
    current_dir: &Path,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(if path.is_absolute() {
            path
        } else {
            current_dir.join(path)
        });
    }
    let parent = gui_config_path.parent().with_context(|| {
        format!(
            "GUI config path `{}` has no parent directory",
            gui_config_path.display()
        )
    })?;
    Ok(parent.join(GUI_CACHE_FILE_NAME))
}

fn local_date() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

fn prepare_for_cache(mut snapshot: WeatherSnapshot) -> Result<Option<(String, WeatherSnapshot)>> {
    if snapshot.stale {
        return Ok(None);
    }
    let Some(station_name) = snapshot
        .station
        .as_ref()
        .map(|station| station.name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
    else {
        return Ok(None);
    };

    snapshot.radar = None;
    snapshot.debug = None;
    if let Some(real) = snapshot.real.as_mut() {
        for alert in &mut real.alerts {
            alert.url = None;
            alert.icon_url = None;
            alert.icon_resource_id = None;
        }
    }
    snapshot.stale = true;
    if snapshot.encoded_len() > MAX_SNAPSHOT_BYTES {
        bail!("GUI weather snapshot for `{station_name}` exceeds {MAX_SNAPSHOT_BYTES} bytes");
    }
    Ok(Some((station_name, snapshot)))
}

struct GuiCacheDatabase {
    connection: Connection,
}

impl GuiCacheDatabase {
    fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().with_context(|| {
            format!(
                "GUI cache path `{}` has no parent directory",
                path.display()
            )
        })?;
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create GUI cache directory {}", parent.display())
        })?;
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open GUI cache {}", path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .context("failed to configure GUI cache busy timeout")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable GUI cache WAL")?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .context("failed to configure GUI cache synchronization")?;

        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("failed to read GUI cache schema version")?;
        match version {
            0 => connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE IF NOT EXISTS weather_snapshot_cache (
                       station_name TEXT PRIMARY KEY NOT NULL,
                       local_date TEXT NOT NULL,
                       cached_at_unix_ms INTEGER NOT NULL,
                       snapshot BLOB NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS weather_snapshot_cache_date
                       ON weather_snapshot_cache(local_date);
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .context("failed to initialize GUI cache schema")?,
            GUI_CACHE_SCHEMA_VERSION => {}
            other => bail!(
                "GUI cache schema version {other} is not supported; expected {GUI_CACHE_SCHEMA_VERSION}"
            ),
        }
        Ok(Self { connection })
    }

    fn load_for_date(
        &mut self,
        local_date: &str,
        station_names: &HashSet<String>,
    ) -> Result<Vec<WeatherSnapshot>> {
        let transaction = self
            .connection
            .transaction()
            .context("failed to start GUI cache read transaction")?;
        transaction
            .execute(
                "DELETE FROM weather_snapshot_cache WHERE local_date <> ?1",
                [local_date],
            )
            .context("failed to prune expired GUI weather snapshots")?;

        let mut snapshots = Vec::new();
        let mut prune = Vec::new();
        {
            let mut statement = transaction
                .prepare(
                    "SELECT station_name, snapshot
                     FROM weather_snapshot_cache
                     WHERE local_date = ?1
                     ORDER BY station_name",
                )
                .context("failed to prepare GUI cache read")?;
            let mut rows = statement
                .query([local_date])
                .context("failed to query GUI cache")?;
            while let Some(row) = rows.next().context("failed to read GUI cache row")? {
                let station_name: String = row.get(0).context("invalid cached station name")?;
                if !station_names.contains(&station_name) {
                    prune.push(station_name);
                    continue;
                }
                let bytes: Vec<u8> = row.get(1).context("invalid cached weather snapshot")?;
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    prune.push(station_name);
                    continue;
                }
                let Ok(mut snapshot) = WeatherSnapshot::decode(bytes.as_slice()) else {
                    prune.push(station_name);
                    continue;
                };
                let decoded_name = snapshot
                    .station
                    .as_ref()
                    .map(|station| station.name.as_str())
                    .unwrap_or_default();
                if decoded_name != station_name {
                    prune.push(station_name);
                    continue;
                }
                snapshot.stale = true;
                snapshot.radar = None;
                snapshot.debug = None;
                snapshots.push(snapshot);
            }
        }
        for station_name in prune {
            transaction
                .execute(
                    "DELETE FROM weather_snapshot_cache WHERE station_name = ?1",
                    [station_name],
                )
                .context("failed to prune unusable GUI weather snapshot")?;
        }
        transaction
            .commit()
            .context("failed to commit GUI cache pruning")?;
        Ok(snapshots)
    }

    fn store(
        &mut self,
        local_date: &str,
        station_name: &str,
        snapshot: &WeatherSnapshot,
    ) -> Result<()> {
        let bytes = snapshot.encode_to_vec();
        let cached_at_unix_ms: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis()
            .try_into()
            .context("GUI cache timestamp exceeds SQLite integer range")?;
        let transaction = self
            .connection
            .transaction()
            .context("failed to start GUI cache write transaction")?;
        transaction
            .execute(
                "DELETE FROM weather_snapshot_cache WHERE local_date <> ?1",
                [local_date],
            )
            .context("failed to prune expired GUI weather snapshots")?;
        transaction
            .execute(
                "INSERT INTO weather_snapshot_cache (
                   station_name, local_date, cached_at_unix_ms, snapshot
                 ) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(station_name) DO UPDATE SET
                   local_date = excluded.local_date,
                   cached_at_unix_ms = excluded.cached_at_unix_ms,
                   snapshot = excluded.snapshot
                 WHERE weather_snapshot_cache.local_date <> excluded.local_date
                    OR weather_snapshot_cache.snapshot <> excluded.snapshot",
                params![station_name, local_date, cached_at_unix_ms, bytes],
            )
            .with_context(|| format!("failed to cache GUI weather for `{station_name}`"))?;
        transaction
            .commit()
            .context("failed to commit GUI weather cache")?;
        Ok(())
    }

    #[cfg(test)]
    fn contains(&self, station_name: &str) -> bool {
        self.connection
            .query_row(
                "SELECT 1 FROM weather_snapshot_cache WHERE station_name = ?1",
                [station_name],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use weather_schema::{DebugPayload, ObservedWeather, RadarInfo, StationRef};

    use super::*;

    fn snapshot(name: &str) -> WeatherSnapshot {
        WeatherSnapshot {
            station: Some(StationRef {
                province: "北京市".to_string(),
                city: "北京".to_string(),
                name: name.to_string(),
                unified_uuid: format!("uuid-{name}"),
            }),
            real: Some(ObservedWeather {
                info: Some("晴".to_string()),
                ..Default::default()
            }),
            radar: Some(RadarInfo {
                title: Some("华北".to_string()),
                image_resource_id: Some("process-local-resource".to_string()),
                ..Default::default()
            }),
            debug: Some(DebugPayload {
                raw_json: "large debug data".to_string(),
                ..Default::default()
            }),
            stale: false,
            ..Default::default()
        }
    }

    #[test]
    fn cache_keeps_only_allowed_current_day_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather-gui.db");
        let mut database = GuiCacheDatabase::open(&path).unwrap();
        let (first_name, first) = prepare_for_cache(snapshot("北京-北京市")).unwrap().unwrap();
        let (second_name, second) = prepare_for_cache(snapshot("上海-上海市")).unwrap().unwrap();
        database.store("2026-07-17", &first_name, &first).unwrap();
        database.store("2026-07-17", &second_name, &second).unwrap();

        let cached = database
            .load_for_date("2026-07-17", &HashSet::from(["北京-北京市".to_string()]))
            .unwrap();

        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].station.as_ref().unwrap().name, "北京-北京市");
        assert!(cached[0].stale);
        assert!(cached[0].radar.is_none());
        assert!(cached[0].debug.is_none());
        assert!(!database.contains("上海-上海市"));
    }

    #[test]
    fn next_day_access_prunes_previous_day() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather-gui.db");
        let mut database = GuiCacheDatabase::open(&path).unwrap();
        let (station_name, cached) = prepare_for_cache(snapshot("北京-北京市")).unwrap().unwrap();
        database
            .store("2026-07-16", &station_name, &cached)
            .unwrap();

        let loaded = database
            .load_for_date("2026-07-17", &HashSet::from([station_name.clone()]))
            .unwrap();

        assert!(loaded.is_empty());
        assert!(!database.contains(&station_name));
    }

    #[test]
    fn corrupt_snapshot_is_pruned_without_hiding_valid_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather-gui.db");
        let mut database = GuiCacheDatabase::open(&path).unwrap();
        let (station_name, cached) = prepare_for_cache(snapshot("北京-北京市")).unwrap().unwrap();
        database
            .store("2026-07-17", &station_name, &cached)
            .unwrap();
        database
            .connection
            .execute(
                "INSERT INTO weather_snapshot_cache (
                   station_name, local_date, cached_at_unix_ms, snapshot
                 ) VALUES (?1, ?2, ?3, ?4)",
                params!["损坏站点", "2026-07-17", 0_i64, vec![0xff_u8]],
            )
            .unwrap();

        let loaded = database
            .load_for_date(
                "2026-07-17",
                &HashSet::from([station_name.clone(), "损坏站点".to_string()]),
            )
            .unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].station.as_ref().unwrap().name, station_name);
        assert!(!database.contains("损坏站点"));
    }

    #[test]
    fn stale_engine_fallback_is_not_prepared_for_storage() {
        let mut stale = snapshot("北京-北京市");
        stale.stale = true;

        assert!(prepare_for_cache(stale).unwrap().is_none());
    }

    #[test]
    fn explicit_cache_path_is_resolved_relative_to_current_directory() {
        let resolved = derive_gui_cache_path(
            Some(PathBuf::from("custom/weather-gui.db")),
            Path::new("/home/test/.weather/config/weather-gui.toml"),
            Path::new("/workspace"),
        )
        .unwrap();

        assert_eq!(resolved, Path::new("/workspace/custom/weather-gui.db"));
    }

    #[test]
    fn default_cache_path_is_next_to_gui_config() {
        let resolved = derive_gui_cache_path(
            None,
            Path::new("/home/test/.weather/config/weather-gui.toml"),
            Path::new("/workspace"),
        )
        .unwrap();

        assert_eq!(
            resolved,
            Path::new("/home/test/.weather/config/weather-gui.db")
        );
    }
}
