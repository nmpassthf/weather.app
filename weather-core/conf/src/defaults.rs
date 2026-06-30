use std::path::PathBuf;

use anyhow::{Context, Result};
use weather_schema::{DEFAULT_ZMQ_PUB_ENDPOINT, DEFAULT_ZMQ_RPC_ENDPOINT};

use crate::types::{AppConfig, ProviderConfig, StationConfig};

/// 序列化默认配置为 TOML，用于初始化配置文件或 `--core-dump-default-config`。
///
/// # 示例
///
/// ```
/// use weather_configure::default_config_toml;
/// let toml = default_config_toml();
/// assert!(toml.contains("[ipc]"));
/// assert!(toml.contains("rpc_endpoint"));
/// ```
pub fn default_config_toml() -> String {
    toml::to_string_pretty(&AppConfig::default()).expect("default config must serialize")
}

/// 默认配置文件路径:用户 home 目录下 `~/.weather/config/weather.toml`。
///
/// 统一用 home 目录,避免 CWD 漂移导致 config 分裂:`cargo r` 与 service 安装的
/// engine 默认指向同一 config。Unix 用 `HOME`,Windows 用 `USERPROFILE`。
pub fn default_config_file() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("neither HOME nor USERPROFILE is set")?;
    Ok(PathBuf::from(home)
        .join(".weather")
        .join("config")
        .join("weather.toml"))
}

pub(crate) fn default_request_timeout_ms() -> u64 {
    3000
}
pub(crate) fn default_startup_timeout_ms() -> u64 {
    8000
}
pub(crate) fn default_engine_lock_path() -> String {
    "engine.lock".to_string()
}
pub(crate) fn default_transport() -> String {
    "tcp".to_string()
}
pub(crate) fn default_rpc_endpoint() -> String {
    DEFAULT_ZMQ_RPC_ENDPOINT.to_string()
}
pub(crate) fn default_pub_endpoint() -> String {
    DEFAULT_ZMQ_PUB_ENDPOINT.to_string()
}
pub(crate) fn default_hmac_mode() -> String {
    "disabled".to_string()
}
pub(crate) fn default_hmac_key() -> String {
    // 易记的默认 dev key,raw ASCII,≤32 字节会自动 padding。
    "weather-dev-default-key".to_string()
}
pub(crate) fn default_db_path() -> String {
    "weather.db".to_string()
}
pub(crate) fn default_db_lock_path() -> String {
    "weather.db.lock".to_string()
}
pub(crate) fn default_db_timezone() -> String {
    "Asia/Shanghai".to_string()
}
pub(crate) fn default_weather_ttl_seconds() -> u64 {
    900
}
pub(crate) fn default_province_ttl_seconds() -> u64 {
    86400
}
pub(crate) fn default_request_timeout_seconds() -> u64 {
    20
}
pub(crate) fn default_provider_name() -> String {
    "nmc".to_string()
}
pub(crate) fn default_providers() -> Vec<ProviderConfig> {
    vec![ProviderConfig {
        name: "nmc".to_string(),
        base_url: "https://www.nmc.cn".to_string(),
        request_timeout_seconds: default_request_timeout_seconds(),
    }]
}
pub(crate) fn default_service_backend() -> String {
    "auto".to_string()
}
pub(crate) fn default_service_scope() -> String {
    "user".to_string()
}
pub(crate) fn default_stations() -> Vec<StationConfig> {
    vec![StationConfig {
        name: "北京-北京市".to_string(),
        enabled: true,
    }]
}
