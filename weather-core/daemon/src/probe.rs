use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use prost::Message;
use serde::Serialize;
use tokio::net::TcpStream;
use weather_configure::{default_config_file, load_or_default};
use weather_schema::*;
use zeromq::{DealerSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

use crate::{
    path::{absolute_config_path, normalize_path, resolve_relative},
    time::now_ms,
};

const PROBE_SAMPLE_ATTEMPTS: usize = 3;
const LOCK_OBSERVATION_ATTEMPTS: usize = 3;
const PROBE_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProbeState {
    NotRunning,
    Starting,
    Running,
    Unhealthy,
    EndpointConflict,
}

impl ProbeState {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotRunning => "not_running",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Unhealthy => "unhealthy",
            Self::EndpointConflict => "endpoint_conflict",
        }
    }
}

#[derive(Debug, Serialize)]
struct ProbeStatus {
    state: ProbeState,
    rpc_endpoint: String,
    pub_endpoint: String,
    config_path: String,
    lock_path: String,
    lock_held: bool,
    lock_age_ms: Option<u64>,
    startup_timeout_ms: u64,
    lock_metadata: Option<EngineLockMetadata>,
    rpc_endpoint_reachable: bool,
    pub_endpoint_reachable: bool,
    engine_status: Option<EngineStatusView>,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct EngineStatusView {
    ready: bool,
    mode: String,
    rpc_endpoint: String,
    pub_endpoint: String,
    config_path: String,
    last_config_error: Option<String>,
    message: Option<String>,
    engine_version: String,
    schema_version: String,
    build_version: String,
    instance_id: String,
}

impl From<EngineStatus> for EngineStatusView {
    fn from(value: EngineStatus) -> Self {
        Self {
            ready: value.ready,
            mode: value.mode,
            rpc_endpoint: value.rpc_endpoint,
            pub_endpoint: value.pub_endpoint,
            config_path: value.config_path,
            last_config_error: value.last_config_error,
            message: value.message,
            engine_version: value.engine_version,
            schema_version: value.schema_version,
            build_version: value.build_version,
            instance_id: value.instance_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockState {
    Missing,
    Free,
    Held,
}

#[derive(Debug, PartialEq, Eq)]
struct LockSnapshot {
    state: LockState,
    identity: Option<same_file::Handle>,
    modified: Option<SystemTime>,
    metadata: Option<EngineLockMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusDisposition {
    MatchingReady,
    MatchingNotReady,
    AdoptedReady,
    Foreign,
}

pub(crate) async fn probe(config: Option<PathBuf>, verbose: bool) -> Result<()> {
    let status = probe_status(config).await?;
    if verbose {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("{}", status.state.as_str());
    }
    Ok(())
}

async fn probe_status(config: Option<PathBuf>) -> Result<ProbeStatus> {
    let strict_config = config.is_some();
    let config_path = absolute_config_path(match config {
        Some(path) => path,
        None => default_config_file()?,
    })?;
    let config = load_or_default(&config_path)?;
    let base_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let lock_path = resolve_relative(&base_dir, &config.engine.lock_path);
    let request_timeout = Duration::from_millis(config.engine.request_timeout_ms.max(1));
    let startup_timeout = Duration::from_millis(config.engine.startup_timeout_ms.max(1));

    for attempt in 0..PROBE_SAMPLE_ATTEMPTS {
        let lock_before = observe_lock(&lock_path)?;
        let (rpc_endpoint_reachable, pub_endpoint_reachable) = tokio::join!(
            tcp_endpoint_reachable(&config.ipc.rpc_endpoint, request_timeout),
            tcp_endpoint_reachable(&config.ipc.pub_endpoint, request_timeout),
        );
        let engine_status = if rpc_endpoint_reachable {
            rpc_endpoint_status(&config).await?
        } else {
            None
        };
        let lock_after = observe_lock(&lock_path)?;
        if lock_before != lock_after {
            if attempt + 1 < PROBE_SAMPLE_ATTEMPTS {
                tokio::time::sleep(PROBE_RETRY_DELAY).await;
                continue;
            }
            bail!(
                "engine state changed during {} consecutive probe samples",
                PROBE_SAMPLE_ATTEMPTS
            );
        }

        let lock_age = lock_age(&lock_after, SystemTime::now());
        let held_metadata = (lock_after.state == LockState::Held)
            .then_some(lock_after.metadata.as_ref())
            .flatten();
        let status_disposition = engine_status
            .as_ref()
            .map(|status| {
                status_disposition(&config_path, &config, status, strict_config, held_metadata)
            })
            .transpose()?;
        let state = classify_probe(
            lock_after.state,
            lock_age,
            startup_timeout,
            rpc_endpoint_reachable,
            pub_endpoint_reachable,
            status_disposition,
        );
        let message = state_message(state, lock_age, startup_timeout, status_disposition);
        let adopted = status_disposition == Some(StatusDisposition::AdoptedReady);
        let active_status = engine_status.as_ref().filter(|_| {
            matches!(
                status_disposition,
                Some(StatusDisposition::MatchingReady | StatusDisposition::AdoptedReady)
            )
        });
        return Ok(ProbeStatus {
            state,
            rpc_endpoint: active_status
                .map(|status| status.rpc_endpoint.clone())
                .unwrap_or_else(|| config.ipc.rpc_endpoint.clone()),
            pub_endpoint: active_status
                .map(|status| status.pub_endpoint.clone())
                .unwrap_or_else(|| config.ipc.pub_endpoint.clone()),
            config_path: active_status
                .map(|status| status.config_path.clone())
                .unwrap_or_else(|| config_path.display().to_string()),
            lock_path: lock_path.display().to_string(),
            lock_held: lock_after.state == LockState::Held,
            lock_age_ms: lock_age.map(duration_ms),
            startup_timeout_ms: duration_ms(startup_timeout),
            lock_metadata: held_metadata.cloned(),
            rpc_endpoint_reachable,
            pub_endpoint_reachable,
            engine_status: engine_status.map(Into::into),
            message: adopted
                .then(|| {
                    "adopted a ready engine discovered at the configured RPC endpoint".to_string()
                })
                .or(message),
        });
    }
    unreachable!("probe sampling loop always returns")
}

fn classify_probe(
    lock_state: LockState,
    lock_age: Option<Duration>,
    startup_timeout: Duration,
    rpc_endpoint_reachable: bool,
    pub_endpoint_reachable: bool,
    status: Option<StatusDisposition>,
) -> ProbeState {
    if status == Some(StatusDisposition::AdoptedReady) {
        return ProbeState::Running;
    }
    if status == Some(StatusDisposition::Foreign) {
        return ProbeState::EndpointConflict;
    }
    if lock_state == LockState::Held {
        if status == Some(StatusDisposition::MatchingReady) {
            return ProbeState::Running;
        }
        return if lock_age.is_none_or(|age| age >= startup_timeout) {
            ProbeState::Unhealthy
        } else {
            ProbeState::Starting
        };
    }
    if rpc_endpoint_reachable || pub_endpoint_reachable || status.is_some() {
        ProbeState::EndpointConflict
    } else {
        ProbeState::NotRunning
    }
}

fn state_message(
    state: ProbeState,
    lock_age: Option<Duration>,
    startup_timeout: Duration,
    status: Option<StatusDisposition>,
) -> Option<String> {
    match state {
        ProbeState::NotRunning | ProbeState::Running => None,
        ProbeState::Starting => Some(format!(
            "engine lock is held and startup is still within {} ms",
            duration_ms(startup_timeout)
        )),
        ProbeState::Unhealthy => Some(format!(
            "engine lock has been held for {} ms without a ready status",
            lock_age.map(duration_ms).unwrap_or_default()
        )),
        ProbeState::EndpointConflict if status == Some(StatusDisposition::Foreign) => Some(
            "RPC endpoint returned a status for a different engine configuration".to_string(),
        ),
        ProbeState::EndpointConflict => Some(
            "one or more configured endpoints are reachable without a healthy engine holding the expected lock"
                .to_string(),
        ),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn lock_age(snapshot: &LockSnapshot, now: SystemTime) -> Option<Duration> {
    if snapshot.state != LockState::Held {
        return None;
    }
    let started_at = snapshot
        .metadata
        .as_ref()
        .and_then(|metadata| {
            UNIX_EPOCH.checked_add(Duration::from_millis(metadata.started_at_unix_ms))
        })
        .or(snapshot.modified)?;
    Some(now.duration_since(started_at).unwrap_or_default())
}

async fn rpc_endpoint_status(
    config: &weather_configure::AppConfig,
) -> Result<Option<EngineStatus>> {
    let timeout = Duration::from_millis(config.engine.request_timeout_ms.max(1));
    let request_id = correlation_id("daemon-request");
    let mut envelope = RpcRequest {
        schema_version: SCHEMA_VERSION.to_string(),
        request_id: request_id.clone(),
        kind: RpcKind::GetEngineStatus as i32,
        timestamp_unix_ms: now_ms(),
        hmac_sha256: Vec::new(),
        payload: Empty {}.encode_to_vec(),
    };
    if let Some(key) = weather_configure::resolve_hmac_key(config)? {
        envelope.hmac_sha256 = weather_schema::rpc_request_hmac(&envelope, &key)?;
    }

    let exchange = async {
        let mut socket = DealerSocket::new();
        socket.connect(&config.ipc.rpc_endpoint).await?;
        socket
            .send(ZmqMessage::from(envelope.encode_to_vec()))
            .await?;
        let message = socket.recv().await?;
        let bytes: Vec<u8> = message.try_into().map_err(anyhow::Error::msg)?;
        let response = RpcResponse::decode(bytes.as_slice())?;
        if response.schema_version != SCHEMA_VERSION || response.request_id != request_id {
            return Err(anyhow!(
                "RPC status response envelope does not match the request"
            ));
        }
        if response.status != ResponseStatus::Ok as i32 {
            return Err(anyhow!("RPC status request was not successful"));
        }
        EngineStatus::decode(response.payload.as_slice()).map_err(Into::into)
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(Ok(status)) => Ok(Some(status)),
        Ok(Err(_)) | Err(_) => Ok(None),
    }
}

async fn tcp_endpoint_reachable(endpoint: &str, timeout: Duration) -> bool {
    let Some(address) = endpoint.strip_prefix("tcp://") else {
        return false;
    };
    matches!(
        tokio::time::timeout(timeout, TcpStream::connect(address)).await,
        Ok(Ok(_))
    )
}

fn status_disposition(
    requested_config: &Path,
    config: &weather_configure::AppConfig,
    status: &EngineStatus,
    strict_config: bool,
    lock_metadata: Option<&EngineLockMetadata>,
) -> Result<StatusDisposition> {
    let requested = normalize_path(requested_config)?;
    let active = normalize_path(Path::new(&status.config_path))?;
    let matches = requested == active
        && status.rpc_endpoint == config.ipc.rpc_endpoint
        && status.pub_endpoint == config.ipc.pub_endpoint;
    if let Some(metadata) = lock_metadata {
        let metadata_config = normalize_path(Path::new(&metadata.config_path))?;
        if metadata_config != active || metadata.instance_id != status.instance_id {
            return Ok(StatusDisposition::Foreign);
        }
    }
    if !matches && !strict_config && status.ready {
        return Ok(StatusDisposition::AdoptedReady);
    }
    if !matches {
        return Ok(StatusDisposition::Foreign);
    }
    Ok(if status.ready {
        StatusDisposition::MatchingReady
    } else {
        StatusDisposition::MatchingNotReady
    })
}

fn observe_lock(path: &Path) -> Result<LockSnapshot> {
    for attempt in 0..LOCK_OBSERVATION_ATTEMPTS {
        if let Some(snapshot) = observe_lock_once(path)? {
            return Ok(snapshot);
        }
        if attempt + 1 < LOCK_OBSERVATION_ATTEMPTS {
            std::thread::sleep(PROBE_RETRY_DELAY);
        }
    }
    bail!(
        "lock path {} changed during {} consecutive observations",
        path.display(),
        LOCK_OBSERVATION_ATTEMPTS
    )
}

fn observe_lock_once(path: &Path) -> Result<Option<LockSnapshot>> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(LockSnapshot {
                state: LockState::Missing,
                identity: None,
                modified: None,
                metadata: None,
            }));
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to open lock file {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect open lock file {}", path.display()))?;
    let identity = same_file::Handle::from_file(
        file.try_clone()
            .with_context(|| format!("failed to clone open lock file {}", path.display()))?,
    )
    .with_context(|| format!("failed to identify open lock file {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read lock age from {}", path.display()))?;
    let lock_metadata = read_lock_metadata(&mut file, path)?;
    let state = match FileExt::try_lock_shared(&file) {
        Ok(()) => {
            let matches = file_matches_path(&file, path)?;
            FileExt::unlock(&file).ok();
            if !matches {
                return Ok(None);
            }
            LockState::Free
        }
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            if !file_matches_path(&file, path)? {
                return Ok(None);
            }
            LockState::Held
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to probe lock {}", path.display()));
        }
    };
    Ok(Some(LockSnapshot {
        state,
        identity: Some(identity),
        modified: Some(modified),
        metadata: lock_metadata,
    }))
}

fn read_lock_metadata(file: &mut File, path: &Path) -> Result<Option<EngineLockMetadata>> {
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .with_context(|| format!("failed to read lock metadata from {}", path.display()))?;
    Ok(serde_json::from_slice::<EngineLockMetadata>(&contents)
        .ok()
        .filter(EngineLockMetadata::is_supported))
}

fn file_matches_path(file: &File, path: &Path) -> Result<bool> {
    let file_handle = same_file::Handle::from_file(
        file.try_clone()
            .with_context(|| format!("failed to clone open lock file {}", path.display()))?,
    )
    .with_context(|| format!("failed to identify open lock file {}", path.display()))?;
    let path_handle = match same_file::Handle::from_path(path) {
        Ok(handle) => handle,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to identify lock path {}", path.display()));
        }
    };
    Ok(file_handle == path_handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_v1_toml() -> String {
        let mut value: toml::Value =
            toml::from_str(&weather_configure::default_config_toml()).unwrap();
        let root = value.as_table_mut().unwrap();
        root.insert("config_version".to_string(), toml::Value::Integer(1));
        root.get_mut("ipc")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "transport".to_string(),
                toml::Value::String("tcp".to_string()),
            );
        root.get_mut("db")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "lock_path".to_string(),
                toml::Value::String("weather.db.lock".to_string()),
            );
        root.insert(
            "daemon".to_string(),
            toml::Value::Table(toml::toml! {
                service_backend = "auto"
                foreground = true
                service_scope = "user"
            }),
        );
        toml::to_string_pretty(&value).unwrap()
    }

    fn classify(
        lock_state: LockState,
        age: Duration,
        rpc: bool,
        publisher: bool,
        status: Option<StatusDisposition>,
    ) -> ProbeState {
        classify_probe(
            lock_state,
            Some(age),
            Duration::from_secs(8),
            rpc,
            publisher,
            status,
        )
    }

    #[test]
    fn classification_covers_all_probe_states() {
        assert_eq!(
            classify(LockState::Missing, Duration::ZERO, false, false, None),
            ProbeState::NotRunning
        );
        assert_eq!(
            classify(
                LockState::Held,
                Duration::from_millis(7_999),
                false,
                false,
                None
            ),
            ProbeState::Starting
        );
        assert_eq!(
            classify(LockState::Held, Duration::from_secs(8), false, false, None),
            ProbeState::Unhealthy
        );
        assert_eq!(
            classify(
                LockState::Held,
                Duration::from_secs(30),
                true,
                true,
                Some(StatusDisposition::MatchingReady)
            ),
            ProbeState::Running
        );
        assert_eq!(
            classify(LockState::Free, Duration::ZERO, false, true, None),
            ProbeState::EndpointConflict
        );
        assert_eq!(
            classify(
                LockState::Held,
                Duration::ZERO,
                true,
                true,
                Some(StatusDisposition::Foreign)
            ),
            ProbeState::EndpointConflict
        );
        assert_eq!(
            classify(
                LockState::Missing,
                Duration::ZERO,
                true,
                true,
                Some(StatusDisposition::AdoptedReady)
            ),
            ProbeState::Running
        );
    }

    #[test]
    fn default_probe_adopts_a_ready_engine_from_another_config() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("default.toml");
        let active = directory.path().join("active.toml");
        let config = weather_configure::AppConfig::default();
        let status = EngineStatus {
            ready: true,
            rpc_endpoint: config.ipc.rpc_endpoint.clone(),
            pub_endpoint: config.ipc.pub_endpoint.clone(),
            config_path: active.display().to_string(),
            ..Default::default()
        };

        assert_eq!(
            status_disposition(&requested, &config, &status, false, None).unwrap(),
            StatusDisposition::AdoptedReady
        );
        assert_eq!(
            status_disposition(&requested, &config, &status, true, None).unwrap(),
            StatusDisposition::Foreign
        );
    }

    #[test]
    fn default_probe_keeps_a_matching_ready_engine_classified_as_matching() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("default.toml");
        std::fs::write(&requested, weather_configure::default_config_toml()).unwrap();
        let config = weather_configure::AppConfig::default();
        let status = EngineStatus {
            ready: true,
            rpc_endpoint: config.ipc.rpc_endpoint.clone(),
            pub_endpoint: config.ipc.pub_endpoint.clone(),
            config_path: requested.display().to_string(),
            ..Default::default()
        };

        assert_eq!(
            status_disposition(&requested, &config, &status, false, None).unwrap(),
            StatusDisposition::MatchingReady
        );
    }

