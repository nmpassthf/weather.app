use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use prost::Message;
use tokio::time::Instant;
use weather_configure::{default_config_file, load_or_default};
use weather_schema::{
    Empty, ResponseStatus, RpcKind, RpcRequest, RpcResponse, SCHEMA_VERSION, ShutdownRequest,
    correlation_id, unix_timestamp_ms,
};
use zeromq::{DealerSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

use crate::{
    path::absolute_config_path,
    probe::{ProbeState, probe_status},
};

const STOP_CLEANUP_GRACE: Duration = Duration::from_secs(12);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) async fn stop(config: Option<PathBuf>) -> Result<()> {
    let status = probe_status(config.clone()).await?;
    match status.state {
        ProbeState::NotRunning => {
            println!("weather daemon is not running");
            return Ok(());
        }
        ProbeState::Running => {}
        state => bail!(probe_error(state, status.message.as_deref())),
    }

    let config_path = absolute_config_path(match config.clone() {
        Some(path) => path,
        None => default_config_file()?,
    })?;
    let app_config = load_or_default(&config_path)?;
    let owner_token = status
        .lock_metadata
        .as_ref()
        .and_then(|metadata| metadata.owner_token.clone());
    request_shutdown(&app_config, &status.rpc_endpoint, owner_token).await?;

    let request_timeout = Duration::from_millis(app_config.engine.request_timeout_ms.max(1));
    wait_until_stopped(config, request_timeout.saturating_add(STOP_CLEANUP_GRACE)).await?;
    println!("weather daemon stopped");
    Ok(())
}

fn probe_error(state: ProbeState, message: Option<&str>) -> String {
    match message {
        Some(message) => format!(
            "cannot stop weather daemon in state {}: {message}",
            state.as_str()
        ),
        None => format!("cannot stop weather daemon in state {}", state.as_str()),
    }
}

async fn request_shutdown(
    config: &weather_configure::AppConfig,
    rpc_endpoint: &str,
    owner_token: Option<String>,
) -> Result<()> {
    let timeout = Duration::from_millis(config.engine.request_timeout_ms.max(1));
    let request_id = correlation_id("daemon-stop");
    let mut request = RpcRequest {
        schema_version: SCHEMA_VERSION.to_string(),
        request_id: request_id.clone(),
        kind: RpcKind::Shutdown as i32,
        timestamp_unix_ms: unix_timestamp_ms()?,
        hmac_sha256: Vec::new(),
        payload: ShutdownRequest { owner_token }.encode_to_vec(),
    };
    let hmac_key = weather_configure::resolve_hmac_key(config)?;
    if let Some(key) = hmac_key {
        request.hmac_sha256 = weather_schema::rpc_request_hmac(&request, &key)?;
    }

    let exchange = async {
        let mut socket = DealerSocket::new();
        socket
            .connect(rpc_endpoint)
            .await
            .with_context(|| format!("failed to connect daemon RPC endpoint {rpc_endpoint}"))?;
        socket
            .send(ZmqMessage::from(request.encode_to_vec()))
            .await
            .context("failed to send daemon shutdown request")?;
        let message = socket
            .recv()
            .await
            .context("failed to receive daemon shutdown response")?;
        let bytes: Vec<u8> = message.try_into().map_err(anyhow::Error::msg)?;
        RpcResponse::decode(bytes.as_slice()).context("failed to decode daemon shutdown response")
    };
    let response = tokio::time::timeout(timeout, exchange)
        .await
        .with_context(|| format!("daemon shutdown request timed out after {timeout:?}"))??;
    validate_shutdown_response(response, &request_id, hmac_key.as_ref())
}

