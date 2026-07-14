use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::DateTime;
use prost::Message;
use rusqlite::{Connection, params};
use tokio::sync::Semaphore;
use weather_configure::{AppConfig, StationConfig, write_config_atomic};
use weather_schema::*;
use weather_updater::{
    ProviderCity, ProviderFuture, ProviderProvince, WeatherFetch, WeatherProvider,
};
use zeromq::{DealerSocket, Socket, SocketRecv, SocketSend, SubSocket, ZmqMessage};

use super::{bind_engine_sockets, close_socket, run_bound_engine_sockets};
use crate::{
    lifecycle::Cancellation,
    refresh::run_refresh_loop,
    runtime::{Engine, EngineExit, EngineRuntime},
    time::WeatherClock,
};

const PROVIDER_NAME: &str = "e2e-test";
const STATION_A_NAME: &str = "北京-北京市";
const STATION_B_NAME: &str = "北京-北京市-朝阳";
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const SUB_BARRIER_ATTEMPT: Duration = Duration::from_millis(75);

struct ManualWeatherClock(AtomicI64);

impl ManualWeatherClock {
    fn new(now_unix_ms: i64) -> Self {
        Self(AtomicI64::new(now_unix_ms))
    }

    fn set(&self, now_unix_ms: i64) {
        self.0.store(now_unix_ms, Ordering::SeqCst);
    }
}

impl WeatherClock for ManualWeatherClock {
    fn now_unix_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

enum WeatherResult {
    Success(f64),
    Failure(String),
}

struct WeatherStep {
    result: WeatherResult,
    gate: Option<Arc<Semaphore>>,
}

impl WeatherStep {
    fn success(temperature: f64) -> Self {
        Self {
            result: WeatherResult::Success(temperature),
            gate: None,
        }
    }

    fn failure(message: &str) -> Self {
        Self {
            result: WeatherResult::Failure(message.to_string()),
            gate: None,
        }
    }

