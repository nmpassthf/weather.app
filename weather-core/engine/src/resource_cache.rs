use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use weather_updater::ProviderResource;

const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);
const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_MAX_REGISTRATIONS: usize = 4_096;
const FAILED_FETCH_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) enum ResourceFetchPlan {
    Ready(ProviderResource),
    Start { source_url: String },
    Pending,
    Failed(String),
    Missing,
}

#[derive(Clone)]
pub(crate) struct ResourceManager {
    inner: Arc<Mutex<ResourceState>>,
    ttl: Duration,
    max_bytes: usize,
    max_registrations: usize,
}

#[derive(Default)]
struct ResourceState {
    registrations: HashMap<String, Registration>,
    cache: HashMap<String, CacheEntry>,
    fetches: HashMap<String, ResourceFetchState>,
    cache_bytes: usize,
    sequence: u64,
}

enum ResourceFetchState {
    Pending,
    Failed {
        message: String,
        expires_at: Instant,
    },
}

struct Registration {
    source_url: String,
    last_used: u64,
}

struct CacheEntry {
    resource: ProviderResource,
    expires_at: Instant,
    last_used: u64,
}

impl Default for ResourceManager {
    fn default() -> Self {
        Self::with_limits(DEFAULT_TTL, DEFAULT_MAX_BYTES, DEFAULT_MAX_REGISTRATIONS)
    }
}

impl ResourceManager {
    fn with_limits(ttl: Duration, max_bytes: usize, max_registrations: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ResourceState::default())),
            ttl,
            max_bytes,
            max_registrations,
        }
    }

    pub(crate) fn register(&self, source_url: &str) -> Option<String> {
        let source_url = source_url.trim();
        if source_url.is_empty() || self.max_registrations == 0 {
            return None;
        }
        let resource_id = format!(
            "resource-{}",
            weather_schema::unified_station_uuid(source_url)
        );
        let mut state = lock_unpoisoned(&self.inner);
        let sequence = next_sequence(&mut state);
        if let Some(registration) = state.registrations.get_mut(&resource_id) {
            if registration.source_url == source_url {
                registration.last_used = sequence;
                return Some(resource_id);
            }
            return None;
        }
        while state.registrations.len() >= self.max_registrations {
            let oldest = state
                .registrations
                .iter()
                .filter(|(id, _)| {
                    !matches!(state.fetches.get(*id), Some(ResourceFetchState::Pending))
                })
                .min_by_key(|(_, registration)| registration.last_used)
                .map(|(id, _)| id.clone())?;
            state.registrations.remove(&oldest);
            state.fetches.remove(&oldest);
            remove_cache_entry(&mut state, &oldest);
        }
        state.registrations.insert(
            resource_id.clone(),
            Registration {
                source_url: source_url.to_string(),
                last_used: sequence,
            },
        );
        Some(resource_id)
    }

    #[cfg(test)]
    pub(crate) fn source_url(&self, resource_id: &str) -> Option<String> {
        let mut state = lock_unpoisoned(&self.inner);
        let sequence = next_sequence(&mut state);
        let registration = state.registrations.get_mut(resource_id)?;
        registration.last_used = sequence;
        Some(registration.source_url.clone())
    }

    #[cfg(test)]
    pub(crate) fn cached(&self, resource_id: &str) -> Option<ProviderResource> {
        let mut state = lock_unpoisoned(&self.inner);
        let now = Instant::now();
        if state
            .cache
            .get(resource_id)
            .is_some_and(|entry| entry.expires_at <= now)
        {
            remove_cache_entry(&mut state, resource_id);
            return None;
        }
        let sequence = next_sequence(&mut state);
        let entry = state.cache.get_mut(resource_id)?;
        entry.last_used = sequence;
        Some(entry.resource.clone())
    }

    pub(crate) fn begin_fetch(&self, resource_id: &str) -> ResourceFetchPlan {
        let mut state = lock_unpoisoned(&self.inner);
        let sequence = next_sequence(&mut state);
        let Some(registration) = state.registrations.get_mut(resource_id) else {
            return ResourceFetchPlan::Missing;
        };
        registration.last_used = sequence;
        let source_url = registration.source_url.clone();
        let now = Instant::now();
        if state
            .cache
            .get(resource_id)
            .is_some_and(|entry| entry.expires_at <= now)
        {
            remove_cache_entry(&mut state, resource_id);
        }
        if let Some(entry) = state.cache.get_mut(resource_id) {
            entry.last_used = sequence;
            return ResourceFetchPlan::Ready(entry.resource.clone());
        }
        if state.fetches.get(resource_id).is_some_and(
            |fetch| matches!(fetch, ResourceFetchState::Failed { expires_at, .. } if *expires_at <= now),
        ) {
            state.fetches.remove(resource_id);
        }
        match state.fetches.get(resource_id) {
            Some(ResourceFetchState::Pending) => ResourceFetchPlan::Pending,
            Some(ResourceFetchState::Failed { message, .. }) => {
                ResourceFetchPlan::Failed(message.clone())
            }
            None => {
                state
                    .fetches
                    .insert(resource_id.to_string(), ResourceFetchState::Pending);
                ResourceFetchPlan::Start { source_url }
            }
        }
    }

    pub(crate) fn finish_fetch(&self, resource_id: &str, error: Option<String>) {
        let mut state = lock_unpoisoned(&self.inner);
        if !state.registrations.contains_key(resource_id) {
            state.fetches.remove(resource_id);
            return;
        }
        match error {
            Some(message) => {
                state.fetches.insert(
                    resource_id.to_string(),
                    ResourceFetchState::Failed {
                        message,
                        expires_at: Instant::now() + FAILED_FETCH_TTL,
                    },
                );
            }
            None => {
                state.fetches.remove(resource_id);
            }
        }
    }

    pub(crate) fn cache(
        &self,
        resource_id: &str,
        resource: ProviderResource,
    ) -> Result<(), String> {
        let resource_bytes = resource.bytes.len();
        if resource_bytes > self.max_bytes {
            return Err(format!(
                "resource `{resource_id}` is {resource_bytes} bytes, exceeding the {} byte cache limit",
                self.max_bytes
            ));
        }
        let mut state = lock_unpoisoned(&self.inner);
        if !state.registrations.contains_key(resource_id) {
            return Err(format!("resource `{resource_id}` is not registered"));
        }
        purge_expired(&mut state, Instant::now());
        remove_cache_entry(&mut state, resource_id);
        while state.cache_bytes.saturating_add(resource_bytes) > self.max_bytes {
            let Some(oldest) = state
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            remove_cache_entry(&mut state, &oldest);
        }
        let sequence = next_sequence(&mut state);
        state.cache_bytes = state.cache_bytes.saturating_add(resource_bytes);
        state.cache.insert(
            resource_id.to_string(),
            CacheEntry {
                resource,
                expires_at: Instant::now() + self.ttl,
                last_used: sequence,
            },
        );
        Ok(())
    }
}

