use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(version, about = "Weather engine daemon/supervisor")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    Run {
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// Override engine.log_level for this daemon process.
        #[arg(long, value_enum)]
        log_level: Option<DaemonLogLevel>,
        #[arg(long)]
        foreground: bool,
        #[arg(long, requires = "foreground", hide = true)]
        owner_token: Option<String>,
        /// Run under the Windows Service Control Manager dispatcher.
        #[arg(long, hide = true, conflicts_with = "foreground")]
        windows_service: bool,
    },
    Probe {
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        #[arg(long)]
        verbose: bool,
    },
    Status {
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        #[arg(long)]
        verbose: bool,
    },
    /// Gracefully stop the active daemon for this configuration.
    Stop {
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
    },
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DaemonLogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl DaemonLogLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// 安装服务。Linux 默认 user scope；Windows SCM 必须使用 --system。
    Install {
        /// 服务后端：systemd 用于 Linux；windows 用于 Windows SCM。
        backend: ServiceBackend,
        /// system scope：Linux 使用 /opt/weather；Windows 使用 ProgramData\Weather。
        #[arg(long)]
        system: bool,
        /// 覆盖 base path。
        #[arg(long)]
        path: Option<PathBuf>,
        /// 指定 config 路径(默认 <base>/config/weather.toml)。
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// 只安装文件/服务布局，不修改 systemd 或 Windows SCM 状态。
        #[arg(long)]
        no_modification_service: bool,
    },
    /// 重新安装并启动 systemd unit 或 Windows SCM service。
    Reinstall {
        /// 服务后端：systemd 用于 Linux；windows 用于 Windows SCM。
        backend: ServiceBackend,
        /// system scope；Windows SCM 必须启用。
        #[arg(long)]
        system: bool,
        /// 覆盖 base path。
        #[arg(long)]
        path: Option<PathBuf>,
        /// 指定 config 路径(默认 <base>/config/weather.toml)。
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// 只重新安装文件/服务布局，不修改 systemd 或 Windows SCM 状态。
        #[arg(long)]
        no_modification_service: bool,
    },
    /// 卸载服务,可选清理数据/二进制。
    Remove {
        backend: ServiceBackend,
        /// 删除 system scope；Windows SCM 必须启用。
        #[arg(long)]
        system: bool,
        /// 安装时使用的 base path。
        #[arg(long)]
        path: Option<PathBuf>,
        /// 安装时使用的 config 路径（默认 <base>/config/weather.toml）。
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// 同时删除 config / db / lock 数据。
        #[arg(long)]
        with_data: bool,
        /// 同时删除 bin 目录。
        #[arg(long)]
        with_bin: bool,
        /// 删除全部 data/bin，并在最后删除 component manifest。
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ServiceBackend {
    /// Linux systemd user/system service。
    Systemd,
    /// Windows Service Control Manager system service。
    Windows,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_foreground_owner_token() {
        let cli = Cli::parse_from([
            "weather-daemon",
            "run",
            "--foreground",
            "--owner-token",
            "owner-token",
        ]);

        let Command::Run {
            foreground,
            owner_token,
            ..
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert!(foreground);
        assert_eq!(owner_token.as_deref(), Some("owner-token"));
    }

    #[test]
    fn parses_run_log_level_override() {
        let cli = Cli::parse_from(["weather-daemon", "run", "--log-level", "debug"]);

        let Command::Run { log_level, .. } = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(log_level, Some(DaemonLogLevel::Debug));
    }

    #[test]
    fn rejects_unknown_run_log_level() {
        assert!(Cli::try_parse_from(["weather-daemon", "run", "--log-level", "verbose"]).is_err());
    }

    #[test]
    fn owner_token_requires_foreground_mode() {
        assert!(
            Cli::try_parse_from(["weather-daemon", "run", "--owner-token", "owner-token",])
                .is_err()
        );
    }

    #[test]
    fn parses_windows_service_mode_and_rejects_foreground() {
        let cli = Cli::parse_from(["weather-daemon", "run", "--windows-service"]);
        let Command::Run {
            windows_service, ..
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert!(windows_service);
        assert!(
            Cli::try_parse_from(["weather-daemon", "run", "--windows-service", "--foreground",])
                .is_err()
        );
    }

    #[test]
    fn parses_service_reinstall_systemd_with_install_options() {
        let cli = Cli::parse_from([
            "weather-daemon",
            "service",
            "reinstall",
            "systemd",
            "--system",
            "--path",
            "/tmp/weather",
            "--config",
            "/tmp/weather/weather.toml",
        ]);

        let Command::Service { command } = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Reinstall {
            backend,
            system,
            path,
            config,
            no_modification_service,
        } = command
        else {
            panic!("expected reinstall command");
        };
        assert!(matches!(backend, ServiceBackend::Systemd));
        assert!(system);
        assert_eq!(path.as_deref(), Some(std::path::Path::new("/tmp/weather")));
        assert_eq!(
            config.as_deref(),
            Some(std::path::Path::new("/tmp/weather/weather.toml"))
        );
        assert!(!no_modification_service);
    }

    #[test]
    fn parses_service_install_no_modification_service() {
        let cli = Cli::parse_from([
            "weather-daemon",
            "service",
            "install",
            "systemd",
            "--no-modification-service",
        ]);

        let Command::Service { command } = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Install {
            no_modification_service,
            ..
        } = command
        else {
            panic!("expected install command");
        };
        assert!(no_modification_service);
    }

    #[test]
    fn parses_service_reinstall_no_modification_service() {
        let cli = Cli::parse_from([
            "weather-daemon",
            "service",
            "reinstall",
            "systemd",
            "--no-modification-service",
        ]);

        let Command::Service { command } = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Reinstall {
            no_modification_service,
            ..
        } = command
        else {
            panic!("expected reinstall command");
        };
        assert!(no_modification_service);
    }

    #[test]
    fn parses_service_remove_layout_options() {
        let cli = Cli::parse_from([
            "weather-daemon",
            "service",
            "remove",
            "systemd",
            "--system",
            "--path",
            "/srv/weather",
            "--config",
            "/etc/weather/weather.toml",
            "--all",
        ]);

        let Command::Service { command } = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Remove {
            backend,
            system,
            path,
            config,
            with_data,
            with_bin,
            all,
        } = command
        else {
            panic!("expected remove command");
        };
        assert!(matches!(backend, ServiceBackend::Systemd));
        assert!(system);
        assert_eq!(path.as_deref(), Some(std::path::Path::new("/srv/weather")));
        assert_eq!(
            config.as_deref(),
            Some(std::path::Path::new("/etc/weather/weather.toml"))
        );
        assert!(!with_data);
        assert!(!with_bin);
        assert!(all);
    }

    #[test]
    fn parses_status_probe_options() {
        let cli = Cli::parse_from([
            "weather-daemon",
            "status",
            "--config",
            "/tmp/weather.toml",
            "--verbose",
        ]);

        let Command::Status { config, verbose } = cli.command else {
            panic!("expected status command");
        };
        assert_eq!(
            config.as_deref(),
            Some(std::path::Path::new("/tmp/weather.toml"))
        );
        assert!(verbose);
    }

    #[test]
    fn parses_stop_config() {
        let cli = Cli::parse_from(["weather-daemon", "stop", "--config", "/tmp/weather.toml"]);

        let Command::Stop { config } = cli.command else {
            panic!("expected stop command");
        };
        assert_eq!(
            config.as_deref(),
            Some(std::path::Path::new("/tmp/weather.toml"))
        );
    }
}
