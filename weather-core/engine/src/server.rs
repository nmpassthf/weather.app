use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;
use prost::bytes::Bytes;
use tokio::{
    sync::{Semaphore, broadcast},
    task::{JoinError, JoinSet},
    time::{Instant, timeout_at},
};
use weather_schema::*;
use zeromq::{PubSocket, RouterSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

use crate::{
    lifecycle::{Cancellation, wait_for_exit},
    limits::{MAX_CONCURRENT_REQUESTS, MAX_RPC_PAYLOAD_BYTES},
    refresh::run_refresh_loop,
    runtime::{Engine, EngineExit},
};

#[cfg(test)]
mod e2e;

pub(crate) type EventSink = broadcast::Sender<(String, EventEnvelope)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Router,
    Signal,
    Refresh,
}

type TaskOutput = (TaskKind, Result<()>);

pub(crate) struct BoundEngineSockets {
    publisher: PubSocket,
    router: RouterSocket,
    pub(crate) rpc_endpoint: String,
    pub(crate) pub_endpoint: String,
}

struct RpcReply {
    identity: Bytes,
    response: RpcResponse,
    exit: Option<EngineExit>,
}

const RESPONSE_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_RPC_OVERHEAD: Duration = Duration::from_secs(2);
const WEATHER_UPSTREAM_STAGES: u32 = 6;
const CATALOG_UPSTREAM_STAGES: u32 = 8;

