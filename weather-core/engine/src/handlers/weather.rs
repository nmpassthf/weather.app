use anyhow::{Context, Result};
use weather_db::ProviderStation;
use weather_schema::*;
use weather_updater::WeatherFetch;

use crate::{
    handlers::RefreshTerminal, runtime::Engine, station::merge_station, time::date_for_tz,
};

struct RefreshCompletionGuard {
    engine: Engine,
    unified_uuid: Option<String>,
    armed: bool,
}

impl RefreshCompletionGuard {
    fn start(engine: &Engine, requested_uuid: &str) -> Self {
        let unified_uuid = (!requested_uuid.trim().is_empty()).then(|| requested_uuid.to_string());
        engine.publish_refresh_started(unified_uuid.as_deref());
        Self {
            engine: engine.clone(),
            unified_uuid,
            armed: true,
        }
    }

    fn set_unified_uuid(&mut self, unified_uuid: String) {
        self.unified_uuid = Some(unified_uuid);
    }

    fn complete(mut self, outcome: RefreshTerminal) {
        self.armed = false;
        self.engine
            .publish_refresh_terminal(self.unified_uuid.as_deref(), outcome);
    }
}

impl Drop for RefreshCompletionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            self.engine.publish_refresh_terminal(
                self.unified_uuid.as_deref(),
                RefreshTerminal::Failure("refresh cancelled before completion".to_string()),
            );
        }
    }
}

impl Engine {
    pub(super) async fn handle_get_weather(&self, request: &RpcRequest) -> RpcResponse {
        self.handle_weather_request(request, false).await
    }

