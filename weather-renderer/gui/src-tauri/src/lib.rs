mod gui_cache;
mod gui_config;

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use prost::Message as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager as _, RunEvent, State, WindowEvent};
use tokio::{sync::Mutex, time::Instant};
use weather_renderer_common::{
    DaemonExecutableNotFound, DaemonSupervisor, EngineClient, EngineEvent, EngineOwnership,
    require_config,
};
use weather_schema::{
    AppConfig, Empty, EngineStatus, EventKind, FetchLogEvent, FuzzyMatchStationsRequest,
    GetResourceRequest, GetResourceResponse, GetTemperatureHistoryRequest,
    GetTemperatureHistoryResponse, GetWeatherRequest, RefreshEvent, ResourceTransferState, RpcKind,
    StationConfig, StationRef, WeatherSnapshot, WeatherSnapshotEvent, unified_station_uuid,
};

use crate::gui_cache::{GuiWeatherCache, resolve_gui_cache_path};
use crate::gui_config::{GuiConfigPayload, GuiConfigStore, resolve_gui_config_path};

const SEARCH_PAGE_SIZE: u32 = 24;
const RESOURCE_CHUNK_BYTES: u32 = 512 * 1024;
const MAX_RESOURCE_BYTES: u64 = 8 * 1024 * 1024;
const RESOURCE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_RESOURCE_RETRY_MS: u32 = 75;
const MIN_RESOURCE_RETRY_MS: u32 = 10;
const MAX_RESOURCE_RETRY_MS: u32 = 1_000;
const GUI_EXIT_ANIMATION_DELAY: Duration = Duration::from_millis(180);

struct RendererSession {
    client: EngineClient,
    ownership: EngineOwnership,
}