pub(crate) async fn run_engine_sockets(
    engine: Engine,
    rpc_endpoint: String,
    pub_endpoint: String,
    mode: String,
) -> Result<EngineExit> {
    let engine_config = engine.config.get().engine;
    let startup_timeout = Duration::from_millis(engine_config.startup_timeout_ms.max(1));
    let request_timeout = Duration::from_millis(engine_config.request_timeout_ms.max(1));
    let sockets = tokio::time::timeout(
        startup_timeout,
        bind_engine_sockets(&rpc_endpoint, &pub_endpoint),
    )
    .await
    .with_context(|| format!("engine socket startup timed out after {startup_timeout:?}"));
    let sockets = match sockets {
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

    run_bound_engine_sockets(engine, sockets, mode).await
}

pub(crate) async fn run_bound_engine_sockets(
    engine: Engine,
    sockets: BoundEngineSockets,
    mode: String,
) -> Result<EngineExit> {
    let BoundEngineSockets {
        publisher,
        router,
        rpc_endpoint,
        pub_endpoint,
    } = sockets;
    let request_timeout =
        Duration::from_millis(engine.config.get().engine.request_timeout_ms.max(1));
    let shutdown_timeout = cleanup_timeout(request_timeout);

    let work_cancellation = Cancellation::new();
    let publisher_cancellation = Cancellation::new();
    let mut work_tasks = JoinSet::<TaskOutput>::new();
    // Subscribe synchronously before publishing the initial status event.
    let publisher_rx = engine.sink.subscribe();
    let mut publisher_task = tokio::spawn(run_publisher(
        publisher,
        publisher_rx,
        publisher_cancellation.clone(),
    ));
    spawn_task(
        &mut work_tasks,
        TaskKind::Router,
        run_router(
            router,
            engine.clone(),
            rpc_endpoint.clone(),
            mode.clone(),
            pub_endpoint.clone(),
            request_timeout,
            work_cancellation.clone(),
        ),
    );
    spawn_task(
        &mut work_tasks,
        TaskKind::Signal,
        run_signal(work_cancellation.clone()),
    );
    spawn_task(
        &mut work_tasks,
        TaskKind::Refresh,
        run_refresh_loop(engine.clone(), work_cancellation.clone()),
    );
    engine.publish_lifecycle_status(
        &mode,
        &rpc_endpoint,
        &pub_endpoint,
        LifecycleState::Starting,
        Some("engine tasks starting".to_string()),
    );
    engine.publish_status(&mode, &rpc_endpoint, &pub_endpoint);

    let mut exit_rx = engine.control.subscribe();
    let (requested_exit, mut critical_failure, mut publisher_finished) = tokio::select! {
        exit = wait_for_exit(&mut exit_rx) => (Some(exit), None, false),
        joined = work_tasks.join_next() => match joined {
            Some(Ok((TaskKind::Signal, Ok(())))) => (Some(EngineExit::Shutdown), None, false),
            Some(Ok((kind, Ok(())))) => (
                None,
                Some(anyhow!("critical engine task {kind:?} exited unexpectedly")),
                false,
            ),
            Some(Ok((kind, Err(err)))) => (
                None,
                Some(err.context(format!("critical engine task {kind:?} failed"))),
                false,
            ),
            Some(Err(err)) => (
                None,
                Some(anyhow!("critical engine task panicked or was cancelled: {err}")),
                false,
            ),
            None => (
                None,
                Some(anyhow!("all critical engine tasks exited unexpectedly")),
                false,
            ),
        },
        joined = &mut publisher_task => (
            None,
            Some(unexpected_publisher_exit(joined)),
            true,
        ),
    };

    work_cancellation.cancel();
    if let Some(error) = drain_work_tasks(&mut work_tasks, shutdown_timeout).await {
        add_failure(&mut critical_failure, error);
    }

    if let Err(error) = bounded_db_shutdown(&engine, shutdown_timeout).await {
        add_failure(&mut critical_failure, error);
    }

    // If the publisher failed while work and the DB were being drained, include
    // that failure in the final lifecycle state. A live publisher is retained
    // until after the terminal event is queued.
    if !publisher_finished && publisher_task.is_finished() {
        publisher_finished = true;
        let joined = (&mut publisher_task).await;
        add_failure(&mut critical_failure, unexpected_publisher_exit(joined));
    }

    let (terminal_state, terminal_message) =
        terminal_lifecycle(requested_exit, critical_failure.as_ref());
    engine.publish_lifecycle_status(
        &mode,
        &rpc_endpoint,
        &pub_endpoint,
        terminal_state,
        terminal_message,
    );

    if !publisher_finished {
        publisher_cancellation.cancel();
        match tokio::time::timeout(shutdown_timeout, &mut publisher_task).await {
            Ok(joined) => {
                if let Some(error) = publisher_cleanup_failure(joined) {
                    add_failure(&mut critical_failure, error);
                }
            }
            Err(_) => {
                publisher_task.abort();
                let _ = publisher_task.await;
                add_failure(
                    &mut critical_failure,
                    anyhow!("publisher cleanup timed out after {shutdown_timeout:?}"),
                );
            }
        }
    }

    match critical_failure {
        Some(error) => Err(error),
        None => Ok(requested_exit.unwrap_or(EngineExit::Shutdown)),
    }
}

fn spawn_task<F>(tasks: &mut JoinSet<TaskOutput>, kind: TaskKind, task: F)
where
    F: FutureResult,
{
    tasks.spawn(async move { (kind, task.await) });
}

trait FutureResult: std::future::Future<Output = Result<()>> + Send + 'static {}
impl<T> FutureResult for T where T: std::future::Future<Output = Result<()>> + Send + 'static {}

async fn drain_work_tasks(
    tasks: &mut JoinSet<TaskOutput>,
    timeout: Duration,
) -> Option<anyhow::Error> {
    let deadline = Instant::now() + timeout;
    let mut failure = None;
    while !tasks.is_empty() {
        match timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(Ok((_kind, Ok(()))))) => {}
            Ok(Some(Ok((kind, Err(error))))) => add_failure(
                &mut failure,
                error.context(format!("engine task {kind:?} cleanup failed")),
            ),
            Ok(Some(Err(error))) => add_failure(
                &mut failure,
                anyhow!("engine task cleanup panicked or was cancelled: {error}"),
            ),
            Ok(None) => break,
            Err(_) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                add_failure(
                    &mut failure,
                    anyhow!("engine task cleanup timed out after {timeout:?}"),
                );
                break;
            }
        }
    }
    failure
}

fn unexpected_publisher_exit(joined: std::result::Result<Result<()>, JoinError>) -> anyhow::Error {
    match joined {
        Ok(Ok(())) => anyhow!("critical engine task Publisher exited unexpectedly"),
        Ok(Err(error)) => error.context("critical engine task Publisher failed"),
        Err(error) => anyhow!("critical engine task Publisher panicked or was cancelled: {error}"),
    }
}

fn publisher_cleanup_failure(
    joined: std::result::Result<Result<()>, JoinError>,
) -> Option<anyhow::Error> {
    match joined {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.context("publisher cleanup failed")),
        Err(error) => Some(anyhow!(
            "publisher cleanup task panicked or was cancelled: {error}"
        )),
    }
}

