use anyhow::{Result, bail};
use serde::Serialize;
use weather_schema::*;

use crate::{
    cli::{
        Cli, CommandKind, ConfigCommand, EngineCommand, OutputFormat, StationAddArgs,
        StationFilterArgs, StationSearchArgs, StationsCommand,
    },
    client::{EngineClient, require_config},
    render::{
        render_configured_stations, render_search_results, render_station_candidates,
        render_station_change, render_status, render_weather,
    },
    search::execute_search,
};

#[derive(Debug, Serialize)]
struct StationChangeOutput {
    message: String,
    stations: Vec<ConfiguredStation>,
}

struct StationUpsertOutput {
    station_name: String,
    already_configured: bool,
    config: AppConfig,
}

pub(crate) async fn run_command(client: &EngineClient, cli: &Cli) -> Result<()> {
    if cli.core_get_default_config {
        return print_config(client, true).await;
    }
    if cli.core_get_config {
        return print_config(client, false).await;
    }
    if cli.core_restart_engine {
        return restart_engine(client).await;
    }

    match &cli.command {
        Some(CommandKind::Once { address, refresh }) => {
            let snapshot =
                fetch_weather(client, address.as_ref(), *refresh, cli.include_debug).await?;
            output(cli.format, &snapshot, || render_weather(&snapshot))
        }
        Some(CommandKind::Search(args)) => {
            let results = search_stations(client, &args.search).await?;
            if args.write {
                upsert_search_result(client, &results, "名称.N").await?;
            }
            output(cli.format, &results, || render_search_results(&results))
        }
        Some(CommandKind::Add(args)) => {
            let results = search_station_to_add(client, args).await?;
            upsert_search_result(client, &results, "名称.N").await?;
            output(cli.format, &results, || render_search_results(&results))
        }
        Some(CommandKind::Stations { command }) => run_stations_command(client, cli, command).await,
        Some(CommandKind::Config { command }) => match command {
            ConfigCommand::Show => print_config(client, false).await,
            ConfigCommand::Defaults => print_config(client, true).await,
        },
        Some(CommandKind::Engine { command }) => match command {
            EngineCommand::Status => output_engine_status(client, cli.format).await,
            EngineCommand::Restart => restart_engine(client).await,
            EngineCommand::Stop => {
                unreachable!("engine stop is handled before engine auto-start")
            }
        },
        Some(CommandKind::Status) => output_engine_status(client, cli.format).await,
        Some(CommandKind::Kill) => unreachable!("kill is handled before engine auto-start"),
        None => run_weather(client, cli).await,
    }
}

async fn run_stations_command(
    client: &EngineClient,
    cli: &Cli,
    command: &StationsCommand,
) -> Result<()> {
    match command {
        StationsCommand::List => {
            let resp = client.all_configured_stations().await?;
            output(cli.format, &resp, || {
                render_configured_stations(&resp.stations)
            })
        }
        StationsCommand::Search(args) => {
            let results = search_stations(client, args).await?;
            output(cli.format, &results, || render_search_results(&results))
        }
        StationsCommand::Add(args) => {
            let results = search_station_to_add(client, args).await?;
            let change = upsert_search_result(client, &results, "stations add 名称.N").await?;
            let message = if change.already_configured {
                format!("已启用已有站点 `{}`", change.station_name)
            } else {
                format!("已添加站点 `{}`", change.station_name)
            };
            output_station_change(cli.format, message, &change.config.stations)
        }
        StationsCommand::Remove { selector } => {
            let mut config = current_config(client).await?;
            let name = remove_station(&mut config, selector)?;
            let updated = update_config(client, config).await?;
            output_station_change(
                cli.format,
                format!("已删除站点 `{name}`"),
                &updated.stations,
            )
        }
        StationsCommand::Enable { selector } => {
            set_station_state(client, cli, selector, true).await
        }
        StationsCommand::Disable { selector } => {
            set_station_state(client, cli, selector, false).await
        }
        StationsCommand::Move { from, to } => {
            let mut config = current_config(client).await?;
            let (name, from_index, to_index) = move_station(&mut config, from, to)?;
            let updated = update_config(client, config).await?;
            output_station_change(
                cli.format,
                format!(
                    "已移动站点 `{name}`：{} -> {}",
                    from_index + 1,
                    to_index + 1
                ),
                &updated.stations,
            )
        }
    }
}

async fn set_station_state(
    client: &EngineClient,
    cli: &Cli,
    selector: &str,
    enabled: bool,
) -> Result<()> {
    let mut config = current_config(client).await?;
    let name = set_station_enabled(&mut config, selector, enabled)?;
    let updated = update_config(client, config).await?;
    let action = if enabled { "已启用" } else { "已停用" };
    output_station_change(
        cli.format,
        format!("{action}站点 `{name}`"),
        &updated.stations,
    )
}

async fn print_config(client: &EngineClient, defaults: bool) -> Result<()> {
    let operation = if defaults {
        "get-default-config"
    } else {
        "get-config"
    };
    let response = client.get_config(defaults).await?;
    let config = require_config(response.config, operation)?;
    let toml = toml::to_string_pretty(&config)
        .map_err(|error| anyhow::anyhow!("failed to serialize config: {error}"))?;
    print!("{toml}");
    Ok(())
}

