use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use weather_configure::{
    AppConfig, ConfigState, ensure_config_file, load_for_engine_startup, load_or_default, validate,
    write_config_atomic,
};
use weather_db::{DatabasePaths, DbActor};
use weather_manifest::{ComponentKind, ComponentManifest};
use weather_schema::{
    ENGINE_LOCK_METADATA_VERSION, EngineLockMetadata, correlation_id, unix_timestamp_ms,
};
use weather_updater::{WeatherProvider, create_weather_provider};

use crate::{
    catalog::CatalogCoordinator,
    lifecycle::EngineControl,
    lock::LockGuard,
    lock::resolve_relative,
    server::{EventSink, run_engine_sockets},
    singleflight::Singleflight,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineExit {
    Shutdown,
    Restart,
}

#[derive(Clone)]
pub(crate) struct Engine {
    pub(crate) config_path: PathBuf,
    pub(crate) launch: EngineLockMetadata,
    pub(crate) config: ConfigState,
    pub(crate) config_commit: Arc<tokio::sync::Mutex<()>>,
    pub(crate) db: DbActor,
    pub(crate) provider: Arc<dyn WeatherProvider>,
    pub(crate) weather_singleflight: Singleflight<(String, bool), weather_schema::WeatherSnapshot>,
    pub(crate) catalog: CatalogCoordinator,
    pub(crate) sink: EventSink,
    pub(crate) control: EngineControl,
}

pub(crate) struct EngineRuntime {
    engine: Engine,
    _engine_lock: LockGuard,
}

pub async fn run_engine_with_owner(
    config_path: PathBuf,
    mode: String,
    owner_token: Option<String>,
) -> Result<EngineExit> {
    let runtime = EngineRuntime::start_with_owner(config_path, owner_token).await?;
    runtime.run_sockets(mode).await
}

impl EngineRuntime {
    #[cfg(test)]
    pub(crate) async fn start(config_path: PathBuf) -> Result<Self> {
        Self::start_with_provider_factory(config_path, None, create_weather_provider).await
    }

    async fn start_with_owner(config_path: PathBuf, owner_token: Option<String>) -> Result<Self> {
        Self::start_with_provider_factory(config_path, owner_token, create_weather_provider).await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_provider(
        config_path: PathBuf,
        provider: Arc<dyn WeatherProvider>,
    ) -> Result<Self> {
        Self::start_with_provider_factory(config_path, None, move |config| {
            validate_injected_provider(config, provider.as_ref())?;
            Ok(provider)
        })
        .await
    }

    #[cfg(test)]
    async fn start_with_provider_and_owner(
        config_path: PathBuf,
        provider: Arc<dyn WeatherProvider>,
        owner_token: String,
    ) -> Result<Self> {
        Self::start_with_provider_factory(config_path, Some(owner_token), move |config| {
            validate_injected_provider(config, provider.as_ref())?;
            Ok(provider)
        })
        .await
    }

    async fn start_with_provider_factory<F>(
        config_path: PathBuf,
        owner_token: Option<String>,
        factory: F,
    ) -> Result<Self>
    where
        F: FnOnce(&weather_configure::UpdaterConfig) -> Result<Arc<dyn WeatherProvider>>,
    {
        let config_path = absolute_config_path(config_path)?;
        // This first load is deliberately read-only: it is used only to find
        // the stable singleton lock, including for legacy in-memory configs.
        let preliminary_config = load_or_default(&config_path)?;
        let base_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let engine_lock_path = resolve_relative(&base_dir, &preliminary_config.engine.lock_path)?;
        let launch = launch_metadata(&config_path, owner_token)?;
        let _engine_lock = LockGuard::engine(engine_lock_path.clone(), &launch)?;

        // Only the singleton owner may create or migrate the persistent
        // config. Migration and station normalization share one atomic write.
        ensure_config_file(&config_path)?;
        let mut config = load_for_engine_startup(&config_path)?;
        let components = ComponentManifest::for_config_path(&config_path);
        components.record(ComponentKind::Config, &config_path)?;
        components.record(ComponentKind::Lock, &engine_lock_path)?;
        // Construct all stable fallible dependencies before taking the DB lock
        // or starting its worker. A constructor error must not leave an
        // asynchronously reaped worker temporarily holding the lock.
        let provider = factory(&config.updater)?;
        // Bind the lock to the canonical DB path. Lexical and symlink aliases
        // converge; hard-link aliases are not supported by suffix-derived
        // sidecars.
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
                launch,
                config: config_state,
                config_commit: Arc::new(tokio::sync::Mutex::new(())),
                db,
                provider,
                weather_singleflight: Singleflight::default(),
                catalog: CatalogCoordinator::default(),
                sink,
                control: EngineControl::new(),
            },
            _engine_lock,
        })
    }

    async fn run_sockets(self, mode: String) -> Result<EngineExit> {
        let ipc = self.engine.config.get().ipc;
        run_engine_sockets(self.engine, ipc.rpc_endpoint, ipc.pub_endpoint, mode).await
    }

    #[cfg(test)]
    pub(crate) fn test_engine(&self) -> Engine {
        self.engine.clone()
    }
}

