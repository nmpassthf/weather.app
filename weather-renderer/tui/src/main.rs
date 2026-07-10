mod cli;
mod client;
mod command;
mod connection;
mod daemon;
mod pagination;
mod render;
mod search;
mod terminal;
mod tui;
mod util;

use std::ffi::OsString;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{
    cli::{Cli, CommandKind, OutputFormat},
    client::EngineClient,
    command::run_command,
    connection::{ConnectionPlan, Endpoints, connection_plan},
    daemon::{DaemonProbeState, DaemonSupervisor, EngineOwnership, probe_state_error},
    tui::run_interactive,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_from(normalized_args());
    let plan = connection_plan(&cli)?;
    let hmac_key = resolve_hmac_key(&cli)?;
    match plan {
        ConnectionPlan::Direct(endpoints) => run_direct(&cli, endpoints, hmac_key).await,
        ConnectionPlan::Managed => run_managed(&cli, hmac_key).await,
    }
}

async fn run_direct(cli: &Cli, endpoints: Endpoints, hmac_key: Option<[u8; 32]>) -> Result<()> {
    let client = EngineClient::connect(endpoints.rpc, endpoints.publisher, hmac_key)
        .await
        .context("failed to connect engine")?;
    if matches!(cli.command.as_ref(), Some(CommandKind::Kill)) {
        let result = client.shutdown().await.map(|_: weather_schema::Empty| {
            println!("engine shutdown accepted");
        });
        client.close().await;
        return result;
    }
    run_session(&client, cli, EngineOwnership::Direct).await
}

async fn run_managed(cli: &Cli, hmac_key: Option<[u8; 32]>) -> Result<()> {
    let daemon = DaemonSupervisor::from_cli(cli)?;
    let probe = daemon.probe().await?;
    if matches!(cli.command.as_ref(), Some(CommandKind::Kill)) {
        match probe.state {
            DaemonProbeState::NotRunning => {
                println!("engine is not running");
                return Ok(());
            }
            DaemonProbeState::Running => {}
            state => bail!(probe_state_error(state, probe.message.as_deref())),
        }
        let client = EngineClient::connect(
            probe.rpc_endpoint.clone(),
            probe.pub_endpoint.clone(),
            hmac_key,
        )
        .await?;
        let result = client.shutdown().await.map(|_: weather_schema::Empty| {
            println!("engine shutdown accepted");
        });
        client.close().await;
        return result;
    }
    let ready = daemon.ensure_ready(probe).await?;
    let client =
        EngineClient::connect(ready.probe.rpc_endpoint, ready.probe.pub_endpoint, hmac_key)
            .await
            .context("failed to connect engine")?;

    run_session(&client, cli, ready.ownership).await
}

async fn run_session(client: &EngineClient, cli: &Cli, ownership: EngineOwnership) -> Result<()> {
    let result = run_connected(client, cli).await;
    if let EngineOwnership::Owned {
        owner_token,
        mut foreground,
    } = ownership
    {
        if client.shutdown_if_owned(&owner_token).await.is_ok() {
            foreground.mark_graceful_shutdown_requested();
        }
        drop(foreground);
    }
    client.close().await;
    result
}

async fn run_connected(client: &EngineClient, cli: &Cli) -> Result<()> {
    if should_start_tui(cli) {
        run_interactive(client, cli).await
    } else {
        run_command(client, cli).await
    }
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
