//! `weather_configure::AppConfig` 与 `weather_schema::AppConfig` 之间的转换。
//!
//! Config v2 no longer exposes several runtime-derived fields. Their protobuf
//! numbers remain reserved for wire compatibility, so inbound values are
//! ignored and outbound values are deterministic.

use weather_schema as schema;

use crate::{
    AppConfig, DbConfig, EngineConfig, IpcConfig, ProviderConfig, StationConfig, UpdaterConfig,
};

impl From<schema::AppConfig> for AppConfig {
    fn from(value: schema::AppConfig) -> Self {
        Self {
            config_version: value.config_version,
            engine: value.engine.unwrap_or_default().into(),
            ipc: value.ipc.unwrap_or_default().into(),
            db: value.db.unwrap_or_default().into(),
            updater: value.updater.unwrap_or_default().into(),
            stations: value.stations.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<AppConfig> for schema::AppConfig {
    fn from(value: AppConfig) -> Self {
        Self {
            config_version: value.config_version,
            engine: Some(value.engine.into()),
            ipc: Some(value.ipc.into()),
            db: Some(value.db.into()),
            updater: Some(value.updater.into()),
            daemon: Some(schema::DaemonConfig {
                service_backend: "auto".to_string(),
                foreground: true,
                service_scope: "user".to_string(),
            }),
            stations: value.stations.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<schema::EngineConfig> for EngineConfig {
    fn from(value: schema::EngineConfig) -> Self {
        Self {
            request_timeout_ms: value.request_timeout_ms,
            startup_timeout_ms: value.startup_timeout_ms,
            lock_path: value.lock_path,
        }
    }
}

impl From<EngineConfig> for schema::EngineConfig {
    fn from(value: EngineConfig) -> Self {
        Self {
            request_timeout_ms: value.request_timeout_ms,
            startup_timeout_ms: value.startup_timeout_ms,
            lock_path: value.lock_path,
        }
    }
}

impl From<schema::IpcConfig> for IpcConfig {
    fn from(value: schema::IpcConfig) -> Self {
        Self {
            rpc_endpoint: value.rpc_endpoint,
            pub_endpoint: value.pub_endpoint,
            hmac: value.hmac,
            hmac_key: value.hmac_key,
            hmac_env_key_name: value.hmac_env_key_name,
        }
    }
}

impl From<IpcConfig> for schema::IpcConfig {
    fn from(value: IpcConfig) -> Self {
        Self {
            transport: "tcp".to_string(),
            rpc_endpoint: value.rpc_endpoint,
            pub_endpoint: value.pub_endpoint,
            hmac: value.hmac,
            hmac_key: value.hmac_key,
            hmac_env_key_name: value.hmac_env_key_name,
        }
    }
}

impl From<schema::DbConfig> for DbConfig {
    fn from(value: schema::DbConfig) -> Self {
        Self {
            path: value.path,
            timezone: value.timezone,
        }
    }
}

impl From<DbConfig> for schema::DbConfig {
    fn from(value: DbConfig) -> Self {
        let lock_path = format!("{}.lock", value.path);
        Self {
            path: value.path,
            lock_path,
            timezone: value.timezone,
        }
    }
}

impl From<schema::UpdaterConfig> for UpdaterConfig {
    fn from(value: schema::UpdaterConfig) -> Self {
        Self {
            weather_ttl_seconds: value.weather_ttl_seconds,
            province_ttl_seconds: value.province_ttl_seconds,
            default_provider: value.default_provider,
            provider: value.provider.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<UpdaterConfig> for schema::UpdaterConfig {
    fn from(value: UpdaterConfig) -> Self {
        Self {
            weather_ttl_seconds: value.weather_ttl_seconds,
            province_ttl_seconds: value.province_ttl_seconds,
            default_provider: value.default_provider,
            provider: value.provider.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<schema::ProviderConfig> for ProviderConfig {
    fn from(value: schema::ProviderConfig) -> Self {
        Self {
            name: value.name,
            base_url: value.base_url,
            request_timeout_seconds: value.request_timeout_seconds,
        }
    }
}

impl From<ProviderConfig> for schema::ProviderConfig {
    fn from(value: ProviderConfig) -> Self {
        Self {
            name: value.name,
            base_url: value.base_url,
            request_timeout_seconds: value.request_timeout_seconds,
        }
    }
}

impl From<schema::StationConfig> for StationConfig {
    fn from(value: schema::StationConfig) -> Self {
        Self {
            name: value.name,
            enabled: value.enabled,
        }
    }
}

impl From<StationConfig> for schema::StationConfig {
    fn from(value: StationConfig) -> Self {
        Self {
            name: value.name,
            enabled: value.enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> AppConfig {
        AppConfig {
            config_version: crate::SUPPORTED_CONFIG_VERSION,
            engine: EngineConfig {
                request_timeout_ms: 3000,
                startup_timeout_ms: 8000,
                lock_path: "engine.lock".to_string(),
            },
            ipc: IpcConfig {
                rpc_endpoint: "tcp://127.0.0.1:44445".to_string(),
                pub_endpoint: "tcp://127.0.0.1:44444".to_string(),
                hmac: "disabled".to_string(),
                hmac_key: "ab".to_string(),
                hmac_env_key_name: String::new(),
            },
            db: DbConfig {
                path: "data/archive.weather.db".to_string(),
                timezone: "Asia/Shanghai".to_string(),
            },
            updater: UpdaterConfig {
                weather_ttl_seconds: 900,
                province_ttl_seconds: 86400,
                default_provider: "nmc".to_string(),
                provider: vec![ProviderConfig {
                    name: "nmc".to_string(),
                    base_url: "https://www.nmc.cn".to_string(),
                    request_timeout_seconds: 20,
                }],
            },
            stations: vec![
                StationConfig {
                    name: "北京-北京市-朝阳".to_string(),
                    enabled: true,
                },
                StationConfig {
                    name: "湖北-湖北省-武汉".to_string(),
                    enabled: false,
                },
            ],
        }
    }

    #[test]
    fn roundtrip_app_config_preserves_all_live_fields() {
        let original = sample_config();

        let schema: schema::AppConfig = original.clone().into();
        let back: AppConfig = schema.into();
        assert_eq!(original, back);
    }

    #[test]
    fn inbound_legacy_wire_fields_do_not_affect_internal_config() {
        let expected = sample_config();
        let mut wire: schema::AppConfig = expected.clone().into();
        wire.ipc.as_mut().unwrap().transport = "legacy-transport".to_string();
        wire.db.as_mut().unwrap().lock_path = "legacy-db.lock".to_string();
        wire.daemon = Some(schema::DaemonConfig {
            service_backend: "legacy".to_string(),
            foreground: false,
            service_scope: "system".to_string(),
        });

        let actual: AppConfig = wire.into();

        assert_eq!(actual, expected);
    }

    #[test]
    fn outbound_legacy_wire_fields_are_stable() {
        let wire: schema::AppConfig = sample_config().into();

        assert_eq!(wire.ipc.unwrap().transport, "tcp");
        assert_eq!(wire.db.unwrap().lock_path, "data/archive.weather.db.lock");
        assert_eq!(
            wire.daemon.unwrap(),
            schema::DaemonConfig {
                service_backend: "auto".to_string(),
                foreground: true,
                service_scope: "user".to_string(),
            }
        );
    }
}
