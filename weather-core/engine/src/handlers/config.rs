use std::path::PathBuf;

use anyhow::{Context, Result};
use weather_configure::{
    AppConfig, ComponentKind, ComponentRegistry, diff_immutable_fields, validate,
};
use weather_schema::*;

use crate::runtime::Engine;

impl Engine {
    pub(crate) async fn handle_get_config(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<GetConfigRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let config = if req.defaults {
            AppConfig::default()
        } else {
            self.config.get()
        };
        let payload = GetConfigResponse {
            config: Some(config.into()),
        };
        self.ok(&request.request_id, payload)
    }

    pub(crate) async fn handle_update_config(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<UpdateConfigRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let Some(schema_config) = req.config else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                "missing config field",
            );
        };
        let new_config: AppConfig = schema_config.into();

        if let Err(err) = validate(&new_config) {
            return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err.to_string());
        }

        let current = self.config.get();
        if let Err(field) = diff_immutable_fields(&current, &new_config) {
            return Self::rpc_error_response(&request.request_id, "CONFIG", field);
        }

        if let Err(err) = self.persist_config(&new_config) {
            return Self::rpc_error_response(&request.request_id, "CONFIG", err.to_string());
        }

        self.config.apply(new_config.clone());

        self.ok(
            &request.request_id,
            UpdateConfigResponse {
                config: Some(new_config.into()),
            },
        )
    }

    fn persist_config(&self, config: &AppConfig) -> Result<()> {
        let toml =
            toml::to_string_pretty(config).context("failed to serialize config for persistence")?;
        write_atomic(&self.config_path, &toml)
    }
}

fn write_atomic(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp = atomic_tmp_path(path);
    let components = ComponentRegistry::for_config_path(path)?;
    components.record(ComponentKind::Temp, &tmp)?;
    components.record(ComponentKind::Config, path)?;
    std::fs::write(&tmp, content)
        .with_context(|| format!("failed to write temp config {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename temp config to {}", path.display()))?;
    Ok(())
}

fn atomic_tmp_path(path: &std::path::Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    tmp.set_file_name(name);
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_tmp_path_appends_tmp_suffix() {
        let p = atomic_tmp_path(std::path::Path::new("/tmp/weather.toml"));
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "weather.toml.tmp");
    }
}
