use std::sync::{Arc, RwLock};

use tokio::sync::watch;

use crate::AppConfig;

#[derive(Clone)]
pub struct ConfigState {
    current: Arc<RwLock<AppConfig>>,
    last_error: Arc<RwLock<Option<String>>>,
    tx: watch::Sender<AppConfig>,
}

impl ConfigState {
    pub fn new(config: AppConfig) -> Self {
        let (tx, _) = watch::channel(config.clone());
        Self {
            current: Arc::new(RwLock::new(config)),
            last_error: Arc::new(RwLock::new(None)),
            tx,
        }
    }

    pub fn get(&self) -> AppConfig {
        self.current.read().expect("config lock poisoned").clone()
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
        *self.current.write().expect("config lock poisoned") = config.clone();
        *self.last_error.write().expect("config lock poisoned") = None;
        let _ = self.tx.send(config);
    }

    pub fn record_error(&self, err: String) {
        *self.last_error.write().expect("config lock poisoned") = Some(err);
    }
}
