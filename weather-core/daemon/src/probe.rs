use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use prost::Message;
use serde::Serialize;
use weather_configure::{
    ComponentKind, ComponentRegistry, default_config_file, ensure_config_file, load_or_default,
};
use weather_schema::*;
use zeromq::{DealerSocket, RepSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

use crate::{
    path::{absolute_config_path, normalize_path, resolve_relative},
    time::{now_ms, request_id},
};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProbeState {
    Running,
    NotRunning,
}

#[derive(Debug, Serialize)]
struct ProbeStatus {
    state: ProbeState,
    rpc_endpoint: String,
    pub_endpoint: String,
    config_path: String,
    lock_path: String,
    lock_held: bool,
    stale_lock_cleaned: bool,
    rpc_endpoint_available: bool,
    pub_endpoint_available: bool,
    engine_status: Option<EngineStatusView>,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct EngineStatusView {
    ready: bool,
    mode: String,
    rpc_endpoint: String,
    pub_endpoint: String,
    config_path: String,
    last_config_error: Option<String>,
    message: Option<String>,
}

impl From<EngineStatus> for EngineStatusView {
    fn from(value: EngineStatus) -> Self {
        Self {
            ready: value.ready,
            mode: value.mode,
            rpc_endpoint: value.rpc_endpoint,
            pub_endpoint: value.pub_endpoint,
            config_path: value.config_path,
            last_config_error: value.last_config_error,
            message: value.message,
        }
    }
}

pub(crate) async fn probe(config: Option<PathBuf>, verbose: bool) -> Result<()> {
    let status = probe_status(config).await?;
    if verbose {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "{}",
            match status.state {
                ProbeState::Running => "running",
                ProbeState::NotRunning => "not_running",
            }
        );
    }
    Ok(())
}

async fn probe_status(config: Option<PathBuf>) -> Result<ProbeStatus> {
    let strict_config = config.is_some();
    let config_path = absolute_config_path(config.unwrap_or(default_config_file()?))?;
    ensure_config_file(&config_path)?;
    let config = load_or_default(&config_path)?;
    let base_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let lock_path = resolve_relative(&base_dir, &config.engine.lock_path);
    ComponentRegistry::for_config_path(&config_path)?.record(ComponentKind::Lock, &lock_path)?;

    if try_lock(&lock_path)? {
        let stale_lock_cleaned = cleanup_stale_lock(&lock_path)?;
        let rpc_available = ensure_endpoint_available(&config.ipc.rpc_endpoint)
            .await
            .is_ok();
        let pub_available = ensure_endpoint_available(&config.ipc.pub_endpoint)
            .await
            .is_ok();
        if rpc_available && pub_available {
            return Ok(ProbeStatus {
                state: ProbeState::NotRunning,
                rpc_endpoint: config.ipc.rpc_endpoint.clone(),
                pub_endpoint: config.ipc.pub_endpoint.clone(),
                config_path: config_path.display().to_string(),
                lock_path: lock_path.display().to_string(),
                lock_held: false,
                stale_lock_cleaned,
                rpc_endpoint_available: true,
                pub_endpoint_available: true,
                engine_status: None,
                message: None,
            });
        }
        if let Some(status) = rpc_endpoint_status(&config).await? {
            if strict_config {
                ensure_same_config(&config_path, &status)?;
            }
            let rpc_endpoint = status.rpc_endpoint.clone();
            let pub_endpoint = status.pub_endpoint.clone();
            let config_path_str = status.config_path.clone();
            let message = if strict_config {
                "endpoint is served by a running engine".to_string()
            } else {
                format!("adopted running engine at {rpc_endpoint} (config {config_path_str})")
            };
            return Ok(ProbeStatus {
                state: ProbeState::Running,
                rpc_endpoint,
                pub_endpoint,
                config_path: config_path_str,
                lock_path: lock_path.display().to_string(),
                lock_held: false,
                stale_lock_cleaned,
                rpc_endpoint_available: rpc_available,
                pub_endpoint_available: pub_available,
                engine_status: Some(status.into()),
                message: Some(message),
            });
        }
        bail!(
            "endpoints {} / {} are already in use, but no weather engine status was returned",
            config.ipc.rpc_endpoint,
            config.ipc.pub_endpoint
        );
    }

    let endpoint_status = rpc_endpoint_status(&config).await?;
    if let Some(status) = &endpoint_status
        && strict_config
    {
        ensure_same_config(&config_path, status)?;
    }
    let message = if endpoint_status.is_none() {
        Some(format!(
            "engine lock is held for {}, but RPC endpoint {} did not return status",
            config_path.display(),
            config.ipc.rpc_endpoint
        ))
    } else if strict_config {
        Some("engine lock is held".to_string())
    } else {
        Some(format!(
            "adopted running engine at {} (config {})",
            endpoint_status
                .as_ref()
                .map(|s| s.rpc_endpoint.as_str())
                .unwrap_or(""),
            endpoint_status
                .as_ref()
                .map(|s| s.config_path.as_str())
                .unwrap_or("")
        ))
    };
    Ok(ProbeStatus {
        state: ProbeState::Running,
        rpc_endpoint: endpoint_status
            .as_ref()
            .map(|s| s.rpc_endpoint.clone())
            .unwrap_or_else(|| config.ipc.rpc_endpoint.clone()),
        pub_endpoint: endpoint_status
            .as_ref()
            .map(|s| s.pub_endpoint.clone())
            .unwrap_or_else(|| config.ipc.pub_endpoint.clone()),
        config_path: endpoint_status
            .as_ref()
            .map(|s| s.config_path.clone())
            .unwrap_or_else(|| config_path.display().to_string()),
        lock_path: lock_path.display().to_string(),
        lock_held: true,
        stale_lock_cleaned: false,
        rpc_endpoint_available: false,
        pub_endpoint_available: false,
        engine_status: endpoint_status.map(Into::into),
        message,
    })
}

async fn rpc_endpoint_status(
    config: &weather_configure::AppConfig,
) -> Result<Option<EngineStatus>> {
    let timeout = Duration::from_millis(config.engine.request_timeout_ms.max(1));
    let mut socket = DealerSocket::new();
    match tokio::time::timeout(timeout, socket.connect(&config.ipc.rpc_endpoint)).await {
        Ok(Ok(())) => {}
        _ => return Ok(None),
    }
    let mut envelope = RpcRequest {
        schema_version: SCHEMA_VERSION.to_string(),
        request_id: request_id(),
        kind: RpcKind::GetEngineStatus as i32,
        timestamp_unix_ms: now_ms(),
        hmac_sha256: Vec::new(),
        payload: Empty {}.encode_to_vec(),
    };
    if let Some(key) = weather_configure::resolve_hmac_key(config)? {
        envelope.hmac_sha256 = weather_schema::rpc_request_hmac(&envelope, &key)?;
    }
    match tokio::time::timeout(
        timeout,
        socket.send(ZmqMessage::from(envelope.encode_to_vec())),
    )
    .await
    {
        Ok(Ok(())) => {}
        _ => return Ok(None),
    }
    let message = match tokio::time::timeout(timeout, socket.recv()).await {
        Ok(Ok(message)) => message,
        _ => return Ok(None),
    };
    let bytes: Vec<u8> = message.try_into().map_err(anyhow::Error::msg)?;
    let response = RpcResponse::decode(bytes.as_slice())?;
    if response.status == ResponseStatus::Error as i32 {
        let err = response.error.unwrap_or(EngineError {
            code: "ENGINE".to_string(),
            message: "unknown engine error".to_string(),
        });
        bail!(
            "RPC endpoint {} returned {}: {}",
            config.ipc.rpc_endpoint,
            err.code,
            err.message
        );
    }
    Ok(Some(EngineStatus::decode(response.payload.as_slice())?))
}

async fn ensure_endpoint_available(endpoint: &str) -> Result<()> {
    let mut socket = RepSocket::new();
    socket
        .bind(endpoint)
        .await
        .with_context(|| format!("endpoint {endpoint} is already in use"))?;
    Ok(())
}

fn ensure_same_config(requested_config: &Path, status: &EngineStatus) -> Result<()> {
    let requested = normalize_path(requested_config)?;
    let active = normalize_path(Path::new(&status.config_path))?;
    if requested != active {
        bail!(
            "endpoint {} is already served by engine config {}, not requested config {}",
            status.rpc_endpoint,
            active.display(),
            requested.display()
        );
    }
    Ok(())
}

fn try_lock(path: &Path) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to probe lock {}", path.display())),
    }
}

fn cleanup_stale_lock(path: &Path) -> Result<bool> {
    let existed = path.exists();
    if existed {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove stale lock {}", path.display()))?;
    }
    Ok(existed)
}
