use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use weather_db::{ProviderCity, ProviderProvince};

use crate::{limits::MAX_CONCURRENT_CATALOG_FETCHES, singleflight::Singleflight};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CityCatalogKey {
    pub(crate) provider: String,
    pub(crate) province_code: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderCatalog {
    pub(crate) provinces: Vec<ProviderProvince>,
    pub(crate) cities: Vec<ProviderCity>,
}

#[derive(Clone)]
pub(crate) struct CatalogCoordinator {
    pub(crate) province_flights: Singleflight<String, Vec<ProviderProvince>>,
    pub(crate) city_flights: Singleflight<CityCatalogKey, Vec<ProviderCity>>,
    pub(crate) population_flights: Singleflight<String, ProviderCatalog>,
    upstream_permits: Arc<Semaphore>,
}

impl Default for CatalogCoordinator {
    fn default() -> Self {
        Self::with_limit(MAX_CONCURRENT_CATALOG_FETCHES)
    }
}

impl CatalogCoordinator {
    fn with_limit(limit: usize) -> Self {
        assert!(limit > 0, "catalog concurrency limit must be positive");
        Self {
            province_flights: Singleflight::default(),
            city_flights: Singleflight::default(),
            population_flights: Singleflight::default(),
            upstream_permits: Arc::new(Semaphore::new(limit)),
        }
    }

    pub(crate) async fn acquire_upstream_permit(&self) -> Result<OwnedSemaphorePermit> {
        self.upstream_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("catalog upstream concurrency limiter is closed"))
    }
}

/// A catalog row remains fresh strictly before its TTL boundary. A zero TTL
/// deliberately forces every non-overlapping request through refresh.
pub(crate) fn catalog_cache_is_fresh(
    fetched_at_unix_ms: i64,
    now_unix_ms: i64,
    ttl_seconds: u64,
) -> bool {
    if ttl_seconds == 0 {
        return false;
    }
    let age_ms = now_unix_ms.saturating_sub(fetched_at_unix_ms).max(0) as u128;
    let ttl_ms = u128::from(ttl_seconds) * 1_000;
    age_ms < ttl_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_is_strict_at_the_ttl_boundary() {
        let fetched = 1_000_000;

        assert!(catalog_cache_is_fresh(fetched, fetched + 59_999, 60));
        assert!(!catalog_cache_is_fresh(fetched, fetched + 60_000, 60));
        assert!(!catalog_cache_is_fresh(fetched, fetched, 0));
    }

    #[test]
    fn freshness_handles_clock_rollback_and_extreme_values() {
        assert!(catalog_cache_is_fresh(10_000, 9_000, 1));
        assert!(catalog_cache_is_fresh(i64::MIN, i64::MAX, u64::MAX));
        assert!(!catalog_cache_is_fresh(i64::MIN, i64::MAX, 1));
    }
}