    fn lock_metadata(config_path: &Path, instance_id: &str) -> EngineLockMetadata {
        EngineLockMetadata {
            version: ENGINE_LOCK_METADATA_VERSION,
            pid: 42,
            instance_id: instance_id.to_string(),
            owner_token: Some("owner-token".to_string()),
            started_at_unix_ms: 1_000,
            config_path: config_path.display().to_string(),
        }
    }

    #[test]
    fn status_instance_must_match_the_held_lock_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("weather.toml");
        std::fs::write(&requested, weather_configure::default_config_toml()).unwrap();
        let config = weather_configure::AppConfig::default();
        let status = EngineStatus {
            ready: true,
            rpc_endpoint: config.ipc.rpc_endpoint.clone(),
            pub_endpoint: config.ipc.pub_endpoint.clone(),
            config_path: requested.display().to_string(),
            instance_id: "rpc-instance".to_string(),
            ..Default::default()
        };
        let metadata = lock_metadata(&requested, "lock-instance");

        assert_eq!(
            status_disposition(&requested, &config, &status, true, Some(&metadata)).unwrap(),
            StatusDisposition::Foreign
        );
    }

    #[test]
    fn lock_age_prefers_versioned_start_time_over_file_mtime() {
        let config_path = Path::new("/tmp/weather.toml");
        let snapshot = LockSnapshot {
            state: LockState::Held,
            identity: None,
            modified: Some(UNIX_EPOCH + Duration::from_secs(10)),
            metadata: Some(lock_metadata(config_path, "instance")),
        };

        assert_eq!(
            lock_age(&snapshot, UNIX_EPOCH + Duration::from_secs(3)),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn observing_a_missing_lock_is_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing").join("engine.lock");

        let snapshot = observe_lock(&path).unwrap();

        assert_eq!(snapshot.state, LockState::Missing);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn observing_a_free_lock_preserves_its_inode_and_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.lock");
        std::fs::write(&path, b"stale marker\n").unwrap();
        let before = same_file::Handle::from_path(&path).unwrap();

        let snapshot = observe_lock(&path).unwrap();

        let after = same_file::Handle::from_path(&path).unwrap();
        assert_eq!(snapshot.state, LockState::Free);
        assert_eq!(before, after);
        assert_eq!(std::fs::read(&path).unwrap(), b"stale marker\n");
    }

    #[test]
    fn observing_an_exclusive_lock_reports_held() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.lock");
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock_exclusive(&owner).unwrap();

        let snapshot = observe_lock(&path).unwrap();

        assert_eq!(snapshot.state, LockState::Held);
        FileExt::unlock(&owner).unwrap();
    }

