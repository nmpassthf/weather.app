use std::{
    error::Error,
    ffi::OsString,
    fmt, io,
    path::Path,
    path::PathBuf,
    process::{Child, Command as StdCommand, ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::{process::Command as TokioCommand, time::Instant};
use weather_schema::{EngineLockMetadata, correlation_id};

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct DaemonSupervisor {
    exe: PathBuf,
    prefix_args: Vec<OsString>,
    config: Option<PathBuf>,
}

#[derive(Debug)]
pub struct DaemonExecutableNotFound {
    executable: PathBuf,
}

impl DaemonExecutableNotFound {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }
}

impl fmt::Display for DaemonExecutableNotFound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "daemon executable `{}` was not found",
            self.executable.display()
        )
    }
}

impl Error for DaemonExecutableNotFound {}

pub struct ForegroundDaemon {
    child: Child,
    graceful_shutdown_requested: bool,
}

pub enum EngineOwnership {
    Direct,
    Adopted,
    Owned {
        owner_token: String,
        foreground: ForegroundDaemon,
    },
}

pub struct ReadyDaemon {
    pub probe: DaemonProbe,
    pub ownership: EngineOwnership,
}

struct SpawnedForeground {
    owner_token: String,
    foreground: ForegroundDaemon,
}

#[derive(Debug, Clone, Copy)]
struct SpawnIdentity<'a> {
    pid: u32,
    owner_token: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessDecision {
    Wait,
    Owned,
    Adopted,
}

impl DaemonSupervisor {
    pub fn new(daemon_exe: Option<PathBuf>, config: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            exe: daemon_exe.unwrap_or(resolve_daemon_exe()?),
            prefix_args: Vec::new(),
            config,
        })
    }

    pub fn for_current_exe(config: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            exe: std::env::current_exe().context("failed to resolve current executable")?,
            prefix_args: vec![OsString::from("daemon")],
            config,
        })
    }

    fn std_command(&self) -> StdCommand {
        let mut command = StdCommand::new(&self.exe);
        command.args(&self.prefix_args);
        command
    }

    fn tokio_command(&self) -> TokioCommand {
        let mut command = TokioCommand::new(&self.exe);
        command.args(&self.prefix_args);
        command
    }

    fn start_foreground(&self, owner_token: &str) -> Result<ForegroundDaemon> {
        let mut command = self.std_command();
        command
            .arg("run")
            .arg("--foreground")
            .arg("--owner-token")
            .arg(owner_token);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| daemon_command_error(&self.exe, "start foreground daemon", error))?;
        Ok(ForegroundDaemon {
            child,
            graceful_shutdown_requested: false,
        })
    }

    pub async fn probe(&self) -> Result<DaemonProbe> {
        self.probe_with_deadline(None).await
    }

    async fn probe_before(&self, deadline: Instant) -> Result<DaemonProbe> {
        self.probe_with_deadline(Some(deadline)).await
    }

    async fn probe_with_deadline(&self, deadline: Option<Instant>) -> Result<DaemonProbe> {
        let mut command = self.tokio_command();
        command.arg("probe").arg("--verbose");
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let output = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, command.output())
                .await
                .with_context(|| {
                    format!("timed out waiting for daemon probe {}", self.exe.display())
                })?,
            None => command.output().await,
        }
        .map_err(|error| daemon_command_error(&self.exe, "run daemon probe", error))?;
        if !output.status.success() {
            bail!("daemon probe failed with status {}", output.status);
        }
        serde_json::from_slice(&output.stdout).context("failed to parse daemon probe status")
    }

    pub async fn ensure_ready(&self, initial: DaemonProbe) -> Result<ReadyDaemon> {
        match initial.state {
            DaemonProbeState::Running => Ok(ReadyDaemon {
                probe: initial,
                ownership: EngineOwnership::Adopted,
            }),
            DaemonProbeState::NotRunning => {
                let owner_token = correlation_id("tui-owner");
                let foreground = self.start_foreground(&owner_token)?;
                let wait = Duration::from_millis(initial.startup_timeout_ms.max(1));
                let deadline = readiness_deadline(wait)?;
                self.wait_for_readiness(
                    deadline,
                    initial.startup_timeout_ms,
                    Some(SpawnedForeground {
                        owner_token,
                        foreground,
                    }),
                )
                .await
            }
            DaemonProbeState::Starting => {
                let wait = startup_wait_duration(&initial);
                let deadline = readiness_deadline(wait)?;
                self.wait_for_readiness(deadline, initial.startup_timeout_ms, None)
                    .await
            }
            state => bail!(probe_state_error(state, initial.message.as_deref())),
        }
    }

    async fn wait_for_readiness(
        &self,
        deadline: Instant,
        startup_timeout_ms: u64,
        mut spawned: Option<SpawnedForeground>,
    ) -> Result<ReadyDaemon> {
        loop {
            if Instant::now() >= deadline {
                bail!("engine did not become ready within {startup_timeout_ms} ms");
            }
            let probe = self.probe_before(deadline).await.with_context(|| {
                format!("engine did not become ready within {startup_timeout_ms} ms")
            })?;
            match readiness_decision(&probe, spawned.as_ref().map(SpawnedForeground::identity))? {
                ReadinessDecision::Owned => {
                    let spawned = spawned
                        .take()
                        .context("owned readiness requires a foreground process")?;
                    return Ok(ReadyDaemon {
                        probe,
                        ownership: EngineOwnership::Owned {
                            owner_token: spawned.owner_token,
                            foreground: spawned.foreground,
                        },
                    });
                }
                ReadinessDecision::Adopted => {
                    drop(spawned.take());
                    return Ok(ReadyDaemon {
                        probe,
                        ownership: EngineOwnership::Adopted,
                    });
                }
                ReadinessDecision::Wait => {}
            }

            if probe.state == DaemonProbeState::NotRunning {
                match spawned.as_mut() {
                    Some(spawned) => {
                        if let Some(status) = spawned.foreground.try_wait()? {
                            bail!("foreground daemon exited with {status} before engine readiness");
                        }
                    }
                    None => bail!("engine stopped while waiting for readiness"),
                }
            }
            let wake = (Instant::now() + READINESS_POLL_INTERVAL).min(deadline);
            tokio::time::sleep_until(wake).await;
        }
    }
}

