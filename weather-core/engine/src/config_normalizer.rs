use std::{path::Path, path::PathBuf};

use anyhow::{Context, Result};
use weather_configure::{AppConfig, ComponentKind, ComponentRegistry, ConfigState};

use crate::station::normalize_station_name;

pub(crate) fn spawn_config_normalizer(config_path: PathBuf, state: ConfigState) {
    ConfigNormalizer::new(config_path, state).spawn();
}

struct ConfigNormalizer {
    config_path: PathBuf,
    state: ConfigState,
}

impl ConfigNormalizer {
    fn new(config_path: PathBuf, state: ConfigState) -> Self {
        Self { config_path, state }
    }

    fn spawn(self) {
        tokio::spawn(async move {
            self.run().await;
        });
    }

    async fn run(self) {
        self.normalize_and_apply().await;
        let mut rx = self.state.subscribe();
        while rx.changed().await.is_ok() {
            self.normalize_and_apply().await;
        }
    }

    async fn normalize_and_apply(&self) {
        match normalize_config_stations(self.state.get(), &self.config_path).await {
            Ok(Some(config)) => self.state.apply(config),
            Ok(None) => {}
            Err(err) => self.state.record_error(err.to_string()),
        }
    }
}

async fn normalize_config_stations(
    mut config: AppConfig,
    config_path: &Path,
) -> Result<Option<AppConfig>> {
    let mut changed = false;
    for station in &mut config.stations {
        if station.name.trim().is_empty() {
            continue;
        }
        let normalized = normalize_station_name(&station.name);
        if station.name != normalized {
            station.name = normalized;
            changed = true;
        }
    }
    if changed {
        if config_path.exists() {
            ComponentRegistry::for_config_path(config_path)?
                .record(ComponentKind::Config, config_path)?;
            std::fs::write(config_path, toml::to_string_pretty(&config)?)
                .with_context(|| format!("failed to update config {}", config_path.display()))?;
        }
        Ok(Some(config))
    } else {
        Ok(None)
    }
}
