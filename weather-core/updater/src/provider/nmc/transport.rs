use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Url;
use serde_json::Value;
use weather_configure::{NetworkConfig, ProviderConfig, ProviderNetworkConfig};
use weather_utils::{HttpNetworkConfig, JsonHttpClient, normalized_network_value};

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
    pub(super) fn new(config: &ProviderConfig, network: &NetworkConfig) -> Result<Self> {
        let network = effective_network(network, &config.network);
        let http = JsonHttpClient::new(
            &config.base_url,
            Duration::from_secs(config.request_timeout_seconds),
            USER_AGENT,
            &network,
        )
        .context("failed to build NMC HTTP client")?;
        let base_url = Url::parse(&config.base_url).context("failed to normalize NMC base URL")?;
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
        let (endpoint, body) = self
            .http
            .get_json_with_endpoint(WEATHER_PATH, &query)
            .await?;
        Ok(JsonDocument { endpoint, body })
    }
}

fn effective_network(
    global: &NetworkConfig,
    provider: &ProviderNetworkConfig,
) -> HttpNetworkConfig {
    resolve_network(HttpNetworkConfig::from_environment(), global, provider)
}

fn resolve_network(
    mut effective: HttpNetworkConfig,
    global: &NetworkConfig,
    provider: &ProviderNetworkConfig,
) -> HttpNetworkConfig {
    overlay_value(&mut effective.http_proxy, global.http_proxy.as_deref());
    overlay_value(&mut effective.https_proxy, global.https_proxy.as_deref());
    overlay_value(&mut effective.no_proxy, global.no_proxy.as_deref());
    overlay_value(&mut effective.all_proxy, global.all_proxy.as_deref());
    effective.allow_insecure = global.allow_insecure;

    overlay_value(&mut effective.http_proxy, provider.http_proxy.as_deref());
    overlay_value(&mut effective.https_proxy, provider.https_proxy.as_deref());
    overlay_value(&mut effective.no_proxy, provider.no_proxy.as_deref());
    overlay_value(&mut effective.all_proxy, provider.all_proxy.as_deref());
    if let Some(allow_insecure) = provider.allow_insecure {
        effective.allow_insecure = allow_insecure;
    }
    effective
}

fn overlay_value(target: &mut Option<String>, value: Option<&str>) {
    if let Some(value) = value {
        *target = normalized_network_value(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_network_overrides_global_and_environment_by_field() {
        let environment = HttpNetworkConfig {
            http_proxy: Some("http://environment-http:8000".to_string()),
            https_proxy: Some("http://environment-https:8001".to_string()),
            no_proxy: Some("environment.internal".to_string()),
            all_proxy: Some("http://environment-all:8002".to_string()),
            allow_insecure: false,
        };
        let global = NetworkConfig {
            http_proxy: Some(" http://global-http:9000 ".to_string()),
            no_proxy: Some(String::new()),
            allow_insecure: true,
            ..Default::default()
        };
        let provider = ProviderNetworkConfig {
            https_proxy: Some("http://provider-https:9100".to_string()),
            all_proxy: Some(String::new()),
            allow_insecure: Some(false),
            ..Default::default()
        };

        let effective = resolve_network(environment, &global, &provider);

        assert_eq!(
            effective.http_proxy.as_deref(),
            Some("http://global-http:9000")
        );
        assert_eq!(
            effective.https_proxy.as_deref(),
            Some("http://provider-https:9100")
        );
        assert_eq!(effective.no_proxy, None);
        assert_eq!(effective.all_proxy, None);
        assert!(!effective.allow_insecure);
    }
}
