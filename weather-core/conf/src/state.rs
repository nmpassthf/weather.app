use std::sync::{Arc, RwLock};

use tokio::sync::watch;

use crate::AppConfig;

#[derive(Clone)]
pub struct ConfigState {
    last_error: Arc<RwLock<Option<String>>>,
    tx: watch::Sender<AppConfig>,
}

impl ConfigState {
    pub fn new(config: AppConfig) -> Self {
        let (tx, _) = watch::channel(config);
        Self {
            last_error: Arc::new(RwLock::new(None)),
            tx,
        }
    }

    pub fn get(&self) -> AppConfig {
        self.tx.borrow().clone()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .read()
            .expect("config lock poisoned")
            .clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<AppConfig> {
        self.tx.subscribe()
    }

    pub fn apply(&self, config: AppConfig) {
        *self.last_error.write().expect("config lock poisoned") = None;
        self.tx.send_replace(config);
    }

    pub fn record_error(&self, err: String) {
        *self.last_error.write().expect("config lock poisoned") = Some(err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_subscriber_observes_latest_config() {
        let state = ConfigState::new(AppConfig::default());
        let mut updated = state.get();
        updated.updater.weather_ttl_seconds += 1;

        state.apply(updated.clone());
        let subscriber = state.subscribe();

        assert_eq!(state.get(), updated);
        assert_eq!(*subscriber.borrow(), updated);
    }

    #[tokio::test]
    async fn existing_subscriber_and_get_share_the_same_value() {
        let state = ConfigState::new(AppConfig::default());
        let mut subscriber = state.subscribe();
        let mut updated = state.get();
        updated.updater.province_ttl_seconds += 1;

        state.apply(updated.clone());
        subscriber.changed().await.unwrap();

        assert_eq!(state.get(), updated);
        assert_eq!(*subscriber.borrow_and_update(), updated);
    }
}
