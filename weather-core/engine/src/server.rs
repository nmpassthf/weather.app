use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;
use prost::bytes::Bytes;
use tokio::{
    sync::{Semaphore, broadcast},
    task::JoinSet,
};
use weather_schema::*;
use zeromq::{PubSocket, RouterSendHalf, RouterSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

use crate::{
    config_normalizer::run_config_normalizer,
    lifecycle::{Cancellation, wait_for_exit},
    limits::{MAX_CONCURRENT_REQUESTS, MAX_RPC_PAYLOAD_BYTES},
    refresh::run_refresh_loop,
    runtime::{Engine, EngineExit},
};

pub(crate) type EventSink = broadcast::Sender<(String, EventEnvelope)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Publisher,
    Router,
    Signal,
    Refresh,
    ConfigNormalizer,
}

type TaskOutput = (TaskKind, Result<()>);

const RESPONSE_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn run_engine_sockets(
    engine: Engine,
    rpc_endpoint: String,
    pub_endpoint: String,
    mode: String,
) -> Result<EngineExit> {
    let engine_config = engine.config.get().engine;
    let startup_timeout = Duration::from_millis(engine_config.startup_timeout_ms.max(1));
    let request_timeout = Duration::from_millis(engine_config.request_timeout_ms.max(1));
    let sockets = tokio::time::timeout(startup_timeout, async {
        let publisher = bind_publisher(&pub_endpoint).await?;
        let router = bind_router(&rpc_endpoint).await?;
        Ok::<_, anyhow::Error>((publisher, router))
    })
    .await
    .with_context(|| format!("engine socket startup timed out after {startup_timeout:?}"));
    let (publisher, router) = match sockets {
        Ok(Ok(sockets)) => sockets,
        Ok(Err(err)) => {
            if let Err(db_err) =
                bounded_db_shutdown(&engine, cleanup_timeout(request_timeout)).await
            {
                return Err(err.context(format!("DB shutdown also failed: {db_err:#}")));
            }
            return Err(err);
        }
        Err(err) => {
            if let Err(db_err) =
                bounded_db_shutdown(&engine, cleanup_timeout(request_timeout)).await
            {
                return Err(err.context(format!("DB shutdown also failed: {db_err:#}")));
            }
            return Err(err);
        }
    };

    let cancellation = Cancellation::new();
    let mut tasks = JoinSet::<TaskOutput>::new();
    // Subscribe synchronously before publishing the initial status event.
    let publisher_rx = engine.sink.subscribe();
    spawn_task(
        &mut tasks,
        TaskKind::Publisher,
        run_publisher(publisher, publisher_rx, cancellation.clone()),
    );
    spawn_task(
        &mut tasks,
        TaskKind::Router,
        run_router(
            router,
            engine.clone(),
            rpc_endpoint.clone(),
            mode.clone(),
            pub_endpoint.clone(),
            request_timeout,
            cancellation.clone(),
        ),
    );
    spawn_task(
        &mut tasks,
        TaskKind::Signal,
        run_signal(cancellation.clone()),
    );
    spawn_task(
        &mut tasks,
        TaskKind::Refresh,
        run_refresh_loop(engine.clone(), cancellation.clone()),
    );
    spawn_task(
        &mut tasks,
        TaskKind::ConfigNormalizer,
        run_config_normalizer(
            engine.config_path.clone(),
            engine.config.clone(),
            cancellation.clone(),
        ),
    );

    engine.publish_status(&mode, &rpc_endpoint, &pub_endpoint);

    let mut exit_rx = engine.control.subscribe();
    let (requested_exit, mut critical_failure) = tokio::select! {
        exit = wait_for_exit(&mut exit_rx) => (Some(exit), None),
        joined = tasks.join_next() => match joined {
            Some(Ok((TaskKind::Signal, Ok(())))) => (Some(EngineExit::Shutdown), None),
            Some(Ok((kind, Ok(())))) => (
                None,
                Some(anyhow!("critical engine task {kind:?} exited unexpectedly")),
            ),
            Some(Ok((kind, Err(err)))) => (
                None,
                Some(err.context(format!("critical engine task {kind:?} failed"))),
            ),
            Some(Err(err)) => (
                None,
                Some(anyhow!("critical engine task panicked or was cancelled: {err}")),
            ),
            None => (
                None,
                Some(anyhow!("all critical engine tasks exited unexpectedly")),
            ),
        },
    };

    cancellation.cancel();
    let shutdown_timeout = cleanup_timeout(request_timeout);
    if tokio::time::timeout(shutdown_timeout, drain_tasks(&mut tasks))
        .await
        .is_err()
    {
        tasks.abort_all();
        drain_tasks(&mut tasks).await;
        if critical_failure.is_none() {
            critical_failure = Some(anyhow!(
                "engine task cleanup timed out after {shutdown_timeout:?}"
            ));
        }
    }

    let db_shutdown = bounded_db_shutdown(&engine, shutdown_timeout).await;
    if let Some(err) = critical_failure {
        if let Err(db_err) = db_shutdown {
            return Err(err.context(format!("DB shutdown also failed: {db_err:#}")));
        }
        return Err(err);
    }
    db_shutdown?;
    Ok(requested_exit.unwrap_or(EngineExit::Shutdown))
}

