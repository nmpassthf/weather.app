pub(crate) mod nmc;

use anyhow::Result;
use weather_schema::{StationRef, WeatherSnapshot};

use crate::{ProviderCity, ProviderProvince};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FetchOptions {
    pub(crate) include_debug: bool,
}

#[allow(async_fn_in_trait)]
pub(crate) trait WeatherProvider {
    fn name(&self) -> &str;
    async fn provinces(&self) -> Result<Vec<ProviderProvince>>;
    async fn cities(&self, provider_province_code: &str) -> Result<Vec<ProviderCity>>;
    async fn weather(
        &self,
        provider_station_id: &str,
        options: FetchOptions,
    ) -> Result<WeatherSnapshot>;
    async fn position(&self, provider_station_id: &str) -> Result<StationRef>;
}