    pub(super) async fn handle_weather_request(
        &self,
        request: &RpcRequest,
        force_refresh: bool,
    ) -> RpcResponse {
        let decoded = decode_message::<GetWeatherRequest>(&request.payload);
        let Ok(mut req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                decoded.unwrap_err().to_string(),
            );
        };
        if force_refresh {
            req.refresh = true;
        }
        match self.get_weather_internal(req).await {
            Ok(snapshot) => {
                self.publish_snapshot(&snapshot);
                self.ok(&request.request_id, snapshot)
            }
            Err(err) => Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::Weather,
                err.to_string(),
            ),
        }
    }

    pub(crate) async fn get_weather_internal(
        &self,
        req: GetWeatherRequest,
    ) -> Result<WeatherSnapshot> {
        let mut completion = req
            .refresh
            .then(|| RefreshCompletionGuard::start(self, &req.unified_uuid));
        let station = match self.resolve_station(&req.unified_uuid).await {
            Ok(station) => station,
            Err(err) => {
                if let Some(completion) = completion.take() {
                    completion.complete(RefreshTerminal::Failure(format!("{err:#}")));
                }
                return Err(err);
            }
        };
        let uuid = station.unified_uuid.clone();
        if let Some(completion) = completion.as_mut() {
            completion.set_unified_uuid(uuid.clone());
        }
        let include_debug = req.include_debug;
        if !req.refresh
            && !include_debug
            && let Some(stored) = self.db.get_latest_snapshot(uuid.clone()).await?
        {
            let config = self.config.get();
            if cached_snapshot_is_fresh(
                stored.fetched_at_unix_ms,
                self.weather_now_unix_ms(),
                config.updater.weather_ttl_seconds,
                &config.db.timezone,
            )? {
                let mut snapshot = stored.snapshot;
                snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                return Ok(snapshot);
            }
        }

        let flight_key = (uuid.clone(), include_debug);
        let singleflight = self.weather_singleflight.clone();
        let result = singleflight
            .run(flight_key, || async move {
                self.fetch_weather_uncached(station, include_debug).await
            })
            .await
            .map(|snapshot| snapshot.as_ref().clone());
        if let Some(completion) = completion.take() {
            let outcome = match &result {
                Ok(snapshot) if snapshot.stale => RefreshTerminal::Stale,
                Ok(_) => RefreshTerminal::Success,
                Err(err) => RefreshTerminal::Failure(format!("{err:#}")),
            };
            completion.complete(outcome);
        }
        result
    }

    async fn fetch_weather_uncached(
        &self,
        station: ProviderStation,
        include_debug: bool,
    ) -> Result<WeatherSnapshot> {
        let uuid = station.unified_uuid.clone();
        match self
            .provider
            .weather(&station.provider_station_id, include_debug)
            .await
        {
            Ok(WeatherFetch {
                mut snapshot,
                mut warnings,
            }) => {
                snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                let debug = snapshot.debug.take();
                let snapshot_for_storage = snapshot.clone();
                snapshot.debug = debug;

                if let Err(err) = self.persist_snapshot(snapshot_for_storage).await {
                    warnings.push(format!("cache write failed: {err:#}"));
                }
                let persisted_warning = warning_summary(&warnings);
                if let Err(err) = self
                    .db
                    .log_fetch(
                        Some(station.unified_uuid.clone()),
                        "rest/weather".to_string(),
                        true,
                        persisted_warning,
                    )
                    .await
                {
                    warnings.push(format!("fetch log write failed: {err:#}"));
                }
                if let Some(debug) = snapshot.debug.as_mut() {
                    debug.warnings.clone_from(&warnings);
                }
                let warning = warning_summary(&warnings);
                self.publish_fetch_log(Some(&station.unified_uuid), "rest/weather", true, warning);
                Ok(snapshot)
            }
            Err(err) => {
                let fetch_error = format!("{err:#}");
                let log_error = self
                    .db
                    .log_fetch(
                        Some(station.unified_uuid.clone()),
                        "rest/weather".to_string(),
                        false,
                        Some(fetch_error.clone()),
                    )
                    .await
                    .err();
                let event_message = match log_error {
                    Some(log_error) => {
                        format!("{fetch_error}; fetch log write failed: {log_error:#}")
                    }
                    None => fetch_error,
                };
                self.publish_fetch_log(
                    Some(&station.unified_uuid),
                    "rest/weather",
                    false,
                    Some(event_message),
                );
                match self.db.get_latest_snapshot(uuid).await {
                    Ok(Some(mut stored)) => {
                        stored.snapshot.station =
                            Some(merge_station(stored.snapshot.station.take(), &station));
                        stored.snapshot.stale = true;
                        Ok(stored.snapshot)
                    }
                    Ok(None) => Err(err),
                    Err(cache_err) => {
                        Err(err.context(format!("stale cache lookup failed: {cache_err:#}")))
                    }
                }
            }
        }
    }

    async fn persist_snapshot(&self, snapshot: WeatherSnapshot) -> Result<()> {
        let fetched_at_unix_ms = self.weather_now_unix_ms();
        self.db
            .put_history_snapshot(snapshot, fetched_at_unix_ms)
            .await
    }

    /// 按 `unified_uuid` 反查 StationRef。
    ///
    /// 先查 DB stations 表；miss 则按 uuid 反推 name 不现实（uuid 是单向哈希），
    /// 因此 miss 时从 config.stations 里找 unified_uuid 匹配项，再走 station_by_name 解析。
    async fn resolve_station(&self, unified_uuid: &str) -> Result<ProviderStation> {
        if unified_uuid.trim().is_empty() {
            let config = self.config.get();
            let name = first_enabled_station_name(&config)
                .context("no enabled station is configured for the default weather request")?
                .to_string();
            return self.station_by_name(&name).await;
        }
        if let Some(station) = self
            .db
            .get_provider_station_by_uuid(
                self.provider.provider_name().to_string(),
                unified_uuid.to_string(),
            )
            .await?
        {
            return Ok(station);
        }
        let config = self.config.get();
        let matched = config
            .stations
            .iter()
            .find(|s| weather_schema::unified_station_uuid(&s.name) == unified_uuid)
            .context("station not found for unified_uuid; call FUZZY_MATCH_STATIONS first to populate DB")?;
        self.station_by_name(&matched.name).await
    }

    pub(crate) async fn station_by_name(&self, name: &str) -> Result<ProviderStation> {
        if let Some(mut station) = self
            .db
            .get_provider_station_by_name(
                self.provider.provider_name().to_string(),
                name.to_string(),
            )
            .await?
        {
            station.name = name.to_string();
            station.unified_uuid = weather_schema::unified_station_uuid(name);
            return Ok(station);
        }
        let mut station = self.resolve_station_name_from_targeted_index(name).await?;
        station.name = name.to_string();
        station.unified_uuid = weather_schema::unified_station_uuid(name);
        Ok(station)
    }
}

