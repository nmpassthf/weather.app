use anyhow::{Result, bail};
use serde::Serialize;
use weather_schema::*;

use crate::{
    cli::{Cli, CommandKind, OutputFormat, StationsCommand},
    client::EngineClient,
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

pub(crate) async fn run_command(client: &EngineClient, cli: &Cli) -> Result<()> {
    if cli.core_get_default_config {
        let resp = client.get_config(true).await?;
        let config = resp.config.unwrap_or_default();
        let toml = toml::to_string_pretty(&config)
            .map_err(|e| anyhow::anyhow!("failed to serialize config: {e}"))?;
        print!("{toml}");
        return Ok(());
    }
    if cli.core_get_config {
        let resp = client.get_config(false).await?;
        let config = resp.config.unwrap_or_default();
        let toml = toml::to_string_pretty(&config)
            .map_err(|e| anyhow::anyhow!("failed to serialize config: {e}"))?;
        print!("{toml}");
        return Ok(());
    }
    if cli.core_restart_engine {
        let _: Empty = client.request(RpcKind::RestartEngine, Empty {}).await?;
        println!("engine restart accepted");
        return Ok(());
    }

    match &cli.command {
        Some(CommandKind::Once { address, refresh }) => {
            let snapshot =
                fetch_weather(client, address.as_ref(), *refresh, cli.include_debug).await?;
            output(cli.format, &snapshot, || render_weather(&snapshot))
        }
        Some(CommandKind::Search {
            query,
            province,
            city,
            station,
            limit,
            write,
        }) => {
            let results =
                execute_search(client, query.as_ref(), province, city, station, *limit).await?;
            if *write {
                write_search_result_to_config(client, &results).await?;
            }
            output(cli.format, &results, || render_search_results(&results))
        }
        Some(CommandKind::Add {
            name,
            province,
            city,
            station,
            limit,
        }) => {
            let results =
                execute_search(client, Some(name), province, city, station, *limit).await?;
            write_search_result_to_config(client, &results).await?;
            output(cli.format, &results, || render_search_results(&results))
        }
        Some(CommandKind::Stations { command }) => run_stations_command(client, cli, command).await,
        Some(CommandKind::Status) => {
            let status = client.status().await?;
            output(cli.format, &status, || render_status(&status))
        }
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
        StationsCommand::Search {
            query,
            province,
            city,
            station,
            limit,
        } => {
            let results =
                execute_search(client, query.as_ref(), province, city, station, *limit).await?;
            output(cli.format, &results, || render_search_results(&results))
        }
        StationsCommand::Add {
            name,
            province,
            city,
            station,
            limit,
        } => {
            let results =
                execute_search(client, Some(name), province, city, station, *limit).await?;
            if results.stations.len() != 1 {
                bail!(
                    "命中 {} 个站点目标，请使用 `stations add 名称.N` 指定要写入的目标。\n{}",
                    results.stations.len(),
                    render_station_candidates(&results)
                );
            }
            let station_name = results.stations[0].name.clone();
            let mut config = current_config(client).await?;
            let already = config.stations.iter().any(|s| s.name == station_name);
            if already {
                if let Some(existing) = config.stations.iter_mut().find(|s| s.name == station_name)
                {
                    existing.enabled = true;
                }
            } else {
                config.stations.push(weather_configure::StationConfig {
                    name: station_name.clone(),
                    enabled: true,
                });
            }
            let updated = update_config(client, config).await?;
            let message = if already {
                format!("已启用已有站点 `{station_name}`")
            } else {
                format!("已添加站点 `{station_name}`")
            };
            output_station_change(cli.format, message, &updated.stations)
        }
        StationsCommand::Remove { selector } => {
            let mut config = current_config(client).await?;
            let index = resolve_station_selector(&config.stations, selector)?;
            let name = config.stations[index].name.clone();
            config.stations.remove(index);
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
            let from_index = resolve_station_selector(&config.stations, from)?;
            let to_index = resolve_station_selector(&config.stations, to)?;
            let name = config.stations[from_index].name.clone();
            let station = config.stations.remove(from_index);
            config.stations.insert(to_index, station);
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
    let index = resolve_station_selector(&config.stations, selector)?;
    let name = config.stations[index].name.clone();
    config.stations[index].enabled = enabled;
    let updated = update_config(client, config).await?;
    let action = if enabled { "已启用" } else { "已停用" };
    output_station_change(
        cli.format,
        format!("{action}站点 `{name}`"),
        &updated.stations,
    )
}

/// 把搜索唯一命中结果写入 config（启用已有或新增）。
async fn write_search_result_to_config(
    client: &EngineClient,
    results: &FuzzyMatchStationsResponse,
) -> Result<()> {
    if results.stations.len() != 1 {
        bail!(
            "命中 {} 个站点目标，请使用 `名称.N` 指定要写入的目标。\n{}",
            results.stations.len(),
            render_station_candidates(results)
        );
    }
    let station_name = results.stations[0].name.clone();
    let mut config = current_config(client).await?;
    if let Some(existing) = config.stations.iter_mut().find(|s| s.name == station_name) {
        existing.enabled = true;
    } else {
        config.stations.push(weather_configure::StationConfig {
            name: station_name,
            enabled: true,
        });
    }
    update_config(client, config).await?;
    Ok(())
}

async fn current_config(client: &EngineClient) -> Result<weather_configure::AppConfig> {
    let resp = client.get_config(false).await?;
    Ok(resp.config.unwrap_or_default().into())
}

async fn update_config(
    client: &EngineClient,
    config: weather_configure::AppConfig,
) -> Result<weather_schema::AppConfig> {
    let resp = client.update_config(config.into()).await?;
    Ok(resp.config.unwrap_or_default())
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
            client
                .resolve_station_uuid(&station.name)
                .await?
                .unified_uuid
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

fn resolve_station_selector(
    stations: &[weather_configure::StationConfig],
    selector: &str,
) -> Result<usize> {
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