fn add_failure(failure: &mut Option<anyhow::Error>, error: anyhow::Error) {
    *failure = Some(match failure.take() {
        Some(previous) => anyhow!("{previous:#}; additionally: {error:#}"),
        None => error,
    });
}

fn cleanup_timeout(request_timeout: Duration) -> Duration {
    request_timeout.clamp(MIN_CLEANUP_TIMEOUT, MAX_CLEANUP_TIMEOUT)
}

fn terminal_lifecycle(
    requested_exit: Option<EngineExit>,
    critical_failure: Option<&anyhow::Error>,
) -> (LifecycleState, Option<String>) {
    match (requested_exit, critical_failure) {
        (_, Some(error)) => (LifecycleState::Failed, Some(format!("{error:#}"))),
        (Some(EngineExit::Restart), None) => (
            LifecycleState::Stopping,
            Some("engine restart requested".to_string()),
        ),
        _ => (
            LifecycleState::Stopping,
            Some("engine shutdown requested".to_string()),
        ),
    }
}

async fn bounded_db_shutdown(engine: &Engine, timeout: Duration) -> Result<()> {
    match tokio::time::timeout(timeout, engine.db.shutdown()).await {
        Ok(result) => result.context("failed to shut down DB actor"),
        Err(_) => bail!("DB shutdown timed out after {timeout:?}"),
    }
}

pub(crate) async fn bind_engine_sockets(
    rpc_endpoint: &str,
    pub_endpoint: &str,
) -> Result<BoundEngineSockets> {
    let (publisher, pub_endpoint) = bind_publisher(pub_endpoint).await?;
    let (router, rpc_endpoint) = match bind_router(rpc_endpoint).await {
        Ok(router) => router,
        Err(error) => {
            return match close_socket(publisher, "PUB").await {
                Ok(()) => Err(error),
                Err(close_error) => Err(anyhow!(
                    "{error:#}; PUB cleanup after RPC bind failure also failed: {close_error:#}"
                )),
            };
        }
    };
    Ok(BoundEngineSockets {
        publisher,
        router,
        rpc_endpoint,
        pub_endpoint,
    })
}

async fn bind_publisher(endpoint: &str) -> Result<(PubSocket, String)> {
    let mut socket = PubSocket::new();
    let endpoint = socket
        .bind(endpoint)
        .await
        .with_context(|| format!("failed to bind PUB endpoint {endpoint}"))?;
    Ok((socket, endpoint.to_string()))
}

async fn bind_router(endpoint: &str) -> Result<(RouterSocket, String)> {
    let mut socket = RouterSocket::new();
    let endpoint = socket
        .bind(endpoint)
        .await
        .with_context(|| format!("failed to bind RPC endpoint {endpoint}"))?;
    Ok((socket, endpoint.to_string()))
}

async fn close_socket<S: Socket>(socket: S, label: &str) -> Result<()> {
    let errors = socket.close().await;
    if errors.is_empty() {
        return Ok(());
    }
    bail!(
        "failed to close {label} socket: {}",
        errors
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn finish_with_cleanup(
    outcome: Result<()>,
    cleanup: Result<()>,
    cleanup_context: &str,
) -> Result<()> {
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context(cleanup_context.to_string())),
        (Err(error), Err(cleanup_error)) => {
            Err(anyhow!("{error:#}; {cleanup_context}: {cleanup_error:#}"))
        }
    }
}

async fn run_publisher(
    mut socket: PubSocket,
    mut rx: broadcast::Receiver<(String, EventEnvelope)>,
    cancellation: Cancellation,
) -> Result<()> {
    let outcome = run_publisher_loop(&mut socket, &mut rx, cancellation).await;
    let cleanup = close_socket(socket, "PUB").await;
    finish_with_cleanup(outcome, cleanup, "PUB socket cleanup failed")
}

async fn run_publisher_loop(
    socket: &mut PubSocket,
    rx: &mut broadcast::Receiver<(String, EventEnvelope)>,
    cancellation: Cancellation,
) -> Result<()> {
    loop {
        let (topic, event) = tokio::select! {
            _ = cancellation.cancelled() => return drain_publisher(socket, rx).await,
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
        send_publisher_event(socket, topic, event).await?;
        if cancellation.is_cancelled() {
            return drain_publisher(socket, rx).await;
        }
    }
}

async fn drain_publisher(
    socket: &mut PubSocket,
    rx: &mut broadcast::Receiver<(String, EventEnvelope)>,
) -> Result<()> {
    loop {
        match rx.try_recv() {
            Ok((topic, event)) => send_publisher_event(socket, topic, event).await?,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                eprintln!(
                    "weather-engine warn: publisher lagged during shutdown; skipped {skipped} events"
                );
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                return Ok(());
            }
        }
    }
}

