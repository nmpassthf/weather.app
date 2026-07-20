use anyhow::{Context, Result};
use chrono::NaiveDate;
use weather_db::{ProviderStation, StoredSnapshot};
use weather_schema::*;
use weather_updater::WeatherFetch;

use super::temperature_history::{parse_full_date, parse_relative_date, parse_temperature};
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
        if !req.refresh && !include_debug {
            match self.db.get_latest_snapshot(uuid.clone()).await? {
                Some(stored) => {
                    let config = self.config.get();
                    if cached_snapshot_is_fresh(
                        stored.fetched_at_unix_ms,
                        self.weather_now_unix_ms(),
                        config.updater.weather_ttl_seconds,
                        &config.db.timezone,
                    )? {
                        log::debug!("weather cache hit station={uuid}");
                        let mut snapshot = stored.snapshot;
                        snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                        if let Err(error) = self
                            .apply_cached_municipality_air(&mut snapshot, &station)
                            .await
                        {
                            log::warn!(
                                "cached municipality air fallback failed station={uuid}: {error:#}"
                            );
                        }
                        if let Err(error) = self.apply_parent_alerts(&mut snapshot, &station).await
                        {
                            log::warn!(
                                "cached parent alert fallback failed station={uuid}: {error:#}"
                            );
                        }
                        return Ok(self.prepare_snapshot_resources(snapshot));
                    }
                    log::debug!("weather cache stale station={uuid}");
                }
                None => log::debug!("weather cache miss station={uuid}"),
            }
        } else {
            log::debug!(
                "weather cache bypassed station={} refresh={} include_debug={}",
                uuid,
                req.refresh,
                include_debug
            );
        }

        log::debug!("weather upstream fetch queued station={uuid}");
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
        result.map(|snapshot| self.prepare_snapshot_resources(snapshot))
    }

    async fn fetch_weather_uncached(
        &self,
        station: ProviderStation,
        include_debug: bool,
    ) -> Result<WeatherSnapshot> {
        let uuid = station.unified_uuid.clone();
        log::debug!(
            "weather upstream fetch started station={} provider={} provider_station={}",
            uuid,
            self.provider.provider_name(),
            station.provider_station_id
        );
        match self
            .provider
            .weather(&station.provider_station_id, include_debug)
            .await
        {
            Ok(WeatherFetch {
                mut snapshot,
                mut warnings,
            }) => {
                log::debug!("weather upstream fetch succeeded station={uuid}");
                snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                if let Err(err) = self.apply_cached_daily_high(&mut snapshot, &station).await {
                    warnings.push(format!("daily high fallback failed: {err:#}"));
                }
                let debug = snapshot.debug.take();
                let snapshot_for_storage = snapshot.clone();
                snapshot.debug = debug;

                if let Err(err) = self.persist_snapshot(snapshot_for_storage).await {
                    warnings.push(format!("cache write failed: {err:#}"));
                }
                if let Err(err) = self
                    .apply_cached_municipality_air(&mut snapshot, &station)
                    .await
                {
                    warnings.push(format!("municipality air fallback failed: {err:#}"));
                }
                let _ = self.apply_parent_alerts(&mut snapshot, &station).await;
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
                log::warn!("weather upstream fetch failed station={uuid}: {err:#}");
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
                        log::warn!(
                            "using stale weather cache after upstream failure station={}",
                            station.unified_uuid
                        );
                        stored.snapshot.station =
                            Some(merge_station(stored.snapshot.station.take(), &station));
                        stored.snapshot.stale = true;
                        let _ = self
                            .apply_cached_municipality_air(&mut stored.snapshot, &station)
                            .await;
                        let _ = self
                            .apply_parent_alerts(&mut stored.snapshot, &station)
                            .await;
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

    fn prepare_snapshot_resources(&self, mut snapshot: WeatherSnapshot) -> WeatherSnapshot {
        if let Some(radar) = snapshot.radar.as_mut() {
            if let Some(image_url) = radar.image_url.take() {
                radar.image_resource_id = self.resources.register(&image_url);
            }
            radar.page_url = None;
        }
        if let Some(real) = snapshot.real.as_mut() {
            for alert in &mut real.alerts {
                if let Some(icon_url) = alert.icon_url.take() {
                    alert.icon_resource_id = self.resources.register(&icon_url);
                }
                alert.url = None;
            }
        }
        snapshot
    }

    async fn apply_parent_alerts(
        &self,
        snapshot: &mut WeatherSnapshot,
        station: &ProviderStation,
    ) -> Result<bool> {
        let Some(parent_name) = parent_station_name(&station.name) else {
            return Ok(false);
        };
        let parent_uuid = unified_station_uuid(&parent_name);
        let Some(stored) = self.db.get_latest_snapshot(parent_uuid).await? else {
            return Ok(false);
        };
        let config = self.config.get();
        if !cached_snapshot_is_fresh(
            stored.fetched_at_unix_ms,
            self.weather_now_unix_ms(),
            config.updater.weather_ttl_seconds,
            &config.db.timezone,
        )? {
            return Ok(false);
        }
        let parent_alerts = stored
            .snapshot
            .real
            .map(|real| real.alerts)
            .unwrap_or_default();
        Ok(merge_parent_alerts(snapshot, parent_alerts))
    }

    async fn apply_cached_municipality_air(
        &self,
        snapshot: &mut WeatherSnapshot,
        station: &ProviderStation,
    ) -> Result<bool> {
        if snapshot.air.is_some() {
            return Ok(false);
        }
        let Some(parent_name) = municipality_air_parent_name(&station.province, &station.city)
        else {
            return Ok(false);
        };
        let parent_uuid = unified_station_uuid(&parent_name);
        let Some(stored) = self.db.get_latest_snapshot(parent_uuid).await? else {
            return Ok(false);
        };
        let config = self.config.get();
        if !cached_snapshot_is_fresh(
            stored.fetched_at_unix_ms,
            self.weather_now_unix_ms(),
            config.updater.weather_ttl_seconds,
            &config.db.timezone,
        )? {
            return Ok(false);
        }
        let Some(air) = stored.snapshot.air else {
            return Ok(false);
        };
        snapshot.air = Some(air);
        Ok(true)
    }

    async fn apply_cached_daily_high(
        &self,
        snapshot: &mut WeatherSnapshot,
        station: &ProviderStation,
    ) -> Result<bool> {
        let config = self.config.get();
        let today = date_for_tz(self.weather_now_unix_ms(), &config.db.timezone)?;
        let Some(stored) = self
            .db
            .get_latest_snapshot(station.unified_uuid.clone())
            .await?
        else {
            return Ok(false);
        };
        Ok(merge_cached_daily_high(snapshot, &stored, &today))
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

fn merge_cached_daily_high(
    snapshot: &mut WeatherSnapshot,
    stored: &StoredSnapshot,
    today: &str,
) -> bool {
    if stored.date != today {
        return false;
    }
    let Some(today) = parse_full_date(today) else {
        return false;
    };
    let Some(high) = snapshot_daily_high(snapshot, today)
        .or_else(|| snapshot_daily_high(&stored.snapshot, today))
    else {
        return false;
    };

    let mut changed = false;
    if let Some(index) = current_forecast_index(snapshot, today)
        && let Some(day) = snapshot
            .predict
            .as_mut()
            .and_then(|forecast| forecast.days.get_mut(index))
        && parse_temperature(day.day_temperature.as_deref())
            .filter(|value| value.is_finite())
            .is_none()
    {
        day.day_temperature = Some(temperature_text(high));
        changed = true;
    }
    if let Some(index) = current_temperature_chart_index(snapshot, today) {
        let chart = &mut snapshot.tempchart[index];
        if chart
            .max_temperature
            .filter(|value| value.is_finite())
            .is_none()
        {
            chart.max_temperature = Some(high);
            changed = true;
        }
    }
    changed
}

fn snapshot_daily_high(snapshot: &WeatherSnapshot, today: NaiveDate) -> Option<f64> {
    current_forecast_index(snapshot, today)
        .and_then(|index| snapshot.predict.as_ref()?.days.get(index))
        .and_then(|day| parse_temperature(day.day_temperature.as_deref()))
        .filter(|value| value.is_finite())
        .or_else(|| {
            current_temperature_chart_index(snapshot, today)
                .and_then(|index| snapshot.tempchart.get(index))
                .and_then(|chart| chart.max_temperature)
                .filter(|value| value.is_finite())
        })
}

fn current_forecast_index(snapshot: &WeatherSnapshot, today: NaiveDate) -> Option<usize> {
    snapshot
        .predict
        .as_ref()?
        .days
        .iter()
        .position(|day| parse_relative_date(&day.date, today) == Some(today))
}

fn current_temperature_chart_index(snapshot: &WeatherSnapshot, today: NaiveDate) -> Option<usize> {
    snapshot
        .tempchart
        .iter()
        .position(|chart| {
            chart
                .date
                .as_deref()
                .and_then(|value| parse_relative_date(value, today))
                == Some(today)
        })
        .or_else(|| {
            (snapshot.tempchart.len() == 1
                && snapshot.tempchart[0]
                    .date
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()))
            .then_some(0)
        })
}

fn temperature_text(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn first_enabled_station_name(config: &weather_configure::AppConfig) -> Option<&str> {
    config
        .stations
        .iter()
        .find(|station| station.enabled)
        .map(|station| station.name.as_str())
}

fn municipality_air_parent_name(province: &str, city: &str) -> Option<String> {
    if !province.ends_with('市') {
        return None;
    }
    let municipality = short_region_name(province);
    if municipality.is_empty() || city == municipality || city == province {
        return None;
    }
    Some(canonical_station_name(province, municipality))
}

fn merge_parent_alerts(snapshot: &mut WeatherSnapshot, parent_alerts: Vec<WeatherAlert>) -> bool {
    if parent_alerts.is_empty() {
        return false;
    }
    let alerts = &mut snapshot
        .real
        .get_or_insert_with(ObservedWeather::default)
        .alerts;
    let initial_len = alerts.len();
    for mut parent_alert in parent_alerts {
        parent_alert.inherited = true;
        if !alerts
            .iter()
            .any(|current| same_alert(current, &parent_alert))
        {
            alerts.push(parent_alert);
        }
    }
    alerts.len() != initial_len
}

fn same_alert(left: &WeatherAlert, right: &WeatherAlert) -> bool {
    left.alert == right.alert
        && left.province == right.province
        && left.city == right.city
        && left.issue_content == right.issue_content
        && left.signal_type == right.signal_type
        && left.signal_level == right.signal_level
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

    struct NightProvider;

    impl WeatherProvider for NightProvider {
        fn provider_name(&self) -> &str {
            "night-test"
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
            Box::pin(async {
                Ok(WeatherFetch {
                    snapshot: WeatherSnapshot {
                        predict: Some(ForecastReport {
                            days: vec![ForecastDay {
                                date: "06-24".to_string(),
                                night_temperature: Some("24".to_string()),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    warnings: Vec::new(),
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
    fn municipality_air_parent_only_applies_to_district_stations() {
        assert_eq!(
            municipality_air_parent_name("北京市", "朝阳").as_deref(),
            Some("北京-北京市")
        );
        assert_eq!(municipality_air_parent_name("北京市", "北京"), None);
        assert_eq!(municipality_air_parent_name("浙江省", "杭州"), None);
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

    #[test]
    fn cached_daily_high_fills_only_missing_fields_for_today() {
        let today = "2026-06-24";
        let stored = StoredSnapshot {
            date: today.to_string(),
            snapshot: WeatherSnapshot {
                tempchart: vec![TemperatureChart {
                    date: Some("06-24".to_string()),
                    max_temperature: Some(34.5),
                    ..Default::default()
                }],
                ..Default::default()
            },
            fetched_at_unix_ms: 1,
        };
        let mut snapshot = WeatherSnapshot {
            predict: Some(ForecastReport {
                days: vec![
                    ForecastDay {
                        date: "06-24".to_string(),
                        ..Default::default()
                    },
                    ForecastDay {
                        date: "06-25".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            tempchart: vec![
                TemperatureChart {
                    date: Some("2026-06-24".to_string()),
                    ..Default::default()
                },
                TemperatureChart {
                    date: Some("2026-06-25".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert!(merge_cached_daily_high(&mut snapshot, &stored, today));
        let forecast = snapshot.predict.unwrap();
        assert_eq!(forecast.days[0].day_temperature.as_deref(), Some("34.5"));
        assert_eq!(forecast.days[1].day_temperature, None);
        assert_eq!(snapshot.tempchart[0].max_temperature, Some(34.5));
        assert_eq!(snapshot.tempchart[1].max_temperature, None);
    }

    #[test]
    fn cached_daily_high_does_not_override_source_or_cross_dates() {
        let cached_snapshot = WeatherSnapshot {
            predict: Some(ForecastReport {
                days: vec![ForecastDay {
                    date: "06-23".to_string(),
                    day_temperature: Some("33".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let previous_day = StoredSnapshot {
            date: "2026-06-23".to_string(),
            snapshot: cached_snapshot.clone(),
            fetched_at_unix_ms: 1,
        };
        let mut snapshot = WeatherSnapshot {
            predict: Some(ForecastReport {
                days: vec![ForecastDay {
                    date: "06-24".to_string(),
                    day_temperature: Some("36".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(!merge_cached_daily_high(
            &mut snapshot,
            &previous_day,
            "2026-06-24"
        ));
        assert_eq!(
            snapshot.predict.as_ref().unwrap().days[0]
                .day_temperature
                .as_deref(),
            Some("36")
        );

        let same_day = StoredSnapshot {
            date: "2026-06-24".to_string(),
            snapshot: WeatherSnapshot {
                predict: Some(ForecastReport {
                    days: vec![ForecastDay {
                        date: "06-24".to_string(),
                        day_temperature: Some("34".to_string()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            fetched_at_unix_ms: 2,
        };
        assert!(!merge_cached_daily_high(
            &mut snapshot,
            &same_day,
            "2026-06-24"
        ));
        assert_eq!(
            snapshot.predict.unwrap().days[0].day_temperature.as_deref(),
            Some("36")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evening_fetch_restores_and_persists_same_day_database_high() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.updater.default_provider = "night-test".to_string();
        config.updater.provider[0].name = "night-test".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let now = 1_782_252_000_000;
        let runtime = EngineRuntime::start_with_provider_and_clock(
            config_path,
            Arc::new(NightProvider),
            Arc::new(FixedWeatherClock(now)),
        )
        .await
        .unwrap();
        let engine = runtime.test_engine();
        let station = warning_station();
        let mut station = ProviderStation {
            provider_name: "night-test".to_string(),
            ..station
        };
        station.unified_uuid = unified_station_uuid(&station.name);
        engine
            .db
            .put_history_snapshot(
                WeatherSnapshot {
                    station: Some(station.public_ref()),
                    predict: Some(ForecastReport {
                        days: vec![ForecastDay {
                            date: "2026-06-24".to_string(),
                            day_temperature: Some("35.5".to_string()),
                            night_temperature: Some("24".to_string()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now,
            )
            .await
            .unwrap();

        let snapshot = engine
            .fetch_weather_uncached(station.clone(), false)
            .await
            .unwrap();
        assert_eq!(
            snapshot.predict.as_ref().unwrap().days[0]
                .day_temperature
                .as_deref(),
            Some("35.5")
        );
        let stored = engine
            .db
            .get_latest_snapshot(station.unified_uuid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.date, "2026-06-24");
        assert_eq!(
            stored.snapshot.predict.unwrap().days[0]
                .day_temperature
                .as_deref(),
            Some("35.5")
        );
        engine.db.shutdown().await.unwrap();
    }

    #[test]
    fn parent_alerts_follow_current_alerts_and_are_deduplicated() {
        let current = WeatherAlert {
            alert: Some("朝阳区雷电预警".to_string()),
            city: Some("朝阳".to_string()),
            signal_level: Some("黄色".to_string()),
            ..Default::default()
        };
        let duplicate = WeatherAlert {
            alert: Some("北京市高温预警".to_string()),
            province: Some("北京市".to_string()),
            city: Some("北京".to_string()),
            signal_level: Some("橙色".to_string()),
            ..Default::default()
        };
        let parent_only = WeatherAlert {
            alert: Some("北京市暴雨预警".to_string()),
            province: Some("北京市".to_string()),
            city: Some("北京".to_string()),
            signal_level: Some("蓝色".to_string()),
            ..Default::default()
        };
        let mut snapshot = WeatherSnapshot {
            real: Some(ObservedWeather {
                alerts: vec![current.clone(), duplicate.clone()],
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(merge_parent_alerts(
            &mut snapshot,
            vec![duplicate, parent_only.clone()]
        ));

        let alerts = snapshot.real.unwrap().alerts;
        assert_eq!(alerts.len(), 3);
        assert_eq!(alerts[0], current);
        assert!(!alerts[0].inherited);
        assert!(!alerts[1].inherited);
        assert_eq!(alerts[2].alert, parent_only.alert);
        assert!(alerts[2].inherited);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outbound_snapshot_replaces_remote_urls_with_registered_resource_ids() {
        let (_directory, _runtime, engine, _provider, _uuid) = blocking_runtime().await;
        let snapshot = WeatherSnapshot {
            real: Some(ObservedWeather {
                alerts: vec![WeatherAlert {
                    url: Some("https://provider.example/alert.html".to_string()),
                    icon_url: Some("https://provider.example/alert.png".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            radar: Some(RadarInfo {
                image_url: Some("https://provider.example/radar.png".to_string()),
                page_url: Some("https://provider.example/radar.html".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let outbound = engine.prepare_snapshot_resources(snapshot);
        let alert = outbound.real.unwrap().alerts.remove(0);
        let radar = outbound.radar.unwrap();

        assert!(alert.url.is_none());
        assert!(alert.icon_url.is_none());
        assert_eq!(
            engine
                .resources
                .source_url(alert.icon_resource_id.as_deref().unwrap())
                .as_deref(),
            Some("https://provider.example/alert.png")
        );
        assert!(radar.image_url.is_none());
        assert!(radar.page_url.is_none());
        assert_eq!(
            engine
                .resources
                .source_url(radar.image_resource_id.as_deref().unwrap())
                .as_deref(),
            Some("https://provider.example/radar.png")
        );
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn district_snapshot_uses_only_fresh_municipality_air_cache() {
        let (_directory, _runtime, engine, _provider, _uuid) = blocking_runtime().await;
        let now = engine.weather_now_unix_ms();
        let config = engine.config.get();
        let parent_name = "北京-北京市".to_string();
        let parent_uuid = unified_station_uuid(&parent_name);
        let parent_snapshot = |aqi| WeatherSnapshot {
            station: Some(StationRef {
                province: "北京市".to_string(),
                city: "北京".to_string(),
                name: parent_name.clone(),
                unified_uuid: parent_uuid.clone(),
            }),
            air: Some(AirQuality {
                aqi: Some(aqi),
                ..Default::default()
            }),
            ..Default::default()
        };
        let child_name = "北京-北京市-朝阳".to_string();
        let child_station = ProviderStation {
            provider_name: "blocking-test".to_string(),
            display_name: child_name.clone(),
            provider_station_id: "CY".to_string(),
            provider_province_code: "ABJ".to_string(),
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            url: "test://chaoyang".to_string(),
            unified_uuid: unified_station_uuid(&child_name),
            name: child_name,
        };

        engine
            .db
            .put_history_snapshot(
                parent_snapshot(30.0),
                now - config.updater.weather_ttl_seconds as i64 * 1_000,
            )
            .await
            .unwrap();
        let mut child = WeatherSnapshot::default();
        assert!(
            !engine
                .apply_cached_municipality_air(&mut child, &child_station)
                .await
                .unwrap()
        );
        assert!(child.air.is_none());

        engine
            .db
            .put_history_snapshot(parent_snapshot(44.0), now - 1_000)
            .await
            .unwrap();
        assert!(
            engine
                .apply_cached_municipality_air(&mut child, &child_station)
                .await
                .unwrap()
        );
        assert_eq!(child.air.and_then(|air| air.aqi), Some(44.0));
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn district_inherits_fresh_parent_alerts_without_mutating_parent_cache() {
        let (_directory, _runtime, engine, _provider, _uuid) = blocking_runtime().await;
        let now = engine.weather_now_unix_ms();
        let parent_name = "北京-北京市".to_string();
        let parent_uuid = unified_station_uuid(&parent_name);
        let parent_station = ProviderStation {
            provider_name: "blocking-test".to_string(),
            display_name: parent_name.clone(),
            provider_station_id: "BJ".to_string(),
            provider_province_code: "ABJ".to_string(),
            province: "北京市".to_string(),
            city: "北京".to_string(),
            url: "test://beijing".to_string(),
            name: parent_name.clone(),
            unified_uuid: parent_uuid.clone(),
        };
        engine
            .db
            .put_provider_station_mapping(parent_station)
            .await
            .unwrap();
        engine
            .db
            .put_history_snapshot(
                WeatherSnapshot {
                    station: Some(StationRef {
                        province: "北京市".to_string(),
                        city: "北京".to_string(),
                        name: parent_name,
                        unified_uuid: parent_uuid.clone(),
                    }),
                    real: Some(ObservedWeather {
                        alerts: vec![WeatherAlert {
                            alert: Some("北京市暴雨蓝色预警".to_string()),
                            province: Some("北京市".to_string()),
                            city: Some("北京".to_string()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                now - 1_000,
            )
            .await
            .unwrap();
        let child_name = "北京-北京市-朝阳".to_string();
        let child_station = ProviderStation {
            provider_name: "blocking-test".to_string(),
            display_name: child_name.clone(),
            provider_station_id: "CY".to_string(),
            provider_province_code: "ABJ".to_string(),
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            url: "test://chaoyang".to_string(),
            unified_uuid: unified_station_uuid(&child_name),
            name: child_name,
        };
        let mut child = WeatherSnapshot {
            real: Some(ObservedWeather {
                alerts: vec![WeatherAlert {
                    alert: Some("朝阳区雷电黄色预警".to_string()),
                    city: Some("朝阳".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(
            engine
                .apply_parent_alerts(&mut child, &child_station)
                .await
                .unwrap()
        );
        let alerts = &child.real.as_ref().unwrap().alerts;
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].alert.as_deref(), Some("朝阳区雷电黄色预警"));
        assert!(!alerts[0].inherited);
        assert_eq!(alerts[1].alert.as_deref(), Some("北京市暴雨蓝色预警"));
        assert!(alerts[1].inherited);

        let stored_parent = engine
            .db
            .get_latest_snapshot(parent_uuid)
            .await
            .unwrap()
            .unwrap();
        assert!(!stored_parent.snapshot.real.unwrap().alerts[0].inherited);
        engine.db.shutdown().await.unwrap();
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