fn warning_summary(warnings: &[String]) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

fn first_enabled_station_name(config: &weather_configure::AppConfig) -> Option<&str> {
    config
        .stations
        .iter()
        .find(|station| station.enabled)
        .map(|station| station.name.as_str())
}

fn cached_snapshot_is_fresh(
    fetched_at_unix_ms: i64,
    now_unix_ms: i64,
    ttl_seconds: u64,
    timezone: &str,
) -> Result<bool> {
    let ttl_ms = ttl_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
    let age_ms = now_unix_ms.saturating_sub(fetched_at_unix_ms).max(0);
    if age_ms >= ttl_ms {
        return Ok(false);
    }
    Ok(date_for_tz(fetched_at_unix_ms, timezone)? == date_for_tz(now_unix_ms, timezone)?)
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use chrono::DateTime;
    use rusqlite::Connection;
    use tokio::{sync::Semaphore, time::timeout};
    use weather_configure::{AppConfig, StationConfig, write_config_atomic};
    use weather_updater::{
        ProviderCity, ProviderFuture, ProviderProvince, WeatherFetch, WeatherProvider,
    };

    use super::*;
    use crate::{runtime::EngineRuntime, time::WeatherClock};

    struct FixedWeatherClock(i64);

    impl WeatherClock for FixedWeatherClock {
        fn now_unix_ms(&self) -> i64 {
            self.0
        }
    }

    struct ActiveWeatherFuture(Arc<AtomicUsize>);

    impl Drop for ActiveWeatherFuture {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct BlockingProvider {
        active: Arc<AtomicUsize>,
        started: Arc<Semaphore>,
    }

    impl BlockingProvider {
        fn new() -> Self {
            Self {
                active: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Semaphore::new(0)),
            }
        }
    }

    impl WeatherProvider for BlockingProvider {
        fn provider_name(&self) -> &str {
            "blocking-test"
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn cities<'a>(
            &'a self,
            _provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<ProviderCity>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn weather<'a>(
            &'a self,
            _provider_station_id: &'a str,
            _include_debug: bool,
        ) -> ProviderFuture<'a, WeatherFetch> {
            let active = Arc::clone(&self.active);
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                let _active = ActiveWeatherFuture(active);
                started.add_permits(1);
                pending::<Result<WeatherFetch>>().await
            })
        }
    }

    async fn blocking_runtime() -> (
        tempfile::TempDir,
        EngineRuntime,
        Engine,
        Arc<BlockingProvider>,
        String,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.updater.default_provider = "blocking-test".to_string();
        config.updater.provider[0].name = "blocking-test".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let provider = Arc::new(BlockingProvider::new());
        let runtime = EngineRuntime::start_with_provider_and_clock(
            config_path,
            provider.clone(),
            Arc::new(FixedWeatherClock(1_782_252_000_000)),
        )
        .await
        .unwrap();
        let engine = runtime.test_engine();
        let name = config.stations[0].name.clone();
        let uuid = unified_station_uuid(&name);
        engine
            .db
            .put_provider_station_mapping(ProviderStation {
                provider_name: "blocking-test".to_string(),
                display_name: name.clone(),
                provider_station_id: "S1".to_string(),
                provider_province_code: "P1".to_string(),
                province: "北京市".to_string(),
                city: "北京".to_string(),
                url: "test://weather".to_string(),
                unified_uuid: uuid.clone(),
                name,
            })
            .await
            .unwrap();
        (directory, runtime, engine, provider, uuid)
    }

    async fn recv_refresh_event(
        events: &mut tokio::sync::broadcast::Receiver<(String, EventEnvelope)>,
    ) -> RefreshEvent {
        let (topic, envelope) = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("refresh event was not published")
            .unwrap();
        assert_eq!(topic, TOPIC_ENGINE_REFRESH);
        assert_eq!(envelope.kind, EventKind::Refresh as i32);
        decode_message(&envelope.payload).unwrap()
    }

    async fn wait_for_provider_start(provider: &BlockingProvider) {
        timeout(Duration::from_secs(5), provider.started.acquire())
            .await
            .expect("weather provider was not called")
            .unwrap()
            .forget();
    }

    struct WarningProvider;

    impl WeatherProvider for WarningProvider {
        fn provider_name(&self) -> &str {
            "warning-test"
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn cities<'a>(
            &'a self,
            _provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<ProviderCity>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn weather<'a>(
            &'a self,
            _provider_station_id: &'a str,
            include_debug: bool,
        ) -> ProviderFuture<'a, WeatherFetch> {
            Box::pin(async move {
                let warnings = vec!["predict: malformed object; ignored".to_string()];
                Ok(WeatherFetch {
                    snapshot: WeatherSnapshot {
                        real: Some(ObservedWeather {
                            temperature: Some(23.5),
                            ..Default::default()
                        }),
                        debug: include_debug.then(|| DebugPayload {
                            provider: "warning-test".to_string(),
                            operation: "weather".to_string(),
                            endpoint: "test://weather".to_string(),
                            warnings: warnings.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    warnings,
                })
            })
        }
    }

    fn warning_station() -> ProviderStation {
        let name = "北京-北京市-朝阳".to_string();
        ProviderStation {
            provider_name: "warning-test".to_string(),
            display_name: name.clone(),
            provider_station_id: "S1".to_string(),
            provider_province_code: "P1".to_string(),
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            url: "https://example.invalid/station".to_string(),
            unified_uuid: weather_schema::unified_station_uuid(&name),
            name,
        }
    }

    fn latest_fetch_message(path: &Path) -> (bool, Option<String>) {
        Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT ok, message FROM upstream_fetch_log ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn default_weather_request_uses_first_enabled_station() {
        let config = AppConfig {
            stations: vec![
                StationConfig {
                    name: "disabled".to_string(),
                    enabled: false,
                },
                StationConfig {
                    name: "first".to_string(),
                    enabled: true,
                },
                StationConfig {
                    name: "second".to_string(),
                    enabled: true,
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            first_enabled_station_name(&config),
            Some(config.stations[1].name.as_str())
        );
    }

    #[test]
    fn default_weather_request_requires_an_enabled_station() {
        let mut config = AppConfig::default();
        for station in &mut config.stations {
            station.enabled = false;
        }

        assert_eq!(first_enabled_station_name(&config), None);
    }

    #[test]
    fn cached_snapshot_requires_age_strictly_below_ttl() {
        let fetched = 1_000_000;

        assert!(cached_snapshot_is_fresh(fetched, fetched + 59_999, 60, "UTC").unwrap());
        assert!(!cached_snapshot_is_fresh(fetched, fetched + 60_000, 60, "UTC").unwrap());
        assert!(!cached_snapshot_is_fresh(fetched, fetched, 0, "UTC").unwrap());
    }

    #[test]
    fn cached_snapshot_must_be_from_same_local_date() {
        let fetched = DateTime::parse_from_rfc3339("2026-06-23T23:59:50+08:00")
            .unwrap()
            .timestamp_millis();
        let now = DateTime::parse_from_rfc3339("2026-06-24T00:00:10+08:00")
            .unwrap()
            .timestamp_millis();

        assert!(!cached_snapshot_is_fresh(fetched, now, 60, "Asia/Shanghai").unwrap());
        assert!(cached_snapshot_is_fresh(now - 1_000, now, 60, "Asia/Shanghai").unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_refresh_emits_one_failure_and_drops_the_active_provider_future() {
        let (_directory, _runtime, engine, provider, uuid) = blocking_runtime().await;
        let mut events = engine.sink.subscribe();
        let request_engine = engine.clone();
        let expected_uuid = uuid.clone();
        let task = tokio::spawn(async move {
            request_engine
                .get_weather_internal(GetWeatherRequest {
                    unified_uuid: uuid,
                    refresh: true,
                    include_debug: false,
                })
                .await
        });
        wait_for_provider_start(&provider).await;

        let started = recv_refresh_event(&mut events).await;
        assert_eq!(
            started.unified_uuid.as_deref(),
            Some(expected_uuid.as_str())
        );
        assert!(started.started);
        assert!(!started.completed);
        assert_eq!(
            RefreshPhase::try_from(started.phase).unwrap(),
            RefreshPhase::Started
        );
        assert_eq!(
            RefreshOutcome::try_from(started.outcome).unwrap(),
            RefreshOutcome::Unspecified
        );
        assert_eq!(started.message, None);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let terminal = recv_refresh_event(&mut events).await;

        assert_eq!(provider.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            terminal.unified_uuid.as_deref(),
            Some(expected_uuid.as_str())
        );
        assert!(!terminal.started);
        assert!(terminal.completed);
        assert_eq!(
            RefreshPhase::try_from(terminal.phase).unwrap(),
            RefreshPhase::Completed
        );
        assert_eq!(
            RefreshOutcome::try_from(terminal.outcome).unwrap(),
            RefreshOutcome::Failure
        );
        assert_eq!(
            terminal.message.as_deref(),
            Some("failure: refresh cancelled before completion")
        );
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_refresh_emits_one_failure_and_drops_the_active_provider_future() {
        let (_directory, _runtime, engine, provider, uuid) = blocking_runtime().await;
        let mut events = engine.sink.subscribe();
        let request_engine = engine.clone();
        let expected_uuid = uuid.clone();
        let task = tokio::spawn(async move {
            timeout(
                Duration::from_millis(100),
                request_engine.get_weather_internal(GetWeatherRequest {
                    unified_uuid: uuid,
                    refresh: true,
                    include_debug: false,
                }),
            )
            .await
        });
        wait_for_provider_start(&provider).await;

        let started = recv_refresh_event(&mut events).await;
        assert_eq!(
            started.unified_uuid.as_deref(),
            Some(expected_uuid.as_str())
        );
        assert!(started.started);
        assert!(!started.completed);
        assert_eq!(
            RefreshPhase::try_from(started.phase).unwrap(),
            RefreshPhase::Started
        );
        assert_eq!(
            RefreshOutcome::try_from(started.outcome).unwrap(),
            RefreshOutcome::Unspecified
        );
        assert_eq!(started.message, None);
        let result = timeout(Duration::from_secs(5), task)
            .await
            .expect("refresh request did not reach its timeout")
            .unwrap();
        assert!(result.is_err());
        let terminal = recv_refresh_event(&mut events).await;

        assert_eq!(provider.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            terminal.unified_uuid.as_deref(),
            Some(expected_uuid.as_str())
        );
        assert!(!terminal.started);
        assert!(terminal.completed);
        assert_eq!(
            RefreshPhase::try_from(terminal.phase).unwrap(),
            RefreshPhase::Completed
        );
        assert_eq!(
            RefreshOutcome::try_from(terminal.outcome).unwrap(),
            RefreshOutcome::Failure
        );
        assert_eq!(
            terminal.message.as_deref(),
            Some("failure: refresh cancelled before completion")
        );
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_and_cache_warnings_reach_database_debug_and_events() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.updater.default_provider = "warning-test".to_string();
        config.updater.provider[0].name = "warning-test".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let provider: Arc<dyn WeatherProvider> = Arc::new(WarningProvider);
        let runtime = EngineRuntime::start_with_provider(config_path, provider)
            .await
            .unwrap();
        let engine = runtime.test_engine();
        let mut events = engine.sink.subscribe();

        let snapshot = engine
            .fetch_weather_uncached(warning_station(), false)
            .await
            .unwrap();
        assert_eq!(snapshot.real.unwrap().temperature, Some(23.5));
        assert!(snapshot.debug.is_none());
        let (_, envelope) = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("fetch event timeout")
            .expect("fetch event channel closed");
        let event: FetchLogEvent = decode_message(&envelope.payload).unwrap();
        assert!(event.ok);
        assert_eq!(
            event.message.as_deref(),
            Some("predict: malformed object; ignored")
        );
        assert_eq!(
            latest_fetch_message(&db_path),
            (true, Some("predict: malformed object; ignored".to_string()))
        );

        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER fail_weather_cache_write
                   BEFORE INSERT ON weather_snapshots_history
                   BEGIN SELECT RAISE(FAIL, 'weather cache blocked'); END;"#,
            )
            .unwrap();
        let snapshot = engine
            .fetch_weather_uncached(warning_station(), true)
            .await
            .unwrap();
        let warnings = snapshot.debug.unwrap().warnings;
        assert_eq!(warnings[0], "predict: malformed object; ignored");
        assert!(warnings[1].contains("cache write failed"), "{warnings:?}");
        let (_, envelope) = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("fetch event timeout")
            .expect("fetch event channel closed");
        let event: FetchLogEvent = decode_message(&envelope.payload).unwrap();
        let message = event.message.unwrap();
        assert!(message.contains("predict: malformed object; ignored"));
        assert!(message.contains("cache write failed"));

        engine.db.shutdown().await.unwrap();
        let (ok, message) = latest_fetch_message(&db_path);
        assert!(ok);
        let message = message.unwrap();
        assert!(message.contains("predict: malformed object; ignored"));
        assert!(message.contains("cache write failed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_log_write_failures_only_extend_event_and_debug_warnings() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.updater.default_provider = "warning-test".to_string();
        config.updater.provider[0].name = "warning-test".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let provider: Arc<dyn WeatherProvider> = Arc::new(WarningProvider);
        let runtime = EngineRuntime::start_with_provider(config_path, provider)
            .await
            .unwrap();
        let engine = runtime.test_engine();
        let mut events = engine.sink.subscribe();
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER fail_weather_fetch_log
                   BEFORE INSERT ON upstream_fetch_log
                   BEGIN SELECT RAISE(FAIL, 'fetch log blocked'); END;"#,
            )
            .unwrap();

        let snapshot = engine
            .fetch_weather_uncached(warning_station(), true)
            .await
            .unwrap();
        let warnings = snapshot.debug.unwrap().warnings;
        assert_eq!(warnings[0], "predict: malformed object; ignored");
        assert!(
            warnings[1].contains("fetch log write failed"),
            "{warnings:?}"
        );
        let (_, envelope) = timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("fetch event timeout")
            .expect("fetch event channel closed");
        let event: FetchLogEvent = decode_message(&envelope.payload).unwrap();
        assert!(event.ok);
        let message = event.message.unwrap();
        assert!(message.contains("predict: malformed object; ignored"));
        assert!(message.contains("fetch log write failed"));
        let fetch_count: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM upstream_fetch_log", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fetch_count, 0);

        engine.db.shutdown().await.unwrap();
    }
}
