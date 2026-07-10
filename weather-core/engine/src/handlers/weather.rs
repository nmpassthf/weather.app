use anyhow::{Context, Result};
use weather_db::ProviderStation;
use weather_schema::*;

use crate::{
    handlers::RefreshTerminal,
    runtime::Engine,
    station::merge_station,
    time::{date_for_tz, now_ms},
};

impl Engine {
    pub(super) async fn handle_get_weather(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<GetWeatherRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        match self.get_weather_internal(req).await {
            Ok(snapshot) => {
                self.publish_snapshot(&snapshot);
                self.ok(&request.request_id, snapshot)
            }
            Err(err) => Self::rpc_error_response(&request.request_id, "WEATHER", err.to_string()),
        }
    }

    pub(super) async fn handle_trigger_refresh(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<GetWeatherRequest>(&request.payload);
        let Ok(mut req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        req.refresh = true;
        match self.get_weather_internal(req).await {
            Ok(snapshot) => {
                self.publish_snapshot(&snapshot);
                self.ok(&request.request_id, snapshot)
            }
            Err(err) => Self::rpc_error_response(&request.request_id, "WEATHER", err.to_string()),
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
                now_ms(),
                config.updater.weather_ttl_seconds,
                &config.db.timezone,
            )? {
                let mut snapshot = stored.snapshot;
                snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                return Ok(snapshot);
            }
        }

        let flight_key = format!("{uuid}\0{include_debug}");
        let singleflight = self.weather_singleflight.clone();
        let result = singleflight
            .run(flight_key, || async move {
                self.fetch_weather_uncached(station, include_debug).await
            })
            .await;
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
            .updater
            .weather_with_debug(&station.provider_station_id, include_debug)
            .await
        {
            Ok(mut snapshot) => {
                snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                let debug = snapshot.debug.take();
                let snapshot_for_storage = snapshot.clone();
                snapshot.debug = debug;

                let mut warnings = Vec::new();
                if let Err(err) = self.persist_snapshot(snapshot_for_storage).await {
                    warnings.push(format!("cache write failed: {err:#}"));
                }
                if let Err(err) = self
                    .db
                    .log_fetch(
                        Some(station.unified_uuid.clone()),
                        "rest/weather".to_string(),
                        true,
                        None,
                    )
                    .await
                {
                    warnings.push(format!("fetch log write failed: {err:#}"));
                }
                let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
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
        let forecast_json = snapshot
            .predict
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize forecast cache")?
            .unwrap_or_default();
        let alerts_json = snapshot
            .real
            .as_ref()
            .and_then(|real| real.alert.as_ref())
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize alert cache")?
            .unwrap_or_default();
        let date = date_for_tz(now_ms(), &self.config.get().db.timezone)?;
        self.db
            .put_history_snapshot(snapshot, forecast_json, alerts_json, date)
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
                self.updater.provider_name().to_string(),
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
                self.updater.provider_name().to_string(),
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
    use chrono::DateTime;
    use weather_configure::{AppConfig, StationConfig};

    use super::*;

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
}
