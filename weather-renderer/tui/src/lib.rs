mod cli;
mod command;
mod connection;
mod presentation;
mod render;
mod search;
mod terminal;
mod tui;
mod util;

use std::ffi::OsString;

use anyhow::{Context, Result, bail};
use weather_renderer_common::{client, daemon, pagination};

use crate::{
    cli::{Cli, OutputFormat, parse_cli_from},
    client::EngineClient,
    command::run_command,
    connection::{ConnectionPlan, Endpoints, connection_plan},
    daemon::{DaemonProbeState, DaemonSupervisor, EngineOwnership, probe_state_error},
    tui::run_interactive,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub embedded_daemon: bool,
}

pub async fn run_from(args: impl IntoIterator<Item = OsString>, options: RunOptions) -> Result<()> {
    let cli = parse_cli_from(args);
    let plan = connection_plan(&cli)?;
    let hmac_key = resolve_hmac_key(&cli)?;
    match plan {
        ConnectionPlan::Direct(endpoints) => run_direct(&cli, endpoints, hmac_key).await,
        ConnectionPlan::Managed => run_managed(&cli, hmac_key, options).await,
    }
}

async fn run_direct(cli: &Cli, endpoints: Endpoints, hmac_key: Option<[u8; 32]>) -> Result<()> {
    let client = EngineClient::connect(endpoints.rpc, endpoints.publisher, hmac_key)
        .await
        .context("failed to connect engine")?;
    if cli.stops_engine() {
        let result = client.shutdown().await.map(|_: weather_schema::Empty| {
            println!("engine shutdown accepted");
        });
        client.close().await;
        return result;
    }
    run_session(&client, cli, EngineOwnership::Direct).await
}

async fn run_managed(cli: &Cli, hmac_key: Option<[u8; 32]>, options: RunOptions) -> Result<()> {
    let daemon = match (&cli.daemon_exe, options.embedded_daemon) {
        (Some(path), _) => DaemonSupervisor::new(Some(path.clone()), cli.config.clone())?,
        (None, true) => DaemonSupervisor::for_current_exe(cli.config.clone())?,
        (None, false) => DaemonSupervisor::new(None, cli.config.clone())?,
    };
    let probe = daemon.probe().await?;
    if cli.stops_engine() {
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
    !cli.has_action() && matches!(cli.format, OutputFormat::Tui)
}
