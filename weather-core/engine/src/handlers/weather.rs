use anyhow::{Context, Result};
use weather_db::ProviderStation;
use weather_schema::*;

use crate::{
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
        if let Ok(station) = self.resolve_station(&req.unified_uuid).await {
            self.publish_refresh(Some(&station.unified_uuid), true, false);
        }
        match self.get_weather_internal(req).await {
            Ok(snapshot) => {
                let unified_uuid = snapshot
                    .station
                    .as_ref()
                    .map(|s| s.unified_uuid.clone())
                    .unwrap_or_default();
                self.publish_refresh(Some(&unified_uuid), false, true);
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
        let station = self.resolve_station(&req.unified_uuid).await?;
        let uuid = station.unified_uuid.clone();
        let include_debug = req.include_debug;
        if !req.refresh
            && !include_debug
            && let Some(stored) = self.db.get_latest_snapshot(uuid.clone()).await?
        {
            let ttl_ms = self.config.get().updater.weather_ttl_seconds as i64 * 1000;
            if now_ms() - stored.fetched_at_unix_ms <= ttl_ms {
                return Ok(stored.snapshot);
            }
        }

        match self
            .updater
            .weather_with_debug(&station.provider_station_id, include_debug)
            .await
        {
            Ok(mut snapshot) => {
                snapshot.station = Some(merge_station(snapshot.station.take(), &station));
                if let Some(station_ref) = snapshot.station.as_mut()
                    && station_ref.unified_uuid.is_empty()
                {
                    station_ref.unified_uuid = uuid.clone();
                }
                let mut snapshot_for_storage = snapshot.clone();
                snapshot_for_storage.debug = None;
                let forecast_json = snapshot_for_storage
                    .predict
                    .as_ref()
                    .map(|p| serde_json::to_string(p).unwrap_or_default())
                    .unwrap_or_default();
                let alerts_json = snapshot_for_storage
                    .real
                    .as_ref()
                    .and_then(|r| r.alert.as_ref())
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default();
                let date = date_for_tz(now_ms(), &self.config.get().db.timezone)?;
                self.db
                    .put_history_snapshot(snapshot_for_storage, forecast_json, alerts_json, date)
                    .await?;
                self.db
                    .log_fetch(
                        Some(station.unified_uuid.clone()),
                        "rest/weather".to_string(),
                        true,
                        None,
                    )
                    .await?;
                self.publish_fetch_log(Some(&station.unified_uuid), "rest/weather", true, None);
                Ok(snapshot)
            }
            Err(err) => {
                self.db
                    .log_fetch(
                        Some(station.unified_uuid.clone()),
                        "rest/weather".to_string(),
                        false,
                        Some(err.to_string()),
                    )
                    .await
                    .ok();
                self.publish_fetch_log(
                    Some(&station.unified_uuid),
                    "rest/weather",
                    false,
                    Some(err.to_string()),
                );
                if let Some(mut stored) = self.db.get_latest_snapshot(uuid).await? {
                    stored.snapshot.stale = true;
                    Ok(stored.snapshot)
                } else {
                    Err(err)
                }
            }
        }
    }

    /// 按 `unified_uuid` 反查 StationRef。
    ///
    /// 先查 DB stations 表；miss 则按 uuid 反推 name 不现实（uuid 是单向哈希），
    /// 因此 miss 时从 config.stations 里找 unified_uuid 匹配项，再走 station_by_name 解析。
    async fn resolve_station(&self, unified_uuid: &str) -> Result<ProviderStation> {
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
            if station.unified_uuid.is_empty() {
                station.unified_uuid = weather_schema::unified_station_uuid(name);
            }
            return Ok(station);
        }
        let mut station = self.resolve_station_name_from_targeted_index(name).await?;
        if station.unified_uuid.is_empty() {
            station.unified_uuid = weather_schema::unified_station_uuid(name);
        }
        Ok(station)
    }
}