struct GuiState {
    session: Mutex<Option<RendererSession>>,
    config_update: Mutex<()>,
    gui_config: Mutex<GuiConfigStore>,
    gui_weather_cache: GuiWeatherCache,
    gui_cache_update: Mutex<()>,
    cached_station_names: Mutex<HashSet<String>>,
    shutdown_started: AtomicBool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapPayload {
    config: AppConfig,
    status: EngineStatus,
    initial_weather: Option<WeatherSnapshot>,
    cached_weather: Vec<WeatherSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum GuiEngineEvent {
    Weather {
        snapshot: Box<WeatherSnapshot>,
    },
    Status {
        status: EngineStatus,
    },
    Fetch {
        topic: String,
        event: FetchLogEvent,
    },
    Refresh {
        topic: String,
        event: RefreshEvent,
    },
    Log {
        level: &'static str,
        message: String,
    },
}

impl GuiState {
    fn new(gui_config: GuiConfigStore, gui_weather_cache: GuiWeatherCache) -> Self {
        Self {
            session: Mutex::new(None),
            config_update: Mutex::new(()),
            gui_config: Mutex::new(gui_config),
            gui_weather_cache,
            gui_cache_update: Mutex::new(()),
            cached_station_names: Mutex::new(HashSet::new()),
            shutdown_started: AtomicBool::new(false),
        }
    }

    async fn configure_cached_stations(&self, config: &AppConfig) -> HashSet<String> {
        let names = config
            .stations
            .iter()
            .map(|station| station.name.clone())
            .collect::<HashSet<_>>();
        *self.cached_station_names.lock().await = names.clone();
        names
    }

    async fn cache_weather_if_configured(&self, app: &AppHandle, snapshot: &WeatherSnapshot) {
        let Some(station_name) = snapshot
            .station
            .as_ref()
            .map(|station| station.name.as_str())
            .filter(|name| !name.is_empty())
        else {
            return;
        };
        let _cache_update = self.gui_cache_update.lock().await;
        if !self
            .cached_station_names
            .lock()
            .await
            .contains(station_name)
        {
            return;
        }
        if let Err(error) = self.gui_weather_cache.store(snapshot.clone()).await {
            emit_gui_log(app, "warning", format!("GUI 天气缓存写入失败：{error:#}"));
        }
    }

    async fn client(&self, app: &AppHandle) -> Result<EngineClient, String> {
        let mut session = self.session.lock().await;
        if let Some(active) = session.as_ref() {
            return Ok(active.client.clone());
        }

        let _ = app.emit("connection-status", "connecting");
        let (rpc_endpoint, pub_endpoint, ownership) = match resolve_direct_endpoints()? {
            Some((rpc, publisher)) => (rpc, publisher, EngineOwnership::Direct),
            None => {
                let daemon = DaemonSupervisor::new(resolve_daemon_exe(app), resolve_config_path())
                    .map_err(display_error)?;
                let probe = daemon.probe().await.map_err(display_daemon_error)?;
                let ready = daemon
                    .ensure_ready(probe)
                    .await
                    .map_err(display_daemon_error)?;
                (
                    ready.probe.rpc_endpoint,
                    ready.probe.pub_endpoint,
                    ready.ownership,
                )
            }
        };
        let client = EngineClient::connect(rpc_endpoint, pub_endpoint, resolve_hmac_key()?)
            .await
            .map_err(display_error)?;

        forward_engine_events(app.clone(), client.subscribe_events());
        *session = Some(RendererSession {
            client: client.clone(),
            ownership,
        });
        let _ = app.emit("connection-status", "connected");
        Ok(client)
    }

    async fn take_session(&self) -> Option<RendererSession> {
        self.session.lock().await.take()
    }

    async fn shutdown_owned(&self) {
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(session) = self.take_session().await {
            close_session(session, true).await;
        }
    }
}

fn resolve_daemon_exe(app: &AppHandle) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WEATHER_DAEMON_EXE") {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        "weather-daemon.exe"
    } else {
        "weather-daemon"
    };
    let resource_dir = app.path().resource_dir().ok()?;
    [
        resource_dir.join("bin").join(name),
        resource_dir.join("resources").join("bin").join(name),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn resolve_config_path() -> Option<PathBuf> {
    std::env::var_os("WEATHER_CONFIG").map(PathBuf::from)
}

fn resolve_direct_endpoints() -> Result<Option<(String, String)>, String> {
    let rpc = std::env::var("WEATHER_RPC_ENDPOINT").ok();
    let publisher = std::env::var("WEATHER_PUB_ENDPOINT").ok();
    match (rpc, publisher) {
        (None, None) => Ok(None),
        (Some(rpc), Some(publisher)) if !rpc.is_empty() && !publisher.is_empty() => {
            Ok(Some((rpc, publisher)))
        }
        _ => Err("WEATHER_RPC_ENDPOINT 与 WEATHER_PUB_ENDPOINT 必须同时设置且不能为空".to_string()),
    }
}

fn resolve_hmac_key() -> Result<Option<[u8; 32]>, String> {
    let direct = std::env::var("WEATHER_HMAC_KEY").ok();
    let from_named_env = std::env::var("WEATHER_HMAC_ENV_KEY_NAME")
        .ok()
        .filter(|name| !name.is_empty())
        .map(|name| {
            std::env::var(&name).map_err(|_| format!("HMAC 环境变量 `{name}` 未设置或不是 UTF-8"))
        })
        .transpose()?;
    let value = direct.or(from_named_env);
    if value.as_ref().is_some_and(String::is_empty) {
        return Err("HMAC key 不能为空".to_string());
    }
    value
        .map(|key| weather_schema::hmac_key_from_str(&key).map_err(display_error))
        .transpose()
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn display_daemon_error(error: anyhow::Error) -> String {
    if let Some(missing) = error.downcast_ref::<DaemonExecutableNotFound>() {
        return format!(
            "未找到命令 `{}`。请先运行 `cargo build -p weather-daemon`，或设置 WEATHER_DAEMON_EXE 指向可执行文件。",
            missing.executable().display()
        );
    }
    display_error(error)
}

fn emit_gui_log(app: &AppHandle, level: &'static str, message: String) {
    let _ = app.emit("engine-event", GuiEngineEvent::Log { level, message });
}

fn forward_engine_events(
    app: AppHandle,
    mut events: tokio::sync::broadcast::Receiver<EngineEvent>,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(payload) = decode_engine_event(event) {
                        if let GuiEngineEvent::Weather { snapshot } = &payload {
                            app.state::<GuiState>()
                                .cache_weather_if_configured(&app, snapshot)
                                .await;
                        }
                        let _ = app.emit("engine-event", payload);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    let _ = app.emit(
                        "engine-event",
                        GuiEngineEvent::Log {
                            level: "warning",
                            message: format!("事件流过载，已跳过 {skipped} 条消息"),
                        },
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn decode_engine_event(event: EngineEvent) -> Option<GuiEngineEvent> {
    match EventKind::try_from(event.envelope.kind).ok()? {
        EventKind::WeatherSnapshot => {
            WeatherSnapshotEvent::decode(event.envelope.payload.as_slice())
                .ok()?
                .snapshot
                .map(|snapshot| GuiEngineEvent::Weather {
                    snapshot: Box::new(snapshot),
                })
        }
        EventKind::EngineStatus => EngineStatus::decode(event.envelope.payload.as_slice())
            .ok()
            .map(|status| GuiEngineEvent::Status { status }),
        EventKind::FetchLog => FetchLogEvent::decode(event.envelope.payload.as_slice())
            .ok()
            .map(|decoded| GuiEngineEvent::Fetch {
                topic: event.topic,
                event: decoded,
            }),
        EventKind::Refresh => RefreshEvent::decode(event.envelope.payload.as_slice())
            .ok()
            .map(|decoded| GuiEngineEvent::Refresh {
                topic: event.topic,
                event: decoded,
            }),
        EventKind::Unspecified => None,
    }
}

async fn close_session(mut session: RendererSession, shutdown_owned: bool) {
    if let EngineOwnership::Owned {
        owner_token,
        foreground,
    } = &mut session.ownership
        && shutdown_owned
        && session.client.shutdown_if_owned(owner_token).await.is_ok()
    {
        foreground.mark_graceful_shutdown_requested();
    }
    session.client.close().await;
}

async fn weather_for(
    client: &EngineClient,
    station_name: &str,
    unified_uuid: Option<String>,
    refresh: bool,
) -> anyhow::Result<WeatherSnapshot> {
    let unified_uuid = unified_uuid
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| unified_station_uuid(station_name));
    client
        .request(
            RpcKind::GetWeather,
            GetWeatherRequest {
                unified_uuid,
                refresh,
                include_debug: false,
            },
        )
        .await
}

#[tauri::command]
async fn bootstrap(app: AppHandle, state: State<'_, GuiState>) -> Result<BootstrapPayload, String> {
    let client = state.client(&app).await.inspect_err(|_error| {
        let _ = app.emit("connection-status", "failed");
    })?;
    let (config_response, status) =
        tokio::try_join!(client.get_config(false), client.status()).map_err(display_error)?;
    let config = require_config(config_response.config, "get-config").map_err(display_error)?;
    let cached_station_names = state.configure_cached_stations(&config).await;
    let _cache_update = state.gui_cache_update.lock().await;
    let cached_weather = match state
        .gui_weather_cache
        .load_today(cached_station_names)
        .await
    {
        Ok(cached) => cached,
        Err(error) => {
            emit_gui_log(&app, "warning", format!("GUI 天气缓存读取失败：{error:#}"));
            Vec::new()
        }
    };
    let initial_station = config
        .stations
        .iter()
        .find(|station| station.enabled)
        .map(|station| station.name.as_str());
    let initial_weather = initial_station.and_then(|name| {
        cached_weather
            .iter()
            .find(|snapshot| {
                snapshot
                    .station
                    .as_ref()
                    .is_some_and(|station| station.name == name)
            })
            .cloned()
    });
    Ok(BootstrapPayload {
        config,
        status,
        initial_weather,
        cached_weather,
    })
}

#[tauri::command]
async fn get_weather(
    app: AppHandle,
    state: State<'_, GuiState>,
    station_name: String,
    unified_uuid: Option<String>,
    refresh: bool,
) -> Result<WeatherSnapshot, String> {
    let client = state.client(&app).await?;
    let snapshot = weather_for(&client, &station_name, unified_uuid, refresh)
        .await
        .map_err(display_error)?;
    state.cache_weather_if_configured(&app, &snapshot).await;
    Ok(snapshot)
}

#[tauri::command]
async fn get_temperature_history(
    app: AppHandle,
    state: State<'_, GuiState>,
    station_name: String,
    unified_uuid: Option<String>,
    before_date: Option<String>,
    page_size: u32,
) -> Result<GetTemperatureHistoryResponse, String> {
    let unified_uuid = unified_uuid
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| unified_station_uuid(&station_name));
    state
        .client(&app)
        .await?
        .request(
            RpcKind::GetTemperatureHistory,
            GetTemperatureHistoryRequest {
                unified_uuid,
                before_date,
                page_size,
            },
        )
        .await
        .map_err(display_error)
}

#[tauri::command]
async fn get_resource_bytes(
    app: AppHandle,
    state: State<'_, GuiState>,
    resource_id: String,
) -> Result<tauri::ipc::Response, String> {
    if resource_id.trim().is_empty() {
        return Err("resource_id 不能为空".to_string());
    }
    let client = state.client(&app).await?;
    let mut assembler = ResourceAssembler::new(resource_id);
    let deadline = Instant::now() + RESOURCE_TRANSFER_TIMEOUT;
    let mut pending_delay = Duration::ZERO;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "资源异步传输等待超过 {} 秒",
                RESOURCE_TRANSFER_TIMEOUT.as_secs()
            ));
        }
        let response: GetResourceResponse = client
            .request(
                RpcKind::GetResource,
                GetResourceRequest {
                    resource_id: assembler.resource_id.clone(),
                    offset: assembler.offset(),
                    max_bytes: RESOURCE_CHUNK_BYTES,
                },
            )
            .await
            .map_err(display_error)?;
        match assembler.push(response)? {
            ResourceProgress::Pending(delay) => {
                pending_delay = if pending_delay.is_zero() {
                    delay
                } else {
                    pending_delay
                        .saturating_mul(2)
                        .max(delay)
                        .min(Duration::from_millis(u64::from(MAX_RESOURCE_RETRY_MS)))
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(format!(
                        "资源异步传输等待超过 {} 秒",
                        RESOURCE_TRANSFER_TIMEOUT.as_secs()
                    ));
                }
                tokio::time::sleep(pending_delay.min(remaining)).await;
            }
            ResourceProgress::Incomplete => pending_delay = Duration::ZERO,
            ResourceProgress::Complete => break,
        }
    }
    Ok(tauri::ipc::Response::new(assembler.finish()?))
}

