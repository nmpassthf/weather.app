mod cli;
mod logging;
mod path;
mod probe;
mod run;
mod service;
mod stop;

use anyhow::Result;
use clap::Parser;

use crate::{
    cli::{Cli, Command, ServiceCommand},
    probe::probe,
    run::run,
    service::{install_service, reinstall_service, uninstall_service},
    stop::stop,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            config,
            log_level,
            foreground,
            owner_token,
        } => run(config, log_level, foreground, owner_token).await,
        Command::Probe { config, verbose } => probe(config, verbose).await,
        Command::Service { command } => match command {
            ServiceCommand::Install {
                backend,
                system,
                path,
                config,
                no_modification_service,
            } => install_service(backend, system, path, config, !no_modification_service),
            ServiceCommand::Reinstall {
                backend,
                system,
                path,
                config,
                no_modification_service,
            } => reinstall_service(backend, system, path, config, !no_modification_service),
            ServiceCommand::Remove {
                backend,
                system,
                path,
                config,
                with_data,
                with_bin,
                all,
            } => uninstall_service(backend, system, path, config, with_data, with_bin, all),
        },
        Command::Status { config, verbose } => probe(config, verbose).await,
        Command::Stop { config } => stop(config).await,
    }
}
