use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, NoProxy, Proxy, Url};
use serde::de::DeserializeOwned;

const MAX_CONNECT_ATTEMPTS: usize = 6;
const INITIAL_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpNetworkConfig {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub allow_insecure: bool,
}

impl HttpNetworkConfig {
    pub fn from_environment() -> Self {
        Self {
            http_proxy: proxy_environment_value("HTTP_PROXY", "http_proxy"),
            https_proxy: proxy_environment_value("HTTPS_PROXY", "https_proxy"),
            no_proxy: proxy_environment_value("NO_PROXY", "no_proxy"),
            all_proxy: proxy_environment_value("ALL_PROXY", "all_proxy"),
            allow_insecure: false,
        }
    }
}

#[derive(Clone)]
pub struct JsonHttpClient {
    client: Client,
    base_url: Url,
}

impl JsonHttpClient {
    pub fn new(
        base_url: impl AsRef<str>,
        timeout: Duration,
        user_agent: impl AsRef<str>,
        network: &HttpNetworkConfig,
    ) -> Result<Self> {
        let mut builder = Client::builder()
            .user_agent(user_agent.as_ref())
            .timeout(timeout)
            .no_proxy()
            .danger_accept_invalid_certs(network.allow_insecure);
        let no_proxy = network.no_proxy.as_deref().and_then(NoProxy::from_string);
        if let Some(proxy_url) = &network.http_proxy {
            let proxy = Proxy::http(proxy_url)
                .context("invalid http_proxy URL")?
                .no_proxy(no_proxy.clone());
            builder = builder.proxy(proxy);
        }
        if let Some(proxy_url) = &network.https_proxy {
            let proxy = Proxy::https(proxy_url)
                .context("invalid https_proxy URL")?
                .no_proxy(no_proxy.clone());
            builder = builder.proxy(proxy);
        }
        if let Some(proxy_url) = &network.all_proxy {
            let proxy = Proxy::all(proxy_url)
                .context("invalid all_proxy URL")?
                .no_proxy(no_proxy);
            builder = builder.proxy(proxy);
        }
        let client = builder.build().context("failed to build HTTP client")?;
        let base_url = Url::parse(base_url.as_ref()).context("invalid base URL")?;
        Ok(Self { client, base_url })
    }

    fn url_for(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self
            .base_url
            .join(path)
            .with_context(|| format!("failed to build URL for {path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        Ok(url)
    }

    pub async fn get_json<T>(&self, path: &str, query: &[(&str, &str)]) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.get_json_with_endpoint(path, query)
            .await
            .map(|(_, body)| body)
    }

    pub async fn get_json_with_endpoint<T>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(Url, T)>
    where
        T: DeserializeOwned,
    {
        let url = self.url_for(path, query)?;
        let mut attempt = 1;
        let mut retry_delay = INITIAL_CONNECT_RETRY_DELAY;
        let response = loop {
            match self.client.get(url.clone()).send().await {
                Ok(response) => break response,
                Err(err) if err.is_connect() && attempt < MAX_CONNECT_ATTEMPTS => {
                    tokio::time::sleep(retry_delay).await;
                    attempt += 1;
                    retry_delay = retry_delay.saturating_mul(2);
                }
                Err(err) => {
                    let context = if attempt == 1 {
                        format!("request failed for {url}")
                    } else {
                        format!("request failed for {url} after {attempt} attempts")
                    };
                    return Err(err).context(context);
                }
            }
        };
        let body = response
            .error_for_status()
            .with_context(|| format!("HTTP status error for {url}"))?
            .json::<T>()
            .await
            .with_context(|| format!("failed to parse JSON response from {url}"))?;
        Ok((url, body))
    }

    pub async fn get_binary_url(&self, url: &Url, max_bytes: usize) -> Result<(String, Vec<u8>)> {
        if max_bytes == 0 {
            anyhow::bail!("binary response limit must be greater than zero");
        }
        let mut attempt = 1;
        let mut retry_delay = INITIAL_CONNECT_RETRY_DELAY;
        let response = loop {
            match self.client.get(url.clone()).send().await {
                Ok(response) => break response,
                Err(err) if err.is_connect() && attempt < MAX_CONNECT_ATTEMPTS => {
                    tokio::time::sleep(retry_delay).await;
                    attempt += 1;
                    retry_delay = retry_delay.saturating_mul(2);
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("binary request failed for {url} after {attempt} attempts")
                    });
                }
            }
        };
        let mut response = response
            .error_for_status()
            .with_context(|| format!("HTTP status error for {url}"))?;
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            anyhow::bail!("binary response from {url} exceeds {max_bytes} bytes");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("application/octet-stream")
            .to_ascii_lowercase();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("failed to read binary response from {url}"))?
        {
            let next_len = body
                .len()
                .checked_add(chunk.len())
                .context("binary response size overflow")?;
            if next_len > max_bytes {
                anyhow::bail!("binary response from {url} exceeds {max_bytes} bytes");
            }
            body.extend_from_slice(&chunk);
        }
        Ok((content_type, body))
    }
}