async fn send_publisher_event(
    socket: &mut PubSocket,
    topic: String,
    event: EventEnvelope,
) -> Result<()> {
    let mut frames = VecDeque::new();
    frames.push_back(Bytes::from(topic));
    frames.push_back(Bytes::from(event.encode_to_vec()));
    let message = ZmqMessage::try_from(frames).expect("non-empty message");
    socket
        .send(message)
        .await
        .context("failed to send PUB event")
}

#[allow(clippy::too_many_arguments)]
async fn run_router(
    mut socket: RouterSocket,
    engine: Engine,
    rpc_endpoint: String,
    mode: String,
    pub_endpoint: String,
    request_timeout: Duration,
    cancellation: Cancellation,
) -> Result<()> {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let mut requests = JoinSet::<RpcReply>::new();

    let outcome = loop {
        let has_requests = !requests.is_empty();
        tokio::select! {
            _ = cancellation.cancelled() => break Ok(()),
            joined = requests.join_next(), if has_requests => {
                match joined {
                    Some(Ok(reply)) => {
                        match send_back(
                            &mut socket,
                            reply.identity,
                            reply.response.encode_to_vec(),
                            cancellation.clone(),
                        ).await {
                            Ok(()) => {
                                if let Some(exit) = reply.exit {
                                    engine.control.request_exit(exit);
                                }
                            }
                            Err(error) => log_response_send_error(&error),
                        }
                    }
                    Some(Err(err)) => {
                        eprintln!("weather-engine warn: RPC request task panicked or was cancelled: {err}");
                    }
                    None => {}
                }
            }
            received = socket.recv() => {
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
                        &mut socket, identity, "", RpcErrorCode::BadRequest, "missing rpc frame",
                        cancellation.clone(),
                    ).await {
                        log_response_send_error(&err);
                    }
                    continue;
                };
                if !frames.is_empty() {
                    let request_id = decoded_request_id(&payload);
                    if let Err(err) = send_back_error(
                        &mut socket, identity, &request_id, RpcErrorCode::BadRequest,
                        "unexpected extra rpc frames", cancellation.clone(),
                    ).await {
                        log_response_send_error(&err);
                    }
                    continue;
                }
                if payload_exceeds_limit(payload.len()) {
                    let request_id = decoded_request_id(&payload);
                    if let Err(err) = send_back_error(
                        &mut socket,
                        identity,
                        &request_id,
                        RpcErrorCode::PayloadTooLarge,
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
                            &mut socket, identity, &request_id, RpcErrorCode::Busy,
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
                requests.spawn(async move {
                    let _permit = permit;
                    process_request(
                        engine,
                        identity,
                        payload,
                        mode,
                        rpc_endpoint,
                        pub_endpoint,
                        request_timeout,
                    ).await
                });
            }
        }
    };

    requests.abort_all();
    while requests.join_next().await.is_some() {}
    let cleanup = close_socket(socket, "ROUTER").await;
    finish_with_cleanup(outcome, cleanup, "ROUTER socket cleanup failed")
}

#[allow(clippy::too_many_arguments)]
async fn process_request(
    engine: Engine,
    identity: Bytes,
    payload: Bytes,
    mode: String,
    rpc_endpoint: String,
    pub_endpoint: String,
    request_timeout: Duration,
) -> RpcReply {
    let (response, exit) = match RpcRequest::decode(payload.as_ref()) {
        Ok(request) => {
            let kind = RpcKind::try_from(request.kind).unwrap_or(RpcKind::Unspecified);
            let request_id = request.request_id.clone();
            let config = engine.config.get();
            let effective_timeout = request_timeout_for(&config, kind, request_timeout);
            let response = match tokio::time::timeout(
                effective_timeout,
                engine.handle_rpc_request(request, &mode, &rpc_endpoint, &pub_endpoint),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => Engine::rpc_error_response(
                    &request_id,
                    RpcErrorCode::Timeout,
                    format!("rpc request timed out after {effective_timeout:?}"),
                ),
            };
            let exit = accepted_exit(kind, &response);
            (response, exit)
        }
        Err(err) => (
            Engine::rpc_error_response(
                "",
                RpcErrorCode::BadRequest,
                format!("invalid rpc request: {err}"),
            ),
            None,
        ),
    };
    RpcReply {
        identity,
        response,
        exit,
    }
}