async fn restart_engine(client: &EngineClient) -> Result<()> {
    let _: Empty = client.request(RpcKind::RestartEngine, Empty {}).await?;
    println!("engine restart accepted");
    Ok(())
}

async fn output_engine_status(client: &EngineClient, format: OutputFormat) -> Result<()> {
    let status = client.status().await?;
    output(format, &status, || render_status(&status))
}

async fn search_stations(
    client: &EngineClient,
    args: &StationSearchArgs,
) -> Result<FuzzyMatchStationsResponse> {
    search_with_filters(client, args.query.as_ref(), &args.filters).await
}

async fn search_station_to_add(
    client: &EngineClient,
    args: &StationAddArgs,
) -> Result<FuzzyMatchStationsResponse> {
    search_with_filters(client, Some(&args.name), &args.filters).await
}

async fn search_with_filters(
    client: &EngineClient,
    query: Option<&String>,
    filters: &StationFilterArgs,
) -> Result<FuzzyMatchStationsResponse> {
    execute_search(
        client,
        query,
        &filters.province,
        &filters.city,
        &filters.station,
        filters.limit,
    )
    .await
}

async fn upsert_search_result(
    client: &EngineClient,
    results: &FuzzyMatchStationsResponse,
    selector_hint: &str,
) -> Result<StationUpsertOutput> {
    let station_name = unique_station_name(results, selector_hint)?;
    let mut config = current_config(client).await?;
    let already_configured = upsert_station(&mut config, station_name.clone());
    let config = update_config(client, config).await?;
    Ok(StationUpsertOutput {
        station_name,
        already_configured,
        config,
    })
}

fn unique_station_name(
    results: &FuzzyMatchStationsResponse,
    selector_hint: &str,
) -> Result<String> {
    if results.stations.len() != 1 {
        bail!(
            "命中 {} 个站点目标，请使用 `{selector_hint}` 指定要写入的目标。\n{}",
            results.stations.len(),
            render_station_candidates(results)
        );
    }
    Ok(results.stations[0].name.clone())
}

async fn current_config(client: &EngineClient) -> Result<AppConfig> {
    let resp = client.get_config(false).await?;
    require_config(resp.config, "get-config")
}

async fn update_config(client: &EngineClient, config: AppConfig) -> Result<AppConfig> {
    let resp = client.update_config(config).await?;
    require_config(resp.config, "update-config")
}

fn upsert_station(config: &mut AppConfig, station_name: String) -> bool {
    if let Some(existing) = config
        .stations
        .iter_mut()
        .find(|station| station.name == station_name)
    {
        existing.enabled = true;
        true
    } else {
        config.stations.push(StationConfig {
            name: station_name,
            enabled: true,
        });
        false
    }
}

fn remove_station(config: &mut AppConfig, selector: &str) -> Result<String> {
    let index = resolve_station_selector(&config.stations, selector)?;
    Ok(config.stations.remove(index).name)
}

fn move_station(config: &mut AppConfig, from: &str, to: &str) -> Result<(String, usize, usize)> {
    let from_index = resolve_station_selector(&config.stations, from)?;
    let to_index = resolve_station_selector(&config.stations, to)?;
    let station = config.stations.remove(from_index);
    let name = station.name.clone();
    config.stations.insert(to_index, station);
    Ok((name, from_index, to_index))
}

fn set_station_enabled(config: &mut AppConfig, selector: &str, enabled: bool) -> Result<String> {
    let index = resolve_station_selector(&config.stations, selector)?;
    let station = &mut config.stations[index];
    station.enabled = enabled;
    Ok(station.name.clone())
}

async fn run_weather(client: &EngineClient, cli: &Cli) -> Result<()> {
    let snapshot = fetch_weather(client, None, false, cli.include_debug).await?;
    output(cli.format, &snapshot, || render_weather(&snapshot))
}

async fn fetch_weather(
    client: &EngineClient,
    address: Option<&String>,
    refresh: bool,
    include_debug: bool,
) -> Result<WeatherSnapshot> {
    let unified_uuid = if let Some(address) = address {
        let station = resolve_address(client, address).await?;
        if station.unified_uuid.is_empty() {
            unified_station_uuid(&station.name)
        } else {
            station.unified_uuid
        }
    } else {
        String::new()
    };
    client
        .request::<GetWeatherRequest, WeatherSnapshot>(
            RpcKind::GetWeather,
            GetWeatherRequest {
                unified_uuid,
                refresh,
                include_debug,
            },
        )
        .await
}

async fn resolve_address(client: &EngineClient, address: &str) -> Result<StationRef> {
    let address = normalize_address(address)?;
    let results = execute_search(
        client,
        Some(&address),
        &None,
        &None,
        &Some(address.clone()),
        10,
    )
    .await?;
    if results.stations.len() != 1 {
        bail!(
            "地址 `{address}` 命中 {} 个站点目标，请使用 `<省>-<市>[-站点]` 精确地址。\n{}",
            results.stations.len(),
            render_station_candidates(&results)
        );
    }
    Ok(results.stations[0].clone())
}

