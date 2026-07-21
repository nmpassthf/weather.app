use std::{ffi::OsString, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(version, about = "TUI weather renderer backed by weather-engine")]
pub(crate) struct Cli {
    #[arg(long, value_enum, default_value_t = OutputFormat::Tui, global = true)]
    pub(crate) format: OutputFormat,
    #[arg(long, global = true, conflicts_with = "rpc_endpoint")]
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
    #[arg(long, hide = true)]
    pub(crate) core_get_default_config: bool,
    #[arg(long, hide = true)]
    pub(crate) core_get_config: bool,
    #[arg(long, hide = true)]
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
    Stations {
        #[command(subcommand)]
        command: StationsCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    #[command(hide = true)]
    Search(LegacySearchArgs),
    #[command(hide = true)]
    Add(StationAddArgs),
    #[command(hide = true)]
    Status,
    #[command(hide = true)]
    Kill,
}

#[derive(Debug, Subcommand)]
pub(crate) enum StationsCommand {
    List,
    Search(StationSearchArgs),
    Add(StationAddArgs),
    Remove { selector: String },
    Enable { selector: String },
    Disable { selector: String },
    Move { from: String, to: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    Show,
    Defaults,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EngineCommand {
    Status,
    Restart,
    Stop,
}

#[derive(Debug, Args)]
pub(crate) struct StationFilterArgs {
    #[arg(long)]
    pub(crate) province: Option<String>,
    #[arg(long)]
    pub(crate) city: Option<String>,
    #[arg(long)]
    pub(crate) station: Option<String>,
    #[arg(long, default_value_t = 10)]
    pub(crate) limit: u32,
}

#[derive(Debug, Args)]
pub(crate) struct StationSearchArgs {
    pub(crate) query: Option<String>,
    #[command(flatten)]
    pub(crate) filters: StationFilterArgs,
}

#[derive(Debug, Args)]
pub(crate) struct StationAddArgs {
    pub(crate) name: String,
    #[command(flatten)]
    pub(crate) filters: StationFilterArgs,
}

#[derive(Debug, Args)]
pub(crate) struct LegacySearchArgs {
    #[command(flatten)]
    pub(crate) search: StationSearchArgs,
    #[arg(long, short = 'w')]
    pub(crate) write: bool,
}

impl Cli {
    pub(crate) fn stops_engine(&self) -> bool {
        matches!(
            self.command.as_ref(),
            Some(CommandKind::Engine {
                command: EngineCommand::Stop
            }) | Some(CommandKind::Kill)
        )
    }

    pub(crate) fn has_action(&self) -> bool {
        self.command.is_some()
            || self.core_get_default_config
            || self.core_get_config
            || self.core_restart_engine
    }
}

pub(crate) fn parse_cli_from(args: impl IntoIterator<Item = OsString>) -> Cli {
    Cli::parse_from(normalize_args(args))
}

fn normalize_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    args.into_iter()
        .map(|arg| match arg.to_str() {
            Some("-core-dump-default-config") => OsString::from("--core-get-default-config"),
            Some("-core-show-current-config") | Some("-core-show-currnet-config") => {
                OsString::from("--core-get-config")
            }
            Some("-core-restart-engine") => OsString::from("--core-restart-engine"),
            _ => arg,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    fn parse_compat(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(normalize_args(args.iter().map(OsString::from)))
    }

    #[test]
    fn canonical_command_matrix_parses() {
        for args in [
            &["weather-tui", "once"][..],
            &["weather-tui", "stations", "list"],
            &["weather-tui", "stations", "search", "北京"],
            &["weather-tui", "stations", "add", "北京-北京市"],
            &["weather-tui", "stations", "remove", "1"],
            &["weather-tui", "stations", "enable", "1"],
            &["weather-tui", "stations", "disable", "1"],
            &["weather-tui", "stations", "move", "1", "2"],
            &["weather-tui", "config", "show"],
            &["weather-tui", "config", "defaults"],
            &["weather-tui", "engine", "status"],
            &["weather-tui", "engine", "restart"],
            &["weather-tui", "engine", "stop"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
        }
    }

    #[test]
    fn canonical_search_accepts_public_filters() {
        let parsed = Cli::try_parse_from([
            "weather-tui",
            "stations",
            "search",
            "北京",
            "--province",
            "北京市",
            "--city",
            "朝阳",
            "--limit",
            "25",
        ])
        .unwrap();

        let Some(CommandKind::Stations {
            command: StationsCommand::Search(search),
        }) = parsed.command
        else {
            panic!("expected canonical station search");
        };
        assert_eq!(search.query.as_deref(), Some("北京"));
        assert_eq!(search.filters.province.as_deref(), Some("北京市"));
        assert_eq!(search.filters.city.as_deref(), Some("朝阳"));
        assert_eq!(search.filters.limit, 25);
    }

    #[test]
    fn compatibility_commands_still_parse() {
        let search = Cli::try_parse_from(["weather-tui", "search", "北京", "--write"]).unwrap();
        assert!(matches!(
            search.command,
            Some(CommandKind::Search(LegacySearchArgs { write: true, .. }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["weather-tui", "add", "北京"])
                .unwrap()
                .command,
            Some(CommandKind::Add(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["weather-tui", "status"])
                .unwrap()
                .command,
            Some(CommandKind::Status)
        ));
        assert!(matches!(
            Cli::try_parse_from(["weather-tui", "kill"])
                .unwrap()
                .command,
            Some(CommandKind::Kill)
        ));
    }

    #[test]
    fn compatibility_core_flags_and_single_dash_spellings_still_parse() {
        assert!(
            parse_compat(&["weather-tui", "--core-get-default-config"])
                .unwrap()
                .core_get_default_config
        );
        assert!(
            parse_compat(&["weather-tui", "--core-get-config"])
                .unwrap()
                .core_get_config
        );
        assert!(
            parse_compat(&["weather-tui", "--core-restart-engine"])
                .unwrap()
                .core_restart_engine
        );
        assert!(
            parse_compat(&["weather-tui", "-core-dump-default-config"])
                .unwrap()
                .core_get_default_config
        );
        assert!(
            parse_compat(&["weather-tui", "-core-show-current-config"])
                .unwrap()
                .core_get_config
        );
        assert!(
            parse_compat(&["weather-tui", "-core-show-currnet-config"])
                .unwrap()
                .core_get_config
        );
        assert!(
            parse_compat(&["weather-tui", "-core-restart-engine"])
                .unwrap()
                .core_restart_engine
        );
    }

    #[test]
    fn compatibility_commands_and_flags_are_hidden_from_help() {
        let help = Cli::command().render_long_help().to_string();

        for canonical in ["\n  once", "\n  stations", "\n  config", "\n  engine"] {
            assert!(
                help.contains(canonical),
                "missing canonical command {canonical}"
            );
        }
        for hidden in [
            "\n  search",
            "\n  add",
            "\n  status",
            "\n  kill",
            "--core-get-default-config",
            "--core-get-config",
            "--core-restart-engine",
        ] {
            assert!(!help.contains(hidden), "legacy spelling leaked: {hidden}");
        }
    }

    #[test]
    fn canonical_search_rejects_internal_provider_filter() {
        let parsed = Cli::try_parse_from([
            "weather-tui",
            "stations",
            "search",
            "北京",
            "--provider-province-code",
            "ABJ",
        ]);

        assert!(parsed.is_err());
    }

    #[test]
    fn canonical_and_compatibility_stop_commands_share_the_predicate() {
        let canonical = Cli::try_parse_from(["weather-tui", "engine", "stop"]).unwrap();
        let compatibility = Cli::try_parse_from(["weather-tui", "kill"]).unwrap();
        let status = Cli::try_parse_from(["weather-tui", "engine", "status"]).unwrap();

        assert!(canonical.stops_engine());
        assert!(compatibility.stops_engine());
        assert!(!status.stops_engine());
    }

    #[test]
    fn no_command_has_no_action_but_compatibility_flags_do() {
        assert!(!Cli::try_parse_from(["weather-tui"]).unwrap().has_action());
        assert!(
            Cli::try_parse_from(["weather-tui", "--core-get-config"])
                .unwrap()
                .has_action()
        );
    }

    #[test]
    fn include_debug_is_explicit_global_flag() {
        let parsed =
            Cli::try_parse_from(["weather-tui", "--include-debug", "--format", "json", "once"])
                .unwrap();

        assert!(parsed.include_debug);
    }

    #[test]
    fn legacy_and_canonical_rpc_endpoints_conflict() {
        let error = Cli::try_parse_from([
            "weather-tui",
            "--endpoint",
            "tcp://127.0.0.1:41001",
            "--rpc-endpoint",
            "tcp://127.0.0.1:41002",
            "engine",
            "status",
        ])
        .unwrap_err();

        let rendered = error.to_string();
        assert!(rendered.contains("--endpoint"));
        assert!(rendered.contains("--rpc-endpoint"));
    }
}
