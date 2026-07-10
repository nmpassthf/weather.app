use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use weather_configure::{
    AppConfig, ComponentKind, ComponentRegistry, ConfigState, ensure_config_file, load_or_default,
    normalize_config_stations, validate, write_config_atomic,
};
use weather_db::{DatabasePaths, DbActor};
use weather_updater::NmcUpdater;

use crate::{
    lifecycle::EngineControl,
    lock::LockGuard,
    lock::resolve_relative,
    server::{EventSink, run_engine_sockets},
    singleflight::WeatherSingleflight,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineExit {
    Shutdown,
    Restart,
}

#[derive(Clone)]
pub(crate) struct Engine {
    pub(crate) config_path: PathBuf,
    pub(crate) config: ConfigState,
    pub(crate) config_commit: Arc<tokio::sync::Mutex<()>>,
    pub(crate) db: DbActor,
    pub(crate) updater: NmcUpdater,
    pub(crate) weather_singleflight: WeatherSingleflight,
    pub(crate) sink: EventSink,
    pub(crate) control: EngineControl,
}

pub struct EngineRuntime {
    engine: Engine,
    _engine_lock: LockGuard,
}

pub async fn run_engine(config_path: PathBuf, mode: String) -> Result<EngineExit> {
    let runtime = EngineRuntime::start(config_path).await?;
    runtime.run_sockets(mode).await
}

impl EngineRuntime {
    pub async fn start(config_path: PathBuf) -> Result<Self> {
        let config_path = absolute_config_path(config_path)?;
        ensure_config_file(&config_path)?;
        let components = ComponentRegistry::for_config_path(&config_path)?;
        components.record(ComponentKind::Config, &config_path)?;
        let mut config = load_or_default(&config_path)?;
        let base_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let engine_lock_path = resolve_relative(&base_dir, &config.engine.lock_path)?;
        components.record(ComponentKind::Lock, &engine_lock_path)?;
        let _engine_lock = LockGuard::exclusive(engine_lock_path)?;
        if normalize_config_stations(&mut config) {
            validate(&config)?;
            write_config_atomic(&config_path, &config)?;
        }
        // Construct all stable fallible dependencies before taking the DB lock
        // or starting its worker. A constructor error must not leave an
        // asynchronously reaped worker temporarily holding the lock.
        let updater = NmcUpdater::new(&config.updater)?;
        // Bind the lock to the canonical DB path and ignore db.lock_path (kept
        // only for TOML compatibility). Lexical and symlink aliases converge;
        // hard-link aliases are not supported by suffix-derived sidecars.
        let db_path = resolve_relative(&base_dir, &config.db.path)?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
        let db_paths = DatabasePaths::canonicalized(db_path)?;
        components.record(ComponentKind::Db, &db_paths.data)?;
        components.record(ComponentKind::Db, &db_paths.wal)?;
        components.record(ComponentKind::Db, &db_paths.shm)?;
        components.record(ComponentKind::Lock, &db_paths.lock)?;
        let db_lock = LockGuard::exclusive(db_paths.lock.clone())?;
        recover_pending_timezone(&db_paths.data, &config_path, &mut config)?;
        let db = DbActor::start_with_lease(db_paths.data, config.db.timezone.clone(), db_lock)?;
        let config_state = ConfigState::new(config);
        let (sink, _) = tokio::sync::broadcast::channel(256);
        Ok(Self {
            engine: Engine {
                config_path,
                config: config_state,
                config_commit: Arc::new(tokio::sync::Mutex::new(())),
                db,
                updater,
                weather_singleflight: WeatherSingleflight::default(),
                sink,
                control: EngineControl::new(),
            },
            _engine_lock,
        })
    }

    pub async fn run_sockets(self, mode: String) -> Result<EngineExit> {
        let ipc = self.engine.config.get().ipc;
        run_engine_sockets(self.engine, ipc.rpc_endpoint, ipc.pub_endpoint, mode).await
    }
}

fn recover_pending_timezone(
    db_path: &Path,
    config_path: &Path,
    config: &mut AppConfig,
) -> Result<bool> {
    let Some(target) = DbActor::inspect_pending_timezone(db_path)? else {
        return Ok(false);
    };
    if config.db.timezone == target {
        return Ok(false);
    }
    let mut recovered = config.clone();
    recovered.db.timezone = target;
    validate(&recovered)?;
    write_config_atomic(config_path, &recovered)?;
    *config = recovered;
    Ok(true)
}

fn absolute_config_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::bail;
    use weather_configure::{AppConfig, StationConfig, load_from_path, write_config_atomic};

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_normalizes_config_before_exposing_live_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.toml");
        let config = AppConfig {
            stations: vec![StationConfig {
                name: " 北京 -  - 朝阳 ".to_string(),
                enabled: false,
            }],
            ..Default::default()
        };
        std::fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();

        let runtime = EngineRuntime::start(path.clone()).await.unwrap();
        let mut expected = config;
        normalize_config_stations(&mut expected);

        assert_eq!(runtime.engine.config.get(), expected);
        assert_eq!(load_from_path(&path).unwrap(), expected);
        runtime.engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_recovery_rolls_pending_timezone_forward_and_clears_marker() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.db.timezone = "UTC".to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let db = DbActor::start(db_path.clone(), "UTC".to_string()).unwrap();
        let failed = db
            .migrate_timezone_bundle(
                "UTC".to_string(),
                "Asia/Shanghai".to_string(),
                || bail!("injected config persistence failure"),
                |_| {},
            )
            .await;
        assert!(failed.is_err());
        db.shutdown().await.unwrap();
        assert_eq!(
            DbActor::inspect_pending_timezone(&db_path)
                .unwrap()
                .as_deref(),
            Some("Asia/Shanghai")
        );

        assert!(recover_pending_timezone(&db_path, &config_path, &mut config).unwrap());
        assert_eq!(config.db.timezone, "Asia/Shanghai");
        assert_eq!(
            load_from_path(&config_path).unwrap().db.timezone,
            "Asia/Shanghai"
        );

        let db = DbActor::start(db_path.clone(), config.db.timezone.clone()).unwrap();
        assert_eq!(
            db.get_db_timezone().await.unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
        assert_eq!(DbActor::inspect_pending_timezone(&db_path).unwrap(), None);
        db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timezone_bundle_keeps_database_file_and_live_config_in_sync() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.db.timezone = "UTC".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let runtime = EngineRuntime::start(config_path.clone()).await.unwrap();

        let migration = runtime
            .engine
            .migrate_db_timezone("Asia/Shanghai".to_string())
            .await
            .unwrap();

        assert_eq!(migration.old_timezone, "UTC");
        assert_eq!(migration.new_timezone, "Asia/Shanghai");
        assert_eq!(runtime.engine.config.get().db.timezone, "Asia/Shanghai");
        assert_eq!(
            load_from_path(&config_path).unwrap().db.timezone,
            "Asia/Shanghai"
        );
        assert_eq!(
            runtime
                .engine
                .db
                .get_db_timezone()
                .await
                .unwrap()
                .as_deref(),
            Some("Asia/Shanghai")
        );
        assert_eq!(DbActor::inspect_pending_timezone(&db_path).unwrap(), None);
        runtime.engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_registers_exact_database_component_paths() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let data = directory.path().join("cache.sqlite3");
        let paths = DatabasePaths::canonicalized(&data).unwrap();
        let mut config = AppConfig::default();
        config.db.path = data.display().to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let runtime = EngineRuntime::start(config_path.clone()).await.unwrap();
        runtime.engine.db.shutdown().await.unwrap();
        let entries = ComponentRegistry::for_config_path(&config_path)
            .unwrap()
            .list()
            .unwrap();
        let mut database_entries = entries
            .iter()
            .filter(|entry| entry.kind == ComponentKind::Db)
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        database_entries.sort();
        let mut expected = vec![paths.data, paths.wal, paths.shm];
        expected.sort();

        assert_eq!(database_entries, expected);
        assert!(
            entries
                .iter()
                .any(|entry| { entry.kind == ComponentKind::Lock && entry.path == paths.lock })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_recovery_waits_until_after_database_lock() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let data = directory.path().join("weather.db");
        let paths = DatabasePaths::canonicalized(&data).unwrap();
        let mut config = AppConfig::default();
        config.db.path = data.display().to_string();
        config.db.timezone = "UTC".to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let db = DbActor::start(data, "UTC".to_string()).unwrap();
        db.migrate_timezone_bundle(
            "UTC".to_string(),
            "Asia/Shanghai".to_string(),
            || bail!("injected config persistence failure"),
            |_| {},
        )
        .await
        .unwrap_err();
        db.shutdown().await.unwrap();

        let blocker = LockGuard::exclusive(paths.lock).unwrap();
        assert!(EngineRuntime::start(config_path.clone()).await.is_err());
        assert_eq!(load_from_path(&config_path).unwrap().db.timezone, "UTC");
        drop(blocker);

        let runtime = EngineRuntime::start(config_path.clone()).await.unwrap();
        assert_eq!(runtime.engine.config.get().db.timezone, "Asia/Shanghai");
        assert_eq!(
            load_from_path(&config_path).unwrap().db.timezone,
            "Asia/Shanghai"
        );
        runtime.engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn database_lock_lease_outlives_runtime_until_worker_join() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let data = directory.path().join("weather.db");
        let lock = DatabasePaths::canonicalized(&data).unwrap().lock;
        let mut config = AppConfig::default();
        config.db.path = data.display().to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let runtime = EngineRuntime::start(config_path).await.unwrap();
        let db = runtime.engine.db.clone();
        drop(runtime);
        assert!(LockGuard::exclusive(lock.clone()).is_err());

        db.shutdown().await.unwrap();
        let reacquired = LockGuard::exclusive(lock).unwrap();
        drop(reacquired);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn updater_construction_failure_never_takes_database_lock() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let data = directory.path().join("weather.db");
        let lock = DatabasePaths::canonicalized(&data).unwrap().lock;
        let mut config = AppConfig::default();
        config.db.path = data.display().to_string();
        config.updater.default_provider = "unsupported".to_string();
        config.updater.provider[0].name = "unsupported".to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let error = EngineRuntime::start(config_path)
            .await
            .err()
            .expect("unsupported updater unexpectedly started")
            .to_string();
        assert!(error.contains("unsupported updater provider"), "{error}");
        assert!(!data.exists());

        let reacquired = LockGuard::exclusive(lock).unwrap();
        drop(reacquired);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_creates_and_canonicalizes_missing_database_parent() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let data = directory
            .path()
            .join("nested")
            .join("database")
            .join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = data.display().to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let runtime = EngineRuntime::start(config_path).await.unwrap();
        let paths = DatabasePaths::canonicalized(&data).unwrap();
        assert_eq!(paths.data, std::fs::canonicalize(&data).unwrap());
        assert!(paths.lock.exists());
        runtime.engine.db.shutdown().await.unwrap();
    }
}
