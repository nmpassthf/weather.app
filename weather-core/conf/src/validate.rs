//! UPDATE_CONFIG 的不可变字段校验：只有 `stations` 与 `updater` 允许变更，
//! `engine`/`ipc`/`db`/`daemon` 必须与 engine 当前 live config 一致。

use crate::AppConfig;

/// 比对 `current`（engine 当前持有的 config）与 `new`（client 下发的 config）。
///
/// 若任一不可变字段（engine/ipc/db/daemon）不一致，返回 `Err(字段名)`；
/// 全部一致返回 `Ok(())`。`updater` 与 `stations` 不参与比对。
pub fn diff_immutable_fields(current: &AppConfig, new: &AppConfig) -> Result<(), String> {
    if current.engine != new.engine {
        return Err("immutable field `engine` changed".to_string());
    }
    if current.ipc != new.ipc {
        return Err("immutable field `ipc` changed".to_string());
    }
    if current.db != new.db {
        return Err("immutable field `db` changed".to_string());
    }
    if current.daemon != new.daemon {
        return Err("immutable field `daemon` changed".to_string());
    }
    Ok(())
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
        assert!(diff_immutable_fields(&config, &config).is_ok());
    }

    #[test]
    fn stations_change_allowed() {
        let current = base();
        let mut new = current.clone();
        new.stations.push(crate::types::StationConfig {
            name: "北京-北京市-朝阳".to_string(),
            enabled: true,
        });
        assert!(diff_immutable_fields(&current, &new).is_ok());
    }

    #[test]
    fn updater_change_allowed() {
        let current = base();
        let mut new = current.clone();
        new.updater.weather_ttl_seconds = 1800;
        assert!(diff_immutable_fields(&current, &new).is_ok());
    }

    #[test]
    fn ipc_change_rejected() {
        let current = base();
        let mut new = current.clone();
        new.ipc.rpc_endpoint = "tcp://127.0.0.1:55555".to_string();
        let err = diff_immutable_fields(&current, &new).unwrap_err();
        assert!(err.contains("`ipc`"), "unexpected error: {err}");
    }

    #[test]
    fn engine_change_rejected() {
        let current = base();
        let mut new = current.clone();
        new.engine.request_timeout_ms = 9999;
        let err = diff_immutable_fields(&current, &new).unwrap_err();
        assert!(err.contains("`engine`"), "unexpected error: {err}");
    }

    #[test]
    fn db_change_rejected() {
        let current = base();
        let mut new = current.clone();
        new.db.timezone = "UTC".to_string();
        let err = diff_immutable_fields(&current, &new).unwrap_err();
        assert!(err.contains("`db`"), "unexpected error: {err}");
    }

    #[test]
    #[allow(dead_code)]
    fn daemon_change_rejected() {
        let current = base();
        let mut new = current.clone();
        new.daemon.foreground = !current.daemon.foreground;
        let err = diff_immutable_fields(&current, &new).unwrap_err();
        assert!(err.contains("`daemon`"), "unexpected error: {err}");
    }

    #[test]
    #[allow(dead_code)]
    fn ipc_partial_eq_works() {
        let a = IpcConfig::default();
        let b = IpcConfig::default();
        assert_eq!(a, b);
    }
}
