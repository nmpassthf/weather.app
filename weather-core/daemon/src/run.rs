use std::path::PathBuf;

use anyhow::Result;
use weather_configure::default_config_file;
use weather_engine::{EngineExit, run_engine};

use crate::path::absolute_config_path;

pub(crate) async fn run(config: Option<PathBuf>, foreground: bool) -> Result<()> {
    let config_path = absolute_config_path(config.unwrap_or(default_config_file()?))?;
    let mode = if foreground { "foreground" } else { "daemon" }.to_string();
    loop {
        match run_engine(config_path.clone(), mode.clone()).await? {
            EngineExit::Shutdown => return Ok(()),
            EngineExit::Restart => {
                eprintln!("weather engine accepted restart; starting a fresh engine instance");
            }
        }
    }
}