fn spawn_task<F>(tasks: &mut JoinSet<TaskOutput>, kind: TaskKind, task: F)
where
    F: FutureResult,
{
    tasks.spawn(async move { (kind, task.await) });
}

trait FutureResult: std::future::Future<Output = Result<()>> + Send + 'static {}
impl<T> FutureResult for T where T: std::future::Future<Output = Result<()>> + Send + 'static {}

async fn drain_tasks(tasks: &mut JoinSet<TaskOutput>) {
    while tasks.join_next().await.is_some() {}
}

fn cleanup_timeout(request_timeout: Duration) -> Duration {
    request_timeout.clamp(MIN_CLEANUP_TIMEOUT, MAX_CLEANUP_TIMEOUT)
}

async fn bounded_db_shutdown(engine: &Engine, timeout: Duration) -> Result<()> {
    match tokio::time::timeout(timeout, engine.db.shutdown()).await {
        Ok(result) => result.context("failed to shut down DB actor"),
        Err(_) => bail!("DB shutdown timed out after {timeout:?}"),
    }
}

async fn bind_publisher(endpoint: &str) -> Result<PubSocket> {
    let mut socket = PubSocket::new();
    socket
        .bind(endpoint)
        .await
        .with_context(|| format!("failed to bind PUB endpoint {endpoint}"))?;
    Ok(socket)
}

async fn bind_router(endpoint: &str) -> Result<RouterSocket> {
    let mut socket = RouterSocket::new();
    socket
        .bind(endpoint)
        .await
        .with_context(|| format!("failed to bind RPC endpoint {endpoint}"))?;
    Ok(socket)
}