struct ResourceAssembler {
    resource_id: String,
    total_size: Option<u64>,
    content_type: Option<String>,
    bytes: Vec<u8>,
    complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceProgress {
    Pending(Duration),
    Incomplete,
    Complete,
}

impl ResourceAssembler {
    fn new(resource_id: String) -> Self {
        Self {
            resource_id,
            total_size: None,
            content_type: None,
            bytes: Vec::new(),
            complete: false,
        }
    }

    fn offset(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn push(&mut self, response: GetResourceResponse) -> Result<ResourceProgress, String> {
        if self.complete {
            return Err("资源已完整接收，不能继续追加分块".to_string());
        }
        if response.resource_id != self.resource_id {
            return Err("资源响应 ID 与请求不一致".to_string());
        }
        let transfer_state = ResourceTransferState::try_from(response.transfer_state)
            .map_err(|_| format!("未知资源传输状态 {}", response.transfer_state))?;
        if transfer_state == ResourceTransferState::Pending {
            if response.complete || !response.data.is_empty() {
                return Err("等待中的资源响应不能包含已完成数据".to_string());
            }
            if response.next_offset != self.offset() {
                return Err("等待中的资源响应改变了分块偏移".to_string());
            }
            let retry_after_ms = if response.retry_after_ms == 0 {
                DEFAULT_RESOURCE_RETRY_MS
            } else {
                response
                    .retry_after_ms
                    .clamp(MIN_RESOURCE_RETRY_MS, MAX_RESOURCE_RETRY_MS)
            };
            return Ok(ResourceProgress::Pending(Duration::from_millis(u64::from(
                retry_after_ms,
            ))));
        }
        if transfer_state != ResourceTransferState::Ready {
            return Err(format!("资源传输尚未就绪：{transfer_state:?}"));
        }
        if response.total_size > MAX_RESOURCE_BYTES {
            return Err(format!(
                "资源大小 {} 字节超过 {} 字节上限",
                response.total_size, MAX_RESOURCE_BYTES
            ));
        }
        if let Some(total_size) = self.total_size {
            if response.total_size != total_size {
                return Err("资源响应总长度在分块间发生变化".to_string());
            }
        } else {
            let capacity = usize::try_from(response.total_size)
                .map_err(|_| "资源总长度无法装入内存".to_string())?;
            self.bytes.reserve(capacity);
            self.total_size = Some(response.total_size);
        }
        if let Some(content_type) = self.content_type.as_deref() {
            if response.content_type != content_type {
                return Err("资源 MIME 类型在分块间发生变化".to_string());
            }
        } else {
            self.content_type = Some(response.content_type.clone());
        }

        let offset = self.offset();
        let data_len =
            u64::try_from(response.data.len()).map_err(|_| "资源分块长度无法表示".to_string())?;
        let expected_next = offset
            .checked_add(data_len)
            .ok_or_else(|| "资源分块偏移溢出".to_string())?;
        if response.next_offset != expected_next {
            return Err(format!(
                "资源分块偏移不连续：期望 {expected_next}，收到 {}",
                response.next_offset
            ));
        }
        if expected_next > response.total_size {
            return Err("资源分块超过声明的总长度".to_string());
        }
        if !response.complete && response.data.is_empty() {
            return Err("资源分块未推进偏移".to_string());
        }
        if response.complete != (expected_next == response.total_size) {
            return Err("资源完成标记与总长度不一致".to_string());
        }
        self.bytes.extend_from_slice(&response.data);
        self.complete = response.complete;
        Ok(if self.complete {
            ResourceProgress::Complete
        } else {
            ResourceProgress::Incomplete
        })
    }

    fn finish(self) -> Result<Vec<u8>, String> {
        if !self.complete {
            return Err("资源响应未完整接收".to_string());
        }
        if self.total_size != Some(self.bytes.len() as u64) {
            return Err("资源实际长度与声明长度不一致".to_string());
        }
        Ok(self.bytes)
    }
}

#[tauri::command]
async fn search_stations(
    app: AppHandle,
    state: State<'_, GuiState>,
    query: String,
) -> Result<Vec<StationRef>, String> {
    let client = state.client(&app).await?;
    let mut cursor = weather_renderer_common::pagination::PageCursor::default();
    let mut stations = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let (page_offset, page_size) = cursor.request(SEARCH_PAGE_SIZE).map_err(display_error)?;
        let page = client
            .request::<_, weather_schema::FuzzyMatchStationsResponse>(
                RpcKind::FuzzyMatchStations,
                FuzzyMatchStationsRequest {
                    query: query.clone(),
                    province: None,
                    page_offset,
                    page_size,
                },
            )
            .await
            .map_err(display_error)?;
        for station in page.stations {
            let key = if station.unified_uuid.is_empty() {
                format!("{}|{}|{}", station.province, station.city, station.name)
            } else {
                station.unified_uuid.clone()
            };
            if seen.insert(key) {
                stations.push(station);
            }
        }
        if !cursor
            .advance(page.has_more, page.next_offset)
            .map_err(display_error)?
        {
            break;
        }
    }
    Ok(stations)
}

#[tauri::command]
async fn update_stations(
    app: AppHandle,
    state: State<'_, GuiState>,
    stations: Vec<StationConfig>,
) -> Result<AppConfig, String> {
    validate_stations(&stations)?;
    let _update = state.config_update.lock().await;
    let client = state.client(&app).await?;
    let response = client.get_config(false).await.map_err(display_error)?;
    let mut config = require_config(response.config, "get-config").map_err(display_error)?;
    config.stations = stations;
    let response = client.update_config(config).await.map_err(display_error)?;
    let updated = require_config(response.config, "update-config").map_err(display_error)?;
    let station_names = state.configure_cached_stations(&updated).await;
    let _cache_update = state.gui_cache_update.lock().await;
    if let Err(error) = state.gui_weather_cache.load_today(station_names).await {
        emit_gui_log(&app, "warning", format!("GUI 天气缓存清理失败：{error:#}"));
    }
    Ok(updated)
}

fn validate_stations(stations: &[StationConfig]) -> Result<(), String> {
    let mut names = HashSet::new();
    for station in stations {
        if station.name.trim().is_empty() {
            return Err("站点名称不能为空".to_string());
        }
        if !names.insert(station.name.as_str()) {
            return Err(format!("站点 `{}` 重复", station.name));
        }
    }
    Ok(())
}

#[tauri::command]
async fn get_config_text(
    app: AppHandle,
    state: State<'_, GuiState>,
    defaults: bool,
) -> Result<String, String> {
    let client = state.client(&app).await?;
    let response = client.get_config(defaults).await.map_err(display_error)?;
    let config = require_config(response.config, "get-config").map_err(display_error)?;
    toml::to_string_pretty(&config).map_err(display_error)
}

#[tauri::command]
async fn get_gui_config(state: State<'_, GuiState>) -> Result<GuiConfigPayload, String> {
    state.gui_config.lock().await.payload()
}

#[tauri::command]
async fn set_gui_debug(
    state: State<'_, GuiState>,
    debug: bool,
) -> Result<GuiConfigPayload, String> {
    state.gui_config.lock().await.set_debug(debug)
}

#[tauri::command]
async fn open_gui_devtools(app: AppHandle, state: State<'_, GuiState>) -> Result<(), String> {
    if !state.gui_config.lock().await.debug_for_launch() {
        return Err("GUI 调试模式未启用".to_string());
    }
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "未找到主窗口".to_string())?;
    window.open_devtools();
    Ok(())
}

