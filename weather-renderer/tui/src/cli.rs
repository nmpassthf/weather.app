use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(version, about = "TUI weather renderer backed by weather-engine")]
pub(crate) struct Cli {
    #[arg(long, value_enum, default_value_t = OutputFormat::Tui, global = true)]
    pub(crate) format: OutputFormat,
    #[arg(long, global = true)]
    pub(crate) endpoint: Option<String>,
    #[arg(long, global = true)]
    pub(crate) rpc_endpoint: Option<String>,
    #[arg(long, global = true)]
    pub(crate) pub_endpoint: Option<String>,
    #[arg(long, short = 'c', global = true)]
    pub(crate) config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub(crate) daemon_exe: Option<PathBuf>,
    #[arg(long, global = true, default_value = "disabled")]
    pub(crate) hmac: String,
    #[arg(long, global = true, default_value = "weather-dev-default-key")]
    pub(crate) hmac_key: String,
    #[arg(long, global = true)]
    pub(crate) hmac_env_key_name: Option<String>,
    #[arg(long)]
    pub(crate) core_get_default_config: bool,
    #[arg(long)]
    pub(crate) core_get_config: bool,
    #[arg(long)]
    pub(crate) core_restart_engine: bool,
    #[arg(long, global = true)]
    pub(crate) include_debug: bool,
    #[command(subcommand)]
    pub(crate) command: Option<CommandKind>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum OutputFormat {
    Tui,
    Json,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CommandKind {
    Once {
        #[arg(long, short = 'a')]
        address: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
    Search {
        query: Option<String>,
        #[arg(long)]
        province: Option<String>,
        #[arg(long)]
        city: Option<String>,
        #[arg(long)]
        station: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
        #[arg(long, short = 'w')]
        write: bool,
    },
    Add {
        name: String,
        #[arg(long)]
        province: Option<String>,
        #[arg(long)]
        city: Option<String>,
        #[arg(long)]
        station: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Stations {
        #[command(subcommand)]
        command: StationsCommand,
    },
    Status,
    Kill,
}

#[derive(Debug, Subcommand)]
pub(crate) enum StationsCommand {
    List,
    Search {
        query: Option<String>,
        #[arg(long)]
        province: Option<String>,
        #[arg(long)]
        city: Option<String>,
        #[arg(long)]
        station: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Add {
        name: String,
        #[arg(long)]
        province: Option<String>,
        #[arg(long)]
        city: Option<String>,
        #[arg(long)]
        station: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Remove {
        selector: String,
    },
    Enable {
        selector: String,
    },
    Disable {
        selector: String,
    },
    Move {
        from: String,
        to: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_rejects_internal_province_identifier_flag() {
        let parsed = Cli::try_parse_from([
            "weather-tui",
            "search",
            "北京",
            "--provider-province-code",
            "ABJ",
        ]);

        assert!(parsed.is_err());
    }

    #[test]
    fn search_accepts_public_province_filter() {
        let parsed =
            Cli::try_parse_from(["weather-tui", "search", "北京", "--province", "北京市"]).unwrap();

        match parsed.command {
            Some(CommandKind::Search { province, .. }) => {
                assert_eq!(province.as_deref(), Some("北京市"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn include_debug_is_explicit_global_flag() {
        let parsed =
            Cli::try_parse_from(["weather-tui", "--include-debug", "--format", "json", "once"])
                .unwrap();

        assert!(parsed.include_debug);
    }
}
