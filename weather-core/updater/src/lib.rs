mod catalog;
mod provider;

pub use catalog::{ProviderCity, ProviderProvince};
pub use provider::{
    ProviderFuture, ProviderResource, WeatherFetch, WeatherProvider, create_weather_provider,
};
