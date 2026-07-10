use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use weather_schema::{StationRef, WeatherSnapshot};

use crate::storage::{self, DbInstance};

type TimezoneFinalize = Box<dyn FnOnce() -> Result<()> + Send + Sync + 'static>;
type PostCommitFailure = Box<dyn FnOnce(String) + Send + Sync + 'static>;

pub struct DbActor {
    inner: Arc<Inner>,
}

struct Inner {
    send_gate: AsyncMutex<()>,
    state: Arc<ActorState>,
    core: Mutex<Core>,
    terminal: watch::Sender<Option<ActorOutcome>>,
    public_handles: AtomicUsize,
}

struct ActorState {
    phase: AtomicU8,
    poison: Mutex<Option<Arc<str>>>,
}

struct Core {
    tx: Option<mpsc::Sender<DbCommand>>,
}

#[derive(Debug, Clone)]
enum ActorOutcome {
    Success,
    Failure(Arc<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ActorPhase {
    Open = 0,
    Poisoned = 1,
    Closing = 2,
    Closed = 3,
}

const ACTOR_CLOSED_ERROR: &str = "database actor is closing or closed";
const ACTOR_POISONED_FALLBACK: &str = "database actor is poisoned";

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
    #[cfg(test)]
    PanicWorker,
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

impl Clone for DbActor {
    fn clone(&self) -> Self {
        self.inner.public_handles.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for DbActor {
    fn drop(&mut self) {
        if self.inner.public_handles.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // A safe API call borrows a public handle. Therefore once the manual
        // public-handle count reaches zero, no admission can still hold the
        // gate or a temporary Sender clone.
        close_sender(&self.inner);
    }
}

impl ActorState {
    fn phase(&self) -> ActorPhase {
        match self.phase.load(Ordering::Acquire) {
            value if value == ActorPhase::Open as u8 => ActorPhase::Open,
            value if value == ActorPhase::Poisoned as u8 => ActorPhase::Poisoned,
            value if value == ActorPhase::Closing as u8 => ActorPhase::Closing,
            _ => ActorPhase::Closed,
        }
    }

    fn poison(&self, message: Arc<str>) -> Arc<str> {
        let stable = {
            let mut poison = lock_unpoisoned(&self.poison);
            poison.get_or_insert(message).clone()
        };
        let _ = self.phase.compare_exchange(
            ActorPhase::Open as u8,
            ActorPhase::Poisoned as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        stable
    }

    fn poison_error(&self) -> Arc<str> {
        lock_unpoisoned(&self.poison)
            .clone()
            .unwrap_or_else(|| Arc::from(ACTOR_POISONED_FALLBACK))
    }
}

impl ActorOutcome {
    fn from_result(result: Result<()>) -> Self {
        match result {
            Ok(()) => Self::Success,
            Err(err) => Self::Failure(Arc::from(format!("{err:#}"))),
        }
    }

    fn into_result(self) -> Result<()> {
        match self {
            Self::Success => Ok(()),
            Self::Failure(message) => Err(anyhow!(message.to_string())),
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Success => "success".to_string(),
            Self::Failure(message) => format!("failure: {message}"),
        }
    }
}

impl DbActor {
    pub fn inspect_pending_timezone(path: &Path) -> Result<Option<String>> {
        storage::inspect_pending_timezone(path)
    }

    pub fn start(path: PathBuf, config_tz: String) -> Result<Self> {
        Self::start_with_lease(path, config_tz, ())
    }

    pub fn start_with_lease<L>(path: PathBuf, config_tz: String, lease: L) -> Result<Self>
    where
        L: Send + 'static,
    {
        Self::start_with_lease_and_opener(lease, move || DbInstance::open(path, &config_tz))
    }

    fn start_with_lease_and_opener<L, F>(lease: L, opener: F) -> Result<Self>
    where
        L: Send + 'static,
        F: FnOnce() -> Result<DbInstance> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<DbCommand>(128);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let state = Arc::new(ActorState {
            phase: AtomicU8::new(ActorPhase::Open as u8),
            poison: Mutex::new(None),
        });
        let worker_state = state.clone();
        let worker = std::thread::Builder::new()
            .name("weather-db-worker".to_string())
            .spawn(move || {
                let lease = lease;
                let mut db = match opener() {
                    Ok(db) => db,
                    Err(err) => {
                        let message: Arc<str> =
                            Arc::from(format!("failed to initialize database: {err:#}"));
                        let _ = ready_tx.send(Err(message.clone()));
                        drop(lease);
                        return ActorOutcome::Failure(message);
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    drop(db);
                    drop(lease);
                    return ActorOutcome::Failure(Arc::from(
                        "database startup readiness receiver disconnected",
                    ));
                }
                let outcome = run_worker(&mut db, rx, &worker_state);
                drop(db);
                drop(lease);
                outcome
            })
            .context("failed to spawn DB actor worker")?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                drop(tx);
                return Err(join_start_failure(
                    format!("DB actor initialization failed: {message}"),
                    worker,
                ));
            }
            Err(err) => {
                drop(tx);
                return Err(join_start_failure(
                    format!("DB actor readiness channel disconnected: {err}"),
                    worker,
                ));
            }
        }

        let (terminal, _) = watch::channel(None);
        let worker_slot = Arc::new(Mutex::new(Some(worker)));
        let reaper_slot = worker_slot.clone();
        let reaper_state = state.clone();
        let reaper_terminal = terminal.clone();
        if let Err(spawn_error) = std::thread::Builder::new()
            .name("weather-db-reaper".to_string())
            .spawn(move || {
                let worker = lock_unpoisoned(&reaper_slot)
                    .take()
                    .expect("DB worker handle was already reaped");
                let outcome = join_worker(worker);
                publish_terminal(&reaper_state, &reaper_terminal, outcome);
            })
        {
            drop(tx);
            let worker = lock_unpoisoned(&worker_slot)
                .take()
                .context("DB worker handle missing after reaper spawn failure")?;
            let outcome = join_worker(worker);
            return Err(anyhow!(
                "failed to spawn DB actor reaper: {spawn_error}; database worker joined with {}",
                outcome.description()
            ));
        }

        Ok(Self {
            inner: Arc::new(Inner {
                send_gate: AsyncMutex::new(()),
                state,
                core: Mutex::new(Core { tx: Some(tx) }),
                terminal,
                public_handles: AtomicUsize::new(1),
            }),
        })
    }

    async fn admit<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T>>) -> DbCommand,
    ) -> Result<oneshot::Receiver<Result<T>>> {
        let _gate = self.inner.send_gate.lock().await;
        match self.inner.state.phase() {
            ActorPhase::Open => {}
            ActorPhase::Poisoned => {
                return Err(anyhow!(self.inner.state.poison_error().to_string()));
            }
            ActorPhase::Closing | ActorPhase::Closed => {
                return Err(anyhow!(ACTOR_CLOSED_ERROR));
            }
        }

        let tx = lock_unpoisoned(&self.inner.core)
            .tx
            .clone()
            .context(ACTOR_CLOSED_ERROR)?;
        let (reply, rx) = oneshot::channel();
        if tx.send(build(reply)).await.is_err() {
            drop(tx);
            close_sender(&self.inner);
            return Err(anyhow!(ACTOR_CLOSED_ERROR));
        }
        Ok(rx)
    }

    async fn call<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T>>) -> DbCommand,
    ) -> Result<T> {
        self.admit(build)
            .await?
            .await
            .context("database actor dropped command reply")?
    }

    pub async fn put_history_snapshot(
        &self,
        snapshot: WeatherSnapshot,
        fetched_at_unix_ms: i64,
    ) -> Result<()> {
        self.call(|reply| DbCommand::PutHistorySnapshot {
            snapshot: Box::new(snapshot),
            fetched_at_unix_ms,
            reply,
        })
        .await
    }

    pub async fn get_latest_snapshot(&self, uuid: String) -> Result<Option<StoredSnapshot>> {
        self.call(|reply| DbCommand::GetLatestSnapshot { uuid, reply })
            .await
    }

    pub async fn replace_provider_provinces(
        &self,
        provider: &str,
        provinces: Vec<ProviderProvince>,
    ) -> Result<()> {
        self.call(|reply| DbCommand::ReplaceProviderProvinces {
            provider: provider.to_string(),
            provinces,
            reply,
        })
        .await
    }

    pub async fn get_provider_provinces(
        &self,
        provider: &str,
    ) -> Result<Option<CatalogCache<ProviderProvince>>> {
        self.call(|reply| DbCommand::GetProviderProvinces {
            provider: provider.to_string(),
            reply,
        })
        .await
    }

    pub async fn resolve_provider_province_code(
        &self,
        provider: &str,
        province: &str,
    ) -> Result<String> {
        self.call(|reply| DbCommand::ResolveProviderProvinceCode {
            provider: provider.to_string(),
            province: province.to_string(),
            reply,
        })
        .await
    }

    pub async fn replace_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
        cities: Vec<ProviderCity>,
    ) -> Result<()> {
        self.call(|reply| DbCommand::ReplaceProviderCities {
            provider: provider.to_string(),
            provider_province_code: provider_province_code.to_string(),
            cities,
            reply,
        })
        .await
    }

    pub async fn get_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
    ) -> Result<Option<CatalogCache<ProviderCity>>> {
        self.call(|reply| DbCommand::GetProviderCities {
            provider: provider.to_string(),
            provider_province_code: provider_province_code.to_string(),
            reply,
        })
        .await
    }

    pub async fn get_provider_station_by_uuid(
        &self,
        provider: String,
        uuid: String,
    ) -> Result<Option<ProviderStation>> {
        self.call(|reply| DbCommand::GetProviderStationByUuid {
            provider,
            uuid,
            reply,
        })
        .await
    }

    pub async fn put_provider_station_mapping(&self, station: ProviderStation) -> Result<()> {
        self.call(|reply| DbCommand::PutProviderStationMapping { station, reply })
            .await
    }

    pub async fn get_provider_station_by_name(
        &self,
        provider: String,
        display_name: String,
    ) -> Result<Option<ProviderStation>> {
        self.call(|reply| DbCommand::GetProviderStationByName {
            provider,
            display_name,
            reply,
        })
        .await
    }

    pub async fn get_db_timezone(&self) -> Result<Option<String>> {
        self.call(|reply| DbCommand::GetDbTimezone { reply }).await
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
        self.enqueue_timezone_bundle(
            old_timezone,
            new_timezone,
            Box::new(finalize),
            Box::new(postcommit_failure),
        )
        .await?
        .await
        .context("database actor dropped timezone migration reply")?
    }

    async fn enqueue_timezone_bundle(
        &self,
        old_timezone: String,
        new_timezone: String,
        finalize: TimezoneFinalize,
        postcommit_failure: PostCommitFailure,
    ) -> Result<oneshot::Receiver<Result<u64>>> {
        self.admit(|reply| DbCommand::MigrateTimezoneBundle {
            old_timezone,
            new_timezone,
            finalize,
            postcommit_failure,
            reply,
        })
        .await
    }

    pub async fn log_fetch(
        &self,
        unified_uuid: Option<String>,
        endpoint: String,
        ok: bool,
        message: Option<String>,
    ) -> Result<()> {
        self.call(|reply| DbCommand::LogFetch {
            unified_uuid,
            endpoint,
            ok,
            message,
            reply,
        })
        .await
    }

    /// Checkpoint the WAL and join the worker. Concurrent calls share one result.
    pub async fn shutdown(&self) -> Result<()> {
        {
            let _gate = self.inner.send_gate.lock().await;
            close_sender(&self.inner);
        }
        self.wait_terminal().await
    }

    async fn wait_terminal(&self) -> Result<()> {
        let mut terminal = self.inner.terminal.subscribe();
        loop {
            if let Some(outcome) = terminal.borrow().clone() {
                return outcome.into_result();
            }
            terminal
                .changed()
                .await
                .context("database actor terminal notification closed")?;
        }
    }
}

