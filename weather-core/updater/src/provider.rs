pub(crate) mod nmc;

use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::{Context, Result, bail};
use weather_configure::UpdaterConfig;
use weather_schema::WeatherSnapshot;

use crate::{ProviderCity, ProviderProvince};
use nmc::NmcProvider;

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct WeatherFetch {
    pub snapshot: WeatherSnapshot,
    pub warnings: Vec<String>,
}

pub trait WeatherProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>>;
    fn cities<'a>(
        &'a self,
        provider_province_code: &'a str,
    ) -> ProviderFuture<'a, Vec<ProviderCity>>;
    fn weather<'a>(
        &'a self,
        provider_station_id: &'a str,
        include_debug: bool,
    ) -> ProviderFuture<'a, WeatherFetch>;
}

pub fn create_weather_provider(config: &UpdaterConfig) -> Result<Arc<dyn WeatherProvider>> {
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
    match provider.name.as_str() {
        "nmc" => Ok(Arc::new(NmcProvider::new(provider)?)),
        other => bail!("unsupported updater provider `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use weather_configure::ProviderConfig;
    use weather_schema::DebugPayload;

    use super::*;

    struct FakeProvider {
        calls: AtomicUsize,
        weather_args: Mutex<Vec<(String, bool)>>,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                weather_args: Mutex::new(Vec::new()),
            }
        }
    }

    impl WeatherProvider for FakeProvider {
        fn provider_name(&self) -> &str {
            "fake"
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                Ok(vec![ProviderProvince {
                    provider_code: "P1".to_string(),
                    name: "province".to_string(),
                    url: "/province".to_string(),
                }])
            })
        }

        fn cities<'a>(
            &'a self,
            provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<ProviderCity>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(vec![ProviderCity {
                    provider_code: "C1".to_string(),
                    provider_province_code: provider_province_code.to_string(),
                    province: "province".to_string(),
                    city: "city".to_string(),
                    url: "/city".to_string(),
                }])
            })
        }

        fn weather<'a>(
            &'a self,
            provider_station_id: &'a str,
            include_debug: bool,
        ) -> ProviderFuture<'a, WeatherFetch> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.weather_args
                .lock()
                .expect("weather args lock")
                .push((provider_station_id.to_string(), include_debug));
            Box::pin(async move {
                Ok(WeatherFetch {
                    snapshot: WeatherSnapshot {
                        debug: include_debug.then(|| DebugPayload {
                            provider: "fake".to_string(),
                            operation: "weather".to_string(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    warnings: Vec::new(),
                })
            })
        }
    }

    fn updater_config(default_provider: &str, providers: Vec<ProviderConfig>) -> UpdaterConfig {
        UpdaterConfig {
            weather_ttl_seconds: 60,
            province_ttl_seconds: 60,
            default_provider: default_provider.to_string(),
            provider: providers,
        }
    }

    #[tokio::test]
    async fn trait_object_dispatch_and_arc_clone_share_one_provider() {
        let fake = Arc::new(FakeProvider::new());
        let provider: Arc<dyn WeatherProvider> = fake.clone();
        let cloned = provider.clone();

        assert!(Arc::ptr_eq(&provider, &cloned));
        assert_eq!(cloned.provider_name(), "fake");
        assert_eq!(cloned.provinces().await.unwrap()[0].provider_code, "P1");
        assert_eq!(provider.cities("P1").await.unwrap()[0].city, "city");
        assert!(
            cloned
                .weather("S1", true)
                .await
                .unwrap()
                .snapshot
                .debug
                .is_some()
        );
        assert_eq!(fake.calls.load(Ordering::Relaxed), 3);
        assert_eq!(
            *fake.weather_args.lock().expect("weather args lock"),
            vec![("S1".to_string(), true)]
        );
    }

    #[test]
    fn factory_builds_the_configured_nmc_provider() {
        let provider = create_weather_provider(&updater_config(
            "nmc",
            vec![ProviderConfig {
                name: "nmc".to_string(),
                base_url: "https://example.invalid".to_string(),
                request_timeout_seconds: 3,
            }],
        ))
        .unwrap();

        assert_eq!(provider.provider_name(), "nmc");
    }

    #[test]
    fn factory_rejects_an_unconfigured_default_provider() {
        let error = create_weather_provider(&updater_config("missing", Vec::new()))
            .err()
            .expect("missing provider should fail");

        assert!(error.to_string().contains("is not configured"));
    }

    #[test]
    fn factory_rejects_an_unsupported_provider() {
        let error = create_weather_provider(&updater_config(
            "other",
            vec![ProviderConfig {
                name: "other".to_string(),
                base_url: "https://example.invalid".to_string(),
                request_timeout_seconds: 3,
            }],
        ))
        .err()
        .expect("unsupported provider should fail");

        assert!(error.to_string().contains("unsupported updater provider"));
    }
}
