use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, oneshot};
use weather_schema::{StationRef, WeatherSnapshot};

use crate::storage::{self, DbInstance};

type TimezoneFinalize = Box<dyn FnOnce() -> Result<()> + Send + Sync + 'static>;
type PostCommitFailure = Box<dyn FnOnce(String) + Send + Sync + 'static>;

#[derive(Clone)]
pub struct DbActor {
    tx: mpsc::Sender<DbCommand>,
}

pub(crate) enum DbCommand {
    PutHistorySnapshot {
        snapshot: Box<WeatherSnapshot>,
        fetched_at_unix_ms: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    GetLatestSnapshot {
        uuid: String,
        reply: oneshot::Sender<Result<Option<StoredSnapshot>>>,
    },
    ReplaceProviderProvinces {
        provider: String,
        provinces: Vec<ProviderProvince>,
        reply: oneshot::Sender<Result<()>>,
    },
    GetProviderProvinces {
        provider: String,
        reply: oneshot::Sender<Result<Option<CatalogCache<ProviderProvince>>>>,
    },
    ResolveProviderProvinceCode {
        provider: String,
        province: String,
        reply: oneshot::Sender<Result<String>>,
    },
    ReplaceProviderCities {
        provider: String,
        provider_province_code: String,
        cities: Vec<ProviderCity>,
        reply: oneshot::Sender<Result<()>>,
    },
    GetProviderCities {
        provider: String,
        provider_province_code: String,
        reply: oneshot::Sender<Result<Option<CatalogCache<ProviderCity>>>>,
    },
    GetProviderStationByUuid {
        provider: String,
        uuid: String,
        reply: oneshot::Sender<Result<Option<ProviderStation>>>,
    },
    PutProviderStationMapping {
        station: ProviderStation,
        reply: oneshot::Sender<Result<()>>,
    },
    GetProviderStationByName {
        provider: String,
        display_name: String,
        reply: oneshot::Sender<Result<Option<ProviderStation>>>,
    },
    GetDbTimezone {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    MigrateTimezoneBundle {
        old_timezone: String,
        new_timezone: String,
        finalize: TimezoneFinalize,
        postcommit_failure: PostCommitFailure,
        reply: oneshot::Sender<Result<u64>>,
    },
    LogFetch {
        unified_uuid: Option<String>,
        endpoint: String,
        ok: bool,
        message: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// graceful shutdown:checkpoint WAL 后退出 db 线程。
    Shutdown { reply: oneshot::Sender<Result<()>> },
}

#[derive(Debug)]
pub struct StoredSnapshot {
    pub snapshot: WeatherSnapshot,
    pub fetched_at_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct CatalogCache<T> {
    pub items: Vec<T>,
    pub fetched_at_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ProviderProvince {
    pub provider_code: String,
    pub name: String,
    pub url: String,
}

impl ProviderProvince {
    pub fn public_ref(&self) -> weather_schema::Province {
        weather_schema::Province {
            name: self.name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCity {
    pub provider_code: String,
    pub provider_province_code: String,
    pub province: String,
    pub city: String,
    pub url: String,
}

impl ProviderCity {
    pub fn public_ref(&self) -> weather_schema::City {
        weather_schema::City {
            province: self.province.clone(),
            city: self.city.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderStation {
    pub provider_name: String,
    pub display_name: String,
    pub provider_station_id: String,
    pub provider_province_code: String,
    pub province: String,
    pub city: String,
    pub url: String,
    pub name: String,
    pub unified_uuid: String,
}

impl ProviderStation {
    pub fn public_ref(&self) -> StationRef {
        StationRef {
            province: self.province.clone(),
            city: self.city.clone(),
            name: self.name.clone(),
            unified_uuid: self.unified_uuid.clone(),
        }
    }
}

impl DbActor {
    pub fn inspect_pending_timezone(path: &Path) -> Result<Option<String>> {
        storage::inspect_pending_timezone(path)
    }

    pub fn start(path: PathBuf, config_tz: String) -> Result<Self> {
        let (tx, mut rx) = mpsc::channel::<DbCommand>(128);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = DbInstance::open(path, &config_tz);
            match result {
                Ok(mut db) => {
                    let _ = ready_tx.send(Ok(()));
                    while let Some(cmd) = rx.blocking_recv() {
                        if matches!(cmd, DbCommand::Shutdown { .. }) {
                            handle(&mut db, cmd);
                            break;
                        }
                        handle(&mut db, cmd);
                    }
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            }
        });
        ready_rx.recv().context("DB actor failed to start")??;
        Ok(Self { tx })
    }

    pub async fn put_history_snapshot(
        &self,
        snapshot: WeatherSnapshot,
        fetched_at_unix_ms: i64,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::PutHistorySnapshot {
                snapshot: Box::new(snapshot),
                fetched_at_unix_ms,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_latest_snapshot(&self, uuid: String) -> Result<Option<StoredSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetLatestSnapshot { uuid, reply })
            .await?;
        rx.await?
    }

    pub async fn replace_provider_provinces(
        &self,
        provider: &str,
        provinces: Vec<ProviderProvince>,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::ReplaceProviderProvinces {
                provider: provider.to_string(),
                provinces,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_provider_provinces(
        &self,
        provider: &str,
    ) -> Result<Option<CatalogCache<ProviderProvince>>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderProvinces {
                provider: provider.to_string(),
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn resolve_provider_province_code(
        &self,
        provider: &str,
        province: &str,
    ) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::ResolveProviderProvinceCode {
                provider: provider.to_string(),
                province: province.to_string(),
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn replace_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
        cities: Vec<ProviderCity>,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::ReplaceProviderCities {
                provider: provider.to_string(),
                provider_province_code: provider_province_code.to_string(),
                cities,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
    ) -> Result<Option<CatalogCache<ProviderCity>>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderCities {
                provider: provider.to_string(),
                provider_province_code: provider_province_code.to_string(),
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_provider_station_by_uuid(
        &self,
        provider: String,
        uuid: String,
    ) -> Result<Option<ProviderStation>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderStationByUuid {
                provider,
                uuid,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn put_provider_station_mapping(&self, station: ProviderStation) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::PutProviderStationMapping { station, reply })
            .await?;
        rx.await?
    }

    pub async fn get_provider_station_by_name(
        &self,
        provider: String,
        display_name: String,
    ) -> Result<Option<ProviderStation>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetProviderStationByName {
                provider,
                display_name,
                reply,
            })
            .await?;
        rx.await?
    }

    pub async fn get_db_timezone(&self) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(DbCommand::GetDbTimezone { reply }).await?;
        rx.await?
    }

    pub async fn migrate_timezone_bundle<F, P>(
        &self,
        old_timezone: String,
        new_timezone: String,
        finalize: F,
        postcommit_failure: P,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<()> + Send + Sync + 'static,
        P: FnOnce(String) + Send + Sync + 'static,
    {
        let rx = self
            .enqueue_timezone_bundle(
                old_timezone,
                new_timezone,
                Box::new(finalize),
                Box::new(postcommit_failure),
            )
            .await?;
        rx.await?
    }

    async fn enqueue_timezone_bundle(
        &self,
        old_timezone: String,
        new_timezone: String,
        finalize: TimezoneFinalize,
        postcommit_failure: PostCommitFailure,
    ) -> Result<oneshot::Receiver<Result<u64>>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::MigrateTimezoneBundle {
                old_timezone,
                new_timezone,
                finalize,
                postcommit_failure,
                reply,
            })
            .await?;
        Ok(rx)
    }

    pub async fn log_fetch(
        &self,
        unified_uuid: Option<String>,
        endpoint: String,
        ok: bool,
        message: Option<String>,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::LogFetch {
                unified_uuid,
                endpoint,
                ok,
                message,
                reply,
            })
            .await?;
        rx.await?
    }

    /// 触发 db actor graceful shutdown:checkpoint WAL 后退出线程。
    pub async fn shutdown(&self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(DbCommand::Shutdown { reply }).await?;
        rx.await?
    }
}

fn handle(db: &mut DbInstance, cmd: DbCommand) {
    match cmd {
        DbCommand::PutHistorySnapshot {
            snapshot,
            fetched_at_unix_ms,
            reply,
        } => {
            let _ = reply.send(db.put_history_snapshot(&snapshot, fetched_at_unix_ms));
        }
        DbCommand::GetLatestSnapshot { uuid, reply } => {
            let _ = reply.send(db.get_latest_snapshot(&uuid));
        }
        DbCommand::ReplaceProviderProvinces {
            provider,
            provinces,
            reply,
        } => {
            let _ = reply.send(db.replace_provider_provinces(&provider, &provinces));
        }
        DbCommand::GetProviderProvinces { provider, reply } => {
            let _ = reply.send(db.get_provider_provinces(&provider));
        }
        DbCommand::ResolveProviderProvinceCode {
            provider,
            province,
            reply,
        } => {
            let _ = reply.send(db.resolve_provider_province_code(&provider, &province));
        }
        DbCommand::ReplaceProviderCities {
            provider,
            provider_province_code,
            cities,
            reply,
        } => {
            let _ =
                reply.send(db.replace_provider_cities(&provider, &provider_province_code, &cities));
        }
        DbCommand::GetProviderCities {
            provider,
            provider_province_code,
            reply,
        } => {
            let _ = reply.send(db.get_provider_cities(&provider, &provider_province_code));
        }
        DbCommand::GetProviderStationByUuid {
            provider,
            uuid,
            reply,
        } => {
            let _ = reply.send(db.get_provider_station_by_uuid(&provider, &uuid));
        }
        DbCommand::PutProviderStationMapping { station, reply } => {
            let _ = reply.send(db.put_provider_station_mapping(&station));
        }
        DbCommand::GetProviderStationByName {
            provider,
            display_name,
            reply,
        } => {
            let _ = reply.send(db.get_provider_station_by_name(&provider, &display_name));
        }
        DbCommand::GetDbTimezone { reply } => {
            let _ = reply.send(db.get_db_timezone());
        }
        DbCommand::MigrateTimezoneBundle {
            old_timezone,
            new_timezone,
            finalize,
            postcommit_failure,
            reply,
        } => {
            let result = match db.migrate_timezone(&old_timezone, &new_timezone) {
                Ok(rewritten) => match run_timezone_finalize(finalize) {
                    Ok(()) => match db.clear_pending_timezone(&new_timezone) {
                        Ok(()) => Ok(rewritten),
                        Err(err) => {
                            let message = format!("failed to clear timezone sync marker: {err:#}");
                            run_postcommit_failure(postcommit_failure, message.clone());
                            Err(anyhow!(message))
                        }
                    },
                    Err(err) => {
                        let message = format!("failed to finalize timezone config: {err:#}");
                        run_postcommit_failure(postcommit_failure, message.clone());
                        Err(anyhow!(message))
                    }
                },
                Err(err) => Err(err),
            };
            let _ = reply.send(result);
        }
        DbCommand::LogFetch {
            unified_uuid,
            endpoint,
            ok,
            message,
            reply,
        } => {
            let _ = reply.send(db.log_fetch(
                unified_uuid.as_deref(),
                &endpoint,
                ok,
                message.as_deref(),
            ));
        }
        DbCommand::Shutdown { reply } => {
            let _ = reply.send(db.checkpoint());
        }
    }
}

fn run_timezone_finalize(finalize: TimezoneFinalize) -> Result<()> {
    match catch_unwind(AssertUnwindSafe(finalize)) {
        Ok(result) => result,
        Err(payload) => Err(anyhow!(
            "timezone config finalizer panicked: {}",
            panic_message(payload)
        )),
    }
}

fn run_postcommit_failure(callback: PostCommitFailure, message: String) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| callback(message))) {
        eprintln!(
            "weather-db warn: timezone post-commit failure callback panicked: {}",
            panic_message(payload)
        );
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use anyhow::bail;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_bundle_caller_still_finalizes_and_clears_pending_marker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let actor = DbActor::start(path.clone(), "UTC".to_string()).unwrap();
        let finalized = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let finalize_flag = finalized.clone();
        let failure_flag = failed.clone();
        let finalize_entered = entered.clone();
        let finalize_release = release.clone();
        let migration_actor = actor.clone();

        let caller = tokio::spawn(async move {
            migration_actor
                .migrate_timezone_bundle(
                    "UTC".to_string(),
                    "Asia/Shanghai".to_string(),
                    move || {
                        finalize_entered.wait();
                        finalize_release.wait();
                        finalize_flag.store(true, Ordering::SeqCst);
                        Ok(())
                    },
                    move |_| failure_flag.store(true, Ordering::SeqCst),
                )
                .await
        });
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();

        assert_eq!(
            actor.get_db_timezone().await.unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
        assert!(finalized.load(Ordering::SeqCst));
        assert!(!failed.load(Ordering::SeqCst));
        assert_eq!(DbActor::inspect_pending_timezone(&path).unwrap(), None);
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn finalize_failure_keeps_pending_marker_and_invokes_callback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let actor = DbActor::start(path.clone(), "UTC".to_string()).unwrap();
        let failure = Arc::new(Mutex::new(None));
        let captured = failure.clone();

        let result = actor
            .migrate_timezone_bundle(
                "UTC".to_string(),
                "Asia/Shanghai".to_string(),
                || bail!("injected finalize failure"),
                move |message| *captured.lock().unwrap() = Some(message),
            )
            .await;

        assert!(result.is_err());
        assert!(
            failure
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|message| message.contains("injected finalize failure"))
        );
        assert_eq!(
            DbActor::inspect_pending_timezone(&path).unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn clear_marker_failure_keeps_pending_marker_and_invokes_callback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let actor = DbActor::start(path.clone(), "UTC".to_string()).unwrap();
        let failure = Arc::new(Mutex::new(None));
        let captured = failure.clone();
        let trigger_path = path.clone();

        let result = actor
            .migrate_timezone_bundle(
                "UTC".to_string(),
                "Asia/Shanghai".to_string(),
                move || {
                    let conn = rusqlite::Connection::open(trigger_path)?;
                    conn.execute_batch(
                        r#"CREATE TRIGGER fail_timezone_marker_delete
                           BEFORE DELETE ON engine_state
                           WHEN OLD.key = 'timezone_config_sync_pending'
                           BEGIN SELECT RAISE(ABORT, 'injected marker delete failure'); END;"#,
                    )?;
                    Ok(())
                },
                move |message| *captured.lock().unwrap() = Some(message),
            )
            .await;

        let err = result.unwrap_err().to_string();
        assert!(err.contains("clear timezone sync marker"), "{err}");
        assert!(
            failure
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|message| message.contains("marker delete failure"))
        );
        assert_eq!(
            DbActor::inspect_pending_timezone(&path).unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn finalizer_panic_is_reported_without_killing_actor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let actor = DbActor::start(path.clone(), "UTC".to_string()).unwrap();
        let callback_called = Arc::new(AtomicBool::new(false));
        let callback_flag = callback_called.clone();

        let result = actor
            .migrate_timezone_bundle(
                "UTC".to_string(),
                "Asia/Shanghai".to_string(),
                || panic!("injected finalizer panic"),
                move |_| callback_flag.store(true, Ordering::SeqCst),
            )
            .await;

        assert!(result.unwrap_err().to_string().contains("panicked"));
        assert!(callback_called.load(Ordering::SeqCst));
        assert_eq!(
            actor.get_db_timezone().await.unwrap().as_deref(),
            Some("Asia/Shanghai")
        );
        actor.shutdown().await.unwrap();
    }
}
