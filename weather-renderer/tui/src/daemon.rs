use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::Cli;

pub(crate) struct DaemonSupervisor {
    exe: PathBuf,
    config: Option<PathBuf>,
}

pub(crate) struct ForegroundDaemon {
    child: Child,
}

impl DaemonSupervisor {
    pub(crate) fn from_cli(cli: &Cli) -> Result<Self> {
        Ok(Self {
            exe: cli.daemon_exe.clone().unwrap_or(resolve_daemon_exe()?),
            config: cli.config.clone(),
        })
    }

    pub(crate) fn start_foreground(&self) -> Result<ForegroundDaemon> {
        let mut command = Command::new(&self.exe);
        command.arg("run").arg("--foreground");
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start foreground daemon {}", self.exe.display()))?;
        Ok(ForegroundDaemon { child })
    }

    pub(crate) fn probe(&self) -> Result<DaemonProbe> {
        let mut command = Command::new(&self.exe);
        command.arg("probe").arg("--verbose");
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        let output = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("failed to run daemon probe {}", self.exe.display()))?;
        if !output.status.success() {
            bail!("daemon probe failed with status {}", output.status);
        }
        serde_json::from_slice(&output.stdout).context("failed to parse daemon probe status")
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonProbe {
    pub(crate) state: DaemonProbeState,
    pub(crate) rpc_endpoint: String,
    pub(crate) pub_endpoint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DaemonProbeState {
    Running,
    NotRunning,
}

impl Drop for ForegroundDaemon {
    fn drop(&mut self) {
        // 先等 engine 自行退出(应为已收到 shutdown RPC);超时才 SIGKILL 兜底。
        if self.child.try_wait().ok().flatten().is_none() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn resolve_daemon_exe() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current
        .parent()
        .context("current executable has no parent")?;
    let name = if cfg!(windows) {
        "weather-daemon.exe"
    } else {
        "weather-daemon"
    };
    let sibling = dir.join(name);
    if sibling.exists() {
        return Ok(sibling);
    }
    Ok(PathBuf::from(name))
}
