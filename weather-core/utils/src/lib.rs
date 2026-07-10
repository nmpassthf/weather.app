use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;

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
    ) -> Result<Self> {
        let client = Client::builder()
            .user_agent(user_agent.as_ref())
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client")?;
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
        let body = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("request failed for {url}"))?
            .error_for_status()
            .with_context(|| format!("HTTP status error for {url}"))?
            .json::<T>()
            .await
            .with_context(|| format!("failed to parse JSON response from {url}"))?;
        Ok((url, body))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use serde::de::IgnoredAny;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn url_for_joins_path_and_encodes_query() {
        let client = JsonHttpClient::new("https://www.nmc.cn/base/", Duration::from_secs(3), "ua")
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
}