fn normalize_address(address: &str) -> Result<String> {
    let parts = address.split('-').map(str::trim).collect::<Vec<_>>();
    if !matches!(parts.len(), 2 | 3) || parts.iter().any(|part| part.is_empty()) {
        bail!("address must be `<省>-<市>[-站点]`");
    }
    Ok(parts.join("-"))
}

fn output_station_change(
    format: OutputFormat,
    message: String,
    stations: &[StationConfig],
) -> Result<()> {
    let configured: Vec<ConfiguredStation> = stations
        .iter()
        .map(|s| ConfiguredStation {
            name: s.name.clone(),
            enabled: s.enabled,
        })
        .collect();
    let value = StationChangeOutput {
        message,
        stations: configured,
    };
    output(format, &value, || {
        render_station_change(&value.message, &value.stations)
    })
}

fn resolve_station_selector(stations: &[StationConfig], selector: &str) -> Result<usize> {
    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 || index > stations.len() {
            bail!("station index {index} is out of range");
        }
        return Ok(index - 1);
    }

    let mut matches = stations
        .iter()
        .enumerate()
        .filter(|(_, station)| station.name == selector);
    let Some((index, _)) = matches.next() else {
        bail!("station `{selector}` is not configured");
    };
    if matches.next().is_some() {
        bail!("station `{selector}` matches more than one configured station; use its list index");
    }
    Ok(index)
}

fn output<T: Serialize>(
    format: OutputFormat,
    value: &T,
    tui: impl FnOnce() -> String,
) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", weather_schema::json_pretty(value)?),
        OutputFormat::Tui => println!("{}", tui()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_config() -> AppConfig {
        AppConfig {
            engine: Some(EngineConfig {
                request_timeout_ms: 111,
                startup_timeout_ms: 222,
                lock_path: "custom-engine.lock".to_string(),
            }),
            ipc: Some(IpcConfig {
                transport: "tcp".to_string(),
                rpc_endpoint: "tcp://example:3001".to_string(),
                pub_endpoint: "tcp://example:3002".to_string(),
                hmac: "disabled".to_string(),
                hmac_key: "wire-key".to_string(),
                hmac_env_key_name: "WIRE_KEY".to_string(),
            }),
            db: Some(DbConfig {
                path: "custom.sqlite3".to_string(),
                lock_path: "custom.sqlite3.lock".to_string(),
                timezone: "UTC".to_string(),
            }),
            updater: Some(UpdaterConfig {
                weather_ttl_seconds: 333,
                province_ttl_seconds: 444,
                default_provider: "custom".to_string(),
                provider: vec![ProviderConfig {
                    name: "custom".to_string(),
                    base_url: "https://example.invalid".to_string(),
                    request_timeout_seconds: 555,
                }],
            }),
            daemon: Some(DaemonConfig {
                service_backend: "custom".to_string(),
                foreground: false,
                service_scope: "system".to_string(),
            }),
            stations: vec![
                StationConfig {
                    name: "first".to_string(),
                    enabled: false,
                },
                StationConfig {
                    name: "second".to_string(),
                    enabled: true,
                },
            ],
            config_version: 77,
        }
    }

    fn without_stations(mut config: AppConfig) -> AppConfig {
        config.stations.clear();
        config
    }

    #[test]
    fn station_upsert_preserves_every_non_station_wire_field() {
        let mut config = wire_config();
        let original = without_stations(config.clone());

        assert!(upsert_station(&mut config, "first".to_string()));
        assert!(!upsert_station(&mut config, "third".to_string()));

        assert_eq!(without_stations(config.clone()), original);
        assert!(config.stations[0].enabled);
        assert_eq!(config.stations[2].name, "third");
        assert!(config.stations[2].enabled);
    }

    #[test]
    fn station_edits_preserve_every_non_station_wire_field() {
        let mut config = wire_config();
        let original = without_stations(config.clone());

        assert_eq!(
            set_station_enabled(&mut config, "2", false).unwrap(),
            "second"
        );
        assert_eq!(
            move_station(&mut config, "2", "1").unwrap(),
            ("second".to_string(), 1, 0)
        );
        assert_eq!(remove_station(&mut config, "first").unwrap(), "first");

        assert_eq!(without_stations(config.clone()), original);
        assert_eq!(
            config.stations,
            vec![StationConfig {
                name: "second".to_string(),
                enabled: false,
            }]
        );
    }

    #[test]
    fn station_selector_errors_preserve_canonical_and_compatibility_hints() {
        let results = FuzzyMatchStationsResponse::default();

        let compatibility = unique_station_name(&results, "名称.N")
            .unwrap_err()
            .to_string();
        let canonical = unique_station_name(&results, "stations add 名称.N")
            .unwrap_err()
            .to_string();

        assert!(compatibility.contains("请使用 `名称.N` 指定要写入的目标"));
        assert!(canonical.contains("请使用 `stations add 名称.N` 指定要写入的目标"));
        assert!(compatibility.ends_with("未命中可写入的站点目标。"));
        assert!(canonical.ends_with("未命中可写入的站点目标。"));
    }
}