    #[test]
    fn observing_a_held_lock_exposes_supported_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.lock");
        let expected = lock_metadata(&path, "instance");
        std::fs::write(&path, serde_json::to_vec(&expected).unwrap()).unwrap();
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock_exclusive(&owner).unwrap();

        let snapshot = observe_lock(&path).unwrap();

        assert_eq!(snapshot.state, LockState::Held);
        assert_eq!(snapshot.metadata, Some(expected));
        FileExt::unlock(&owner).unwrap();
    }

    #[tokio::test]
    async fn tcp_reachability_uses_a_bounded_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("tcp://{}", listener.local_addr().unwrap());

        assert!(tcp_endpoint_reachable(&endpoint, Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn missing_config_probe_does_not_create_any_local_state() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("missing").join("config");
        let config_path = parent.join("weather.toml");

        let _ = probe_status(Some(config_path.clone())).await.unwrap();

        assert!(!config_path.exists());
        assert!(!parent.exists());
        assert!(!directory.path().join("missing").exists());
    }

    #[tokio::test]
    async fn legacy_config_probe_migrates_only_in_memory() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let content = legacy_v1_toml();
        std::fs::write(&config_path, &content).unwrap();

        let _ = probe_status(Some(config_path.clone())).await.unwrap();

        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), content);
        assert!(!directory.path().join("engine.lock").exists());
        assert!(!directory.path().join("component-manifest.toml").exists());
    }
}
