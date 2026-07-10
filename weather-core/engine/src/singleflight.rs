use std::{
    collections::HashMap,
    future::Future,
    hash::Hash,
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{Result, anyhow};
use tokio::sync::OnceCell;

type SharedResult<V> = std::result::Result<Arc<V>, Arc<str>>;

struct Flight<V> {
    result: OnceCell<SharedResult<V>>,
}

impl<V> Flight<V> {
    fn new() -> Self {
        Self {
            result: OnceCell::new(),
        }
    }
}

struct FlightEntry<V> {
    flight: Arc<Flight<V>>,
    participants: usize,
}

struct State<K, V> {
    flights: HashMap<K, FlightEntry<V>>,
}

impl<K, V> State<K, V> {
    fn new() -> Self {
        Self {
            flights: HashMap::new(),
        }
    }
}

/// Coalesces concurrent work for the same logical key.
///
/// Only overlapping calls share a result. Once the final participant leaves,
/// the completed flight is removed and a later call starts new work. If the
/// participant currently running the initializer is cancelled, `OnceCell`
/// allows another participant to run its own initializer.
pub(crate) struct Singleflight<K, V> {
    state: Arc<Mutex<State<K, V>>>,
}

impl<K, V> Clone for Singleflight<K, V> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<K, V> Default for Singleflight<K, V> {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::new())),
        }
    }
}

impl<K, V> Singleflight<K, V>
where
    K: Eq + Hash + Clone,
{
    pub(crate) async fn run<F, Fut>(&self, key: K, work: F) -> Result<Arc<V>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V>>,
    {
        let lease = self.join(key);
        let result = lease
            .flight
            .result
            .get_or_init(|| async move {
                work()
                    .await
                    .map(Arc::new)
                    .map_err(|error| Arc::<str>::from(format!("{error:#}")))
            })
            .await
            .clone();

        // Keep the lease alive across initialization, then remove the flight
        // before returning if this was its final participant.
        drop(lease);

        result.map_err(|message| anyhow!(message.to_string()))
    }

    fn join(&self, key: K) -> FlightLease<K, V> {
        let flight = {
            let mut state = lock_unpoisoned(&self.state);
            if let Some(entry) = state.flights.get_mut(&key) {
                entry.participants += 1;
                Arc::clone(&entry.flight)
            } else {
                let flight = Arc::new(Flight::new());
                state.flights.insert(
                    key.clone(),
                    FlightEntry {
                        flight: Arc::clone(&flight),
                        participants: 1,
                    },
                );
                flight
            }
        };

        FlightLease {
            key,
            flight,
            state: Arc::clone(&self.state),
        }
    }

    #[cfg(test)]
    fn active_flights(&self) -> usize {
        lock_unpoisoned(&self.state).flights.len()
    }

    #[cfg(test)]
    fn participants(&self, key: &K) -> usize {
        lock_unpoisoned(&self.state)
            .flights
            .get(key)
            .map_or(0, |entry| entry.participants)
    }
}

struct FlightLease<K, V>
where
    K: Eq + Hash,
{
    key: K,
    flight: Arc<Flight<V>>,
    state: Arc<Mutex<State<K, V>>>,
}

