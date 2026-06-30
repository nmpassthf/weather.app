use anyhow::{Context, Result, bail};
use weather_configure::UpdaterConfig;
use weather_schema::{StationRef, WeatherSnapshot};

use crate::provider::{FetchOptions, WeatherProvider, nmc::NmcProvider};
use crate::{ProviderCity, ProviderProvince};

#[derive(Clone)]
pub struct NmcUpdater {
    provider: ProviderBackend,
}

#[derive(Clone)]
enum ProviderBackend {
    Nmc(NmcProvider),
}

impl NmcUpdater {
    pub fn new(config: &UpdaterConfig) -> Result<Self> {
        let provider = config
            .provider
            .iter()
            .find(|provider| provider.name == config.default_provider)
            .with_context(|| {
                format!(
                    "updater.default_provider `{}` is not configured",
                    config.default_provider
                )
            })?;
        let provider = match provider.name.as_str() {
            "nmc" => ProviderBackend::Nmc(NmcProvider::new(provider)?),
            other => bail!("unsupported updater provider `{other}`"),
        };
        Ok(Self { provider })
    }

    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    pub async fn provinces(&self) -> Result<Vec<ProviderProvince>> {
        self.provider.provinces().await
    }

    pub async fn cities(&self, provider_province_code: &str) -> Result<Vec<ProviderCity>> {
        self.provider.cities(provider_province_code).await
    }

    pub async fn weather(&self, provider_station_id: &str) -> Result<WeatherSnapshot> {
        self.weather_with_options(provider_station_id, FetchOptions::default())
            .await
    }

    pub async fn weather_with_debug(
        &self,
        provider_station_id: &str,
        include_debug: bool,
    ) -> Result<WeatherSnapshot> {
        self.weather_with_options(provider_station_id, FetchOptions { include_debug })
            .await
    }

    pub async fn position(&self, provider_station_id: &str) -> Result<StationRef> {
        self.provider.position(provider_station_id).await
    }

    async fn weather_with_options(
        &self,
        provider_station_id: &str,
        options: FetchOptions,
    ) -> Result<WeatherSnapshot> {
        self.provider.weather(provider_station_id, options).await
    }
}

impl ProviderBackend {
    fn name(&self) -> &str {
        match self {
            ProviderBackend::Nmc(provider) => provider.name(),
        }
    }

    async fn provinces(&self) -> Result<Vec<ProviderProvince>> {
        match self {
            ProviderBackend::Nmc(provider) => provider.provinces().await,
        }
    }

    async fn cities(&self, provider_province_code: &str) -> Result<Vec<ProviderCity>> {
        match self {
            ProviderBackend::Nmc(provider) => provider.cities(provider_province_code).await,
        }
    }

    async fn weather(
        &self,
        provider_station_id: &str,
        options: FetchOptions,
    ) -> Result<WeatherSnapshot> {
        match self {
            ProviderBackend::Nmc(provider) => provider.weather(provider_station_id, options).await,
        }
    }

    async fn position(&self, provider_station_id: &str) -> Result<StationRef> {
        match self {
            ProviderBackend::Nmc(provider) => provider.position(provider_station_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather_configure::{ProviderConfig, UpdaterConfig};

    #[test]
    fn rejects_unsupported_default_provider() {
        let result = NmcUpdater::new(&UpdaterConfig {
            weather_ttl_seconds: 60,
            province_ttl_seconds: 60,
            default_provider: "other".to_string(),
            provider: vec![ProviderConfig {
                name: "other".to_string(),
                base_url: "https://example.invalid".to_string(),
                request_timeout_seconds: 3,
            }],
        });
        let err = result.err().expect("unsupported provider should fail");

        assert!(err.to_string().contains("unsupported updater provider"));
    }
}
