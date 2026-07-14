use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use weather_schema::{DEFAULT_ZMQ_PUB_ENDPOINT, DEFAULT_ZMQ_RPC_ENDPOINT};

use crate::cli::Cli;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoints {
    pub(crate) rpc: String,
    pub(crate) publisher: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectionPlan {
    Direct(Endpoints),
    Managed,
}

pub(crate) fn connection_plan(cli: &Cli) -> Result<ConnectionPlan> {
    connection_plan_with_fallback(cli, || configured_endpoints(cli))
}

fn connection_plan_with_fallback(
    cli: &Cli,
    load_fallback: impl FnOnce() -> Result<Endpoints>,
) -> Result<ConnectionPlan> {
    if cli.endpoint.is_some() && cli.rpc_endpoint.is_some() {
        bail!("--endpoint cannot be used with --rpc-endpoint");
    }

    let rpc = cli.rpc_endpoint.as_ref().or(cli.endpoint.as_ref());
    let publisher = cli.pub_endpoint.as_ref();
    match (rpc, publisher) {
        (None, None) => Ok(ConnectionPlan::Managed),
        (Some(rpc), Some(publisher)) => Ok(ConnectionPlan::Direct(Endpoints {
            rpc: rpc.clone(),
            publisher: publisher.clone(),
        })),
        (rpc, publisher) => {
            let fallback = load_fallback()?;
            Ok(ConnectionPlan::Direct(Endpoints {
                rpc: rpc.cloned().unwrap_or(fallback.rpc),
                publisher: publisher.cloned().unwrap_or(fallback.publisher),
            }))
        }
    }
}

fn configured_endpoints(cli: &Cli) -> Result<Endpoints> {
    let config_path = match &cli.config {
        Some(path) => path.clone(),
        None => default_config_file()?,
    };
    let content = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(default_endpoints());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read config {}", config_path.display()));
        }
    };
    let config: EndpointConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse config endpoints {}", config_path.display()))?;
    Ok(Endpoints {
        rpc: config.ipc.rpc_endpoint,
        publisher: config.ipc.pub_endpoint,
    })
}

#[derive(Debug, Deserialize)]
struct EndpointConfig {
    ipc: EndpointIpcConfig,
}

#[derive(Debug, Deserialize)]
struct EndpointIpcConfig {
    rpc_endpoint: String,
    pub_endpoint: String,
}

fn default_endpoints() -> Endpoints {
    Endpoints {
        rpc: DEFAULT_ZMQ_RPC_ENDPOINT.to_string(),
        publisher: DEFAULT_ZMQ_PUB_ENDPOINT.to_string(),
    }
}

