use serde::{Deserialize, Serialize};

use crate::defaults::*;

pub const SUPPORTED_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub config_version: u32,
    pub engine: EngineConfig,
    pub ipc: IpcConfig,
    pub db: DbConfig,
    pub updater: UpdaterConfig,
    pub daemon: DaemonConfig,
    pub stations: Vec<StationConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub request_timeout_ms: u64,
    pub startup_timeout_ms: u64,
    pub lock_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcConfig {
    pub transport: String,
    pub rpc_endpoint: String,
    pub pub_endpoint: String,
    /// HMAC 模式:`"disabled"`(默认,不签名)/ `"hmac_key"`(用 config 里 hmac_key 字段)/
    /// `"hmac_env_key"`(从环境变量读 key,变量名见 hmac_env_key_name)。
    pub hmac: String,
    pub hmac_key: String,
    pub hmac_env_key_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    pub path: String,
    pub lock_path: String,
    pub timezone: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdaterConfig {
    pub weather_ttl_seconds: u64,
    pub province_ttl_seconds: u64,
    pub default_provider: String,
    pub provider: Vec<ProviderConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub request_timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub service_backend: String,
    pub foreground: bool,
    pub service_scope: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationConfig {
    pub name: String,
    pub enabled: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: SUPPORTED_CONFIG_VERSION,
            engine: EngineConfig::default(),
            ipc: IpcConfig::default(),
            db: DbConfig::default(),
            updater: UpdaterConfig::default(),
            daemon: DaemonConfig::default(),
            stations: default_stations(),
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            request_timeout_ms: default_request_timeout_ms(),
            startup_timeout_ms: default_startup_timeout_ms(),
            lock_path: default_engine_lock_path(),
        }
    }
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            rpc_endpoint: default_rpc_endpoint(),
            pub_endpoint: default_pub_endpoint(),
            hmac: default_hmac_mode(),
            hmac_key: default_hmac_key(),
            hmac_env_key_name: String::new(),
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            lock_path: default_db_lock_path(),
            timezone: default_db_timezone(),
        }
    }
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            weather_ttl_seconds: default_weather_ttl_seconds(),
            province_ttl_seconds: default_province_ttl_seconds(),
            default_provider: default_provider_name(),
            provider: default_providers(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            service_backend: default_service_backend(),
            foreground: true,
            service_scope: default_service_scope(),
        }
    }
}
