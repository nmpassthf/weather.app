use std::path::PathBuf;

use anyhow::Result;
use weather_configure::{default_config_file, load_or_default};
use weather_engine::{EngineExit, run_engine_with_owner};

use crate::{cli::DaemonLogLevel, logging, path::absolute_config_path};

pub(crate) async fn run(
    config: Option<PathBuf>,
    log_level: Option<DaemonLogLevel>,
    foreground: bool,
    owner_token: Option<String>,
) -> Result<()> {
    let config_path = absolute_config_path(config.unwrap_or(default_config_file()?))?;
    let mode = if foreground { "foreground" } else { "daemon" }.to_string();
    let configured_level = load_or_default(&config_path)?.engine.log_level;
    let mut effective_level =
        log_level.map_or(configured_level, |level| level.as_str().to_string());
    logging::configure(&effective_level)?;
    loop {
        log::info!(
            "starting weather engine mode={} config={} log_level={}",
            mode,
            config_path.display(),
            effective_level
        );
        match run_engine_with_owner(config_path.clone(), mode.clone(), owner_token.clone()).await {
            Ok(EngineExit::Shutdown) => {
                log::info!("weather engine stopped");
                return Ok(());
            }
            Ok(EngineExit::Restart) => {
                log::info!("weather engine accepted restart; starting a fresh engine instance");
                if log_level.is_none() {
                    effective_level = load_or_default(&config_path)?.engine.log_level;
                    logging::configure(&effective_level)?;
                }
            }
            Err(error) => {
                log::error!("weather engine stopped with error: {error:#}");
                return Err(error);
            }
        }
    }
}