fn daemon_command_error(executable: &Path, operation: &str, error: io::Error) -> anyhow::Error {
    if error.kind() == io::ErrorKind::NotFound {
        return anyhow::Error::new(DaemonExecutableNotFound::new(executable));
    }
    anyhow::Error::new(error).context(format!("failed to {operation} {}", executable.display()))
}

#[derive(Debug, Deserialize)]
pub struct DaemonProbe {
    pub state: DaemonProbeState,
    pub rpc_endpoint: String,
    pub pub_endpoint: String,
    pub lock_age_ms: Option<u64>,
    pub startup_timeout_ms: u64,
    pub lock_metadata: Option<EngineLockMetadata>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonProbeState {
    NotRunning,
    Starting,
    Running,
    Unhealthy,
    EndpointConflict,
}

fn readiness_decision(
    probe: &DaemonProbe,
    spawned: Option<SpawnIdentity<'_>>,
) -> Result<ReadinessDecision> {
    match probe.state {
        DaemonProbeState::NotRunning | DaemonProbeState::Starting => Ok(ReadinessDecision::Wait),
        DaemonProbeState::Running => {
            let Some(spawned) = spawned else {
                return Ok(ReadinessDecision::Adopted);
            };
            let owned = probe.lock_metadata.as_ref().is_some_and(|metadata| {
                metadata.pid == spawned.pid
                    && metadata.owner_token.as_deref() == Some(spawned.owner_token)
            });
            Ok(if owned {
                ReadinessDecision::Owned
            } else {
                ReadinessDecision::Adopted
            })
        }
        state => bail!(probe_state_error(state, probe.message.as_deref())),
    }
}

fn startup_wait_duration(probe: &DaemonProbe) -> Duration {
    Duration::from_millis(
        probe
            .startup_timeout_ms
            .saturating_sub(probe.lock_age_ms.unwrap_or_default()),
    )
}

fn readiness_deadline(wait: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(wait)
        .context("configured engine startup timeout is too large")
}

pub fn probe_state_error(state: DaemonProbeState, message: Option<&str>) -> String {
    let state = match state {
        DaemonProbeState::NotRunning => "not_running",
        DaemonProbeState::Starting => "starting",
        DaemonProbeState::Running => "running",
        DaemonProbeState::Unhealthy => "unhealthy",
        DaemonProbeState::EndpointConflict => "endpoint_conflict",
    };
    match message {
        Some(message) => format!("engine probe reported {state}: {message}"),
        None => format!("engine probe reported {state}"),
    }
}

impl ForegroundDaemon {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .context("failed to inspect foreground daemon process")
    }

    pub fn mark_graceful_shutdown_requested(&mut self) {
        self.graceful_shutdown_requested = true;
    }
}

impl SpawnedForeground {
    fn identity(&self) -> SpawnIdentity<'_> {
        SpawnIdentity {
            pid: self.foreground.id(),
            owner_token: &self.owner_token,
        }
    }
}