async fn run_publisher(
    mut socket: PubSocket,
    mut rx: broadcast::Receiver<(String, EventEnvelope)>,
    cancellation: Cancellation,
) -> Result<()> {
    loop {
        let (topic, event) = tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            event = rx.recv() => match event {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!("weather-engine warn: publisher lagged; skipped {skipped} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) if cancellation.is_cancelled() => {
                    return Ok(());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("engine event channel closed unexpectedly");
                }
            }
        };
        let mut frames = VecDeque::new();
        frames.push_back(Bytes::from(topic));
        frames.push_back(Bytes::from(event.encode_to_vec()));
        let message = ZmqMessage::try_from(frames).expect("non-empty message");
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            sent = socket.send(message) => {
                sent.context("failed to send PUB event")?;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_router(
    socket: RouterSocket,
    engine: Engine,
    rpc_endpoint: String,
    mode: String,
    pub_endpoint: String,
    request_timeout: Duration,
    cancellation: Cancellation,
) -> Result<()> {
    let (send_half, mut recv_half) = socket.split();
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let mut requests = JoinSet::<Result<()>>::new();

    let outcome = loop {
        let has_requests = !requests.is_empty();
        tokio::select! {
            _ = cancellation.cancelled() => break Ok(()),
            joined = requests.join_next(), if has_requests => {
                match joined {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(err))) => {
                        eprintln!("weather-engine warn: RPC request task failed: {err:#}");
                    }
                    Some(Err(err)) => {
                        eprintln!("weather-engine warn: RPC request task panicked or was cancelled: {err}");
                    }
                    None => {}
                }
            }
            received = recv_half.recv() => {
                let message = match received {
                    Ok(message) => message,
                    Err(err) => break Err(anyhow!("ROUTER receive failed: {err}")),
                };
                let mut frames = message.into_vecdeque();
                let Some(identity) = frames.pop_front() else {
                    continue;
                };
                let Some(payload) = frames.pop_front() else {
                    if let Err(err) = send_back_error(
                        send_half.clone(), identity, "", "BAD_REQUEST", "missing rpc frame",
                        cancellation.clone(),
                    ).await {
                        log_response_send_error(&err);
                    }
                    continue;
                };
                if !frames.is_empty() {
                    let request_id = decoded_request_id(&payload);
                    if let Err(err) = send_back_error(
                        send_half.clone(), identity, &request_id, "BAD_REQUEST",
                        "unexpected extra rpc frames", cancellation.clone(),
                    ).await {
                        log_response_send_error(&err);
                    }
                    continue;
                }
                if payload_exceeds_limit(payload.len()) {
                    let request_id = decoded_request_id(&payload);
                    if let Err(err) = send_back_error(
                        send_half.clone(),
                        identity,
                        &request_id,
                        "PAYLOAD_TOO_LARGE",
                        &format!(
                            "rpc payload {} bytes exceeds maximum {MAX_RPC_PAYLOAD_BYTES}",
                            payload.len()
                        ),
                        cancellation.clone(),
                    ).await {
                        log_response_send_error(&err);
                    }
                    continue;
                }
                let permit = match permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        let request_id = decoded_request_id(&payload);
                        if let Err(err) = send_back_error(
                            send_half.clone(), identity, &request_id, "BUSY",
                            "maximum concurrent RPC requests reached", cancellation.clone(),
                        ).await {
                            log_response_send_error(&err);
                        }
                        continue;
                    }
                };
                let engine = engine.clone();
                let mode = mode.clone();
                let pub_endpoint = pub_endpoint.clone();
                let rpc_endpoint = rpc_endpoint.clone();
                let send_half = send_half.clone();
                let request_cancellation = cancellation.clone();
                requests.spawn(async move {
                    let _permit = permit;
                    process_request(
                        engine,
                        send_half,
                        identity,
                        payload,
                        mode,
                        rpc_endpoint,
                        pub_endpoint,
                        request_timeout,
                        request_cancellation,
                    ).await
                });
            }
        }
    };

    requests.abort_all();
    while requests.join_next().await.is_some() {}
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn process_request(
    engine: Engine,
    send_half: RouterSendHalf,
    identity: Bytes,
    payload: Bytes,
    mode: String,
    rpc_endpoint: String,
    pub_endpoint: String,
    request_timeout: Duration,
    cancellation: Cancellation,
) -> Result<()> {
    let (response, exit) = match RpcRequest::decode(payload.as_ref()) {
        Ok(request) => {
            let kind = RpcKind::try_from(request.kind).unwrap_or(RpcKind::Unspecified);
            let request_id = request.request_id.clone();
            let response = match tokio::time::timeout(
                request_timeout,
                engine.handle_rpc_request(request, &mode, &rpc_endpoint, &pub_endpoint),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => Engine::rpc_error_response(
                    &request_id,
                    "TIMEOUT",
                    format!("rpc request timed out after {request_timeout:?}"),
                ),
            };
            let exit = accepted_exit(kind, &response);
            (response, exit)
        }
        Err(err) => (
            Engine::rpc_error_response("", "BAD_REQUEST", format!("invalid rpc request: {err}")),
            None,
        ),
    };
    send_back(send_half, identity, response.encode_to_vec(), cancellation).await?;
    if let Some(exit) = exit {
        engine.control.request_exit(exit);
    }
    Ok(())
}