fn next_sequence(state: &mut ResourceState) -> u64 {
    state.sequence = state.sequence.wrapping_add(1);
    state.sequence
}

fn purge_expired(state: &mut ResourceState, now: Instant) {
    let expired = state
        .cache
        .iter()
        .filter(|(_, entry)| entry.expires_at <= now)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in expired {
        remove_cache_entry(state, &id);
    }
}

fn remove_cache_entry(state: &mut ResourceState, resource_id: &str) {
    if let Some(entry) = state.cache.remove(resource_id) {
        state.cache_bytes = state.cache_bytes.saturating_sub(entry.resource.bytes.len());
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(bytes: &[u8]) -> ProviderResource {
        ProviderResource {
            content_type: "image/png".to_string(),
            bytes: Arc::from(bytes),
        }
    }

    #[test]
    fn registry_is_deterministic_but_does_not_resolve_unknown_ids() {
        let manager = ResourceManager::default();
        let first = manager
            .register("https://provider.example/radar.png")
            .unwrap();
        let second = manager
            .register("https://provider.example/radar.png")
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            manager.source_url(&first).as_deref(),
            Some("https://provider.example/radar.png")
        );
        assert_eq!(manager.source_url("resource-unknown"), None);
    }

    #[test]
    fn cached_resources_reuse_the_same_byte_allocation() {
        let manager = ResourceManager::default();
        let id = manager
            .register("https://provider.example/radar.png")
            .unwrap();
        let original = resource(b"png");
        let original_bytes = Arc::clone(&original.bytes);
        manager.cache(&id, original).unwrap();

        let cached = manager.cached(&id).unwrap();
        assert!(Arc::ptr_eq(&original_bytes, &cached.bytes));
    }

    #[test]
    fn expired_resources_are_removed() {
        let manager = ResourceManager::with_limits(Duration::ZERO, 32, 4);
        let id = manager
            .register("https://provider.example/radar.png")
            .unwrap();
        manager.cache(&id, resource(b"png")).unwrap();

        assert!(manager.cached(&id).is_none());
    }

    #[test]
    fn capacity_evicts_the_least_recently_used_resource() {
        let manager = ResourceManager::with_limits(Duration::from_secs(60), 6, 4);
        let first = manager
            .register("https://provider.example/one.png")
            .unwrap();
        let second = manager
            .register("https://provider.example/two.png")
            .unwrap();
        manager.cache(&first, resource(b"1111")).unwrap();
        manager.cache(&second, resource(b"2222")).unwrap();

        assert!(manager.cached(&first).is_none());
        assert_eq!(manager.cached(&second).unwrap().bytes.as_ref(), b"2222");
    }

    #[test]
    fn asynchronous_fetches_start_once_and_preserve_failures_briefly() {
        let manager = ResourceManager::default();
        let id = manager
            .register("https://provider.example/radar.png")
            .unwrap();

        assert!(matches!(
            manager.begin_fetch(&id),
            ResourceFetchPlan::Start { .. }
        ));
        assert!(matches!(
            manager.begin_fetch(&id),
            ResourceFetchPlan::Pending
        ));

        manager.finish_fetch(&id, Some("upstream failed".to_string()));
        assert!(matches!(
            manager.begin_fetch(&id),
            ResourceFetchPlan::Failed(message) if message == "upstream failed"
        ));
        assert!(matches!(
            manager.begin_fetch("resource-missing"),
            ResourceFetchPlan::Missing
        ));
    }

    #[test]
    fn active_fetch_registration_is_not_evicted() {
        let manager = ResourceManager::with_limits(Duration::from_secs(60), 32, 1);
        let id = manager
            .register("https://provider.example/radar.png")
            .unwrap();
        assert!(matches!(
            manager.begin_fetch(&id),
            ResourceFetchPlan::Start { .. }
        ));

        assert!(
            manager
                .register("https://provider.example/other.png")
                .is_none()
        );
        assert!(matches!(
            manager.begin_fetch(&id),
            ResourceFetchPlan::Pending
        ));
    }

    #[test]
    fn oversized_resource_fails_instead_of_restarting_forever() {
        let manager = ResourceManager::with_limits(Duration::from_secs(60), 2, 1);
        let id = manager
            .register("https://provider.example/radar.png")
            .unwrap();

        let error = manager.cache(&id, resource(b"png")).unwrap_err();
        assert!(
            error.contains("exceeding the 2 byte cache limit"),
            "{error}"
        );
    }
}
