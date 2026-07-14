use std::path::Path;

use weather_configure::{
    AppConfig, ConfigState, PreparedConfig, normalize_config_stations, prepare_config_atomic,
    restart_required_fields, validate,
};
use weather_schema::*;

use crate::runtime::Engine;

impl Engine {
    pub(crate) async fn handle_get_config(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<GetConfigRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                decoded.unwrap_err().to_string(),
            );
        };
        let config = if req.defaults {
            AppConfig::default()
        } else {
            self.config.get()
        };
        let payload = GetConfigResponse {
            config: Some(config.into()),
        };
        self.ok(&request.request_id, payload)
    }

    pub(crate) async fn handle_update_config(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<UpdateConfigRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                decoded.unwrap_err().to_string(),
            );
        };
        let Some(schema_config) = req.config else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                "missing config field",
            );
        };

        match commit_config(
            &self.config_commit,
            &self.config,
            &self.config_path,
            schema_config.into(),
        )
        .await
        {
            Ok(config) => self.ok(
                &request.request_id,
                UpdateConfigResponse {
                    config: Some(config.into()),
                },
            ),
            Err(ConfigCommitError::Invalid(message)) => {
                Self::rpc_error_response(&request.request_id, RpcErrorCode::BadRequest, message)
            }
            Err(ConfigCommitError::RestartRequired(fields)) => Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::RestartRequired,
                restart_required_message(&fields),
            ),
            Err(ConfigCommitError::Persistence(message)) => {
                Self::rpc_error_response(&request.request_id, RpcErrorCode::Config, message)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigCommitError {
    Invalid(String),
    RestartRequired(Vec<&'static str>),
    Persistence(String),
}

async fn commit_config(
    commit_lock: &tokio::sync::Mutex<()>,
    state: &ConfigState,
    path: &Path,
    candidate: AppConfig,
) -> Result<AppConfig, ConfigCommitError> {
    commit_config_with_preparer(commit_lock, state, path, candidate, |path, config| {
        prepare_config_atomic(&path, &config)
    })
    .await
}

async fn commit_config_with_preparer<F>(
    commit_lock: &tokio::sync::Mutex<()>,
    state: &ConfigState,
    path: &Path,
    mut candidate: AppConfig,
    prepare: F,
) -> Result<AppConfig, ConfigCommitError>
where
    F: FnOnce(std::path::PathBuf, AppConfig) -> anyhow::Result<PreparedConfig> + Send + 'static,
{
    let _commit = commit_lock.lock().await;
    let current = state.get();

    normalize_config_stations(&mut candidate);
    validate(&candidate).map_err(|err| ConfigCommitError::Invalid(err.to_string()))?;

    let restart_fields = restart_required_fields(&current, &candidate);
    if !restart_fields.is_empty() {
        return Err(ConfigCommitError::RestartRequired(restart_fields));
    }

    if candidate == current {
        return Ok(current);
    }

    let prepare_path = path.to_path_buf();
    let prepare_candidate = candidate.clone();
    let prepared =
        match tokio::task::spawn_blocking(move || prepare(prepare_path, prepare_candidate)).await {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(err)) => return Err(persistence_error(state, format!("{err:#}"))),
            Err(err) => {
                return Err(persistence_error(
                    state,
                    format!("config prepare task failed: {err}"),
                ));
            }
        };

    // There is deliberately no cancellation point between replacing the file
    // and publishing the matching authoritative live value.
    if let Err(err) = prepared.persist() {
        return Err(persistence_error(state, format!("{err:#}")));
    }
    state.apply(candidate.clone());
    Ok(candidate)
}

fn persistence_error(state: &ConfigState, message: String) -> ConfigCommitError {
    state.record_error(message.clone());
    ConfigCommitError::Persistence(message)
}

fn restart_required_message(fields: &[&str]) -> String {
    format!(
        "configuration changes require an engine restart: {}",
        fields.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use weather_configure::{ProviderConfig, StationConfig, load_from_path, write_config_atomic};

    use super::*;

    fn configured_file() -> (tempfile::TempDir, std::path::PathBuf, AppConfig) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.toml");
        let config = AppConfig::default();
        write_config_atomic(&path, &config).unwrap();
        (directory, path, config)
    }

    fn no_temporary_files(directory: &Path) -> bool {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_change_requires_restart_without_mutating_config() {
        let (_directory, path, initial) = configured_file();
        let state = ConfigState::new(initial.clone());
        let subscriber = state.subscribe();
        let commit_lock = tokio::sync::Mutex::new(());
        let mut candidate = initial.clone();
        candidate.updater.default_provider = "other".to_string();
        candidate.updater.provider.push(ProviderConfig {
            name: "other".to_string(),
            base_url: "https://example.invalid".to_string(),
            request_timeout_seconds: 1,
        });

        let error = commit_config(&commit_lock, &state, &path, candidate)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            ConfigCommitError::RestartRequired(vec![
                "updater.default_provider",
                "updater.provider"
            ])
        );
        assert_eq!(load_from_path(&path).unwrap(), initial);
        assert_eq!(state.get(), initial);
        assert_eq!(*subscriber.borrow(), initial);
        assert!(!subscriber.has_changed().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persistence_failure_does_not_apply_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config-is-a-directory");
        std::fs::create_dir(&path).unwrap();
        let initial = AppConfig::default();
        let state = ConfigState::new(initial.clone());
        let subscriber = state.subscribe();
        let commit_lock = tokio::sync::Mutex::new(());
        let mut candidate = initial.clone();
        candidate.updater.weather_ttl_seconds += 1;

        let error = commit_config(&commit_lock, &state, &path, candidate)
            .await
            .unwrap_err();

        assert!(matches!(error, ConfigCommitError::Persistence(_)));
        assert_eq!(state.get(), initial);
        assert_eq!(*subscriber.borrow(), initial);
        assert!(!subscriber.has_changed().unwrap());
        assert!(state.last_error().is_some());
        assert!(no_temporary_files(directory.path()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unchanged_candidate_skips_persistence_and_watch_notification() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config-is-a-directory");
        std::fs::create_dir(&path).unwrap();
        let initial = AppConfig::default();
        let state = ConfigState::new(initial.clone());
        let subscriber = state.subscribe();
        let commit_lock = tokio::sync::Mutex::new(());

        let committed = commit_config(&commit_lock, &state, &path, initial.clone())
            .await
            .unwrap();

        assert_eq!(committed, initial);
        assert_eq!(state.get(), initial);
        assert!(!subscriber.has_changed().unwrap());
    }

    #[tokio::test]
    async fn update_succeeds_on_current_thread_runtime() {
        let (_directory, path, initial) = configured_file();
        let state = ConfigState::new(initial.clone());
        let commit_lock = tokio::sync::Mutex::new(());
        let mut candidate = initial;
        candidate.updater.weather_ttl_seconds += 1;

        let committed = commit_config(&commit_lock, &state, &path, candidate.clone())
            .await
            .unwrap();

        assert_eq!(committed, candidate);
        assert_eq!(state.get(), candidate);
        assert_eq!(load_from_path(&path).unwrap(), candidate);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_updates_leave_disk_state_and_watch_consistent() {
        let (directory, path, initial) = configured_file();
        let state = ConfigState::new(initial.clone());
        let subscriber = state.subscribe();
        let commit_lock = Arc::new(tokio::sync::Mutex::new(()));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let mut first = initial.clone();
        first.updater.weather_ttl_seconds = 111;
        first.stations = vec![StationConfig {
            name: " 北京 - 北京市 - 朝阳 ".to_string(),
            enabled: true,
        }];
        let mut normalized_first = first.clone();
        normalize_config_stations(&mut normalized_first);

        let mut second = initial;
        second.updater.province_ttl_seconds = 222;
        second.stations = vec![StationConfig {
            name: "湖北-湖北省-武汉".to_string(),
            enabled: false,
        }];
        let normalized_second = second.clone();

        let first_task = {
            let barrier = barrier.clone();
            let commit_lock = commit_lock.clone();
            let state = state.clone();
            let path = path.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                commit_config(&commit_lock, &state, &path, first).await
            })
        };
        let second_task = {
            let barrier = barrier.clone();
            let commit_lock = commit_lock.clone();
            let state = state.clone();
            let path = path.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                commit_config(&commit_lock, &state, &path, second).await
            })
        };

        barrier.wait().await;
        let first_result = first_task.await.unwrap().unwrap();
        let second_result = second_task.await.unwrap().unwrap();
        assert_eq!(first_result, normalized_first);
        assert_eq!(second_result, normalized_second);

        let live = state.get();
        assert!(live == normalized_first || live == normalized_second);
        assert_eq!(load_from_path(&path).unwrap(), live);
        assert_eq!(*subscriber.borrow(), live);
        assert!(no_temporary_files(directory.path()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn aborted_prepare_cannot_block_or_overwrite_a_later_commit() {
        let (directory, path, initial) = configured_file();
        let state = ConfigState::new(initial.clone());
        let subscriber = state.subscribe();
        let commit_lock = Arc::new(tokio::sync::Mutex::new(()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();

        let mut interrupted_candidate = initial.clone();
        interrupted_candidate.updater.weather_ttl_seconds += 1;
        let interrupted_task = {
            let commit_lock = commit_lock.clone();
            let state = state.clone();
            let path = path.clone();
            tokio::spawn(async move {
                commit_config_with_preparer(
                    &commit_lock,
                    &state,
                    &path,
                    interrupted_candidate,
                    move |path, config| {
                        let _ = started_tx.send(());
                        resume_rx.recv().unwrap();
                        let prepared = prepare_config_atomic(&path, &config)?;
                        let _ = prepared_tx.send(());
                        finish_rx.recv().unwrap();
                        Ok(prepared)
                    },
                )
                .await
            })
        };

        started_rx.await.unwrap();
        interrupted_task.abort();
        assert!(interrupted_task.await.unwrap_err().is_cancelled());

        let mut final_candidate = initial;
        final_candidate.updater.province_ttl_seconds += 1;
        let final_task = {
            let commit_lock = commit_lock.clone();
            let state = state.clone();
            let path = path.clone();
            let final_candidate = final_candidate.clone();
            tokio::spawn(async move {
                commit_config(&commit_lock, &state, &path, final_candidate).await
            })
        };

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), final_task)
                .await
                .expect("later commit remained blocked by cancelled prepare")
                .unwrap()
                .unwrap(),
            final_candidate
        );
        assert_eq!(load_from_path(&path).unwrap(), final_candidate);
        assert_eq!(state.get(), final_candidate);
        assert_eq!(*subscriber.borrow(), final_candidate);

        resume_tx.send(()).unwrap();
        prepared_rx.await.unwrap();
        assert_eq!(load_from_path(&path).unwrap(), final_candidate);
        assert_eq!(state.get(), final_candidate);
        assert!(!no_temporary_files(directory.path()));

        finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !no_temporary_files(directory.path()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled prepared config was not removed");

        assert_eq!(load_from_path(&path).unwrap(), final_candidate);
        assert_eq!(state.get(), final_candidate);
        assert!(no_temporary_files(directory.path()));
    }

    #[test]
    fn restart_required_message_is_stable() {
        assert_eq!(
            restart_required_message(&["engine", "updater.provider"]),
            "configuration changes require an engine restart: engine, updater.provider"
        );
    }
}