async fn prepare_gui_exit(state: &GuiState, minimum_delay: Duration) {
    tokio::join!(state.shutdown_owned(), tokio::time::sleep(minimum_delay),);
}

#[tauri::command]
async fn exit_gui(app: AppHandle, state: State<'_, GuiState>) -> Result<(), String> {
    let _ = app.emit("gui-close-requested", ());
    prepare_gui_exit(&state, GUI_EXIT_ANIMATION_DELAY).await;
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn engine_status(app: AppHandle, state: State<'_, GuiState>) -> Result<EngineStatus, String> {
    state
        .client(&app)
        .await?
        .status()
        .await
        .map_err(display_error)
}

#[tauri::command]
async fn restart_engine(app: AppHandle, state: State<'_, GuiState>) -> Result<String, String> {
    let _: Empty = state
        .client(&app)
        .await?
        .request(RpcKind::RestartEngine, Empty {})
        .await
        .map_err(display_error)?;
    Ok("引擎已接受重启请求".to_string())
}

#[tauri::command]
async fn stop_engine(state: State<'_, GuiState>) -> Result<String, String> {
    let Some(mut session) = state.take_session().await else {
        return Ok("引擎未连接".to_string());
    };
    let result = session.client.shutdown().await.map_err(display_error);
    if result.is_ok()
        && let EngineOwnership::Owned { foreground, .. } = &mut session.ownership
    {
        foreground.mark_graceful_shutdown_requested();
    }
    session.client.close().await;
    result.map(|_| "引擎已接受停止请求".to_string())
}

pub fn run() {
    let gui_config_path =
        resolve_gui_config_path().expect("failed to resolve weather GUI config path");
    let gui_cache_path =
        resolve_gui_cache_path(&gui_config_path).expect("failed to resolve weather GUI cache path");
    let gui_config = GuiConfigStore::open(gui_config_path);
    let gui_weather_cache = GuiWeatherCache::new(gui_cache_path);
    let debug = gui_config.debug_for_launch();
    let mut context = tauri::generate_context!();
    let main_window = context
        .config_mut()
        .app
        .windows
        .iter_mut()
        .find(|window| window.label == "main")
        .expect("weather GUI main window is missing from Tauri config");
    main_window.devtools = Some(debug);

    let app = tauri::Builder::default()
        .manage(GuiState::new(gui_config, gui_weather_cache))
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            get_weather,
            get_temperature_history,
            get_resource_bytes,
            search_stations,
            update_stations,
            get_config_text,
            get_gui_config,
            set_gui_debug,
            open_gui_devtools,
            exit_gui,
            engine_status,
            restart_engine,
            stop_engine,
        ])
        .build(context)
        .expect("failed to build weather GUI");

    app.run(|app_handle, event| match event {
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } if label == "main" => {
            api.prevent_close();
            let app_handle = app_handle.clone();
            let _ = app_handle.emit("gui-close-requested", ());
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<GuiState>();
                tokio::join!(
                    state.shutdown_owned(),
                    tokio::time::sleep(GUI_EXIT_ANIMATION_DELAY),
                );
                app_handle.exit(0);
            });
        }
        RunEvent::ExitRequested { .. } | RunEvent::Exit => {
            let state = app_handle.state::<GuiState>();
            tauri::async_runtime::block_on(state.shutdown_owned());
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gui_exit_preparation_finishes_shutdown_before_returning() {
        let temp = tempfile::tempdir().unwrap();
        let state = GuiState::new(
            GuiConfigStore::open(temp.path().join("weather-gui.toml")),
            GuiWeatherCache::new(temp.path().join("weather-gui.sqlite")),
        );

        prepare_gui_exit(&state, Duration::ZERO).await;

        assert!(state.shutdown_started.load(Ordering::Acquire));
    }

    #[test]
    fn missing_daemon_command_has_an_actionable_chinese_message() {
        let message = display_daemon_error(anyhow::Error::new(DaemonExecutableNotFound::new(
            "weather-daemon",
        )));

        assert_eq!(
            message,
            "未找到命令 `weather-daemon`。请先运行 `cargo build -p weather-daemon`，或设置 WEATHER_DAEMON_EXE 指向可执行文件。"
        );
    }

    fn resource_response(
        data: &[u8],
        total_size: u64,
        next_offset: u64,
        complete: bool,
    ) -> GetResourceResponse {
        GetResourceResponse {
            resource_id: "resource-id".to_string(),
            content_type: "image/png".to_string(),
            data: data.to_vec(),
            total_size,
            next_offset,
            complete,
            cache_hit: false,
            transfer_state: ResourceTransferState::Ready as i32,
            retry_after_ms: 0,
        }
    }

    #[test]
    fn resource_assembler_combines_contiguous_chunks() {
        let mut assembler = ResourceAssembler::new("resource-id".to_string());
        assert_eq!(
            assembler
                .push(resource_response(b"abc", 6, 3, false))
                .unwrap(),
            ResourceProgress::Incomplete
        );
        assert_eq!(
            assembler
                .push(resource_response(b"def", 6, 6, true))
                .unwrap(),
            ResourceProgress::Complete
        );
        assert_eq!(assembler.finish().unwrap(), b"abcdef");
    }

    #[test]
    fn resource_assembler_waits_for_pending_responses_without_advancing() {
        let mut assembler = ResourceAssembler::new("resource-id".to_string());
        let mut pending = resource_response(b"", 0, 0, false);
        pending.content_type.clear();
        pending.transfer_state = ResourceTransferState::Pending as i32;
        pending.retry_after_ms = 25;

        assert_eq!(
            assembler.push(pending).unwrap(),
            ResourceProgress::Pending(Duration::from_millis(25))
        );
        assert_eq!(assembler.offset(), 0);
    }

    #[test]
    fn resource_assembler_rejects_unspecified_transfer_state() {
        let mut assembler = ResourceAssembler::new("resource-id".to_string());
        let mut response = resource_response(b"abc", 3, 3, true);
        response.transfer_state = ResourceTransferState::Unspecified as i32;

        assert!(assembler.push(response).is_err());
    }

    #[test]
    fn resource_assembler_rejects_nonadvancing_and_oversized_responses() {
        let mut stalled = ResourceAssembler::new("resource-id".to_string());
        assert!(stalled.push(resource_response(b"", 1, 0, false)).is_err());

        let mut oversized = ResourceAssembler::new("resource-id".to_string());
        assert!(
            oversized
                .push(resource_response(b"", MAX_RESOURCE_BYTES + 1, 0, false,))
                .is_err()
        );
    }
}
