use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use toml::Value;

use crate::{
    AppConfig, ComponentKind, ComponentRegistry, SUPPORTED_CONFIG_VERSION, default_config_toml,
    normalize_station_name,
};

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
    if path.exists() {
        load_from_path(path)
    } else {
        let config = AppConfig::default();
        validate(&config)?;
        Ok(config)
    }
}

pub fn ensure_config_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, default_config_toml())
        .with_context(|| format!("failed to write default config {}", path.display()))?;
    ComponentRegistry::for_config_path(path)?.record(ComponentKind::Config, path)?;
    Ok(())
}

pub fn load_from_path(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    validate_toml_compatibility(&content)
        .map_err(|err| anyhow::anyhow!("config {} is incompatible: {err}", path.display()))?;
    let config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse TOML config {}", path.display()))?;
    validate(&config)?;
    Ok(config)
}

pub fn validate_toml_compatibility(content: &str) -> Result<()> {
    let actual: Value = toml::from_str(content).context("failed to parse TOML config")?;
    let supported: Value =
        toml::from_str(&default_config_toml()).context("failed to parse default config")?;
    let actual_version = actual
        .get("config_version")
        .and_then(Value::as_integer)
        .and_then(|value| u32::try_from(value).ok());
    let mut missing = Vec::new();
    let mut extra = Vec::new();
    collect_field_diff("", &supported, &actual, &mut missing, &mut extra);
    missing.sort();
    missing.dedup();
    extra.sort();
    extra.dedup();

    if actual_version != Some(SUPPORTED_CONFIG_VERSION) {
        bail!(
            "config version incompatible: file config_version={}, engine supports {}\n{}",
            actual_version
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<missing>".to_string()),
            SUPPORTED_CONFIG_VERSION,
            format_field_diff(&missing, &extra)
        );
    }
    if !missing.is_empty() || !extra.is_empty() {
        bail!(
            "unsupported config fields\n{}",
            format_field_diff(&missing, &extra)
        );
    }
    Ok(())
}

fn collect_field_diff(
    prefix: &str,
    supported: &Value,
    actual: &Value,
    missing: &mut Vec<String>,
    extra: &mut Vec<String>,
) {
    match (supported, actual) {
        (Value::Table(supported), Value::Table(actual)) => {
            for (key, supported_value) in supported {
                let path = join_path(prefix, key);
                match actual.get(key) {
                    Some(actual_value) => {
                        collect_field_diff(&path, supported_value, actual_value, missing, extra)
                    }
                    None => missing.push(path),
                }
            }
            for key in actual.keys() {
                if !supported.contains_key(key) {
                    extra.push(join_path(prefix, key));
                }
            }
        }
        (Value::Array(supported), Value::Array(actual)) => {
            if let Some(supported_item) = supported.first() {
                for actual_item in actual {
                    collect_field_diff(prefix, supported_item, actual_item, missing, extra);
                }
            }
        }
        _ => {}
    }
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn format_field_diff(missing: &[String], extra: &[String]) -> String {
    let mut lines = Vec::new();
    if !missing.is_empty() {
        lines.push("missing fields:".to_string());
        lines.extend(missing.iter().map(|field| format!("  - {field}")));
    }
    if !extra.is_empty() {
        lines.push("extra fields:".to_string());
        lines.extend(extra.iter().map(|field| format!("  - {field}")));
    }
    if lines.is_empty() {
        lines.push("missing fields:".to_string());
        lines.push("  - <none>".to_string());
        lines.push("extra fields:".to_string());
        lines.push("  - <none>".to_string());
    }
    lines.join("\n")
}

pub fn validate(config: &AppConfig) -> Result<()> {
    if config.config_version != SUPPORTED_CONFIG_VERSION {
        bail!(
            "config_version {} is incompatible; engine supports {}",
            config.config_version,
            SUPPORTED_CONFIG_VERSION
        );
    }
    if config.ipc.transport != "tcp" {
        bail!("only ipc.transport = \"tcp\" is implemented in this build");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AppConfig, IpcConfig};
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

    #[test]
    fn valid_config_passes() {
        assert!(validate(&base_config()).is_ok());
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
    fn load_rejects_missing_config_version_with_field_diff() {
        let path = temp_config_path("missing-version");
        let mut content = default_config_toml();
        content = content
            .lines()
            .filter(|line| !line.starts_with("config_version"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, content).unwrap();

        let err = load_from_path(&path).unwrap_err().to_string();

        let _ = std::fs::remove_file(&path);
        assert!(err.contains("config version incompatible"), "{err}");
        assert!(err.contains("file config_version=<missing>"), "{err}");
        assert!(err.contains("missing fields:"), "{err}");
        assert!(err.contains("config_version"), "{err}");
    }

    #[test]
    fn load_rejects_old_enable_hmac_field_as_extra() {
        let path = temp_config_path("old-hmac");
        let mut value: toml::Value = toml::from_str(&default_config_toml()).unwrap();
        value
            .get_mut("ipc")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert("enable_hmac".to_string(), toml::Value::Boolean(true));
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let err = load_from_path(&path).unwrap_err().to_string();

        let _ = std::fs::remove_file(&path);
        assert!(err.contains("unsupported config fields"), "{err}");
        assert!(err.contains("extra fields:"), "{err}");
        assert!(err.contains("ipc.enable_hmac"), "{err}");
    }

    #[test]
    fn load_rejects_missing_required_nested_field() {
        let path = temp_config_path("missing-nested");
        let mut value: toml::Value = toml::from_str(&default_config_toml()).unwrap();
        value
            .get_mut("ipc")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .remove("hmac_env_key_name");
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let err = load_from_path(&path).unwrap_err().to_string();

        let _ = std::fs::remove_file(&path);
        assert!(err.contains("unsupported config fields"), "{err}");
        assert!(err.contains("missing fields:"), "{err}");
        assert!(err.contains("ipc.hmac_env_key_name"), "{err}");
    }

    #[test]
    fn load_rejects_extra_array_table_field() {
        let path = temp_config_path("extra-array-field");
        let mut value: toml::Value = toml::from_str(&default_config_toml()).unwrap();
        value
            .get_mut("stations")
            .and_then(toml::Value::as_array_mut)
            .unwrap()[0]
            .as_table_mut()
            .unwrap()
            .insert("legacy".to_string(), toml::Value::Boolean(true));
        std::fs::write(&path, toml::to_string_pretty(&value).unwrap()).unwrap();

        let err = load_from_path(&path).unwrap_err().to_string();

        let _ = std::fs::remove_file(&path);
        assert!(err.contains("unsupported config fields"), "{err}");
        assert!(err.contains("extra fields:"), "{err}");
        assert!(err.contains("stations.legacy"), "{err}");
    }

    #[test]
    fn rejects_non_tcp_transport() {
        let mut config = base_config();
        config.ipc.transport = "ipc".to_string();
        assert!(validate(&config).is_err());
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
