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
const MAX_RESOURCE_BYTES: usize = 8 * 1024 * 1024;

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

    pub(super) async fn resource(&self, source_url: &str) -> Result<(String, Vec<u8>)> {
        let url = Url::parse(source_url).context("invalid NMC resource URL")?;
        if url.scheme() != self.base_url.scheme()
            || url.host_str() != self.base_url.host_str()
            || url.port_or_known_default() != self.base_url.port_or_known_default()
        {
            anyhow::bail!("NMC resource URL is outside the configured provider origin");
        }
        let (content_type, bytes) = self.http.get_binary_url(&url, MAX_RESOURCE_BYTES).await?;
        if !matches!(
            content_type.as_str(),
            "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif"
        ) {
            anyhow::bail!("unsupported NMC resource content type `{content_type}`");
        }
        Ok((content_type, bytes))
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn transport(base_url: String) -> NmcTransport {
        NmcTransport::new(
            &ProviderConfig {
                name: "nmc".to_string(),
                base_url,
                request_timeout_seconds: 3,
                network: ProviderNetworkConfig {
                    http_proxy: Some(String::new()),
                    https_proxy: Some(String::new()),
                    all_proxy: Some(String::new()),
                    ..Default::default()
                },
            },
            &NetworkConfig::default(),
        )
        .unwrap()
    }

    async fn resource_server(content_type: &str, body: &[u8], content_length: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let content_type = content_type.to_string();
        let body = body.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2_048];
            let _ = stream.read(&mut request).await.unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        format!("http://{address}/nmc/")
    }

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

    #[tokio::test]
    async fn resource_rejects_urls_outside_the_configured_origin() {
        let transport = transport("http://127.0.0.1:41001/nmc/".to_string());

        let error = transport
            .resource("http://127.0.0.1:41002/radar.png")
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("outside the configured provider origin"));
    }

    #[tokio::test]
    async fn resource_accepts_same_origin_images() {
        let base_url = resource_server("image/png", b"png", 3).await;
        let transport = transport(base_url.clone());

        let (content_type, bytes) = transport
            .resource(&format!("{base_url}radar.png"))
            .await
            .unwrap();

        assert_eq!(content_type, "image/png");
        assert_eq!(bytes, b"png");
    }

    #[tokio::test]
    async fn resource_rejects_non_image_content_types() {
        let base_url = resource_server("text/html; charset=utf-8", b"html", 4).await;
        let transport = transport(base_url.clone());

        let error = transport
            .resource(&format!("{base_url}radar.html"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unsupported NMC resource content type `text/html`"));
    }

    #[tokio::test]
    async fn resource_rejects_declared_sizes_above_the_limit() {
        let base_url = resource_server("image/png", b"", MAX_RESOURCE_BYTES + 1).await;
        let transport = transport(base_url.clone());

        let error = transport
            .resource(&format!("{base_url}radar.png"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("exceeds 8388608 bytes"));
    }
}