fn payload_exceeds_limit(len: usize) -> bool {
    len > MAX_RPC_PAYLOAD_BYTES
}

fn accepted_exit(kind: RpcKind, response: &RpcResponse) -> Option<EngineExit> {
    if response.status != ResponseStatus::Accepted as i32 {
        return None;
    }
    match kind {
        RpcKind::RestartEngine => Some(EngineExit::Restart),
        RpcKind::Shutdown => Some(EngineExit::Shutdown),
        _ => None,
    }
}

fn decoded_request_id(payload: &Bytes) -> String {
    RpcRequest::decode(payload.as_ref())
        .map(|request| request.request_id)
        .unwrap_or_default()
}

async fn send_back(
    mut send_half: RouterSendHalf,
    identity: Bytes,
    payload: Vec<u8>,
    cancellation: Cancellation,
) -> Result<()> {
    let mut frames = VecDeque::new();
    frames.push_back(identity);
    frames.push_back(Bytes::from(payload));
    let message = ZmqMessage::try_from(frames).expect("non-empty message");
    tokio::select! {
        _ = cancellation.cancelled() => bail!("RPC response send cancelled"),
        sent = tokio::time::timeout(RESPONSE_SEND_TIMEOUT, send_half.send(message)) => {
            match sent {
                Ok(result) => result.context("failed to send RPC response"),
                Err(_) => bail!("RPC response send timed out after {RESPONSE_SEND_TIMEOUT:?}"),
            }
        }
    }
}

async fn send_back_error(
    send_half: RouterSendHalf,
    identity: Bytes,
    request_id: &str,
    code: &str,
    message: &str,
    cancellation: Cancellation,
) -> Result<()> {
    let response = Engine::rpc_error_response(request_id, code, message);
    send_back(send_half, identity, response.encode_to_vec(), cancellation).await
}

fn log_response_send_error(err: &anyhow::Error) {
    eprintln!("weather-engine warn: failed to send RPC response: {err:#}");
}

async fn run_signal(cancellation: Cancellation) -> Result<()> {
    tokio::select! {
        _ = cancellation.cancelled() => Ok(()),
        result = wait_for_signal() => result,
    }
}

/// 等待 OS 信号:Unix 下 SIGINT/SIGTERM,Windows 下 Ctrl+C。
async fn wait_for_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepted_shutdown_and_restart_responses_request_exit() {
        let mut response = Engine::rpc_error_response("id", "ERR", "failed");
        assert_eq!(accepted_exit(RpcKind::Shutdown, &response), None);

        response.status = ResponseStatus::Accepted as i32;
        assert_eq!(
            accepted_exit(RpcKind::Shutdown, &response),
            Some(EngineExit::Shutdown)
        );
        assert_eq!(
            accepted_exit(RpcKind::RestartEngine, &response),
            Some(EngineExit::Restart)
        );
        assert_eq!(accepted_exit(RpcKind::Ping, &response), None);
    }

    #[test]
    fn oversized_payload_limit_is_strict() {
        assert!(!payload_exceeds_limit(MAX_RPC_PAYLOAD_BYTES));
        assert!(payload_exceeds_limit(MAX_RPC_PAYLOAD_BYTES + 1));
    }

    #[test]
    fn decoded_request_id_preserves_correlation() {
        let payload = Bytes::from(
            RpcRequest {
                request_id: "request-42".to_string(),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        assert_eq!(decoded_request_id(&payload), "request-42");
    }

    #[test]
    fn cleanup_timeout_is_clamped() {
        assert_eq!(
            cleanup_timeout(Duration::from_millis(1)),
            MIN_CLEANUP_TIMEOUT
        );
        assert_eq!(
            cleanup_timeout(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            cleanup_timeout(Duration::from_secs(60)),
            MAX_CLEANUP_TIMEOUT
        );
    }
}
