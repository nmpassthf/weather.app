use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml::Value;

use crate::{
    AppConfig, DbConfig, EngineConfig, IpcConfig, SUPPORTED_CONFIG_VERSION, StationConfig,
    UpdaterConfig, normalize_config_stations, normalize_station_name, write_config_atomic,
};

const FIRST_CONFIG_VERSION: u32 = 1;

struct ConfigMigration {
    /// Version produced by this migration.
    version: u32,
    name: &'static str,
    apply: fn(&mut Value) -> Result<()>,
}

const CONFIG_MIGRATIONS: &[ConfigMigration] = &[ConfigMigration {
    version: 2,
    name: "remove legacy runtime fields",
    apply: migrate_v1_to_v2,
}];

#[derive(Debug)]
struct LoadedConfig {
    config: AppConfig,
    source_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAppConfigV1 {
    config_version: u32,
    engine: EngineConfig,
    ipc: LegacyIpcConfigV1,
    db: LegacyDbConfigV1,
    updater: UpdaterConfig,
    daemon: LegacyDaemonConfigV1,
    stations: Vec<StationConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyIpcConfigV1 {
    transport: String,
    rpc_endpoint: String,
    pub_endpoint: String,
    hmac: String,
    hmac_key: String,
    hmac_env_key_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDbConfigV1 {
    path: String,
    #[serde(rename = "lock_path")]
    _lock_path: String,
    timezone: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDaemonConfigV1 {
    #[serde(rename = "service_backend")]
    _service_backend: String,
    #[serde(rename = "foreground")]
    _foreground: bool,
    #[serde(rename = "service_scope")]
    _service_scope: String,
}

/// 按 `ipc.hmac` 模式解析出实际 HMAC key。
///
/// - `disabled` → `Ok(None)`
/// - `hmac_key` → 读 `ipc.hmac_key` 字段
/// - `hmac_env_key` → 读环境变量 `ipc.hmac_env_key_name`
///
/// 调用方应先通过 `validate()` 确保配置合法,此函数仅在 validate 通过后调用。
pub fn resolve_hmac_key(config: &AppConfig) -> Result<Option<[u8; 32]>> {
    match config.ipc.hmac.as_str() {
        "disabled" => Ok(None),
        "hmac_key" => Ok(Some(weather_schema::hmac_key_from_str(
            &config.ipc.hmac_key,
        )?)),
        "hmac_env_key" => {
            let value = std::env::var(&config.ipc.hmac_env_key_name).with_context(|| {
                format!(
                    "ipc.hmac_env_key_name `{}` is not set",
                    config.ipc.hmac_env_key_name
                )
            })?;
            Ok(Some(weather_schema::hmac_key_from_str(&value)?))
        }
        other => bail!("invalid ipc.hmac mode `{other}`"),
    }
}

pub fn load_or_default(path: &Path) -> Result<AppConfig> {
    if path
        .try_exists()
        .with_context(|| format!("failed to inspect config {}", path.display()))?
    {
        load_from_path(path)
    } else {
        let config = AppConfig::default();
        validate(&config)?;
        Ok(config)
    }
}

pub fn ensure_config_file(path: &Path) -> Result<()> {
    if path
        .try_exists()
        .with_context(|| format!("failed to inspect config {}", path.display()))?
    {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    let config = AppConfig::default();
    validate(&config)?;
    write_config_atomic(path, &config)
        .with_context(|| format!("failed to write default config {}", path.display()))
}

pub fn load_from_path(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let loaded = load_from_str(&content)
        .map_err(|err| anyhow::anyhow!("config {} is incompatible: {err:#}", path.display()))?;
    validate(&loaded.config)?;
    Ok(loaded.config)
}

/// Load, migrate and normalize the configuration after the engine owns its
/// singleton lock. A legacy document is replaced atomically only after every
/// migration and validation step succeeds.
pub fn load_for_engine_startup(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let LoadedConfig {
        mut config,
        source_version,
    } = load_from_str(&content)
        .map_err(|err| anyhow::anyhow!("config {} is incompatible: {err:#}", path.display()))?;
    let normalized = normalize_config_stations(&mut config);
    validate(&config)?;
    if source_version != SUPPORTED_CONFIG_VERSION || normalized {
        write_config_atomic(path, &config)?;
    }
    Ok(config)
}

fn load_from_str(content: &str) -> Result<LoadedConfig> {
    let value: Value = toml::from_str(content).context("failed to parse TOML config")?;
    let source_version = config_version(&value)?;
    let migrated = run_config_migrations(value, SUPPORTED_CONFIG_VERSION, CONFIG_MIGRATIONS)?;
    let config: AppConfig = migrated
        .try_into()
        .context("failed to deserialize current config shape")?;
    Ok(LoadedConfig {
        config,
        source_version,
    })
}

fn run_config_migrations(
    mut value: Value,
    target_version: u32,
    migrations: &[ConfigMigration],
) -> Result<Value> {
    validate_migration_registry(target_version, migrations)?;
    let source_version = config_version(&value)?;
    if source_version < FIRST_CONFIG_VERSION {
        bail!(
            "config_version {source_version} predates the earliest supported version {FIRST_CONFIG_VERSION}"
        );
    }
    if source_version > target_version {
        bail!("config_version {source_version} is newer than supported version {target_version}");
    }

    for migration in migrations
        .iter()
        .filter(|migration| migration.version > source_version)
    {
        (migration.apply)(&mut value).with_context(|| {
            format!(
                "config migration to v{} ({}) failed",
                migration.version, migration.name
            )
        })?;
        set_config_version(&mut value, migration.version).with_context(|| {
            format!(
                "config migration to v{} ({}) produced an invalid document",
                migration.version, migration.name
            )
        })?;
    }

    Ok(value)
}

fn validate_migration_registry(target_version: u32, migrations: &[ConfigMigration]) -> Result<()> {
    if target_version < FIRST_CONFIG_VERSION {
        bail!("invalid target config version {target_version}");
    }
    let expected_len = usize::try_from(target_version - FIRST_CONFIG_VERSION)
        .context("target config version does not fit in usize")?;
    if migrations.len() != expected_len {
        bail!(
            "config migration registry must contain {expected_len} steps through v{target_version}, found {}",
            migrations.len()
        );
    }
    for (index, migration) in migrations.iter().enumerate() {
        let expected = FIRST_CONFIG_VERSION + u32::try_from(index)? + 1;
        if migration.version != expected {
            bail!(
                "config migration registry gap: expected target v{expected}, found v{} ({})",
                migration.version,
                migration.name
            );
        }
    }
    Ok(())
}

fn config_version(value: &Value) -> Result<u32> {
    let raw = value
        .as_table()
        .context("config root must be a TOML table")?
        .get("config_version")
        .context("missing required config_version")?
        .as_integer()
        .context("config_version must be a non-negative integer")?;
    u32::try_from(raw).context("config_version must fit in u32")
}

fn set_config_version(value: &mut Value, version: u32) -> Result<()> {
    let table = value
        .as_table_mut()
        .context("config root must remain a TOML table")?;
    table.insert(
        "config_version".to_string(),
        Value::Integer(i64::from(version)),
    );
    Ok(())
}

fn migrate_v1_to_v2(value: &mut Value) -> Result<()> {
    let legacy: LegacyAppConfigV1 = value
        .clone()
        .try_into()
        .context("invalid v1 config shape")?;
    let LegacyAppConfigV1 {
        config_version,
        engine,
        ipc,
        db,
        updater,
        daemon,
        stations,
    } = legacy;
    let LegacyIpcConfigV1 {
        transport,
        rpc_endpoint,
        pub_endpoint,
        hmac,
        hmac_key,
        hmac_env_key_name,
    } = ipc;
    if transport != "tcp" {
        bail!("only legacy ipc.transport = \"tcp\" can be migrated");
    }
    let LegacyDbConfigV1 {
        path,
        _lock_path: _,
        timezone,
    } = db;
    let LegacyDaemonConfigV1 {
        _service_backend: _,
        _foreground: _,
        _service_scope: _,
    } = daemon;
    let current = AppConfig {
        config_version,
        engine,
        ipc: IpcConfig {
            rpc_endpoint,
            pub_endpoint,
            hmac,
            hmac_key,
            hmac_env_key_name,
        },
        db: DbConfig { path, timezone },
        updater,
        stations,
    };
    *value = Value::try_from(current).context("failed to construct v2 config")?;
    Ok(())
}

pub fn validate(config: &AppConfig) -> Result<()> {
    if config.config_version != SUPPORTED_CONFIG_VERSION {
        bail!(
            "config_version {} is incompatible; engine supports {}",
            config.config_version,
            SUPPORTED_CONFIG_VERSION
        );
    }
    if !matches!(
        config.engine.log_level.as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    ) {
        bail!(
            "engine.log_level `{}` is invalid; expected off / error / warn / info / debug / trace",
            config.engine.log_level
        );
    }
    if !config.ipc.rpc_endpoint.starts_with("tcp://") {
        bail!("ipc.rpc_endpoint must be a tcp:// endpoint");
    }
    if !config.ipc.pub_endpoint.starts_with("tcp://") {
        bail!("ipc.pub_endpoint must be a tcp:// endpoint");
    }
    if config.ipc.rpc_endpoint == config.ipc.pub_endpoint {
        bail!("ipc.rpc_endpoint and ipc.pub_endpoint must differ");
    }
    match config.ipc.hmac.as_str() {
        "disabled" => {}
        "hmac_key" => {
            if config.ipc.hmac_key.is_empty() {
                bail!("ipc.hmac = \"hmac_key\" requires non-empty ipc.hmac_key");
            }
            weather_schema::hmac_key_from_str(&config.ipc.hmac_key)?;
        }
        "hmac_env_key" => {
            if config.ipc.hmac_env_key_name.is_empty() {
                bail!("ipc.hmac = \"hmac_env_key\" requires non-empty ipc.hmac_env_key_name");
            }
            if std::env::var(&config.ipc.hmac_env_key_name).is_err() {
                bail!(
                    "ipc.hmac_env_key_name `{}` is not set in environment",
                    config.ipc.hmac_env_key_name
                );
            }
        }
        other => {
            bail!("ipc.hmac `{other}` is invalid; expected disabled / hmac_key / hmac_env_key");
        }
    }
    if config.db.path.trim().is_empty() {
        bail!("db.path must not be empty");
    }
    if chrono_tz::Tz::from_str(&config.db.timezone).is_err() {
        bail!(
            "db.timezone `{}` is not a valid IANA timezone (e.g. Asia/Shanghai)",
            config.db.timezone
        );
    }
    if config.updater.provider.is_empty() {
        bail!("updater.provider must contain at least one provider");
    }
    if !config
        .updater
        .provider
        .iter()
        .any(|provider| provider.name == config.updater.default_provider)
    {
        bail!("updater.default_provider must match a configured provider");
    }
    validate_network_proxy_urls(
        "updater.network",
        config.updater.network.http_proxy.as_deref(),
        config.updater.network.https_proxy.as_deref(),
        config.updater.network.all_proxy.as_deref(),
    )?;
    for provider in &config.updater.provider {
        validate_network_proxy_urls(
            &format!("updater provider `{}` network", provider.name),
            provider.network.http_proxy.as_deref(),
            provider.network.https_proxy.as_deref(),
            provider.network.all_proxy.as_deref(),
        )?;
    }
    let mut station_names = HashSet::new();
    for station in &config.stations {
        let normalized = normalize_station_name(&station.name);
        if normalized.is_empty() {
            bail!("station.name must not be empty");
        }
        if !station_names.insert(normalized.clone()) {
            bail!("duplicate station.name `{normalized}` after normalization");
        }
    }
    Ok(())
}

fn validate_network_proxy_urls(
    scope: &str,
    http_proxy: Option<&str>,
    https_proxy: Option<&str>,
    all_proxy: Option<&str>,
) -> Result<()> {
    for (field, value) in [
        ("http_proxy", http_proxy),
        ("https_proxy", https_proxy),
        ("all_proxy", all_proxy),
    ] {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };
        let parsed = url::Url::parse(value)
            .with_context(|| format!("{scope}.{field} is not a valid proxy URL"))?;
        if !matches!(
            parsed.scheme(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ) || parsed.host_str().is_none()
        {
            bail!(
                "{scope}.{field} must use http, https, socks4, socks4a, socks5, or socks5h and include a host"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        default_config_toml,
        types::{AppConfig, IpcConfig},
    };
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn base_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.ipc.rpc_endpoint = "tcp://127.0.0.1:44445".to_string();
        config.ipc.pub_endpoint = "tcp://127.0.0.1:44444".to_string();
        config
    }

    fn temp_config_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "weather-config-test-{name}-{}-{}.toml",
            std::process::id(),
            nanos
        ))
    }

    fn legacy_v1_value() -> Value {
        let mut value: Value = toml::from_str(&default_config_toml()).unwrap();
        let root = value.as_table_mut().unwrap();
        root.insert("config_version".to_string(), Value::Integer(1));
        root.get_mut("ipc")
            .and_then(Value::as_table_mut)
            .unwrap()
            .insert("transport".to_string(), Value::String("tcp".to_string()));
        root.get_mut("db")
            .and_then(Value::as_table_mut)
            .unwrap()
            .insert(
                "lock_path".to_string(),
                Value::String("weather.db.lock".to_string()),
            );
        root.insert(
            "daemon".to_string(),
            Value::Table(toml::toml! {
                service_backend = "auto"
                foreground = true
                service_scope = "user"
            }),
        );
        value
    }

    fn legacy_v1_toml() -> String {
        toml::to_string_pretty(&legacy_v1_value()).unwrap()
    }

    fn synthetic_v3_migration(_value: &mut Value) -> Result<()> {
        Ok(())
    }

    fn failing_v3_migration(value: &mut Value) -> Result<()> {
        value
            .as_table_mut()
            .unwrap()
            .insert("partial".to_string(), Value::Boolean(true));
        bail!("injected migration failure")
    }

    #[test]
    fn valid_config_passes() {
        assert!(validate(&base_config()).is_ok());
    }

    #[test]
    fn validates_global_and_provider_network_proxy_urls() {
        let mut config = base_config();
        config.updater.network.http_proxy = Some("http://127.0.0.1:8123".to_string());
        config.updater.provider[0].network.https_proxy =
            Some("https://provider-proxy.example:8443".to_string());
        assert!(validate(&config).is_ok());

        config.updater.provider[0].network.all_proxy = Some("ftp://127.0.0.1:21".to_string());
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("network.all_proxy must use http, https, socks4"));
    }

    #[test]
    fn ensuring_config_has_no_component_manifest_side_effect() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.toml");

        ensure_config_file(&path).unwrap();

        assert!(path.is_file());
        assert!(!directory.path().join("component-manifest.toml").exists());
        assert!(
            !directory
                .path()
                .join("component-manifest.toml.lock")
                .exists()
        );
    }

    #[test]
    fn validate_rejects_wrong_config_version() {
        let mut config = base_config();
        config.config_version = SUPPORTED_CONFIG_VERSION + 1;
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("config_version"), "{err}");
        assert!(err.contains("engine supports"), "{err}");
    }