fn default_config_file() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("neither HOME nor USERPROFILE is set")?;
    Ok(PathBuf::from(home)
        .join(".weather")
        .join("config")
        .join("weather.toml"))
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs};

    use clap::Parser;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(args)
    }

    fn endpoints(rpc: &str, publisher: &str) -> Endpoints {
        Endpoints {
            rpc: rpc.to_string(),
            publisher: publisher.to_string(),
        }
    }

    #[test]
    fn no_explicit_endpoint_is_managed_without_loading_fallback() {
        let cli = cli(&["weather-tui", "status"]);

        let plan = connection_plan_with_fallback(&cli, || {
            panic!("managed planning must not load endpoint fallback")
        })
        .unwrap();

        assert_eq!(plan, ConnectionPlan::Managed);
    }

    #[test]
    fn complete_canonical_endpoints_are_direct_without_loading_fallback() {
        let cli = cli(&[
            "weather-tui",
            "--rpc-endpoint",
            "tcp://remote:41001",
            "--pub-endpoint",
            "tcp://remote:41002",
            "status",
        ]);

        let plan = connection_plan_with_fallback(&cli, || {
            panic!("complete direct endpoints must not load endpoint fallback")
        })
        .unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://remote:41001", "tcp://remote:41002"))
        );
    }

    #[test]
    fn complete_legacy_endpoints_are_direct_without_loading_fallback() {
        let cli = cli(&[
            "weather-tui",
            "--endpoint",
            "tcp://remote:42001",
            "--pub-endpoint",
            "tcp://remote:42002",
            "status",
        ]);

        let plan = connection_plan_with_fallback(&cli, || {
            panic!("complete direct endpoints must not load endpoint fallback")
        })
        .unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://remote:42001", "tcp://remote:42002"))
        );
    }

    #[test]
    fn rpc_only_direct_plan_uses_configured_publisher() {
        let cli = cli(&[
            "weather-tui",
            "--rpc-endpoint",
            "tcp://remote:43001",
            "status",
        ]);
        let calls = Cell::new(0);

        let plan = connection_plan_with_fallback(&cli, || {
            calls.set(calls.get() + 1);
            Ok(endpoints("tcp://config:43001", "tcp://config:43002"))
        })
        .unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://remote:43001", "tcp://config:43002"))
        );
    }

    #[test]
    fn publisher_only_direct_plan_uses_configured_rpc() {
        let cli = cli(&[
            "weather-tui",
            "--pub-endpoint",
            "tcp://remote:44002",
            "status",
        ]);
        let calls = Cell::new(0);

        let plan = connection_plan_with_fallback(&cli, || {
            calls.set(calls.get() + 1);
            Ok(endpoints("tcp://config:44001", "tcp://config:44002"))
        })
        .unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://config:44001", "tcp://remote:44002"))
        );
    }

    #[test]
    fn legacy_rpc_only_direct_plan_uses_configured_publisher() {
        let cli = cli(&["weather-tui", "--endpoint", "tcp://remote:45001", "status"]);

        let plan = connection_plan_with_fallback(&cli, || {
            Ok(endpoints("tcp://config:45001", "tcp://config:45002"))
        })
        .unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://remote:45001", "tcp://config:45002"))
        );
    }

    #[test]
    fn single_endpoint_reads_missing_side_from_selected_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        fs::write(
            &config_path,
            r#"
config_version = 1

[engine]
request_timeout_ms = 3000

[ipc]
transport = "tcp"
rpc_endpoint = "tcp://config:46001"
pub_endpoint = "tcp://config:46002"

[daemon]
foreground = true
"#,
        )
        .unwrap();
        let cli = Cli::parse_from([
            "weather-tui",
            "--config",
            config_path.to_str().unwrap(),
            "--rpc-endpoint",
            "tcp://remote:46001",
            "status",
        ]);

        let plan = connection_plan(&cli).unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://remote:46001", "tcp://config:46002"))
        );
    }

    #[test]
    fn single_endpoint_reads_future_config_shape_without_legacy_fields() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        fs::write(
            &config_path,
            r#"
config_version = 2

[ipc]
rpc_endpoint = "tcp://config:46101"
pub_endpoint = "tcp://config:46102"

[db]
path = "weather.db"
"#,
        )
        .unwrap();
        let cli = Cli::parse_from([
            "weather-tui",
            "--config",
            config_path.to_str().unwrap(),
            "--pub-endpoint",
            "tcp://remote:46102",
            "status",
        ]);

        let plan = connection_plan(&cli).unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://config:46101", "tcp://remote:46102"))
        );
    }

    #[test]
    fn single_endpoint_uses_defaults_without_creating_missing_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("missing.toml");
        let cli = Cli::parse_from([
            "weather-tui",
            "--config",
            config_path.to_str().unwrap(),
            "--pub-endpoint",
            "tcp://remote:46502",
            "status",
        ]);

        let plan = connection_plan(&cli).unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints(
                weather_schema::DEFAULT_ZMQ_RPC_ENDPOINT,
                "tcp://remote:46502"
            ))
        );
        assert!(!config_path.exists());
    }

    #[test]
    fn complete_direct_endpoints_ignore_invalid_selected_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("invalid.toml");
        fs::write(&config_path, "this is not valid TOML = [").unwrap();
        let cli = Cli::parse_from([
            "weather-tui",
            "--config",
            config_path.to_str().unwrap(),
            "--rpc-endpoint",
            "tcp://remote:47001",
            "--pub-endpoint",
            "tcp://remote:47002",
            "status",
        ]);

        let plan = connection_plan(&cli).unwrap();

        assert_eq!(
            plan,
            ConnectionPlan::Direct(endpoints("tcp://remote:47001", "tcp://remote:47002"))
        );
    }

    #[test]
    fn planner_defensively_rejects_legacy_and_canonical_rpc_endpoints() {
        let mut cli = cli(&["weather-tui", "status"]);
        cli.endpoint = Some("tcp://remote:48001".to_string());
        cli.rpc_endpoint = Some("tcp://remote:48002".to_string());

        let error = connection_plan_with_fallback(&cli, || {
            panic!("conflicting endpoints must fail before loading fallback")
        })
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "--endpoint cannot be used with --rpc-endpoint"
        );
    }
}
