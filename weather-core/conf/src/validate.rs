//! UPDATE_CONFIG 热更新边界：仅站点和两个 TTL 可以在线提交。

use crate::AppConfig;

/// Return restart-required fields in stable configuration order.
pub fn restart_required_fields(current: &AppConfig, new: &AppConfig) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if current.engine != new.engine {
        fields.push("engine");
    }
    if current.ipc != new.ipc {
        fields.push("ipc");
    }
    if current.db != new.db {
        fields.push("db");
    }
    if current.updater.default_provider != new.updater.default_provider {
        fields.push("updater.default_provider");
    }
    if current.updater.provider != new.updater.provider {
        fields.push("updater.provider");
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AppConfig, IpcConfig};

    fn base() -> AppConfig {
        let mut config = AppConfig::default();
        config.ipc.rpc_endpoint = "tcp://127.0.0.1:44445".to_string();
        config.ipc.pub_endpoint = "tcp://127.0.0.1:44444".to_string();
        config
    }

    #[test]
    fn identical_config_passes() {
        let config = base();
        assert!(restart_required_fields(&config, &config).is_empty());
    }

    #[test]
    fn stations_change_allowed() {
        let current = base();
        let mut new = current.clone();
        new.stations.push(crate::types::StationConfig {
            name: "北京-北京市-朝阳".to_string(),
            enabled: true,
        });
        assert!(restart_required_fields(&current, &new).is_empty());
    }

    #[test]
    fn updater_ttl_changes_are_hot() {
        let current = base();
        let mut new = current.clone();
        new.updater.weather_ttl_seconds = 1800;
        new.updater.province_ttl_seconds = 3600;
        assert!(restart_required_fields(&current, &new).is_empty());
    }

    #[test]
    fn restart_required_fields_have_stable_order() {
        let current = base();
        let mut new = current.clone();
        new.engine.request_timeout_ms = 9999;
        new.ipc.rpc_endpoint = "tcp://127.0.0.1:55555".to_string();
        new.db.timezone = "UTC".to_string();
        new.updater.default_provider = "other".to_string();
        new.updater.provider[0].base_url = "https://example.invalid".to_string();

        assert_eq!(
            restart_required_fields(&current, &new),
            vec![
                "engine",
                "ipc",
                "db",
                "updater.default_provider",
                "updater.provider",
            ]
        );
    }

    #[test]
    #[allow(dead_code)]
    fn ipc_partial_eq_works() {
        let a = IpcConfig::default();
        let b = IpcConfig::default();
        assert_eq!(a, b);
    }
}
