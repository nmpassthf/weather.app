use anyhow::{Context, Result};
use weather_db::ProviderStation;
use weather_schema::*;
use weather_updater::WeatherFetch;

use crate::{
    handlers::RefreshTerminal, runtime::Engine, station::merge_station, time::date_for_tz,
};

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
        let station = match self.resolve_station(&req.unified_uuid).await {
            Ok(station) => station,
            Err(err) => {
                if req.refresh {
                    let uuid =
                        (!req.unified_uuid.trim().is_empty()).then_some(req.unified_uuid.as_str());
                    self.publish_refresh_terminal(
                        uuid,
                        RefreshTerminal::Failure(format!("{err:#}")),
                    );
                }
                return Err(err);
            }
        };
        let uuid = station.unified_uuid.clone();
        let include_debug = req.include_debug;
        if req.refresh {
            self.publish_refresh_started(Some(&uuid));
        }
        if !req.refresh
            && !include_debug
            && let Some(stored) = self.db.get_latest_snapshot(uuid.clone()).await?
        {
            let config = self.config.get();
            if cached_snapshot_is_fresh(
                stored.fetched_at_unix_ms,
                unix_timestamp_ms().unwrap_or_default(),
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
        if req.refresh {
            let outcome = match &result {
                Ok(snapshot) if snapshot.stale => RefreshTerminal::Stale,
                Ok(_) => RefreshTerminal::Success,
                Err(err) => RefreshTerminal::Failure(format!("{err:#}")),
            };
            self.publish_refresh_terminal(Some(&uuid), outcome);
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
        let fetched_at_unix_ms = unix_timestamp_ms().unwrap_or_default();
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
    use std::{path::Path, sync::Arc, time::Duration};

    use chrono::DateTime;
    use rusqlite::Connection;
    use tokio::time::timeout;
    use weather_configure::{AppConfig, StationConfig, write_config_atomic};
    use weather_updater::{
        ProviderCity, ProviderFuture, ProviderProvince, WeatherFetch, WeatherProvider,
    };

    use super::*;
    use crate::runtime::EngineRuntime;

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