    fn gated(mut self, gate: Arc<Semaphore>) -> Self {
        self.gate = Some(gate);
        self
    }
}

#[derive(Default)]
struct ProviderCounters {
    total: AtomicUsize,
    active: AtomicUsize,
    completed: AtomicUsize,
    dropped: AtomicUsize,
    by_station: Mutex<HashMap<String, usize>>,
}

struct ActiveWeatherCall {
    counters: Arc<ProviderCounters>,
    completed: bool,
}

impl ActiveWeatherCall {
    fn start(counters: Arc<ProviderCounters>) -> Self {
        counters.active.fetch_add(1, Ordering::SeqCst);
        Self {
            counters,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.completed = true;
        self.counters.completed.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for ActiveWeatherCall {
    fn drop(&mut self) {
        self.counters.active.fetch_sub(1, Ordering::SeqCst);
        if !self.completed {
            self.counters.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Default)]
struct ScriptedWeatherProvider {
    steps: Mutex<HashMap<String, VecDeque<WeatherStep>>>,
    counters: Arc<ProviderCounters>,
}

impl ScriptedWeatherProvider {
    fn queue(&self, station: &str, step: WeatherStep) {
        self.steps
            .lock()
            .expect("weather script lock")
            .entry(station.to_string())
            .or_default()
            .push_back(step);
    }

    fn total_calls(&self) -> usize {
        self.counters.total.load(Ordering::SeqCst)
    }

    fn active_calls(&self) -> usize {
        self.counters.active.load(Ordering::SeqCst)
    }

    fn completed_calls(&self) -> usize {
        self.counters.completed.load(Ordering::SeqCst)
    }

    fn dropped_calls(&self) -> usize {
        self.counters.dropped.load(Ordering::SeqCst)
    }

    fn station_calls(&self, station: &str) -> usize {
        self.counters
            .by_station
            .lock()
            .expect("weather counter lock")
            .get(station)
            .copied()
            .unwrap_or_default()
    }
}

impl WeatherProvider for ScriptedWeatherProvider {
    fn provider_name(&self) -> &str {
        PROVIDER_NAME
    }

    fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
        Box::pin(async {
            Ok(vec![ProviderProvince {
                provider_code: "ABJ".to_string(),
                name: "北京市".to_string(),
                url: "/province/ABJ".to_string(),
            }])
        })
    }

    fn cities<'a>(
        &'a self,
        provider_province_code: &'a str,
    ) -> ProviderFuture<'a, Vec<ProviderCity>> {
        Box::pin(async move {
            if provider_province_code != "ABJ" {
                bail!("unexpected province code {provider_province_code}");
            }
            Ok(vec![
                ProviderCity {
                    provider_code: "A".to_string(),
                    provider_province_code: "ABJ".to_string(),
                    province: "北京市".to_string(),
                    city: "北京".to_string(),
                    url: "/weather/A".to_string(),
                },
                ProviderCity {
                    provider_code: "B".to_string(),
                    provider_province_code: "ABJ".to_string(),
                    province: "北京市".to_string(),
                    city: "朝阳".to_string(),
                    url: "/weather/B".to_string(),
                },
            ])
        })
    }

    fn weather<'a>(
        &'a self,
        provider_station_id: &'a str,
        _include_debug: bool,
    ) -> ProviderFuture<'a, WeatherFetch> {
        Box::pin(async move {
            self.counters.total.fetch_add(1, Ordering::SeqCst);
            *self
                .counters
                .by_station
                .lock()
                .expect("weather counter lock")
                .entry(provider_station_id.to_string())
                .or_default() += 1;
            let active = ActiveWeatherCall::start(Arc::clone(&self.counters));
            let step = self
                .steps
                .lock()
                .expect("weather script lock")
                .get_mut(provider_station_id)
                .and_then(VecDeque::pop_front)
                .with_context(|| format!("no weather step queued for {provider_station_id}"))?;
            if let Some(gate) = step.gate {
                gate.acquire()
                    .await
                    .context("weather gate closed")?
                    .forget();
            }
            let result = match step.result {
                WeatherResult::Success(temperature) => Ok(WeatherFetch {
                    snapshot: WeatherSnapshot {
                        real: Some(ObservedWeather {
                            temperature: Some(temperature),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    warnings: Vec::new(),
                }),
                WeatherResult::Failure(message) => Err(anyhow!(message)),
            };
            active.complete();
            result
        })
    }
}

struct RawClient {
    dealer: DealerSocket,
    subscriber: SubSocket,
}

struct RawEvent {
    topic: String,
    envelope: EventEnvelope,
}

impl RawClient {
    async fn connect(rpc_endpoint: &str, pub_endpoint: &str) -> Result<Self> {
        let mut dealer = DealerSocket::new();
        dealer.connect(rpc_endpoint).await?;
        let mut subscriber = SubSocket::new();
        subscriber.connect(pub_endpoint).await?;
        subscriber.subscribe("").await?;
        Ok(Self { dealer, subscriber })
    }

    async fn send_request<M: Message>(&mut self, kind: RpcKind, payload: M) -> Result<String> {
        let request_id = correlation_id("e2e-request");
        let request = RpcRequest {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: request_id.clone(),
            kind: kind as i32,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
            hmac_sha256: Vec::new(),
            payload: payload.encode_to_vec(),
        };
        tokio::time::timeout(
            WAIT_TIMEOUT,
            self.dealer.send(ZmqMessage::from(request.encode_to_vec())),
        )
        .await
        .context("raw RPC send timed out")??;
        Ok(request_id)
    }

    async fn receive_response(&mut self, request_id: &str) -> Result<RpcResponse> {
        let message = tokio::time::timeout(WAIT_TIMEOUT, self.dealer.recv())
            .await
            .context("raw RPC receive timed out")??;
        let mut frames = message.into_vecdeque();
        let payload = frames
            .pop_front()
            .context("raw RPC response had no payload frame")?;
        if !frames.is_empty() {
            bail!("raw RPC response had extra frames");
        }
        let response = RpcResponse::decode(payload.as_ref())?;
        if response.schema_version != SCHEMA_VERSION || response.request_id != request_id {
            bail!("raw RPC response envelope did not match request");
        }
        Ok(response)
    }

    async fn request<M: Message>(&mut self, kind: RpcKind, payload: M) -> Result<RpcResponse> {
        let request_id = self.send_request(kind, payload).await?;
        self.receive_response(&request_id).await
    }

    async fn receive_event(&mut self) -> Result<RawEvent> {
        self.receive_event_with_timeout(WAIT_TIMEOUT)
            .await?
            .context("raw event receive timed out")
    }

    async fn receive_event_with_timeout(&mut self, wait: Duration) -> Result<Option<RawEvent>> {
        let message = match tokio::time::timeout(wait, self.subscriber.recv()).await {
            Ok(message) => message?,
            Err(_) => return Ok(None),
        };
        let mut frames = message.into_vecdeque();
        let topic = frames.pop_front().context("raw event had no topic frame")?;
        let payload = frames
            .pop_front()
            .context("raw event had no envelope frame")?;
        if !frames.is_empty() {
            bail!("raw event had extra frames");
        }
        Ok(Some(RawEvent {
            topic: String::from_utf8(topic.to_vec())?,
            envelope: EventEnvelope::decode(payload.as_ref())?,
        }))
    }

    async fn close(self) -> Result<()> {
        let mut errors = self.dealer.close().await;
        errors.extend(self.subscriber.close().await);
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "raw client socket close failed: {}",
                errors
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
    }
}

fn test_config(
    directory: &tempfile::TempDir,
    rpc_endpoint: String,
    pub_endpoint: String,
) -> AppConfig {
    let mut config = AppConfig::default();
    config.engine.lock_path = directory.path().join("engine.lock").display().to_string();
    config.ipc.rpc_endpoint = rpc_endpoint;
    config.ipc.pub_endpoint = pub_endpoint;
    config.db.path = directory.path().join("weather.db").display().to_string();
    config.updater.weather_ttl_seconds = 3_600;
    config.updater.province_ttl_seconds = 3_600;
    config.updater.default_provider = PROVIDER_NAME.to_string();
    config.updater.provider[0].name = PROVIDER_NAME.to_string();
    config.stations = vec![
        StationConfig {
            name: STATION_A_NAME.to_string(),
            enabled: true,
        },
        StationConfig {
            name: STATION_B_NAME.to_string(),
            enabled: false,
        },
    ];
    config
}

async fn wait_until(mut condition: impl FnMut() -> bool) -> Result<()> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("condition wait timed out")
}

async fn establish_subscription(
    client: &mut RawClient,
    engine: &Engine,
    rpc_endpoint: &str,
    pub_endpoint: &str,
) -> Result<()> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            engine.publish_status("e2e", rpc_endpoint, pub_endpoint);
            let Some(event) = client
                .receive_event_with_timeout(SUB_BARRIER_ATTEMPT)
                .await?
            else {
                continue;
            };
            if event.topic == TOPIC_ENGINE_STATUS
                && event.envelope.kind == EventKind::EngineStatus as i32
            {
                let status = EngineStatus::decode(event.envelope.payload.as_slice())?;
                if status.ready
                    && status.rpc_endpoint == rpc_endpoint
                    && status.pub_endpoint == pub_endpoint
                {
                    return Ok(());
                }
            }
        }
    })
    .await
    .context("SUB subscription barrier timed out")?
}

