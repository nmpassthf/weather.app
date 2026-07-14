mod dto;
mod mapper;
mod transport;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use weather_configure::ProviderConfig;
use weather_schema::DebugPayload;

use crate::{ProviderCity, ProviderProvince, WeatherFetch};

use super::{ProviderFuture, WeatherProvider};
use dto::decode_weather_response;
use mapper::{map_cities, map_provinces, map_weather};
use transport::NmcTransport;

#[derive(Clone)]
pub(crate) struct NmcProvider {
    name: String,
    transport: NmcTransport,
}

impl NmcProvider {
    pub(crate) fn new(config: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            name: config.name.clone(),
            transport: NmcTransport::new(config)?,
        })
    }
}

impl WeatherProvider for NmcProvider {
    fn provider_name(&self) -> &str {
        &self.name
    }

    fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
        Box::pin(async move {
            let rows = self.transport.provinces().await?;
            map_provinces(rows, self.transport.base_url())
        })
    }

    fn cities<'a>(
        &'a self,
        provider_province_code: &'a str,
    ) -> ProviderFuture<'a, Vec<ProviderCity>> {
        Box::pin(async move {
            let rows = self.transport.cities(provider_province_code).await?;
            map_cities(rows, provider_province_code, self.transport.base_url())
        })
    }

    fn weather<'a>(
        &'a self,
        provider_station_id: &'a str,
        include_debug: bool,
    ) -> ProviderFuture<'a, WeatherFetch> {
        Box::pin(async move {
            let document = self.transport.weather(provider_station_id).await?;
            let endpoint = document.endpoint.to_string();
            let raw_json = include_debug.then(|| document.body.to_string());
            let data = decode_weather_response(document.body).with_context(|| {
                format!("failed to decode NMC weather response from {endpoint}")
            })?;
            let mapped = map_weather(data, self.transport.base_url());
            let warnings = mapped.warnings;
            let mut snapshot = mapped.value;
            if include_debug {
                snapshot.debug = Some(DebugPayload {
                    provider: self.provider_name().to_string(),
                    operation: "weather".to_string(),
                    endpoint,
                    raw_json: raw_json.unwrap_or_default(),
                    warnings: warnings.clone(),
                });
            }
            Ok(WeatherFetch { snapshot, warnings })
        })
    }
}