fn validate_shutdown_response(
    response: RpcResponse,
    request_id: &str,
    hmac_key: Option<&[u8; 32]>,
) -> Result<()> {
    if response.schema_version != SCHEMA_VERSION || response.request_id != request_id {
        bail!("daemon shutdown response envelope does not match the request");
    }
    if response.status == ResponseStatus::Error as i32 {
        let error = response
            .error
            .context("daemon shutdown was rejected without an error message")?;
        bail!("{}: {}", error.code, error.message);
    }
    if response.status != ResponseStatus::Accepted as i32 {
        bail!("daemon shutdown response was not accepted");
    }
    if let Some(key) = hmac_key {
        let expected = weather_schema::rpc_response_hmac(&response, key)?;
        if response.hmac_sha256 != expected {
            bail!("daemon shutdown response has an invalid HMAC");
        }
    }
    Empty::decode(response.payload.as_slice())
        .context("daemon shutdown response payload is invalid")?;
    Ok(())
}

async fn wait_until_stopped(config: Option<PathBuf>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            bail!("timed out after {timeout:?} waiting for weather daemon to stop");
        }
        if probe_status(config.clone())
            .await
            .is_ok_and(|status| status.state == ProbeState::NotRunning)
        {
            return Ok(());
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather_configure::{AppConfig, write_config_atomic};
    use weather_schema::EngineError;

    fn response(status: ResponseStatus) -> RpcResponse {
        RpcResponse {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: "request-id".to_string(),
            status: status as i32,
            timestamp_unix_ms: 1,
            hmac_sha256: Vec::new(),
            payload: Empty {}.encode_to_vec(),
            error: None,
        }
    }

    #[test]
    fn accepted_shutdown_response_is_validated() {
        assert!(
            validate_shutdown_response(response(ResponseStatus::Accepted), "request-id", None)
                .is_ok()
        );
        assert!(
            validate_shutdown_response(response(ResponseStatus::Ok), "request-id", None).is_err()
        );
    }

    #[test]
    fn signed_shutdown_response_requires_the_expected_hmac() {
        let key = weather_schema::hmac_key_from_str("stop-test").unwrap();
        let mut signed = response(ResponseStatus::Accepted);
        signed.hmac_sha256 = weather_schema::rpc_response_hmac(&signed, &key).unwrap();

        assert!(validate_shutdown_response(signed.clone(), "request-id", Some(&key)).is_ok());
        signed.payload.push(1);
        assert!(validate_shutdown_response(signed, "request-id", Some(&key)).is_err());
    }

    #[test]
    fn rejected_shutdown_preserves_engine_diagnostics() {
        let mut rejected = response(ResponseStatus::Error);
        rejected.error = Some(EngineError {
            code: "OWNER_MISMATCH".to_string(),
            message: "engine ownership changed".to_string(),
        });

        let error = validate_shutdown_response(rejected, "request-id", None)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "OWNER_MISMATCH: engine ownership changed");
    }

    fn reserve_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("tcp://{address}")
    }

    async fn wait_for_state(config_path: &std::path::Path, expected: ProbeState) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if probe_status(Some(config_path.to_path_buf()))
                    .await
                    .is_ok_and(|status| status.state == expected)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon did not reach expected probe state");
    }

    #[tokio::test]
    async fn stop_command_shuts_down_a_running_daemon_and_waits_for_exit() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let mut config = AppConfig::default();
        config.ipc.rpc_endpoint = reserve_endpoint();
        config.ipc.pub_endpoint = reserve_endpoint();
        config.db.path = "weather.db".to_string();
        config.stations.clear();
        write_config_atomic(&config_path, &config).unwrap();

        let daemon_config = config_path.clone();
        let daemon = tokio::spawn(async move {
            crate::run::run(
                Some(daemon_config),
                true,
                Some("stop-test-owner".to_string()),
            )
            .await
        });
        wait_for_state(&config_path, ProbeState::Running).await;

        stop(Some(config_path.clone())).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), daemon)
            .await
            .expect("daemon process did not exit")
            .unwrap()
            .unwrap();
        wait_for_state(&config_path, ProbeState::NotRunning).await;
    }
}
