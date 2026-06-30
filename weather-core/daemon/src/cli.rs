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
        #[arg(long)]
        foreground: bool,
    },
    Probe {
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        #[arg(long)]
        verbose: bool,
    },
    Status,
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// 安装服务。默认 user 模式(base=~/.weather),--system 装 /opt/weather(需 root)。
    Install {
        /// 服务后端:systemd(Unix)/ windows(Win)。
        backend: ServiceBackend,
        /// 显式 system 模式。默认 user。
        #[arg(long)]
        system: bool,
        /// 覆盖 base path(默认 user: ~/.weather,system: /opt/weather 或 Win Program Files)。
        #[arg(long)]
        path: Option<PathBuf>,
        /// 指定 config 路径(默认 <base>/config/weather.toml)。
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// 只安装文件/unit,不直接修改系统服务状态；打印需要手动执行的 next steps。
        #[arg(long)]
        no_modification_service: bool,
    },
    /// 重新安装已存在的服务，并重载/启动 systemd unit。
    Reinstall {
        /// 服务后端:systemd(Unix)/ windows(Win)。
        backend: ServiceBackend,
        /// 显式 system 模式。默认 user。
        #[arg(long)]
        system: bool,
        /// 覆盖 base path(默认 user: ~/.weather,system: /opt/weather 或 Win Program Files)。
        #[arg(long)]
        path: Option<PathBuf>,
        /// 指定 config 路径(默认 <base>/config/weather.toml)。
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// 只重新安装文件/unit,不直接修改系统服务状态；打印需要手动执行的 next steps。
        #[arg(long)]
        no_modification_service: bool,
    },
    /// 卸载服务,可选清理数据/二进制。
    Remove {
        backend: ServiceBackend,
        /// 同时删除 config / db / lock 数据。
        #[arg(long)]
        with_data: bool,
        /// 同时删除 bin 目录。
        #[arg(long)]
        with_bin: bool,
        /// 等价于 --with-data --with-bin。
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, Clone, ValueEnum)]
pub(crate) enum ServiceBackend {
    Systemd,
    Windows,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
}