impl Drop for ForegroundDaemon {
    fn drop(&mut self) {
        if self.graceful_shutdown_requested && self.child.try_wait().ok().flatten().is_none() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        if self.child.try_wait().ok().flatten().is_none() {
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

#[cfg(test)]
mod tests {
    use weather_schema::ENGINE_LOCK_METADATA_VERSION;

    use super::*;

    fn metadata(pid: u32, owner_token: &str) -> EngineLockMetadata {
        EngineLockMetadata {
            version: ENGINE_LOCK_METADATA_VERSION,
            pid,
            instance_id: "instance".to_string(),
            owner_token: Some(owner_token.to_string()),
            started_at_unix_ms: 1_788_000_000_000,
            config_path: "/tmp/weather.toml".to_string(),
        }
    }

    fn probe(state: DaemonProbeState, lock_metadata: Option<EngineLockMetadata>) -> DaemonProbe {
        DaemonProbe {
            state,
            rpc_endpoint: "tcp://127.0.0.1:41001".to_string(),
            pub_endpoint: "tcp://127.0.0.1:41002".to_string(),
            lock_age_ms: None,
            startup_timeout_ms: 8_000,
            lock_metadata,
            message: None,
        }
    }

    #[test]
    fn matching_spawn_identity_becomes_owned() {
        let probe = probe(DaemonProbeState::Running, Some(metadata(42, "owner-token")));

        assert_eq!(
            readiness_decision(
                &probe,
                Some(SpawnIdentity {
                    pid: 42,
                    owner_token: "owner-token",
                })
            )
            .unwrap(),
            ReadinessDecision::Owned
        );
    }

    #[test]
    fn competing_start_loser_adopts_the_winner() {
        let probe = probe(
            DaemonProbeState::Running,
            Some(metadata(84, "winner-token")),
        );

        assert_eq!(
            readiness_decision(
                &probe,
                Some(SpawnIdentity {
                    pid: 42,
                    owner_token: "loser-token",
                })
            )
            .unwrap(),
            ReadinessDecision::Adopted
        );
    }

    #[test]
    fn preexisting_ready_engine_is_adopted() {
        let probe = probe(
            DaemonProbeState::Running,
            Some(metadata(42, "another-owner")),
        );

        assert_eq!(
            readiness_decision(&probe, None).unwrap(),
            ReadinessDecision::Adopted
        );
    }

    #[test]
    fn transient_probe_states_keep_waiting() {
        assert_eq!(
            readiness_decision(&probe(DaemonProbeState::NotRunning, None), None).unwrap(),
            ReadinessDecision::Wait
        );
        assert_eq!(
            readiness_decision(&probe(DaemonProbeState::Starting, None), None).unwrap(),
            ReadinessDecision::Wait
        );
    }

    #[test]
    fn unhealthy_and_conflicting_states_fail_readiness() {
        assert!(readiness_decision(&probe(DaemonProbeState::Unhealthy, None), None).is_err());
        assert!(
            readiness_decision(&probe(DaemonProbeState::EndpointConflict, None), None).is_err()
        );
    }

    #[test]
    fn existing_start_uses_only_the_configured_remaining_time() {
        let mut probe = probe(DaemonProbeState::Starting, None);
        probe.lock_age_ms = Some(7_999);
        assert_eq!(startup_wait_duration(&probe), Duration::from_millis(1));

        probe.lock_age_ms = Some(8_000);
        assert_eq!(startup_wait_duration(&probe), Duration::ZERO);
    }

    #[test]
    fn direct_ownership_is_an_explicit_state() {
        assert!(matches!(EngineOwnership::Direct, EngineOwnership::Direct));
    }

    #[test]
    fn embedded_invocation_prefixes_daemon_subcommand() {
        let supervisor = DaemonSupervisor {
            exe: PathBuf::from("weather.app"),
            prefix_args: vec![OsString::from("daemon")],
            config: None,
        };

        let command = supervisor.std_command();
        assert_eq!(command.get_program(), "weather.app");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("daemon")]
        );
    }

    #[test]
    fn missing_daemon_command_preserves_a_typed_error() {
        let error = daemon_command_error(
            Path::new("weather-daemon"),
            "run daemon probe",
            io::Error::from(io::ErrorKind::NotFound),
        );
        let missing = error
            .downcast_ref::<DaemonExecutableNotFound>()
            .expect("missing command should retain its type");

        assert_eq!(missing.executable(), Path::new("weather-daemon"));
        assert_eq!(
            error.to_string(),
            "daemon executable `weather-daemon` was not found"
        );
    }

    #[test]
    fn other_daemon_spawn_errors_keep_operation_context() {
        let error = daemon_command_error(
            Path::new("weather-daemon"),
            "run daemon probe",
            io::Error::from(io::ErrorKind::PermissionDenied),
        );

        assert_eq!(
            error.to_string(),
            "failed to run daemon probe weather-daemon"
        );
    }
}
