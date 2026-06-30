mod cli;
mod client;
mod command;
mod daemon;
mod render;
mod search;
mod tui;
mod util;

use std::ffi::OsString;

use anyhow::{Context, Result};
use clap::Parser;

use crate::{
    cli::{Cli, CommandKind, OutputFormat},
    client::EngineClient,
    command::run_command,
    daemon::{DaemonProbeState, DaemonSupervisor},
    tui::run_interactive,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_from(normalized_args());
    let mut foreground = None;
    let daemon = DaemonSupervisor::from_cli(&cli)?;
    let probe = daemon.probe()?;
    let rpc_endpoint = cli
        .rpc_endpoint
        .clone()
        .or_else(|| cli.endpoint.clone())
        .unwrap_or(probe.rpc_endpoint.clone());
    let pub_endpoint = cli
        .pub_endpoint
        .clone()
        .unwrap_or(probe.pub_endpoint.clone());
    let hmac_key = resolve_hmac_key(&cli)?;
    if matches!(cli.command, Some(CommandKind::Kill)) {
        if matches!(probe.state, DaemonProbeState::NotRunning) {
            println!("engine is not running");
            return Ok(());
        }
        let client = EngineClient::connect(rpc_endpoint, pub_endpoint, hmac_key).await?;
        let _: weather_schema::Empty = client.shutdown().await?;
        println!("engine shutdown accepted");
        return Ok(());
    }
    if matches!(probe.state, DaemonProbeState::NotRunning) {
        foreground = Some(daemon.start_foreground()?);
    }
    let client = EngineClient::connect(rpc_endpoint, pub_endpoint, hmac_key)
        .await
        .context("failed to connect engine")?;

    let result = if should_start_tui(&cli) {
        run_interactive(&client, &cli).await
    } else {
        run_command(&client, &cli).await
    };
    if foreground.is_some() {
        let _ = client.shutdown().await;
    }
    drop(foreground);
    result
}

fn resolve_hmac_key(cli: &Cli) -> Result<Option<[u8; 32]>> {
    match cli.hmac.as_str() {
        "disabled" => Ok(None),
        "hmac_key" => {
            if cli.hmac_key.is_empty() {
                anyhow::bail!("--hmac=hmac_key requires non-empty --hmac-key");
            }
            Ok(Some(weather_schema::hmac_key_from_str(&cli.hmac_key)?))
        }
        "hmac_env_key" => {
            let name = cli.hmac_env_key_name.as_ref().ok_or_else(|| {
                anyhow::anyhow!("--hmac=hmac_env_key requires --hmac-env-key-name")
            })?;
            let value = std::env::var(name)
                .with_context(|| format!("environment variable `{name}` is not set"))?;
            Ok(Some(weather_schema::hmac_key_from_str(&value)?))
        }
        other => anyhow::bail!("invalid --hmac mode `{other}`"),
    }
}

fn should_start_tui(cli: &Cli) -> bool {
    cli.command.is_none()
        && !cli.core_get_default_config
        && !cli.core_get_config
        && !cli.core_restart_engine
        && matches!(cli.format, OutputFormat::Tui)
}

fn normalized_args() -> Vec<OsString> {
    std::env::args_os()
        .map(|arg| match arg.to_str() {
            Some("-core-dump-default-config") => OsString::from("--core-get-default-config"),
            Some("-core-show-current-config") => OsString::from("--core-get-config"),
            Some("-core-show-currnet-config") => OsString::from("--core-get-config"),
            Some("-core-restart-engine") => OsString::from("--core-restart-engine"),
            _ => arg,
        })
        .collect()
}