impl<K, V> Drop for FlightLease<K, V>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        let mut state = lock_unpoisoned(&self.state);
        let remove = state
            .flights
            .get_mut(&self.key)
            .filter(|entry| Arc::ptr_eq(&entry.flight, &self.flight))
            .is_some_and(|entry| {
                debug_assert!(entry.participants > 0);
                entry.participants -= 1;
                entry.participants == 0
            });

        if remove {
            state.flights.remove(&self.key);
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Context;
    use tokio::{sync::Semaphore, time::timeout};

    use super::*;

    async fn wait_for(mut condition: impl FnMut() -> bool) {
        timeout(std::time::Duration::from_secs(5), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not met before timeout");
    }

    #[tokio::test]
    async fn overlapping_calls_for_the_same_key_share_one_result() {
        let flights = Singleflight::<String, usize>::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));

        let first = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.acquire().await.unwrap().forget();
                        Ok(7)
                    })
                    .await
            })
        };

        wait_for(|| calls.load(Ordering::SeqCst) == 1).await;

        let second = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(99)
                    })
                    .await
            })
        };

        wait_for(|| flights.participants(&"station".to_owned()) == 2).await;
        release.add_permits(1);

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*first, 7);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(flights.active_flights(), 0);
    }

    #[tokio::test]
    async fn different_keys_run_independently() {
        let flights = Singleflight::<&'static str, &'static str>::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));

        let first = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                flights
                    .run("first", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.acquire().await.unwrap().forget();
                        Ok("one")
                    })
                    .await
            })
        };
        let second = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                flights
                    .run("second", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.acquire().await.unwrap().forget();
                        Ok("two")
                    })
                    .await
            })
        };

        wait_for(|| calls.load(Ordering::SeqCst) == 2).await;
        assert_eq!(flights.active_flights(), 2);
        release.add_permits(2);

        assert_eq!(*first.await.unwrap().unwrap(), "one");
        assert_eq!(*second.await.unwrap().unwrap(), "two");
        assert_eq!(flights.active_flights(), 0);
    }

    #[tokio::test]
    async fn overlapping_failures_share_the_same_error() {
        let flights = Singleflight::<String, ()>::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));

        let first = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        release.acquire().await.unwrap().forget();
                        Err(anyhow!("upstream failed")).context("catalog refresh")
                    })
                    .await
            })
        };

        wait_for(|| calls.load(Ordering::SeqCst) == 1).await;

        let second = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
            })
        };

        wait_for(|| flights.participants(&"station".to_owned()) == 2).await;
        release.add_permits(1);

        let first = first.await.unwrap().unwrap_err();
        let second = second.await.unwrap().unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(format!("{first:#}"), "catalog refresh: upstream failed");
        assert_eq!(format!("{second:#}"), format!("{first:#}"));
        assert_eq!(flights.active_flights(), 0);
    }

    #[tokio::test]
    async fn sequential_calls_start_new_work() {
        let flights = Singleflight::<&'static str, usize>::default();
        let calls = Arc::new(AtomicUsize::new(0));

        let first = flights
            .run("station", {
                let calls = Arc::clone(&calls);
                || async move { Ok(calls.fetch_add(1, Ordering::SeqCst) + 1) }
            })
            .await
            .unwrap();
        let second = flights
            .run("station", {
                let calls = Arc::clone(&calls);
                || async move { Ok(calls.fetch_add(1, Ordering::SeqCst) + 1) }
            })
            .await
            .unwrap();

        assert_eq!(*first, 1);
        assert_eq!(*second, 2);
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(flights.active_flights(), 0);
    }

    #[tokio::test]
    async fn cancelled_initializer_allows_a_waiter_to_take_over() {
        let flights = Singleflight::<String, usize>::default();
        let calls = Arc::new(AtomicUsize::new(0));

        let leader = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                        Ok(1)
                    })
                    .await
            })
        };
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await;

        let waiter = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(2)
                    })
                    .await
            })
        };
        wait_for(|| flights.participants(&"station".to_owned()) == 2).await;

        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        let result = timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiter did not take over initialization")
            .unwrap()
            .unwrap();

        assert_eq!(*result, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(flights.active_flights(), 0);
    }

    #[tokio::test]
    async fn cancelling_the_only_participant_removes_the_flight() {
        let flights = Singleflight::<String, usize>::default();
        let calls = Arc::new(AtomicUsize::new(0));

        let task = {
            let flights = flights.clone();
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .run("station".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                        Ok(1)
                    })
                    .await
            })
        };
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(flights.active_flights(), 1);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(flights.active_flights(), 0);

        let result = flights
            .run("station".to_owned(), {
                let calls = Arc::clone(&calls);
                || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(2)
                }
            })
            .await
            .unwrap();
        assert_eq!(*result, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(flights.active_flights(), 0);
    }
}