fn close_sender(inner: &Inner) {
    mark_closing(&inner.state);
    drop(lock_unpoisoned(&inner.core).tx.take());
}

fn mark_closing(state: &ActorState) {
    let mut phase = state.phase.load(Ordering::Acquire);
    loop {
        if phase == ActorPhase::Closing as u8 || phase == ActorPhase::Closed as u8 {
            return;
        }
        match state.phase.compare_exchange_weak(
            phase,
            ActorPhase::Closing as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(actual) => phase = actual,
        }
    }
}

fn join_worker(worker: JoinHandle<ActorOutcome>) -> ActorOutcome {
    match worker.join() {
        Ok(outcome) => outcome,
        Err(payload) => ActorOutcome::Failure(Arc::from(format!(
            "database actor worker panicked: {}",
            panic_message(payload)
        ))),
    }
}

fn publish_terminal(
    state: &ActorState,
    terminal: &watch::Sender<Option<ActorOutcome>>,
    outcome: ActorOutcome,
) {
    state
        .phase
        .store(ActorPhase::Closed as u8, Ordering::Release);
    terminal.send_replace(Some(outcome));
}

fn join_start_failure(readiness: String, worker: JoinHandle<ActorOutcome>) -> anyhow::Error {
    match worker.join() {
        Ok(outcome) => anyhow!(
            "{readiness}; database worker joined with {}",
            outcome.description()
        ),
        Err(payload) => anyhow!(
            "{readiness}; database worker joined after panic: {}",
            panic_message(payload)
        ),
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run_worker(
    db: &mut DbInstance,
    mut rx: mpsc::Receiver<DbCommand>,
    state: &ActorState,
) -> ActorOutcome {
    let mut poison = None;
    loop {
        let Some(command) = rx.blocking_recv() else {
            return ActorOutcome::from_result(db.checkpoint());
        };
        if let Some(message) = poison.as_ref() {
            command.reject(message);
            continue;
        }
        if let Some(message) = handle(db, command) {
            poison = Some(state.poison(message));
        }
    }
}

impl DbCommand {
    fn reject(self, message: &Arc<str>) {
        match self {
            Self::PutHistorySnapshot { reply, .. }
            | Self::ReplaceProviderProvinces { reply, .. }
            | Self::ReplaceProviderCities { reply, .. }
            | Self::PutProviderStationMapping { reply, .. }
            | Self::LogFetch { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::GetLatestSnapshot { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::GetProviderProvinces { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::ResolveProviderProvinceCode { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::GetProviderCities { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::GetProviderStationByUuid { reply, .. }
            | Self::GetProviderStationByName { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::GetDbTimezone { reply } => {
                let _ = reply.send(rejected_command(message));
            }
            Self::MigrateTimezoneBundle { reply, .. } => {
                let _ = reply.send(rejected_command(message));
            }
            #[cfg(test)]
            Self::PanicWorker => {}
        }
    }
}

fn rejected_command<T>(message: &Arc<str>) -> Result<T> {
    Err(anyhow!(message.to_string()))
}

fn handle(db: &mut DbInstance, cmd: DbCommand) -> Option<Arc<str>> {
    match cmd {
        DbCommand::PutHistorySnapshot {
            snapshot,
            fetched_at_unix_ms,
            reply,
        } => {
            let _ = reply.send(db.put_history_snapshot(&snapshot, fetched_at_unix_ms));
            None
        }
        DbCommand::GetLatestSnapshot { uuid, reply } => {
            let _ = reply.send(db.get_latest_snapshot(&uuid));
            None
        }
        DbCommand::ReplaceProviderProvinces {
            provider,
            provinces,
            reply,
        } => {
            let _ = reply.send(db.replace_provider_provinces(&provider, &provinces));
            None
        }
        DbCommand::GetProviderProvinces { provider, reply } => {
            let _ = reply.send(db.get_provider_provinces(&provider));
            None
        }
        DbCommand::ResolveProviderProvinceCode {
            provider,
            province,
            reply,
        } => {
            let _ = reply.send(db.resolve_provider_province_code(&provider, &province));
            None
        }
        DbCommand::ReplaceProviderCities {
            provider,
            provider_province_code,
            cities,
            reply,
        } => {
            let _ =
                reply.send(db.replace_provider_cities(&provider, &provider_province_code, &cities));
            None
        }
        DbCommand::GetProviderCities {
            provider,
            provider_province_code,
            reply,
        } => {
            let _ = reply.send(db.get_provider_cities(&provider, &provider_province_code));
            None
        }
        DbCommand::GetProviderStationByUuid {
            provider,
            uuid,
            reply,
        } => {
            let _ = reply.send(db.get_provider_station_by_uuid(&provider, &uuid));
            None
        }
        DbCommand::PutProviderStationMapping { station, reply } => {
            let _ = reply.send(db.put_provider_station_mapping(&station));
            None
        }
        DbCommand::GetProviderStationByName {
            provider,
            display_name,
            reply,
        } => {
            let _ = reply.send(db.get_provider_station_by_name(&provider, &display_name));
            None
        }
        DbCommand::GetDbTimezone { reply } => {
            let _ = reply.send(db.get_db_timezone());
            None
        }
        DbCommand::MigrateTimezoneBundle {
            old_timezone,
            new_timezone,
            finalize,
            postcommit_failure,
            reply,
        } => {
            let rewritten = match db.migrate_timezone(&old_timezone, &new_timezone) {
                Ok(rewritten) => rewritten,
                Err(err) => {
                    let _ = reply.send(Err(err));
                    return None;
                }
            };
            let postcommit_error = match run_timezone_finalize(finalize) {
                Ok(()) => db
                    .clear_pending_timezone(&new_timezone)
                    .err()
                    .map(|err| format!("failed to clear timezone sync marker: {err:#}")),
                Err(err) => Some(format!("failed to finalize timezone config: {err:#}")),
            };
            if let Some(message) = postcommit_error {
                let poison: Arc<str> = Arc::from(format!(
                    "database actor poisoned after timezone post-commit failure: {message}"
                ));
                run_postcommit_failure(postcommit_failure, poison.to_string());
                let _ = reply.send(Err(anyhow!(poison.to_string())));
                Some(poison)
            } else {
                let _ = reply.send(Ok(rewritten));
                None
            }
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
            None
        }
        #[cfg(test)]
        DbCommand::PanicWorker => panic!("injected post-ready worker panic"),
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
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::bail;

    use super::*;

    struct CountedLease(Arc<AtomicUsize>);

    impl Drop for CountedLease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct SignaledLease(Option<std::sync::mpsc::Sender<()>>);

    impl Drop for SignaledLease {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn initialization_error_joins_worker_before_returning() {
        let drops = Arc::new(AtomicUsize::new(0));
        let result = DbActor::start_with_lease_and_opener(CountedLease(drops.clone()), || {
            Err(anyhow!("injected open failure"))
        });
        let error = match result {
            Ok(_) => panic!("initialization unexpectedly succeeded"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("injected open failure"), "{error}");
        assert!(error.contains("worker joined"), "{error}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pre_ready_panic_disconnects_readiness_and_joins_worker() {
        let drops = Arc::new(AtomicUsize::new(0));
        let result = DbActor::start_with_lease_and_opener(CountedLease(drops.clone()), || {
            panic!("injected pre-ready panic")
        });
        let error = match result {
            Ok(_) => panic!("initialization unexpectedly succeeded"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("readiness channel disconnected"), "{error}");
        assert!(error.contains("joined after panic"), "{error}");
        assert!(error.contains("injected pre-ready panic"), "{error}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_last_clone_closes_channel_and_reaps_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let actor =
            DbActor::start_with_lease(path, "UTC".to_string(), SignaledLease(Some(dropped_tx)))
                .unwrap();
        let last = actor.clone();

        drop(actor);
        assert!(dropped_rx.try_recv().is_err());
        drop(last);

        dropped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("last clone drop did not reap the DB worker");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn normal_command_admission_is_linearized_with_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let actor = DbActor::start(directory.path().join("weather.db"), "UTC".to_string()).unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(33));
        let mut commands = Vec::new();
        for _ in 0..32 {
            let actor = actor.clone();
            let barrier = barrier.clone();
            commands.push(tokio::spawn(async move {
                barrier.wait().await;
                actor.get_db_timezone().await
            }));
        }
        let shutdown_actor = actor.clone();
        let shutdown_barrier = barrier.clone();
        let shutdown = tokio::spawn(async move {
            shutdown_barrier.wait().await;
            shutdown_actor.shutdown().await
        });

        for command in commands {
            let result = tokio::time::timeout(Duration::from_secs(2), command)
                .await
                .expect("normal command hung")
                .unwrap();
            if let Err(error) = result {
                assert_eq!(error.to_string(), ACTOR_CLOSED_ERROR);
            }
        }
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown hung")
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_shutdown_is_idempotent_and_joins_once() {
        let directory = tempfile::tempdir().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let actor = DbActor::start_with_lease(
            directory.path().join("weather.db"),
            "UTC".to_string(),
            CountedLease(drops.clone()),
        )
        .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut callers = Vec::new();
        for _ in 0..16 {
            let actor = actor.clone();
            let barrier = barrier.clone();
            callers.push(tokio::spawn(async move {
                barrier.wait().await;
                actor.shutdown().await.map_err(|error| error.to_string())
            }));
        }

        let mut outcomes = Vec::new();
        for caller in callers {
            outcomes.push(
                tokio::time::timeout(Duration::from_secs(2), caller)
                    .await
                    .expect("concurrent shutdown hung")
                    .unwrap(),
            );
        }
        assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(actor.inner.state.phase(), ActorPhase::Closed);
        assert!(lock_unpoisoned(&actor.inner.core).tx.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn canceled_shutdown_caller_does_not_cancel_reaping() {
        let directory = tempfile::tempdir().unwrap();
        let actor = DbActor::start(directory.path().join("weather.db"), "UTC".to_string()).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let finalize_entered = entered.clone();
        let finalize_release = release.clone();
        let migration_actor = actor.clone();
        let migration = tokio::spawn(async move {
            migration_actor
                .migrate_timezone_bundle(
                    "UTC".to_string(),
                    "Asia/Shanghai".to_string(),
                    move || {
                        finalize_entered.wait();
                        finalize_release.wait();
                        Ok(())
                    },
                    |_| {},
                )
                .await
        });
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .unwrap();

        let first_actor = actor.clone();
        let first = tokio::spawn(async move { first_actor.shutdown().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while actor.inner.state.phase() != ActorPhase::Closing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first shutdown did not close admission");
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        migration.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), actor.shutdown())
            .await
            .expect("later shutdown did not observe terminal state")
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), actor.shutdown())
            .await
            .expect("closed shutdown did not return immediately")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_ready_worker_panic_publishes_one_stable_terminal_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let actor = DbActor::start_with_lease(
            directory.path().join("weather.db"),
            "UTC".to_string(),
            CountedLease(drops.clone()),
        )
        .unwrap();
        {
            let _gate = actor.inner.send_gate.lock().await;
            let tx = lock_unpoisoned(&actor.inner.core).tx.clone().unwrap();
            tx.send(DbCommand::PanicWorker).await.unwrap();
        }

        let terminal = tokio::time::timeout(Duration::from_secs(2), actor.wait_terminal())
            .await
            .expect("reaper did not publish the worker panic")
            .unwrap_err()
            .to_string();
        assert!(terminal.contains("injected post-ready worker panic"));
        assert_eq!(actor.inner.state.phase(), ActorPhase::Closed);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(actor.shutdown().await.unwrap_err().to_string(), terminal);
        assert_eq!(actor.shutdown().await.unwrap_err().to_string(), terminal);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn admitted_bundle_finishes_before_shutdown_checkpoint_and_join() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let actor = DbActor::start(path.clone(), "UTC".to_string()).unwrap();
        let finalized = Arc::new(AtomicBool::new(false));
        let finalized_flag = finalized.clone();
        let bundle = actor
            .enqueue_timezone_bundle(
                "UTC".to_string(),
                "Asia/Shanghai".to_string(),
                Box::new(move || {
                    finalized_flag.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                Box::new(|_| {}),
            )
            .await
            .unwrap();

        actor.shutdown().await.unwrap();
        assert_eq!(bundle.await.unwrap().unwrap(), 0);
        assert!(finalized.load(Ordering::SeqCst));
        assert_eq!(DbActor::inspect_pending_timezone(&path).unwrap(), None);
    }

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

        let poison = result.unwrap_err().to_string();
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
        assert_eq!(
            actor.get_db_timezone().await.unwrap_err().to_string(),
            poison
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

        let poison = result.unwrap_err().to_string();
        assert!(poison.contains("clear timezone sync marker"), "{poison}");
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
        assert_eq!(
            actor.get_db_timezone().await.unwrap_err().to_string(),
            poison
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn finalizer_panic_poisons_commands_but_not_shutdown() {
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

        let poison = result.unwrap_err().to_string();
        assert!(poison.contains("panicked"));
        assert!(callback_called.load(Ordering::SeqCst));
        assert_eq!(
            actor.get_db_timezone().await.unwrap_err().to_string(),
            poison
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn postcommit_failure_rejects_queued_and_new_commands_without_execution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("weather.db");
        let actor = DbActor::start(path.clone(), "UTC".to_string()).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let finalize_entered = entered.clone();
        let finalize_release = release.clone();
        let migration_actor = actor.clone();
        let migration = tokio::spawn(async move {
            migration_actor
                .migrate_timezone_bundle(
                    "UTC".to_string(),
                    "Asia/Shanghai".to_string(),
                    move || {
                        finalize_entered.wait();
                        finalize_release.wait();
                        bail!("injected finalize failure")
                    },
                    |_| {},
                )
                .await
        });
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .unwrap();

        let queued = actor
            .admit(|reply| DbCommand::LogFetch {
                unified_uuid: None,
                endpoint: "must-not-execute".to_string(),
                ok: false,
                message: None,
                reply,
            })
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();

        let poison = migration.await.unwrap().unwrap_err().to_string();
        assert_eq!(queued.await.unwrap().unwrap_err().to_string(), poison);
        assert_eq!(
            actor.get_db_timezone().await.unwrap_err().to_string(),
            poison
        );
        actor.shutdown().await.unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM upstream_fetch_log", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}