fn proxy_environment_value(uppercase: &str, lowercase: &str) -> Option<String> {
    match std::env::var(uppercase) {
        Ok(value) => normalized_network_value(&value),
        Err(_) => std::env::var(lowercase)
            .ok()
            .and_then(|value| normalized_network_value(&value)),
    }
}

pub fn normalized_network_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use serde::de::IgnoredAny;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn url_for_joins_path_and_encodes_query() {
        let client = JsonHttpClient::new(
            "https://www.nmc.cn/base/",
            Duration::from_secs(3),
            "ua",
            &HttpNetworkConfig::default(),
        )
        .expect("client");

        let url = client
            .url_for(
                "rest/weather",
                &[("stationid", "MjX fi"), ("lang", "zh-CN")],
            )
            .expect("url");

        assert_eq!(
            url.as_str(),
            "https://www.nmc.cn/base/rest/weather?stationid=MjX+fi&lang=zh-CN"
        );
    }

    #[tokio::test]
    async fn get_json_errors_include_url_context() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write response");
        });
        let client = JsonHttpClient::new(
            format!("http://{addr}"),
            Duration::from_secs(3),
            "weather-test",
            &HttpNetworkConfig::default(),
        )
        .expect("client");

        let err = client
            .get_json::<IgnoredAny>("rest/weather", &[("stationid", "MjXfi")])
            .await
            .expect_err("500 should fail");

        let message = format!("{err:#}");
        assert!(message.contains("rest/weather"));
        assert!(message.contains("stationid=MjXfi"));
    }

    #[tokio::test]
    async fn explicit_proxy_routes_requests_without_resolving_the_origin() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy listener");
        let addr = listener.local_addr().expect("proxy address");
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept proxy request");
            let mut buffer = [0_u8; 2048];
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer)
                .await
                .expect("read proxy request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .await
                .expect("write proxy response");
            String::from_utf8_lossy(&buffer[..read]).into_owned()
        });
        let proxy_url = format!("http://{addr}");
        let network = HttpNetworkConfig {
            http_proxy: Some(proxy_url),
            ..Default::default()
        };
        let client = JsonHttpClient::new(
            "http://origin.invalid/base/",
            Duration::from_secs(2),
            "weather-test",
            &network,
        )
        .expect("client");

        client
            .get_json::<IgnoredAny>("rest/weather", &[])
            .await
            .expect("proxied response");

        let request = proxy.await.expect("proxy task");
        assert!(
            request.starts_with("GET http://origin.invalid/base/rest/weather HTTP/1.1\r\n"),
            "{request:?}"
        );
    }

    #[tokio::test]
    async fn no_proxy_ip_bypasses_every_configured_proxy_rule() {
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("origin listener");
        let origin_addr = origin.local_addr().expect("origin address");
        let origin = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.expect("accept origin request");
            let mut buffer = [0_u8; 2048];
            tokio::io::AsyncReadExt::read(&mut stream, &mut buffer)
                .await
                .expect("read origin request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .await
                .expect("write origin response");
        });
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy listener");
        let proxy_url = format!("http://{}", proxy.local_addr().expect("proxy address"));
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = proxy.accept().await.expect("accept proxy request");
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write proxy response");
        });
        let network = HttpNetworkConfig {
            http_proxy: Some(proxy_url.clone()),
            https_proxy: Some(proxy_url.clone()),
            all_proxy: Some(proxy_url),
            no_proxy: Some("127.0.0.1".to_string()),
            allow_insecure: false,
        };
        let client = JsonHttpClient::new(
            format!("http://{origin_addr}"),
            Duration::from_secs(2),
            "weather-test",
            &network,
        )
        .expect("client");

        client
            .get_json::<IgnoredAny>("rest/weather", &[])
            .await
            .expect("no_proxy should route directly to the origin");

        origin.await.expect("origin task");
        assert!(!proxy.is_finished(), "request unexpectedly reached proxy");
        proxy.abort();
    }

    #[tokio::test]
    async fn get_json_retries_connect_failures() {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = reservation.local_addr().expect("reserved address");
        drop(reservation);

        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let listener = tokio::net::TcpListener::bind(addr).await.expect("listener");
            let (mut stream, _) = listener.accept().await.expect("accept");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                )
                .await
                .expect("write response");
        });
        let client = JsonHttpClient::new(
            format!("http://{addr}"),
            Duration::from_secs(2),
            "weather-test",
            &HttpNetworkConfig::default(),
        )
        .expect("client");

        #[derive(serde::Deserialize)]
        struct Payload {
            ok: bool,
        }

        let body = client
            .get_json::<Payload>("rest/weather", &[])
            .await
            .expect("a later connection attempt should succeed");

        assert!(body.ok);
        server.await.expect("server task");
    }
}