fn request_timeout_for(
    config: &weather_configure::AppConfig,
    kind: RpcKind,
    configured: Duration,
) -> Duration {
    let upstream_stages = match kind {
        RpcKind::GetWeather | RpcKind::TriggerRefresh => WEATHER_UPSTREAM_STAGES,
        RpcKind::ListProvinces => 1,
        RpcKind::ListCities
        | RpcKind::FuzzyMatchStations
        | RpcKind::BatchListRegions
        | RpcKind::ResolveStationUuid => CATALOG_UPSTREAM_STAGES,
        _ => return configured,
    };
    let provider_timeout = config
        .updater
        .provider
        .iter()
        .find(|provider| provider.name == config.updater.default_provider)
        .map(|provider| Duration::from_secs(provider.request_timeout_seconds.max(1)))
        .unwrap_or(configured);
    configured.max(
        provider_timeout
            .saturating_mul(upstream_stages)
            .saturating_add(UPSTREAM_RPC_OVERHEAD),
    )
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
    socket: &mut RouterSocket,
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
        sent = tokio::time::timeout(RESPONSE_SEND_TIMEOUT, socket.send(message)) => {
            match sent {
                Ok(result) => result.context("failed to send RPC response"),
                Err(_) => bail!("RPC response send timed out after {RESPONSE_SEND_TIMEOUT:?}"),
            }
        }
    }
}