    #[test]
    fn load_rejects_missing_config_version() {
        let path = temp_config_path("missing-version");
        let mut content = default_config_toml();
        content = content
            .lines()
            .filter(|line| !line.starts_with("config_version"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, content).unwrap();

        let err = load_from_path(&path).unwrap_err();

        let _ = std::fs::remove_file(&path);
        assert!(format!("{err:#}").contains("missing required config_version"));
    }

    #[test]
    fn current_shape_rejects_unknown_fields() {
        let path = temp_config_path("current-unknown");
        let mut value: Value = toml::from_str(&default_config_toml()).unwrap();
        value
            .get_mut("ipc")
            .and_then(Value::as_table_mut)
            .unwrap()
            .insert("enable_hmac".to_string(), toml::Value::Boolean(true));
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let err = load_from_path(&path).unwrap_err();

        let _ = std::fs::remove_file(&path);
        assert!(format!("{err:#}").contains("unknown field `enable_hmac`"));
    }

    #[test]
    fn current_config_without_network_uses_compatible_defaults() {
        let mut value: Value = toml::from_str(&default_config_toml()).unwrap();
        value
            .get_mut("updater")
            .and_then(Value::as_table_mut)
            .unwrap()
            .remove("network");

        let loaded = load_from_str(&toml::to_string_pretty(&value).unwrap()).unwrap();

        assert_eq!(
            loaded.config.updater.network,
            crate::NetworkConfig::default()
        );
        assert!(loaded.config.updater.provider[0].network.is_empty());
    }

    #[test]
    fn current_config_without_log_level_uses_info() {
        let mut value: Value = toml::from_str(&default_config_toml()).unwrap();
        value
            .get_mut("engine")
            .and_then(Value::as_table_mut)
            .unwrap()
            .remove("log_level");

        let loaded = load_from_str(&toml::to_string_pretty(&value).unwrap()).unwrap();

        assert_eq!(loaded.config.engine.log_level, "info");
    }

    #[test]
    fn validate_rejects_invalid_engine_log_level() {
        let mut config = base_config();
        config.engine.log_level = "verbose".to_string();

        let err = validate(&config).unwrap_err().to_string();

        assert!(
            err.contains("engine.log_level `verbose` is invalid"),
            "{err}"
        );
    }

    #[test]
    fn legacy_v1_shape_rejects_missing_fields() {
        let path = temp_config_path("legacy-missing");
        let mut value = legacy_v1_value();
        value
            .get_mut("ipc")
            .and_then(Value::as_table_mut)
            .unwrap()
            .remove("hmac_env_key_name");
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let err = load_from_path(&path).unwrap_err();

        let _ = std::fs::remove_file(&path);
        assert!(format!("{err:#}").contains("missing field `hmac_env_key_name`"));
    }

    #[test]
    fn legacy_v1_shape_rejects_unknown_fields() {
        let path = temp_config_path("legacy-unknown");
        let mut value = legacy_v1_value();
        value
            .get_mut("daemon")
            .and_then(Value::as_table_mut)
            .unwrap()
            .insert("unexpected".to_string(), Value::Boolean(true));
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let err = load_from_path(&path).unwrap_err();

        let _ = std::fs::remove_file(&path);
        assert!(format!("{err:#}").contains("unknown field `unexpected`"));
    }

    #[test]
    fn legacy_v1_rejects_non_tcp_transport() {
        let mut value = legacy_v1_value();
        value
            .get_mut("ipc")
            .and_then(Value::as_table_mut)
            .unwrap()
            .insert("transport".to_string(), Value::String("ipc".to_string()));

        let err = load_from_str(&toml::to_string_pretty(&value).unwrap()).unwrap_err();

        assert!(format!("{err:#}").contains("legacy ipc.transport"));
    }

    #[test]
    fn legacy_v1_load_is_read_only() {
        let path = temp_config_path("legacy-read-only");
        let content = legacy_v1_toml();
        std::fs::write(&path, &content).unwrap();

        let loaded = load_from_path(&path).unwrap();

        assert_eq!(loaded, AppConfig::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn engine_startup_persists_migration_and_normalization_once() {
        let path = temp_config_path("legacy-engine-startup");
        let mut value = legacy_v1_value();
        value
            .get_mut("stations")
            .and_then(Value::as_array_mut)
            .unwrap()[0]
            .as_table_mut()
            .unwrap()
            .insert(
                "name".to_string(),
                Value::String(" 北京 - 北京市 ".to_string()),
            );
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let loaded = load_for_engine_startup(&path).unwrap();
        let persisted = std::fs::read_to_string(&path).unwrap();

        assert_eq!(loaded.config_version, SUPPORTED_CONFIG_VERSION);
        assert_eq!(loaded.stations[0].name, "北京-北京市");
        assert!(!persisted.contains("transport ="));
        assert!(!persisted.contains("lock_path = \"weather.db.lock\""));
        assert!(!persisted.contains("[daemon]"));
        assert!(persisted.contains("lock_path = \"engine.lock\""));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn migration_runner_accepts_an_appended_contiguous_step() {
        let migrations = [
            ConfigMigration {
                version: 2,
                name: "v2",
                apply: migrate_v1_to_v2,
            },
            ConfigMigration {
                version: 3,
                name: "v3",
                apply: synthetic_v3_migration,
            },
        ];

        let migrated = run_config_migrations(legacy_v1_value(), 3, &migrations).unwrap();

        assert_eq!(config_version(&migrated).unwrap(), 3);
    }

    #[test]
    fn migration_runner_rejects_registry_gaps() {
        let migrations = [
            ConfigMigration {
                version: 2,
                name: "v2",
                apply: migrate_v1_to_v2,
            },
            ConfigMigration {
                version: 4,
                name: "v4",
                apply: synthetic_v3_migration,
            },
        ];

        let err = run_config_migrations(legacy_v1_value(), 3, &migrations).unwrap_err();

        assert!(err.to_string().contains("registry gap"), "{err:#}");
    }

    #[test]
    fn failed_migration_does_not_expose_partial_mutation() {
        let migrations = [
            ConfigMigration {
                version: 2,
                name: "v2",
                apply: migrate_v1_to_v2,
            },
            ConfigMigration {
                version: 3,
                name: "failing v3",
                apply: failing_v3_migration,
            },
        ];
        let original: Value = toml::from_str(&default_config_toml()).unwrap();

        assert!(run_config_migrations(original.clone(), 3, &migrations).is_err());
        assert!(original.get("partial").is_none());
    }

    #[test]
    fn rejects_future_missing_and_invalid_versions() {
        let mut future: Value = toml::from_str(&default_config_toml()).unwrap();
        set_config_version(&mut future, SUPPORTED_CONFIG_VERSION + 1).unwrap();
        assert!(
            format!(
                "{:#}",
                run_config_migrations(future, SUPPORTED_CONFIG_VERSION, CONFIG_MIGRATIONS,)
                    .unwrap_err()
            )
            .contains("newer than supported")
        );

        let mut missing: Value = toml::from_str(&default_config_toml()).unwrap();
        missing.as_table_mut().unwrap().remove("config_version");
        assert!(
            format!(
                "{:#}",
                run_config_migrations(missing, SUPPORTED_CONFIG_VERSION, CONFIG_MIGRATIONS,)
                    .unwrap_err()
            )
            .contains("missing required config_version")
        );

        let mut invalid: Value = toml::from_str(&default_config_toml()).unwrap();
        invalid.as_table_mut().unwrap().insert(
            "config_version".to_string(),
            Value::String("two".to_string()),
        );
        assert!(
            format!(
                "{:#}",
                run_config_migrations(invalid, SUPPORTED_CONFIG_VERSION, CONFIG_MIGRATIONS,)
                    .unwrap_err()
            )
            .contains("non-negative integer")
        );
    }

    #[test]
    fn rejects_same_rpc_and_pub_endpoint() {
        let mut config = base_config();
        config.ipc.pub_endpoint = config.ipc.rpc_endpoint.clone();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_overlong_hmac_key() {
        let mut config = base_config();
        config.ipc.hmac = "hmac_key".to_string();
        config.ipc.hmac_key = "a".repeat(33);
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_empty_hmac_key_when_enabled() {
        let mut config = base_config();
        config.ipc.hmac = "hmac_key".to_string();
        config.ipc.hmac_key = String::new();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn accepts_empty_hmac_key_when_disabled() {
        let mut config = base_config();
        config.ipc.hmac = "disabled".to_string();
        config.ipc.hmac_key = String::new();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn accepts_short_hmac_key_when_enabled() {
        let mut config = base_config();
        config.ipc.hmac = "hmac_key".to_string();
        config.ipc.hmac_key = "ab".to_string();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn rejects_invalid_hmac_mode() {
        let mut config = base_config();
        config.ipc.hmac = "bogus".to_string();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_hmac_env_key_without_env_name() {
        let mut config = base_config();
        config.ipc.hmac = "hmac_env_key".to_string();
        config.ipc.hmac_env_key_name = String::new();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_empty_enabled_station() {
        let mut config = base_config();
        config.stations = vec![crate::types::StationConfig {
            name: "  ".to_string(),
            enabled: true,
        }];
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_empty_disabled_station() {
        let mut config = base_config();
        config.stations = vec![crate::types::StationConfig {
            name: " -  - ".to_string(),
            enabled: false,
        }];

        let err = validate(&config).unwrap_err().to_string();

        assert!(err.contains("station.name must not be empty"), "{err}");
    }

    #[test]
    fn rejects_station_duplicates_after_normalization() {
        let mut config = base_config();
        config.stations = vec![
            crate::types::StationConfig {
                name: "北京 - 北京市 - 朝阳".to_string(),
                enabled: true,
            },
            crate::types::StationConfig {
                name: "北京-北京市-朝阳".to_string(),
                enabled: false,
            },
        ];

        let err = validate(&config).unwrap_err().to_string();

        assert!(err.contains("duplicate station.name"), "{err}");
        assert!(err.contains("北京-北京市-朝阳"), "{err}");
    }

    #[test]
    fn ipc_default_endpoints_match_schema_constants() {
        let ipc = IpcConfig::default();
        assert_eq!(ipc.rpc_endpoint, weather_schema::DEFAULT_ZMQ_RPC_ENDPOINT);
        assert_eq!(ipc.pub_endpoint, weather_schema::DEFAULT_ZMQ_PUB_ENDPOINT);
    }

    #[test]
    fn rejects_invalid_db_timezone() {
        let mut config = base_config();
        config.db.timezone = "Not/A/Zone".to_string();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn accepts_default_db_timezone() {
        assert_eq!(base_config().db.timezone, "Asia/Shanghai");
        assert!(validate(&base_config()).is_ok());
    }
}