fn assert_refresh_terminal(event: &RefreshEvent, uuid: &str, outcome: RefreshOutcome) {
    assert_eq!(event.unified_uuid.as_deref(), Some(uuid));
    assert!(!event.started);
    assert!(event.completed);
    assert_eq!(event.phase, RefreshPhase::Completed as i32);
    assert_eq!(event.outcome, outcome as i32);
    let expected_message = match outcome {
        RefreshOutcome::Success => "success",
        RefreshOutcome::Stale => "stale",
        RefreshOutcome::Failure => "failure:",
        RefreshOutcome::Unspecified => unreachable!("terminal outcome must be specified"),
    };
    let message = event.message.as_deref().expect("terminal refresh message");
    if outcome == RefreshOutcome::Failure {
        assert!(message.starts_with(expected_message));
    } else {
        assert_eq!(message, expected_message);
    }
}

async fn wait_for_terminal(
    client: &mut RawClient,
    uuid: &str,
    outcome: RefreshOutcome,
) -> Result<()> {
    loop {
        let event = client.receive_event().await?;
        if event.topic != TOPIC_ENGINE_REFRESH || event.envelope.kind != EventKind::Refresh as i32 {
            continue;
        }
        let refresh = RefreshEvent::decode(event.envelope.payload.as_slice())?;
        if refresh.unified_uuid.as_deref() == Some(uuid)
            && refresh.phase == RefreshPhase::Completed as i32
            && refresh.outcome == outcome as i32
        {
            assert_refresh_terminal(&refresh, uuid, outcome);
            return Ok(());
        }
    }
}

