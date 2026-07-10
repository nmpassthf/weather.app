use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Url;
use serde_json::Value;
use weather_configure::ProviderConfig;
use weather_utils::JsonHttpClient;

use super::dto::{CityDto, ProvinceDto};

const USER_AGENT: &str = "weather.app/0.1";
const PROVINCES_PATH: &str = "rest/province/all";
const WEATHER_PATH: &str = "rest/weather";

pub(super) struct JsonDocument {
    pub(super) endpoint: Url,
    pub(super) body: Value,
}

#[derive(Clone)]
pub(super) struct NmcTransport {
    http: JsonHttpClient,
    base_url: Url,
}

impl NmcTransport {
    pub(super) fn new(config: &ProviderConfig) -> Result<Self> {
        let http = JsonHttpClient::new(
            &config.base_url,
            Duration::from_secs(config.request_timeout_seconds),
            USER_AGENT,
        )
        .context("failed to build NMC HTTP client")?;
        let base_url = http
            .url_for("", &[])
            .context("failed to normalize NMC base URL")?;
        Ok(Self { http, base_url })
    }

    pub(super) fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub(super) async fn provinces(&self) -> Result<Vec<ProvinceDto>> {
        self.http.get_json(PROVINCES_PATH, &[]).await
    }

    pub(super) async fn cities(&self, provider_province_code: &str) -> Result<Vec<CityDto>> {
        self.http
            .get_json(&format!("rest/province/{provider_province_code}"), &[])
            .await
    }

    pub(super) async fn weather(&self, provider_station_id: &str) -> Result<JsonDocument> {
        let query = [("stationid", provider_station_id)];
        let endpoint = self.http.url_for(WEATHER_PATH, &query)?;
        let body = self.http.get_json(WEATHER_PATH, &query).await?;
        Ok(JsonDocument { endpoint, body })
    }
}
