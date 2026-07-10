use std::{collections::HashMap, future::Future, sync::Arc};

use anyhow::{Result, anyhow};
use tokio::sync::{Mutex, OnceCell};
use weather_schema::WeatherSnapshot;

type SharedResult = std::result::Result<WeatherSnapshot, String>;
type Flight = OnceCell<SharedResult>;

/// Coalesces concurrent upstream weather fetches for the same logical key.
///
/// The completed cell is removed after waiters have obtained an `Arc`, so this
/// only coalesces overlapping requests; a later forced refresh still performs
/// a new fetch.
#[derive(Clone, Default)]
pub(crate) struct WeatherSingleflight {
    flights: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
}

impl WeatherSingleflight {
    pub(crate) async fn run<F, Fut>(&self, key: String, fetch: F) -> Result<WeatherSnapshot>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<WeatherSnapshot>>,
    {
        let flight = {
            let mut flights = self.flights.lock().await;
            flights
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Flight::new()))
                .clone()
        };

        let result = flight
            .get_or_init(|| async {
                match fetch().await {
                    Ok(snapshot) => Ok(snapshot),
                    Err(err) => Err(format!("{err:#}")),
                }
            })
            .await
            .clone();

        let mut flights = self.flights.lock().await;
        if flights
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &flight))
        {
            flights.remove(&key);
        }
        drop(flights);

        result.map_err(|message| anyhow!(message))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;

    #[tokio::test]
    async fn overlapping_calls_share_one_fetch() {
        let flights = WeatherSingleflight::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());

        let first = {
            let flights = flights.clone();
            let calls = calls.clone();
            let release = release.clone();
            tokio::spawn(async move {
                flights
                    .run("station".to_string(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        Ok(WeatherSnapshot {
                            stale: true,
                            ..Default::default()
                        })
                    })
                    .await
            })
        };

        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }

        let second = {
            let flights = flights.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                flights
                    .run("station".to_string(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(WeatherSnapshot::default())
                    })
                    .await
            })
        };

        loop {
            let waiter_joined = {
                let flights = flights.flights.lock().await;
                flights
                    .get("station")
                    .is_some_and(|flight| Arc::strong_count(flight) >= 3)
            };
            if waiter_joined {
                break;
            }
            tokio::task::yield_now().await;
        }
        release.notify_one();
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(first.stale);
        assert!(second.stale);
    }

    #[tokio::test]
    async fn completed_forced_calls_start_new_fetches() {
        let flights = WeatherSingleflight::default();
        let calls = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let calls = calls.clone();
            flights
                .run("station".to_string(), || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(WeatherSnapshot::default())
                })
                .await
                .unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelled_leader_allows_waiter_to_take_over() {
        let flights = WeatherSingleflight::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(Notify::new());

        let leader = {
            let flights = flights.clone();
            let calls = calls.clone();
            let blocked = blocked.clone();
            tokio::spawn(async move {
                flights
                    .run("station".to_string(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        blocked.notified().await;
                        Ok(WeatherSnapshot::default())
                    })
                    .await
            })
        };
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }

        let waiter = {
            let flights = flights.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                flights
                    .run("station".to_string(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(WeatherSnapshot {
                            stale: true,
                            ..Default::default()
                        })
                    })
                    .await
            })
        };
        loop {
            let waiter_joined = {
                let flights = flights.flights.lock().await;
                flights
                    .get("station")
                    .is_some_and(|flight| Arc::strong_count(flight) >= 3)
            };
            if waiter_joined {
                break;
            }
            tokio::task::yield_now().await;
        }

        leader.abort();
        let snapshot = waiter.await.unwrap().unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(snapshot.stale);
    }
}