async fn wait_for_failure_events(
    client: &mut RawClient,
    uuid: &str,
    refresh_outcome: RefreshOutcome,
) -> Result<()> {
    let mut saw_terminal = false;
    let mut saw_fetch_failure = false;
    while !(saw_terminal && saw_fetch_failure) {
        let event = client.receive_event().await?;
        match (event.topic.as_str(), event.envelope.kind) {
            (TOPIC_ENGINE_REFRESH, kind) if kind == EventKind::Refresh as i32 => {
                let refresh = RefreshEvent::decode(event.envelope.payload.as_slice())?;
                if refresh.unified_uuid.as_deref() == Some(uuid)
                    && refresh.phase == RefreshPhase::Completed as i32
                    && refresh.outcome == refresh_outcome as i32
                {
                    assert_refresh_terminal(&refresh, uuid, refresh_outcome);
                    saw_terminal = true;
                }
            }
            (TOPIC_ENGINE_LOG, kind) if kind == EventKind::FetchLog as i32 => {
                let fetch = FetchLogEvent::decode(event.envelope.payload.as_slice())?;
                if fetch.unified_uuid.as_deref() == Some(uuid)
                    && fetch.outcome == FetchOutcome::Failure as i32
                {
                    assert!(!fetch.ok);
                    assert!(
                        fetch
                            .message
                            .as_deref()
                            .is_some_and(|message| !message.is_empty())
                    );
                    saw_fetch_failure = true;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn wait_for_internal_terminal(
    events: &mut tokio::sync::broadcast::Receiver<(String, EventEnvelope)>,
    uuid: &str,
) -> Result<()> {
    for _ in 0..20_000 {
        match events.try_recv() {
            Ok((topic, envelope))
                if topic == TOPIC_ENGINE_REFRESH && envelope.kind == EventKind::Refresh as i32 =>
            {
                let refresh = RefreshEvent::decode(envelope.payload.as_slice())?;
                if refresh.unified_uuid.as_deref() == Some(uuid)
                    && refresh.phase == RefreshPhase::Completed as i32
                    && refresh.outcome == RefreshOutcome::Success as i32
                {
                    assert_refresh_terminal(&refresh, uuid, RefreshOutcome::Success);
                    return Ok(());
                }
            }
            Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::task::yield_now().await;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                bail!("internal event channel closed before refresh terminal");
            }
        }
    }
    bail!("refresh terminal was not observed within bounded yields")
}

#[tokio::test]
async fn raw_default_once_reports_terminal_outcomes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let sockets = bind_engine_sockets("tcp://127.0.0.1:0", "tcp://127.0.0.1:0").await?;
    let rpc_endpoint = sockets.rpc_endpoint.clone();
    let pub_endpoint = sockets.pub_endpoint.clone();
    let config_path = directory.path().join("weather.toml");
    write_config_atomic(
        &config_path,
        &test_config(&directory, rpc_endpoint.clone(), pub_endpoint.clone()),
    )?;

    let provider = Arc::new(ScriptedWeatherProvider::default());
    let startup_gate = Arc::new(Semaphore::new(0));
    provider.queue(
        "A",
        WeatherStep::success(21.0).gated(Arc::clone(&startup_gate)),
    );
    provider.queue("A", WeatherStep::failure("station A offline"));
    provider.queue("B", WeatherStep::failure("station B offline"));

    let mut client = RawClient::connect(&rpc_endpoint, &pub_endpoint).await?;
    let runtime = EngineRuntime::start_with_provider(config_path, provider.clone()).await?;
    let engine = runtime.test_engine();
    let server_engine = engine.clone();
    let server = tokio::spawn(async move {
        let result = run_bound_engine_sockets(server_engine, sockets, "e2e".to_string()).await;
        drop(runtime);
        result
    });

    wait_until(|| provider.active_calls() == 1).await?;
    let ping = client.request(RpcKind::Ping, Empty {}).await?;
    assert_eq!(ping.status, ResponseStatus::Ok as i32);
    establish_subscription(&mut client, &engine, &rpc_endpoint, &pub_endpoint).await?;

    let default_id = client
        .send_request(
            RpcKind::GetWeather,
            GetWeatherRequest {
                unified_uuid: String::new(),
                refresh: false,
                include_debug: false,
            },
        )
        .await?;
    startup_gate.add_permits(1);
    let default = client.receive_response(&default_id).await?;
    assert_eq!(default.status, ResponseStatus::Ok as i32);
    let default_snapshot = WeatherSnapshot::decode(default.payload.as_slice())?;
    assert_eq!(
        default_snapshot
            .station
            .as_ref()
            .map(|station| station.name.as_str()),
        Some(STATION_A_NAME)
    );
    assert_eq!(
        default_snapshot
            .real
            .as_ref()
            .and_then(|real| real.temperature),
        Some(21.0)
    );
    assert_eq!(provider.total_calls(), 1);

    let station_a_uuid = unified_station_uuid(STATION_A_NAME);
    wait_for_terminal(&mut client, &station_a_uuid, RefreshOutcome::Success).await?;

    let stale = client
        .request(
            RpcKind::GetWeather,
            GetWeatherRequest {
                unified_uuid: station_a_uuid.clone(),
                refresh: true,
                include_debug: false,
            },
        )
        .await?;
    assert_eq!(stale.status, ResponseStatus::Ok as i32);
    let stale_snapshot = WeatherSnapshot::decode(stale.payload.as_slice())?;
    assert!(stale_snapshot.stale);
    assert_eq!(
        stale_snapshot
            .real
            .as_ref()
            .and_then(|real| real.temperature),
        Some(21.0)
    );
    assert_eq!(provider.total_calls(), 2);
    assert_eq!(provider.station_calls("A"), 2);
    wait_for_failure_events(&mut client, &station_a_uuid, RefreshOutcome::Stale).await?;

    let station_b_uuid = unified_station_uuid(STATION_B_NAME);
    let failed = client
        .request(
            RpcKind::GetWeather,
            GetWeatherRequest {
                unified_uuid: station_b_uuid.clone(),
                refresh: true,
                include_debug: false,
            },
        )
        .await?;
    assert_eq!(failed.status, ResponseStatus::Error as i32);
    assert_eq!(
        failed.error.as_ref().map(|error| error.code.as_str()),
        Some("WEATHER")
    );
    assert_eq!(provider.total_calls(), 3);
    assert_eq!(provider.station_calls("B"), 1);
    wait_for_failure_events(&mut client, &station_b_uuid, RefreshOutcome::Failure).await?;

    let shutdown = client
        .request(RpcKind::Shutdown, ShutdownRequest { owner_token: None })
        .await?;
    assert_eq!(shutdown.status, ResponseStatus::Accepted as i32);
    assert_eq!(
        tokio::time::timeout(WAIT_TIMEOUT, server).await???,
        EngineExit::Shutdown
    );
    client.close().await?;
    Ok(())
}

#[tokio::test]
async fn ttl_expiry_refetches_automatically() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let sockets = bind_engine_sockets("tcp://127.0.0.1:0", "tcp://127.0.0.1:0").await?;
    let rpc_endpoint = sockets.rpc_endpoint.clone();
    let pub_endpoint = sockets.pub_endpoint.clone();
    let config_path = directory.path().join("weather.toml");
    let mut config = test_config(&directory, rpc_endpoint.clone(), pub_endpoint.clone());
    config.updater.weather_ttl_seconds = 1;
    write_config_atomic(&config_path, &config)?;

    let provider = Arc::new(ScriptedWeatherProvider::default());
    provider.queue("A", WeatherStep::success(21.0));
    provider.queue("A", WeatherStep::success(22.0));

    let mut client = RawClient::connect(&rpc_endpoint, &pub_endpoint).await?;
    let runtime = EngineRuntime::start_with_provider(config_path, provider.clone()).await?;
    let engine = runtime.test_engine();
    let server = tokio::spawn(async move {
        let result = run_bound_engine_sockets(engine, sockets, "e2e".to_string()).await;
        drop(runtime);
        result
    });

    wait_until(|| provider.completed_calls() == 2).await?;
    let shutdown = client
        .request(RpcKind::Shutdown, ShutdownRequest { owner_token: None })
        .await?;
    assert_eq!(shutdown.status, ResponseStatus::Accepted as i32);
    assert_eq!(
        tokio::time::timeout(WAIT_TIMEOUT, server).await???,
        EngineExit::Shutdown
    );
    assert_eq!(provider.total_calls(), 2);
    assert_eq!(provider.station_calls("A"), 2);
    assert_eq!(provider.active_calls(), 0);
    client.close().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn local_date_rollover_refetches_before_ttl() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("weather.toml");
    let db_path = directory.path().join("weather.db");
    let mut config = test_config(
        &directory,
        "tcp://127.0.0.1:0".to_string(),
        "tcp://localhost:0".to_string(),
    );
    config.updater.weather_ttl_seconds = 86_400;
    write_config_atomic(&config_path, &config)?;

    let before_midnight = DateTime::parse_from_rfc3339("2026-06-23T15:59:30Z")?.timestamp_millis();
    let midnight = DateTime::parse_from_rfc3339("2026-06-23T16:00:00Z")?.timestamp_millis();
    let clock = Arc::new(ManualWeatherClock::new(before_midnight));
    let provider = Arc::new(ScriptedWeatherProvider::default());
    provider.queue("A", WeatherStep::success(21.0));
    provider.queue("A", WeatherStep::success(22.0));

    let runtime =
        EngineRuntime::start_with_provider_and_clock(config_path, provider.clone(), clock.clone())
            .await?;
    let engine = runtime.test_engine();
    let mut events = engine.sink.subscribe();
    let cancellation = Cancellation::new();
    let refresh_engine = engine.clone();
    let refresh_cancellation = cancellation.clone();
    let refresh =
        tokio::spawn(async move { run_refresh_loop(refresh_engine, refresh_cancellation).await });
    let station_a_uuid = unified_station_uuid(STATION_A_NAME);

    wait_for_internal_terminal(&mut events, &station_a_uuid).await?;
    assert_eq!(provider.completed_calls(), 1);

    clock.set(midnight);
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_internal_terminal(&mut events, &station_a_uuid).await?;
    assert_eq!(provider.completed_calls(), 2);
    assert_eq!(provider.active_calls(), 0);

    cancellation.cancel();
    refresh.await??;
    engine.db.shutdown().await?;
    drop(engine);
    drop(runtime);

    let connection = Connection::open(db_path)?;
    let distinct_dates: i64 = connection.query_row(
        "SELECT COUNT(DISTINCT date) FROM weather_snapshots_history WHERE unified_uuid = ?1",
        params![station_a_uuid],
        |row| row.get(0),
    )?;
    assert_eq!(distinct_dates, 2);
    Ok(())
}

#[tokio::test]
async fn repeated_restart_and_shutdown_release_resources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first_sockets = tokio::time::timeout(
        WAIT_TIMEOUT,
        bind_engine_sockets("tcp://127.0.0.1:0", "tcp://127.0.0.1:0"),
    )
    .await
    .context("initial socket bind timed out")??;
    let rpc_endpoint = first_sockets.rpc_endpoint.clone();
    let pub_endpoint = first_sockets.pub_endpoint.clone();
    let config_path = directory.path().join("weather.toml");
    write_config_atomic(
        &config_path,
        &test_config(&directory, rpc_endpoint.clone(), pub_endpoint.clone()),
    )?;

    let provider = Arc::new(ScriptedWeatherProvider::default());
    let mut initial_sockets = Some(first_sockets);
    let exits = [
        EngineExit::Restart,
        EngineExit::Restart,
        EngineExit::Shutdown,
    ];

    for (round, expected_exit) in exits.into_iter().enumerate() {
        let expected_calls = round + 1;
        provider.queue("A", WeatherStep::success(20.0 + expected_calls as f64));
        let sockets = match initial_sockets.take() {
            Some(sockets) => sockets,
            None => tokio::time::timeout(
                WAIT_TIMEOUT,
                bind_engine_sockets(&rpc_endpoint, &pub_endpoint),
            )
            .await
            .with_context(|| format!("socket rebind for round {expected_calls} timed out"))??,
        };
        assert_eq!(sockets.rpc_endpoint, rpc_endpoint);
        assert_eq!(sockets.pub_endpoint, pub_endpoint);

        let mut client = tokio::time::timeout(
            WAIT_TIMEOUT,
            RawClient::connect(&rpc_endpoint, &pub_endpoint),
        )
        .await
        .with_context(|| format!("raw client connect for round {expected_calls} timed out"))??;
        let runtime = tokio::time::timeout(
            WAIT_TIMEOUT,
            EngineRuntime::start_with_provider(config_path.clone(), provider.clone()),
        )
        .await
        .with_context(|| format!("runtime start for round {expected_calls} timed out"))??;
        let engine = runtime.test_engine();
        let server = tokio::spawn(async move {
            let result = run_bound_engine_sockets(engine, sockets, "e2e".to_string()).await;
            drop(runtime);
            result
        });

        wait_until(|| provider.completed_calls() == expected_calls).await?;
        let response = match expected_exit {
            EngineExit::Restart => client.request(RpcKind::RestartEngine, Empty {}).await?,
            EngineExit::Shutdown => {
                client
                    .request(RpcKind::Shutdown, ShutdownRequest { owner_token: None })
                    .await?
            }
        };
        assert_eq!(response.status, ResponseStatus::Accepted as i32);
        assert_eq!(
            tokio::time::timeout(WAIT_TIMEOUT, server)
                .await
                .with_context(|| format!("server exit for round {expected_calls} timed out"))???,
            expected_exit
        );
        client.close().await?;

        assert_eq!(provider.total_calls(), expected_calls);
        assert_eq!(provider.station_calls("A"), expected_calls);
        assert_eq!(provider.completed_calls(), expected_calls);
        assert_eq!(provider.active_calls(), 0);
        assert_eq!(provider.dropped_calls(), 0);
        assert_eq!(Arc::strong_count(&provider), 1);
    }

    let final_sockets = tokio::time::timeout(
        WAIT_TIMEOUT,
        bind_engine_sockets(&rpc_endpoint, &pub_endpoint),
    )
    .await
    .context("final socket rebind timed out")??;
    assert_eq!(final_sockets.rpc_endpoint, rpc_endpoint);
    assert_eq!(final_sockets.pub_endpoint, pub_endpoint);
    tokio::time::timeout(WAIT_TIMEOUT, close_socket(final_sockets.router, "ROUTER"))
        .await
        .context("final ROUTER close timed out")??;
    tokio::time::timeout(WAIT_TIMEOUT, close_socket(final_sockets.publisher, "PUB"))
        .await
        .context("final PUB close timed out")??;
    Ok(())
}