async fn send_back_error(
    socket: &mut RouterSocket,
    identity: Bytes,
    request_id: &str,
    code: RpcErrorCode,
    message: &str,
    cancellation: Cancellation,
) -> Result<()> {
    let response = Engine::rpc_error_response(request_id, code, message);
    send_back(socket, identity, response.encode_to_vec(), cancellation).await
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::sync::Semaphore;
    use weather_configure::{AppConfig, write_config_atomic};
    use weather_updater::{
        ProviderCity, ProviderFuture, ProviderProvince, WeatherFetch, WeatherProvider,
    };

    use super::*;
    use crate::runtime::EngineRuntime;

    struct SlowColdStartProvider {
        weather_started: AtomicUsize,
        weather_release: Arc<Semaphore>,
    }

    impl WeatherProvider for SlowColdStartProvider {
        fn provider_name(&self) -> &str {
            "cold-start-test"
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
            Box::pin(async {
                Ok(vec![ProviderProvince {
                    provider_code: "P1".to_string(),
                    name: "北京市".to_string(),
                    url: "/province".to_string(),
                }])
            })
        }

        fn cities<'a>(
            &'a self,
            provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<ProviderCity>> {
            Box::pin(async move {
                Ok(vec![ProviderCity {
                    provider_code: "A".to_string(),
                    provider_province_code: provider_province_code.to_string(),
                    province: "北京市".to_string(),
                    city: "北京".to_string(),
                    url: "/city".to_string(),
                }])
            })
        }

        fn weather<'a>(
            &'a self,
            _provider_station_id: &'a str,
            _include_debug: bool,
        ) -> ProviderFuture<'a, WeatherFetch> {
            self.weather_started.fetch_add(1, Ordering::SeqCst);
            let release = self.weather_release.clone();
            Box::pin(async move {
                release.acquire().await.unwrap().forget();
                Ok(WeatherFetch {
                    snapshot: WeatherSnapshot::default(),
                    warnings: Vec::new(),
                })
            })
        }
    }

    #[test]
    fn only_accepted_shutdown_and_restart_responses_request_exit() {
        let mut response = Engine::rpc_error_response("id", RpcErrorCode::Engine, "failed");
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

    #[test]
    fn network_backed_requests_outlive_the_short_control_rpc_timeout() {
        let config = weather_configure::AppConfig::default();
        let configured = Duration::from_secs(3);

        assert_eq!(
            request_timeout_for(&config, RpcKind::GetWeather, configured),
            Duration::from_secs(122)
        );
        assert_eq!(
            request_timeout_for(&config, RpcKind::ListProvinces, configured),
            Duration::from_secs(22)
        );
        assert_eq!(
            request_timeout_for(&config, RpcKind::FuzzyMatchStations, configured),
            Duration::from_secs(162)
        );
        assert_eq!(
            request_timeout_for(&config, RpcKind::Ping, configured),
            configured
        );
        assert_eq!(
            request_timeout_for(&config, RpcKind::GetResource, configured),
            configured
        );
    }

    #[tokio::test]
    async fn cold_weather_refresh_is_not_cancelled_by_the_control_rpc_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let mut config = AppConfig::default();
        config.engine.request_timeout_ms = 1;
        config.db.path = directory.path().join("weather.db").display().to_string();
        config.updater.default_provider = "cold-start-test".to_string();
        config.updater.provider[0].name = "cold-start-test".to_string();
        config.updater.provider[0].request_timeout_seconds = 1;
        write_config_atomic(&config_path, &config).unwrap();

        let release = Arc::new(Semaphore::new(0));
        let provider = Arc::new(SlowColdStartProvider {
            weather_started: AtomicUsize::new(0),
            weather_release: release.clone(),
        });
        let runtime = EngineRuntime::start_with_provider(config_path, provider.clone())
            .await
            .unwrap();
        let engine = runtime.test_engine();
        let request = RpcRequest {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: "cold-start-weather".to_string(),
            kind: RpcKind::GetWeather as i32,
            timestamp_unix_ms: 0,
            hmac_sha256: Vec::new(),
            payload: GetWeatherRequest {
                unified_uuid: unified_station_uuid("北京-北京市"),
                refresh: true,
                include_debug: false,
            }
            .encode_to_vec(),
        };
        let task = tokio::spawn(process_request(
            engine.clone(),
            Bytes::from_static(b"cold-start-client"),
            Bytes::from(request.encode_to_vec()),
            "test".to_string(),
            "tcp://127.0.0.1:1".to_string(),
            "tcp://127.0.0.1:2".to_string(),
            Duration::from_millis(1),
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.weather_started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cold weather request did not reach the provider");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished(), "request used the 1ms control timeout");

        release.add_permits(1);
        let reply = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cold weather request did not finish")
            .unwrap();
        assert_eq!(reply.response.status, ResponseStatus::Ok as i32);
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dynamic_endpoints_are_reported_and_can_be_rebound_after_close() {
        let sockets = bind_engine_sockets("tcp://127.0.0.1:0", "tcp://127.0.0.1:0")
            .await
            .unwrap();
        let BoundEngineSockets {
            publisher,
            router,
            rpc_endpoint,
            pub_endpoint,
        } = sockets;

        assert_ne!(endpoint_port(&rpc_endpoint), 0);
        assert_ne!(endpoint_port(&pub_endpoint), 0);
        assert_ne!(rpc_endpoint, pub_endpoint);

        close_socket(router, "ROUTER").await.unwrap();
        close_socket(publisher, "PUB").await.unwrap();

        let rebound = bind_engine_sockets(&rpc_endpoint, &pub_endpoint)
            .await
            .unwrap();
        assert_eq!(rebound.rpc_endpoint, rpc_endpoint);
        assert_eq!(rebound.pub_endpoint, pub_endpoint);
        close_socket(rebound.router, "ROUTER").await.unwrap();
        close_socket(rebound.publisher, "PUB").await.unwrap();
    }

    fn endpoint_port(endpoint: &str) -> u16 {
        endpoint
            .rsplit(':')
            .next()
            .expect("endpoint port")
            .parse()
            .expect("numeric endpoint port")
    }

    #[test]
    fn terminal_lifecycle_distinguishes_exit_and_failure() {
        assert_eq!(
            terminal_lifecycle(Some(EngineExit::Shutdown), None),
            (
                LifecycleState::Stopping,
                Some("engine shutdown requested".to_string())
            )
        );
        assert_eq!(
            terminal_lifecycle(Some(EngineExit::Restart), None),
            (
                LifecycleState::Stopping,
                Some("engine restart requested".to_string())
            )
        );

        let failure = anyhow!("router failed");
        assert_eq!(
            terminal_lifecycle(None, Some(&failure)),
            (LifecycleState::Failed, Some("router failed".to_string()))
        );

        let cleanup_failure = anyhow!("DB shutdown timed out");
        assert_eq!(
            terminal_lifecycle(Some(EngineExit::Restart), Some(&cleanup_failure)),
            (
                LifecycleState::Failed,
                Some("DB shutdown timed out".to_string())
            )
        );
    }
}
