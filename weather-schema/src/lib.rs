mod codec;
mod constants;
mod correlation;
mod crypto;
mod diagnostics;
mod lifecycle;
mod station;
mod time;
mod generated {
    include!(concat!(env!("OUT_DIR"), "/weather.schema.v1.rs"));
}
mod uuid;

pub use codec::*;
pub use constants::*;
pub use correlation::*;
pub use crypto::*;
pub use diagnostics::*;
pub use generated::*;
pub use lifecycle::*;
pub use station::*;
pub use time::*;
pub use uuid::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_legacy_wire_values() -> AppConfig {
        AppConfig {
            engine: Some(EngineConfig {
                request_timeout_ms: 3_000,
                startup_timeout_ms: 8_000,
                lock_path: "engine.lock".to_string(),
            }),
            ipc: Some(IpcConfig {
                transport: "tcp".to_string(),
                rpc_endpoint: "tcp://127.0.0.1:44445".to_string(),
                pub_endpoint: "tcp://127.0.0.1:44444".to_string(),
                hmac: "disabled".to_string(),
                hmac_key: "key".to_string(),
                hmac_env_key_name: String::new(),
            }),
            db: Some(DbConfig {
                path: "weather.db".to_string(),
                lock_path: "weather.db.lock".to_string(),
                timezone: "Asia/Shanghai".to_string(),
            }),
            updater: Some(UpdaterConfig {
                weather_ttl_seconds: 900,
                province_ttl_seconds: 86_400,
                default_provider: "nmc".to_string(),
                provider: vec![ProviderConfig {
                    name: "nmc".to_string(),
                    base_url: "https://www.nmc.cn".to_string(),
                    request_timeout_seconds: 20,
                }],
            }),
            daemon: Some(DaemonConfig {
                service_backend: "auto".to_string(),
                foreground: true,
                service_scope: "user".to_string(),
            }),
            stations: vec![StationConfig {
                name: "北京-北京市".to_string(),
                enabled: true,
            }],
            config_version: 2,
        }
    }

    #[test]
    fn schema_toml_omits_legacy_wire_only_fields() {
        let content = toml::to_string_pretty(&config_with_legacy_wire_values()).unwrap();

        assert!(!content.contains("transport ="));
        assert!(!content.contains("lock_path = \"weather.db.lock\""));
        assert!(!content.contains("[daemon]"));
        assert!(content.contains("lock_path = \"engine.lock\""));

        let decoded: AppConfig = toml::from_str(&content).unwrap();
        assert_eq!(decoded.ipc.unwrap().transport, "");
        assert_eq!(decoded.db.unwrap().lock_path, "");
        assert!(decoded.daemon.is_none());
    }

    #[test]
    fn schema_toml_still_accepts_legacy_wire_only_fields() {
        let mut value = toml::Value::try_from(config_with_legacy_wire_values()).unwrap();
        value
            .get_mut("ipc")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "transport".to_string(),
                toml::Value::String("legacy".to_string()),
            );
        value
            .get_mut("db")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "lock_path".to_string(),
                toml::Value::String("legacy.lock".to_string()),
            );
        value.as_table_mut().unwrap().insert(
            "daemon".to_string(),
            toml::Value::Table(toml::toml! {
                service_backend = "legacy"
                foreground = false
                service_scope = "system"
            }),
        );

        let decoded: AppConfig = value.try_into().unwrap();

        assert_eq!(decoded.ipc.unwrap().transport, "legacy");
        assert_eq!(decoded.db.unwrap().lock_path, "legacy.lock");
        assert_eq!(decoded.daemon.unwrap().service_backend, "legacy");
    }

    #[test]
    fn rpc_kind_v1_numbers_are_frozen() {
        let expected = [
            (RpcKind::Unspecified, 0),
            (RpcKind::Ping, 1),
            (RpcKind::GetEngineStatus, 2),
            (RpcKind::GetWeather, 10),
            (RpcKind::ListProvinces, 11),
            (RpcKind::ListCities, 12),
            (RpcKind::FuzzyMatchStations, 13),
            (RpcKind::ListConfiguredStations, 14),
            (RpcKind::BatchListRegions, 15),
            (RpcKind::ResolveStationUuid, 16),
            (RpcKind::MigrateDbTimezone, 17),
            (RpcKind::GetConfig, 18),
            (RpcKind::UpdateConfig, 19),
            (RpcKind::TriggerRefresh, 30),
            (RpcKind::RestartEngine, 40),
            (RpcKind::Shutdown, 41),
        ];

        for (kind, number) in expected {
            assert_eq!(kind as i32, number, "wire number changed for {kind:?}");
            assert_eq!(RpcKind::try_from(number), Ok(kind));
        }
    }
}