fn launch_metadata(config_path: &Path, owner_token: Option<String>) -> Result<EngineLockMetadata> {
    if owner_token.as_deref().is_some_and(str::is_empty) {
        bail!("engine owner token must not be empty");
    }
    let started_at_unix_ms = u64::try_from(unix_timestamp_ms()?)
        .context("engine start timestamp does not fit in u64 milliseconds")?;
    Ok(EngineLockMetadata {
        version: ENGINE_LOCK_METADATA_VERSION,
        pid: std::process::id(),
        instance_id: correlation_id("engine-instance"),
        owner_token,
        started_at_unix_ms,
        config_path: config_path.display().to_string(),
    })
}

#[cfg(test)]
fn validate_injected_provider(
    config: &weather_configure::UpdaterConfig,
    provider: &dyn WeatherProvider,
) -> Result<()> {
    let provider_name = provider.provider_name();
    if provider_name.trim().is_empty() {
        bail!("injected weather provider name must not be empty");
    }
    if provider_name != config.default_provider {
        bail!(
            "injected weather provider `{provider_name}` does not match updater.default_provider `{}`",
            config.default_provider
        );
    }
    Ok(())
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use anyhow::bail;
    use weather_configure::{
        AppConfig, StationConfig, load_from_path, normalize_config_stations, write_config_atomic,
    };
    use weather_schema::{
        DebugPayload, Empty, ListCitiesRequest, ListCitiesResponse, ListProvincesRequest,
        ListProvincesResponse, ResponseStatus, RpcKind, RpcRequest, SCHEMA_VERSION,
        ShutdownRequest, WeatherSnapshot, decode_message, encode_message,
    };
    use weather_updater::{
        ProviderCity, ProviderFuture, ProviderProvince, WeatherFetch, WeatherProvider,
    };

    use super::*;

    fn legacy_v1_toml(config: &AppConfig) -> String {
        let mut value = toml::Value::try_from(config.clone()).unwrap();
        let root = value.as_table_mut().unwrap();
        root.insert("config_version".to_string(), toml::Value::Integer(1));
        root.get_mut("ipc")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "transport".to_string(),
                toml::Value::String("tcp".to_string()),
            );
        root.get_mut("db")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "lock_path".to_string(),
                toml::Value::String("weather.db.lock".to_string()),
            );
        root.insert(
            "daemon".to_string(),
            toml::Value::Table(toml::toml! {
                service_backend = "auto"
                foreground = true
                service_scope = "user"
            }),
        );
        toml::to_string_pretty(&value).unwrap()
    }

    struct ScriptedProvider {
        name: String,
        calls: AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(name: impl Into<String>) -> Self {
            Self {
                name: name.into(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl WeatherProvider for ScriptedProvider {
        fn provider_name(&self) -> &str {
            &self.name
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                Ok(vec![ProviderProvince {
                    provider_code: "P1".to_string(),
                    name: "province".to_string(),
                    url: "/province".to_string(),
                }])
            })
        }

        fn cities<'a>(
            &'a self,
            provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<ProviderCity>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(vec![ProviderCity {
                    provider_code: "S1".to_string(),
                    provider_province_code: provider_province_code.to_string(),
                    province: "province".to_string(),
                    city: "city".to_string(),
                    url: "/city".to_string(),
                }])
            })
        }

        fn weather<'a>(
            &'a self,
            _provider_station_id: &'a str,
            include_debug: bool,
        ) -> ProviderFuture<'a, WeatherFetch> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(WeatherFetch {
                    snapshot: WeatherSnapshot {
                        debug: include_debug.then(|| DebugPayload {
                            provider: "scripted".to_string(),
                            operation: "weather".to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    warnings: Vec::new(),
                })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_provider_drives_engine_and_is_shared_by_clones() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let mut config = AppConfig::default();
        config.db.path = directory.path().join("weather.db").display().to_string();
        config.updater.default_provider = "scripted".to_string();
        config.updater.provider[0].name = "scripted".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let scripted = Arc::new(ScriptedProvider::new("scripted"));
        let provider: Arc<dyn WeatherProvider> = scripted.clone();

        let runtime = EngineRuntime::start_with_provider(config_path, provider.clone())
            .await
            .unwrap();
        let cloned_engine = runtime.engine.clone();

        assert!(Arc::ptr_eq(&provider, &cloned_engine.provider));
        assert_eq!(cloned_engine.provider.provider_name(), "scripted");
        let provinces_response = cloned_engine
            .handle_rpc_request(
                RpcRequest {
                    schema_version: SCHEMA_VERSION.to_string(),
                    request_id: "provinces".to_string(),
                    kind: RpcKind::ListProvinces as i32,
                    payload: encode_message(&ListProvincesRequest {
                        page_offset: 0,
                        page_size: 10,
                    }),
                    ..Default::default()
                },
                "test",
                "rpc",
                "pub",
            )
            .await;
        assert_eq!(
            provinces_response.status,
            ResponseStatus::Ok as i32,
            "{:?}",
            provinces_response.error
        );
        assert_eq!(
            decode_message::<ListProvincesResponse>(&provinces_response.payload)
                .unwrap()
                .provinces[0]
                .name,
            "province"
        );
        let cities_response = cloned_engine
            .handle_rpc_request(
                RpcRequest {
                    schema_version: SCHEMA_VERSION.to_string(),
                    request_id: "cities".to_string(),
                    kind: RpcKind::ListCities as i32,
                    payload: encode_message(&ListCitiesRequest {
                        province: "province".to_string(),
                        page_offset: 0,
                        page_size: 10,
                    }),
                    ..Default::default()
                },
                "test",
                "rpc",
                "pub",
            )
            .await;
        assert_eq!(
            cities_response.status,
            ResponseStatus::Ok as i32,
            "{:?}",
            cities_response.error
        );
        assert_eq!(
            decode_message::<ListCitiesResponse>(&cities_response.payload)
                .unwrap()
                .cities[0]
                .city,
            "city"
        );
        assert!(
            cloned_engine
                .provider
                .weather("S1", true)
                .await
                .unwrap()
                .snapshot
                .debug
                .is_some()
        );
        assert_eq!(scripted.calls.load(Ordering::Relaxed), 3);
        runtime.engine.db.shutdown().await.unwrap();
    }

    #[test]
    fn restart_launches_keep_owner_but_receive_fresh_instance_ids() {
        let path = Path::new("/tmp/weather.toml");
        let first = launch_metadata(path, Some("owner-token".to_string())).unwrap();
        let second = launch_metadata(path, Some("owner-token".to_string())).unwrap();

        assert_eq!(first.owner_token, second.owner_token);
        assert_ne!(first.instance_id, second.instance_id);
        assert!(launch_metadata(path, Some(String::new())).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conditional_shutdown_is_accepted_only_for_the_current_owner() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let mut config = AppConfig::default();
        config.db.path = directory.path().join("weather.db").display().to_string();
        config.updater.default_provider = "scripted".to_string();
        config.updater.provider[0].name = "scripted".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let provider: Arc<dyn WeatherProvider> = Arc::new(ScriptedProvider::new("scripted"));
        let runtime = EngineRuntime::start_with_provider_and_owner(
            config_path,
            provider,
            "winner-token".to_string(),
        )
        .await
        .unwrap();
        let engine = runtime.engine.clone();

        let mismatch = engine
            .handle_rpc_request(
                RpcRequest {
                    schema_version: SCHEMA_VERSION.to_string(),
                    request_id: "mismatch".to_string(),
                    kind: RpcKind::Shutdown as i32,
                    payload: encode_message(&ShutdownRequest {
                        owner_token: Some("loser-token".to_string()),
                    }),
                    ..Default::default()
                },
                "test",
                "rpc",
                "pub",
            )
            .await;
        assert_eq!(mismatch.status, ResponseStatus::Error as i32);
        assert_eq!(mismatch.error.unwrap().code, "OWNER_MISMATCH");

        let matching = engine
            .handle_rpc_request(
                RpcRequest {
                    schema_version: SCHEMA_VERSION.to_string(),
                    request_id: "matching".to_string(),
                    kind: RpcKind::Shutdown as i32,
                    payload: encode_message(&ShutdownRequest {
                        owner_token: Some("winner-token".to_string()),
                    }),
                    ..Default::default()
                },
                "test",
                "rpc",
                "pub",
            )
            .await;
        assert_eq!(matching.status, ResponseStatus::Accepted as i32);

        let legacy = engine
            .handle_rpc_request(
                RpcRequest {
                    schema_version: SCHEMA_VERSION.to_string(),
                    request_id: "legacy".to_string(),
                    kind: RpcKind::Shutdown as i32,
                    payload: encode_message(&Empty {}),
                    ..Default::default()
                },
                "test",
                "rpc",
                "pub",
            )
            .await;
        assert_eq!(legacy.status, ResponseStatus::Accepted as i32);
        assert_eq!(
            engine.status("test", "rpc", "pub").instance_id,
            engine.launch.instance_id
        );

        runtime.engine.db.shutdown().await.unwrap();
    }

    async fn assert_injected_provider_is_rejected(provider_name: &str, expected_error: &str) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let db_lock = DatabasePaths::canonicalized(&db_path).unwrap().lock;
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.updater.default_provider = "scripted".to_string();
        config.updater.provider[0].name = "scripted".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let provider: Arc<dyn WeatherProvider> = Arc::new(ScriptedProvider::new(provider_name));

        let error = EngineRuntime::start_with_provider(config_path, provider)
            .await
            .err()
            .expect("invalid injected provider unexpectedly started")
            .to_string();

        assert!(error.contains(expected_error), "{error}");
        assert!(!db_path.exists());
        let lock = LockGuard::exclusive(db_lock).unwrap();
        drop(lock);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_provider_rejects_an_empty_name_before_database_startup() {
        assert_injected_provider_is_rejected("  ", "name must not be empty").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_provider_rejects_a_mismatched_name_before_database_startup() {
        assert_injected_provider_is_rejected(
            "other",
            "does not match updater.default_provider `scripted`",
        )
        .await;
    }

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
    async fn legacy_config_is_persisted_only_after_engine_lock_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let original = legacy_v1_toml(&AppConfig::default());
        std::fs::write(&config_path, &original).unwrap();
        let blocker = LockGuard::exclusive(directory.path().join("engine.lock")).unwrap();

        assert!(EngineRuntime::start(config_path.clone()).await.is_err());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!directory.path().join("component-manifest.toml").exists());

        drop(blocker);
        let runtime = EngineRuntime::start(config_path.clone()).await.unwrap();
        let persisted = std::fs::read_to_string(&config_path).unwrap();
        let value: toml::Value = toml::from_str(&persisted).unwrap();

        assert_eq!(
            value
                .get("config_version")
                .and_then(toml::Value::as_integer),
            Some(i64::from(weather_configure::SUPPORTED_CONFIG_VERSION))
        );
        assert!(value.get("daemon").is_none());
        assert!(value.get("ipc").unwrap().get("transport").is_none());
        assert!(value.get("db").unwrap().get("lock_path").is_none());
        assert!(value.get("engine").unwrap().get("lock_path").is_some());
        runtime.engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_config_is_created_only_by_engine_lock_owner() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let blocker = LockGuard::exclusive(directory.path().join("engine.lock")).unwrap();

        assert!(EngineRuntime::start(config_path.clone()).await.is_err());
        assert!(!config_path.exists());
        assert!(!directory.path().join("component-manifest.toml").exists());
        assert!(
            !directory
                .path()
                .join("component-manifest.toml.lock")
                .exists()
        );

        drop(blocker);
        let runtime = EngineRuntime::start(config_path.clone()).await.unwrap();

        assert!(config_path.exists());
        assert_eq!(
            load_from_path(&config_path).unwrap().config_version,
            weather_configure::SUPPORTED_CONFIG_VERSION
        );
        assert!(directory.path().join("component-manifest.toml").exists());
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
    async fn runtime_manifest_records_exact_database_component_paths() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let data = directory.path().join("cache.sqlite3");
        let paths = DatabasePaths::canonicalized(&data).unwrap();
        let mut config = AppConfig::default();
        config.db.path = data.display().to_string();
        write_config_atomic(&config_path, &config).unwrap();

        let runtime = EngineRuntime::start(config_path.clone()).await.unwrap();
        runtime.engine.db.shutdown().await.unwrap();
        let entries = ComponentManifest::for_config_path(&config_path)
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
