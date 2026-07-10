use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use prost::Message as _;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
    },
};
use tokio::sync::{broadcast, mpsc};
use weather_schema::*;

use crate::{
    cli::Cli,
    client::{EngineClient, EngineEvent},
    terminal::TerminalGuard,
    util::{degrees, hectopascal, meter_per_second, mm, percent, text, wind_summary},
};

const EVENT_TICK: Duration = Duration::from_millis(200);
const WEATHER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const SEARCH_PAGE_SIZE: u32 = 12;
const MAX_LOG_LINES: usize = 64;

pub(crate) async fn run_interactive(client: &EngineClient, _cli: &Cli) -> Result<()> {
    let mut terminal = TerminalGuard::new()?;
    let mut app = TuiApp::load(client).await?;
    app.refresh_weather_or_log(client, false).await;
    let mut events = client.subscribe_events();

    let (search_append_tx, mut search_append_rx) = mpsc::channel::<SearchAppend>(32);

    loop {
        app.drain_events(&mut events);
        while let Ok(append) = search_append_rx.try_recv() {
            app.apply_search_append(append);
        }
        app.refresh_weather_if_due(client).await.ok();
        terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(EVENT_TICK)? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // Ctrl+C 等同 q:触发 TUI 退出(foreground engine 由 main.rs 发 shutdown RPC)。
            if key.code == KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                break;
            }
            if app.handle_key(client, key.code, &search_append_tx).await? {
                break;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Manage,
    ManageMove,
    ManageAddSearch,
    ManageBrowseSearch,
    About,
}

/// Normal 模式下 Tab 切换的主界面面板焦点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelFocus {
    Stations,
    Current,
    Forecast,
    Alert,
}

impl PanelFocus {
    const ORDER: [Self; 4] = [Self::Stations, Self::Current, Self::Forecast, Self::Alert];

    fn next(self, app: &TuiApp) -> Self {
        let start = Self::ORDER
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or_default();
        for step in 1..=Self::ORDER.len() {
            let candidate = Self::ORDER[(start + step) % Self::ORDER.len()];
            if !candidate.hidden(app) {
                return candidate;
            }
        }
        self
    }

    fn hidden(self, app: &TuiApp) -> bool {
        match self {
            Self::Alert => app
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.real.as_ref())
                .and_then(|real| real.alert.as_ref())
                .is_none(),
            _ => false,
        }
    }
}

/// 后台分页搜索追加任务回传给主循环的消息。
struct SearchAppend {
    generation: u64,
    stations: Vec<StationRef>,
    has_more: bool,
    next_offset: u32,
    finished: bool,
}

struct TuiApp {
    config: weather_schema::AppConfig,
    stations: Vec<ConfiguredStation>,
    selected_station: usize,
    snapshot: Option<WeatherSnapshot>,
    mode: InputMode,
    focus: PanelFocus,
    forecast_scroll: usize,
    alert_scroll: usize,
    logs: VecDeque<String>,
    last_weather_refresh: Option<Instant>,
    manage_selected: usize,
    manage_move_from: Option<usize>,
    hidden_stations: HashMap<String, bool>,
    preview_snapshot: Option<WeatherSnapshot>,
    search_query: String,
    search_results: Vec<StationRef>,
    selected_result: usize,
    search_has_more: bool,
    search_next_offset: u32,
    search_loading: bool,
    search_generation: u64,
    about_status: Option<EngineStatus>,
}

impl TuiApp {
    #[cfg(test)]
    fn empty_for_test() -> Self {
        Self {
            config: weather_schema::AppConfig::default(),
            stations: Vec::new(),
            selected_station: 0,
            snapshot: None,
            mode: InputMode::Normal,
            focus: PanelFocus::Stations,
            forecast_scroll: 0,
            alert_scroll: 0,
            logs: VecDeque::from(["ready".to_string()]),
            last_weather_refresh: None,
            manage_selected: 0,
            manage_move_from: None,
            hidden_stations: HashMap::new(),
            preview_snapshot: None,
            search_query: String::new(),
            search_results: Vec::new(),
            selected_result: 0,
            search_has_more: false,
            search_next_offset: 0,
            search_loading: false,
            search_generation: 0,
            about_status: None,
        }
    }

    async fn load(client: &EngineClient) -> Result<Self> {
        let config = client.get_config(false).await?.config.unwrap_or_default();
        let stations = config
            .stations
            .iter()
            .map(|s| ConfiguredStation {
                name: s.name.clone(),
                enabled: s.enabled,
            })
            .collect::<Vec<_>>();
        let about_status = client.status().await.ok();
        Ok(Self {
            config,
            stations,
            selected_station: 0,
            snapshot: None,
            mode: InputMode::Normal,
            focus: PanelFocus::Stations,
            forecast_scroll: 0,
            alert_scroll: 0,
            logs: VecDeque::from(["ready".to_string()]),
            last_weather_refresh: None,
            manage_selected: 0,
            manage_move_from: None,
            hidden_stations: HashMap::new(),
            preview_snapshot: None,
            search_query: String::new(),
            search_results: Vec::new(),
            selected_result: 0,
            search_has_more: false,
            search_next_offset: 0,
            search_loading: false,
            search_generation: 0,
            about_status,
        })
    }

    async fn handle_key(
        &mut self,
        client: &EngineClient,
        code: KeyCode,
        search_append_tx: &mpsc::Sender<SearchAppend>,
    ) -> Result<bool> {
        match self.mode {
            InputMode::Normal => self.handle_normal_key(client, code).await,
            InputMode::Manage => self.handle_manage_key(client, code).await,
            InputMode::ManageMove => self.handle_move_key(client, code).await,
            InputMode::ManageAddSearch | InputMode::ManageBrowseSearch => {
                self.handle_search_key(client, code, search_append_tx).await
            }
            InputMode::About => self.handle_about_key(client, code).await,
        }
    }

    async fn handle_normal_key(&mut self, client: &EngineClient, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('r') => self.refresh_weather_or_log(client, true).await,
            KeyCode::Tab => {
                self.focus = self.focus.next(self);
                self.push_log(format!("focus: {:?}", self.focus));
            }
            KeyCode::Esc => {
                self.focus = PanelFocus::Stations;
                self.forecast_scroll = 0;
                self.alert_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                PanelFocus::Stations => {
                    if self.select_station(1) {
                        self.refresh_weather_or_log(client, false).await;
                    }
                }
                PanelFocus::Forecast => {
                    self.scroll_forecast(1);
                }
                PanelFocus::Alert => {
                    self.alert_scroll = self.alert_scroll.saturating_add(1);
                }
                _ => {}
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                PanelFocus::Stations => {
                    if self.select_station(-1) {
                        self.refresh_weather_or_log(client, false).await;
                    }
                }
                PanelFocus::Forecast => {
                    self.scroll_forecast(-1);
                }
                PanelFocus::Alert => {
                    self.alert_scroll = self.alert_scroll.saturating_sub(1);
                }
                _ => {}
            },
            KeyCode::PageDown if self.focus == PanelFocus::Forecast => {
                self.scroll_forecast(5);
            }
            KeyCode::PageUp if self.focus == PanelFocus::Forecast => {
                self.scroll_forecast(-5);
            }
            KeyCode::Char('m') => {
                self.mode = InputMode::Manage;
                self.manage_selected = self
                    .selected_station
                    .min(self.config.stations.len().saturating_sub(1));
                self.push_log("manage mode");
            }
            KeyCode::Char('?') => {
                if let Err(err) = self.refresh_about(client).await {
                    self.push_log(format!("about refresh failed: {err}"));
                }
                self.mode = InputMode::About;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_about_key(&mut self, client: &EngineClient, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Char('r') => {
                if let Err(err) = self.refresh_about(client).await {
                    self.push_log(format!("about refresh failed: {err}"));
                }
            }
            _ => {
                self.mode = InputMode::Normal;
            }
        }
        Ok(false)
    }

    async fn refresh_about(&mut self, client: &EngineClient) -> Result<()> {
        self.about_status = Some(client.status().await?);
        Ok(())
    }

    async fn handle_manage_key(&mut self, client: &EngineClient, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.push_log("manage closed");
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.manage_selected =
                    moved_index(self.manage_selected, self.config.stations.len(), 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.manage_selected =
                    moved_index(self.manage_selected, self.config.stations.len(), -1);
            }
            KeyCode::Char(' ') => self.toggle_selected(client).await?,
            KeyCode::Char('d') => self.delete_selected(client).await?,
            KeyCode::Char('a') => self.enter_search(InputMode::ManageAddSearch),
            KeyCode::Char('s') => self.enter_search(InputMode::ManageBrowseSearch),
            KeyCode::Char('M') => {
                if !self.config.stations.is_empty() {
                    self.manage_move_from = Some(self.manage_selected);
                    self.mode = InputMode::ManageMove;
                    self.push_log("move mode: j/k to move, Enter to confirm");
                }
            }
            KeyCode::Char('h') => {
                let Some(station) = self.config.stations.get(self.manage_selected) else {
                    return Ok(false);
                };
                let name = station.name.clone();
                let hidden = self.hidden_stations.get(&name).copied().unwrap_or(false);
                let new_hidden = !hidden;
                if new_hidden {
                    self.hidden_stations.insert(name.clone(), true);
                } else {
                    self.hidden_stations.remove(&name);
                }
                self.push_log(if new_hidden {
                    format!("hid {name}")
                } else {
                    format!("showed {name}")
                });
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_move_key(&mut self, client: &EngineClient, code: KeyCode) -> Result<bool> {
        let Some(from) = self.manage_move_from else {
            self.mode = InputMode::Manage;
            return Ok(false);
        };
        match code {
            KeyCode::Esc => {
                self.manage_move_from = None;
                self.mode = InputMode::Manage;
                self.push_log("move cancelled");
            }
            KeyCode::Enter => {
                let _ = from;
                self.mode = InputMode::Manage;
                self.persist_stations_order(client).await?;
                self.push_log("move confirmed");
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.swap_move_target(from, 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.swap_move_target(from, -1);
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_search_key(
        &mut self,
        client: &EngineClient,
        code: KeyCode,
        search_append_tx: &mpsc::Sender<SearchAppend>,
    ) -> Result<bool> {
        match code {
            KeyCode::Esc => {
                self.exit_search_to_manage();
            }
            KeyCode::Enter => {
                self.search_first_page(client).await?;
                self.spawn_remaining_pages(client.clone(), search_append_tx.clone());
            }
            KeyCode::Backspace => {
                self.search_query.pop();
            }
            KeyCode::Char('a') if !self.search_results.is_empty() => {
                if self.mode == InputMode::ManageAddSearch {
                    self.add_selected_result(client).await?;
                } else {
                    self.preview_selected(client).await?;
                }
            }
            KeyCode::Down => self.select_result(1),
            KeyCode::Up => self.select_result(-1),
            KeyCode::Char(ch) => self.search_query.push(ch),
            _ => {}
        }
        Ok(false)
    }

    fn enter_search(&mut self, mode: InputMode) {
        self.mode = mode;
        self.search_query.clear();
        self.search_results.clear();
        self.selected_result = 0;
        self.search_has_more = false;
        self.search_next_offset = 0;
        self.search_loading = false;
        self.search_generation = self.search_generation.wrapping_add(1);
        self.preview_snapshot = None;
        self.push_log(match mode {
            InputMode::ManageAddSearch => "add search: type query, Enter to search, a to add",
            InputMode::ManageBrowseSearch => {
                "browse search: type query, Enter to search, a to preview"
            }
            _ => "",
        });
    }

    fn exit_search_to_manage(&mut self) {
        self.mode = InputMode::Manage;
        self.preview_snapshot = None;
        self.push_log("search closed");
    }

    fn select_station(&mut self, delta: isize) -> bool {
        let len = self.stations.len();
        if len == 0 {
            return false;
        }
        let original = self.selected_station.min(len - 1);
        let mut next = original;
        for _ in 0..len {
            next = moved_index(next, len, delta);
            if next == original {
                break;
            }
            let name = &self.stations[next].name;
            if !self.hidden_stations.get(name).copied().unwrap_or(false) {
                break;
            }
        }
        if next == original {
            return false;
        }
        self.selected_station = next;
        self.forecast_scroll = 0;
        self.alert_scroll = 0;
        true
    }

    fn scroll_forecast(&mut self, delta: isize) {
        let len = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.predict.as_ref())
            .map(|predict| predict.days.len())
            .unwrap_or_default();
        if len == 0 {
            self.forecast_scroll = 0;
            return;
        }
        self.forecast_scroll = self
            .forecast_scroll
            .saturating_add_signed(delta)
            .min(len - 1);
    }

    fn select_result(&mut self, delta: isize) {
        self.selected_result = moved_index(self.selected_result, self.search_results.len(), delta);
    }

    async fn refresh_weather(&mut self, client: &EngineClient, refresh: bool) -> Result<()> {
        let Some(station) = self.stations.get(self.selected_station) else {
            self.push_log("no configured stations");
            return Ok(());
        };
        let name = station.name.clone();
        let resp = client
            .request::<ResolveStationUuidRequest, ResolveStationUuidResponse>(
                RpcKind::ResolveStationUuid,
                ResolveStationUuidRequest { name: name.clone() },
            )
            .await?;
        let unified_uuid = resp.unified_uuid;
        let snapshot = client
            .request::<GetWeatherRequest, WeatherSnapshot>(
                RpcKind::GetWeather,
                GetWeatherRequest {
                    unified_uuid,
                    refresh,
                    include_debug: false,
                },
            )
            .await?;
        self.snapshot = Some(snapshot);
        if self.focus.hidden(self) {
            self.focus = PanelFocus::Stations;
        }
        self.last_weather_refresh = Some(Instant::now());
        self.push_log(format!("loaded {name}"));
        Ok(())
    }

    async fn refresh_weather_or_log(&mut self, client: &EngineClient, refresh: bool) {
        if let Err(err) = self.refresh_weather(client, refresh).await {
            self.last_weather_refresh = Some(Instant::now());
            self.record_weather_refresh_error(err);
        }
    }

    fn record_weather_refresh_error(&mut self, err: impl std::fmt::Display) {
        self.push_log(format!("refresh failed: {err}"));
    }

    async fn toggle_selected(&mut self, client: &EngineClient) -> Result<()> {
        let Some(station) = self.config.stations.get_mut(self.manage_selected) else {
            return Ok(());
        };
        station.enabled = !station.enabled;
        let new_enabled = station.enabled;
        let name = station.name.clone();
        self.apply_update_config(client).await?;
        self.push_log(format!(
            "{} {name}",
            if new_enabled { "enabled" } else { "disabled" }
        ));
        Ok(())
    }

    async fn delete_selected(&mut self, client: &EngineClient) -> Result<()> {
        if self.config.stations.is_empty() {
            return Ok(());
        }
        let name = self
            .config
            .stations
            .get(self.manage_selected)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.config.stations.remove(self.manage_selected);
        if self.manage_selected >= self.config.stations.len() {
            self.manage_selected = self.config.stations.len().saturating_sub(1);
        }
        self.apply_update_config(client).await?;
        self.push_log(format!("removed {name}"));
        Ok(())
    }

    async fn add_selected_result(&mut self, client: &EngineClient) -> Result<()> {
        let Some(station) = self.search_results.get(self.selected_result) else {
            self.push_log("no selected search result");
            return Ok(());
        };
        let name = station.name.clone();
        let already = self.config.stations.iter().any(|s| s.name == name);
        if already {
            if let Some(existing) = self.config.stations.iter_mut().find(|s| s.name == name) {
                existing.enabled = true;
            }
            self.apply_update_config(client).await?;
            self.push_log(format!("enabled existing {name}"));
        } else {
            self.config.stations.push(StationConfig {
                name: name.clone(),
                enabled: true,
            });
            self.apply_update_config(client).await?;
            self.manage_selected = self.config.stations.len() - 1;
            self.push_log(format!("added {name}"));
        }
        self.exit_search_to_manage();
        Ok(())
    }

    async fn preview_selected(&mut self, client: &EngineClient) -> Result<()> {
        let Some(station) = self.search_results.get(self.selected_result) else {
            return Ok(());
        };
        let unified_uuid = if station.unified_uuid.is_empty() {
            client
                .resolve_station_uuid(&station.name)
                .await?
                .unified_uuid
        } else {
            station.unified_uuid.clone()
        };
        match client
            .request::<GetWeatherRequest, WeatherSnapshot>(
                RpcKind::GetWeather,
                GetWeatherRequest {
                    unified_uuid,
                    refresh: false,
                    include_debug: false,
                },
            )
            .await
        {
            Ok(snapshot) => {
                self.preview_snapshot = Some(snapshot);
                self.push_log(format!("preview {}", station.name));
            }
            Err(err) => {
                self.push_log(format!("preview failed: {err}"));
            }
        }
        Ok(())
    }

    fn swap_move_target(&mut self, from: usize, delta: isize) {
        let len = self.config.stations.len();
        if len < 2 {
            return;
        }
        let to = moved_index(self.manage_selected, len, delta);
        if to == self.manage_selected {
            return;
        }
        self.config.stations.swap(self.manage_selected, to);
        self.manage_selected = to;
        let _ = from;
    }

    async fn persist_stations_order(&mut self, client: &EngineClient) -> Result<()> {
        self.manage_move_from = None;
        self.apply_update_config(client).await?;
        Ok(())
    }

    /// 把本地 `self.config` 下发给 engine,用返回的最终值回填本地状态。
    async fn apply_update_config(&mut self, client: &EngineClient) -> Result<()> {
        let resp = client.update_config(self.config.clone()).await?;
        self.config = resp.config.unwrap_or_default();
        self.stations = self
            .config
            .stations
            .iter()
            .map(|s| ConfiguredStation {
                name: s.name.clone(),
                enabled: s.enabled,
            })
            .collect();
        if self.selected_station >= self.stations.len() {
            self.selected_station = self.stations.len().saturating_sub(1);
        }
        if self.manage_selected >= self.config.stations.len() {
            self.manage_selected = self.config.stations.len().saturating_sub(1);
        }
        Ok(())
    }

    /// 第 1 页已同步加载。若有更多页，spawn 后台任务依次请求并通过 channel 回传。
    fn spawn_remaining_pages(
        &self,
        client: EngineClient,
        search_append_tx: mpsc::Sender<SearchAppend>,
    ) {
        if !self.search_has_more {
            return;
        }
        let query = self.search_query.clone();
        let mut next_offset = self.search_next_offset;
        let generation = self.search_generation;
        tokio::spawn(async move {
            loop {
                let result = client
                    .request::<FuzzyMatchStationsRequest, FuzzyMatchStationsResponse>(
                        RpcKind::FuzzyMatchStations,
                        FuzzyMatchStationsRequest {
                            query: query.clone(),
                            province: None,
                            page_offset: next_offset,
                            page_size: SEARCH_PAGE_SIZE,
                        },
                    )
                    .await;
                let Ok(resp) = result else {
                    let _ = search_append_tx
                        .send(SearchAppend {
                            generation,
                            stations: Vec::new(),
                            has_more: false,
                            next_offset,
                            finished: true,
                        })
                        .await;
                    return;
                };
                let has_more = resp.has_more;
                next_offset = resp.next_offset;
                let finished = !has_more;
                if search_append_tx
                    .send(SearchAppend {
                        generation,
                        stations: resp.stations,
                        has_more,
                        next_offset,
                        finished,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                if finished {
                    return;
                }
            }
        });
    }

    async fn search_first_page(&mut self, client: &EngineClient) -> Result<()> {
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_results.clear();
        self.selected_result = 0;
        self.search_next_offset = 0;
        self.search_has_more = false;
        self.search_loading = true;
        self.search_next_page(client).await?;
        Ok(())
    }

    async fn search_next_page(&mut self, client: &EngineClient) -> Result<()> {
        let results = client
            .request::<FuzzyMatchStationsRequest, FuzzyMatchStationsResponse>(
                RpcKind::FuzzyMatchStations,
                FuzzyMatchStationsRequest {
                    query: self.search_query.clone(),
                    province: None,
                    page_offset: self.search_next_offset,
                    page_size: SEARCH_PAGE_SIZE,
                },
            )
            .await?;
        append_unique_stations(&mut self.search_results, results.stations);
        self.search_has_more = results.has_more;
        self.search_next_offset = results.next_offset;
        if self.selected_result >= self.search_results.len() && !self.search_results.is_empty() {
            self.selected_result = self.search_results.len() - 1;
        }
        Ok(())
    }

    fn apply_search_append(&mut self, append: SearchAppend) {
        if append.generation != self.search_generation {
            return;
        }
        append_unique_stations(&mut self.search_results, append.stations);
        self.search_has_more = append.has_more;
        self.search_next_offset = append.next_offset;
        if self.selected_result >= self.search_results.len() && !self.search_results.is_empty() {
            self.selected_result = self.search_results.len() - 1;
        }
        if append.finished {
            self.search_loading = false;
        }
    }

    async fn refresh_weather_if_due(&mut self, client: &EngineClient) -> Result<()> {
        let due = self
            .last_weather_refresh
            .is_none_or(|last| last.elapsed() >= WEATHER_REFRESH_INTERVAL);
        if due {
            self.refresh_weather_or_log(client, false).await;
        }
        Ok(())
    }

    fn push_log(&mut self, message: impl Into<String>) {
        self.logs.push_back(message.into());
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }
    }

    fn log_station_label(&self, unified_uuid: &str) -> String {
        if unified_uuid.is_empty() {
            return "global".to_string();
        }
        if let Some(station) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.station.as_ref())
            .filter(|station| station.unified_uuid == unified_uuid)
        {
            let name = if station.name.is_empty() {
                format!("{}-{}", station.province, station.city)
            } else {
                station.name.clone()
            };
            return format!("{name}({unified_uuid})");
        }
        if let Some(station) = self
            .search_results
            .iter()
            .find(|station| station.unified_uuid == unified_uuid)
        {
            return format!("{}({unified_uuid})", station.name);
        }
        if let Some(station) = self
            .config
            .stations
            .iter()
            .find(|station| weather_schema::unified_station_uuid(&station.name) == unified_uuid)
        {
            return format!("{}({unified_uuid})", station.name);
        }
        unified_uuid.to_string()
    }

    /// 消费订阅通道中的全部事件，按 topic 更新本地状态。
    fn drain_events(&mut self, events: &mut broadcast::Receiver<EngineEvent>) {
        while let Ok(event) = events.try_recv() {
            let kind = event.envelope.kind;
            let payload = event.envelope.payload;
            match kind {
                kind if kind == EventKind::WeatherSnapshot as i32 => {
                    if let Ok(decoded) = WeatherSnapshotEvent::decode(payload.as_slice()) {
                        let incoming_uuid = decoded
                            .snapshot
                            .as_ref()
                            .and_then(|s| s.station.as_ref())
                            .map(|s| s.unified_uuid.clone());
                        let current_uuid = self
                            .snapshot
                            .as_ref()
                            .and_then(|s| s.station.as_ref())
                            .map(|s| s.unified_uuid.clone());
                        if incoming_uuid.as_deref().is_some_and(|u| !u.is_empty())
                            && incoming_uuid == current_uuid
                        {
                            self.snapshot = decoded.snapshot;
                            self.last_weather_refresh = Some(Instant::now());
                        }
                    }
                }
                kind if kind == EventKind::EngineStatus as i32 => {
                    if let Ok(status) = EngineStatus::decode(payload.as_slice()) {
                        self.push_log(format!("engine {} ready={}", status.mode, status.ready));
                    }
                }
                kind if kind == EventKind::FetchLog as i32 => {
                    if let Ok(log) = FetchLogEvent::decode(payload.as_slice()) {
                        let station = self.log_station_label(&log.unified_uuid.unwrap_or_default());
                        let state = if log.ok { "ok" } else { "fail" };
                        self.push_log(format!(
                            "{} {station} {state}: {}",
                            event.topic, log.endpoint
                        ));
                    }
                }
                kind if kind == EventKind::Refresh as i32 => {
                    if let Ok(refresh) = RefreshEvent::decode(payload.as_slice()) {
                        let station =
                            self.log_station_label(&refresh.unified_uuid.unwrap_or_default());
                        if refresh.started {
                            self.push_log(format!("{} {station} started", event.topic));
                        } else if refresh.completed {
                            self.push_log(format!("{} {station} completed", event.topic));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn moved_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

fn append_unique_stations(target: &mut Vec<StationRef>, source: Vec<StationRef>) {
    for station in source {
        let duplicate = target.iter().any(|item| {
            if !item.unified_uuid.is_empty() && !station.unified_uuid.is_empty() {
                item.unified_uuid == station.unified_uuid
            } else {
                item.name == station.name
                    && item.province == station.province
                    && item.city == station.city
            }
        });
        if !duplicate {
            target.push(station);
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let chunks = main_layout(frame.area());

    draw_header(frame, chunks[0], app);
    draw_body(frame, chunks[1], app);
    draw_footer(frame, chunks[2], app);

    match app.mode {
        InputMode::Manage | InputMode::ManageMove => {
            draw_manage(frame, centered_rect(80, 70, frame.area()), app);
        }
        InputMode::ManageAddSearch | InputMode::ManageBrowseSearch => {
            draw_search(frame, centered_rect(72, 70, frame.area()), app);
        }
        InputMode::About => {
            draw_about(frame, centered_rect(60, 70, frame.area()), app);
        }
        InputMode::Normal => {}
    }
}

fn main_layout(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(4),
        ])
        .split(area)
        .to_vec()
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = if let Some(station) = app.snapshot.as_ref().and_then(|s| s.station.as_ref()) {
        if station.name.is_empty() {
            format!("{} {}", station.province, station.city)
        } else {
            station.name.clone()
        }
    } else {
        "Weather".to_string()
    };
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL).title("TUI")),
        area,
    );
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &mut TuiApp) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(50)])
        .split(area);
    draw_stations(frame, columns[0], app);

    let has_alert = !PanelFocus::Alert.hidden(app);
    let right = right_panel_layout(columns[1], has_alert);
    draw_current(frame, right[0], app.snapshot.as_ref(), app.focus);
    draw_forecast(
        frame,
        right[1],
        app.snapshot.as_ref(),
        app.focus,
        app.forecast_scroll,
    );
    if has_alert {
        draw_alert(
            frame,
            right[2],
            app.snapshot.as_ref(),
            app.focus,
            app.alert_scroll,
        );
    }
}

fn right_panel_layout(area: Rect, has_alert: bool) -> Vec<Rect> {
    let constraints = if has_alert {
        vec![
            Constraint::Length(8),
            Constraint::Min(2),
            Constraint::Min(3),
        ]
    } else {
        vec![Constraint::Length(8), Constraint::Min(2)]
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

fn focused_block(title: &str, focus: PanelFocus, target: PanelFocus) -> Block<'_> {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    if focus == target {
        block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        block
    }
}

fn draw_stations(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    // 把可见站点映射回原 config 索引，保留 selected_station 在原列表中的语义。
    let visible: Vec<(usize, &ConfiguredStation)> = app
        .stations
        .iter()
        .enumerate()
        .filter(|(_, s)| !app.hidden_stations.get(&s.name).copied().unwrap_or(false))
        .collect();
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new("All stations hidden (m to manage)").block(focused_block(
                "Stations",
                app.focus,
                PanelFocus::Stations,
            )),
            area,
        );
        return;
    }
    let items = visible
        .iter()
        .map(|(index, station)| {
            let marker = if station.enabled { "[x]" } else { "[ ]" };
            ListItem::new(format!("{:>2}. {marker} {}", index + 1, station.name))
        })
        .collect::<Vec<_>>();
    // selected_station 是原 config 索引；转换为可见列表中的位置。
    let visible_pos = visible
        .iter()
        .position(|(index, _)| *index == app.selected_station)
        .unwrap_or(0);
    let mut state = ListState::default();
    state.select(Some(visible_pos));
    frame.render_stateful_widget(
        List::new(items)
            .block(focused_block("Stations", app.focus, PanelFocus::Stations))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
        area,
        &mut state,
    );
}

fn draw_current(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&WeatherSnapshot>,
    focus: PanelFocus,
) {
    let rows = current_rows(snapshot).into_iter().map(|(label, value)| {
        Row::new(vec![
            Cell::from(label).style(Style::default().fg(Color::Yellow)),
            Cell::from(value),
        ])
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(14), Constraint::Min(10)]).block(focused_block(
            "Current",
            focus,
            PanelFocus::Current,
        )),
        area,
    );
}

fn current_rows(snapshot: Option<&WeatherSnapshot>) -> Vec<(&'static str, String)> {
    if let Some(snapshot) = snapshot
        && let Some(real) = snapshot.real.as_ref()
    {
        let today = snapshot
            .predict
            .as_ref()
            .and_then(|predict| predict.days.first());
        let temperature_chart = matching_temperature_chart(snapshot, today);
        let pressure = real
            .air_pressure
            .or_else(|| snapshot.passedchart.iter().find_map(|chart| chart.pressure));
        vec![
            ("Weather", text(real.info.as_deref()).to_string()),
            (
                "Temperature",
                temperature_summary(real.temperature, today, temperature_chart),
            ),
            ("Feels like", degrees(real.feel_temperature)),
            (
                "Humidity",
                format!(
                    "{}  rain {}  pressure {}",
                    percent(real.humidity),
                    mm(real.rain),
                    hectopascal(pressure)
                ),
            ),
            (
                "Comfort",
                format!(
                    "{}  index {}",
                    text(real.comfort_label.as_deref()),
                    text(real.comfort_index.as_deref())
                ),
            ),
            (
                "Wind",
                format!(
                    "{} {}  {}",
                    wind_summary(real.wind_direct.as_deref(), real.wind_power.as_deref()),
                    meter_per_second(real.wind_speed),
                    real.wind_degree
                        .map(|value| format!("{value:.0}°"))
                        .unwrap_or_else(|| "-".to_string())
                ),
            ),
            ("Sunrise", text(real.sunrise.as_deref()).to_string()),
            ("Sunset", text(real.sunset.as_deref()).to_string()),
            ("Published", text(real.publish_time.as_deref()).to_string()),
        ]
    } else {
        vec![("Status", "No weather snapshot loaded.".to_string())]
    }
}

fn temperature_summary(
    current: Option<f64>,
    today: Option<&ForecastDay>,
    temperature_chart: Option<&TemperatureChart>,
) -> String {
    let mut parts = vec![format!("current {}", degrees(current))];
    let high = today
        .map(|today| forecast_degrees(today.day_temperature.as_deref()))
        .filter(|value| value != "-")
        .or_else(|| {
            temperature_chart.and_then(|chart| chart.max_temperature.map(|v| degrees(Some(v))))
        });
    let low = today
        .map(|today| forecast_degrees(today.night_temperature.as_deref()))
        .filter(|value| value != "-")
        .or_else(|| {
            temperature_chart.and_then(|chart| chart.min_temperature.map(|v| degrees(Some(v))))
        });
    if high.is_some() || low.is_some() {
        parts.push(format!("high {}", high.unwrap_or_else(|| "-".to_string())));
        parts.push(format!("low {}", low.unwrap_or_else(|| "-".to_string())));
    }
    parts.join("  ")
}

fn matching_temperature_chart<'a>(
    snapshot: &'a WeatherSnapshot,
    today: Option<&ForecastDay>,
) -> Option<&'a TemperatureChart> {
    let today_key = today.map(|day| date_key(&day.date));
    if let Some(today_key) = today_key {
        snapshot
            .tempchart
            .iter()
            .find(|chart| chart.date.as_deref().map(date_key).as_deref() == Some(&today_key))
            .or_else(|| snapshot.tempchart.first())
    } else {
        snapshot.tempchart.first()
    }
}

fn date_key(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

fn forecast_degrees(value: Option<&str>) -> String {
    let value = text(value);
    if value == "-" || value.ends_with("℃") {
        value.to_string()
    } else {
        format!("{value}℃")
    }
}

fn draw_forecast(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&WeatherSnapshot>,
    focus: PanelFocus,
    forecast_scroll: usize,
) {
    let visible_rows = area.height.saturating_sub(3) as usize;
    let rows = snapshot
        .and_then(|s| s.predict.as_ref())
        .map(|predict| {
            let start = forecast_scroll.min(predict.days.len().saturating_sub(1));
            predict
                .days
                .iter()
                .skip(start)
                .take(visible_rows.max(1))
                .map(|day| {
                    Row::new(vec![
                        day.date.clone(),
                        text(day.day_info.as_deref()).to_string(),
                        text(day.night_info.as_deref()).to_string(),
                        format!(
                            "{}/{}",
                            text(day.day_temperature.as_deref()),
                            text(day.night_temperature.as_deref())
                        ),
                        tui_forecast_wind_summary(day),
                        mm(day.precipitation),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(vec![
        "Date", "Day", "Night", "Temp", "Wind", "Rain",
    ]))
    .block(focused_block("Forecast", focus, PanelFocus::Forecast));
    frame.render_widget(table, area);
}

fn draw_alert(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&WeatherSnapshot>,
    focus: PanelFocus,
    alert_scroll: usize,
) {
    let lines = alert_text_lines(snapshot)
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((alert_scroll as u16, 0))
            .block(focused_block("Alert", focus, PanelFocus::Alert))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn alert_text_lines(snapshot: Option<&WeatherSnapshot>) -> Vec<String> {
    if let Some(alert) = snapshot
        .and_then(|s| s.real.as_ref())
        .and_then(|real| real.alert.as_ref())
    {
        let mut lines = vec![format!(
            "{} {}",
            text(alert.alert.as_deref()),
            text(alert.signal_level.as_deref())
        )];
        lines.extend(alert_split_lines(alert.issue_content.as_deref()));
        lines.extend(alert_split_lines(alert.prevention.as_deref()));
        lines
    } else {
        vec!["No active alert. Reserved for warning details.".to_string()]
    }
}

fn alert_split_lines(value: Option<&str>) -> Vec<String> {
    text(value)
        .split('；')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn tui_forecast_wind_summary(day: &ForecastDay) -> String {
    let day_wind = wind_summary(
        day.day_wind_direct.as_deref(),
        day.day_wind_power.as_deref(),
    );
    let night_wind = wind_summary(
        day.night_wind_direct.as_deref(),
        day.night_wind_power.as_deref(),
    );
    if day_wind == "-" && night_wind == "-" {
        wind_summary(day.wind_direct.as_deref(), day.wind_power.as_deref())
    } else if night_wind == "-" || day_wind == night_wind {
        day_wind
    } else {
        format!("{day_wind}/{night_wind}")
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(56), Constraint::Min(24)])
        .split(area);
    draw_key_hints(frame, chunks[0], app.mode, app.focus);
    draw_logs(frame, chunks[1], app);
}

fn draw_key_hints(frame: &mut Frame<'_>, area: Rect, mode: InputMode, focus: PanelFocus) {
    let lines = match mode {
        InputMode::Normal => {
            let second = match focus {
                PanelFocus::Stations => "j/k or Up/Down select station   Tab focus",
                PanelFocus::Forecast => "j/k Up/Down scroll forecast   PgUp/PgDn page",
                PanelFocus::Alert => "j/k or Up/Down scroll alert   Tab focus",
                PanelFocus::Current => "Tab focus   r refresh",
            };
            vec![
                Line::from("q quit   r refresh   m manage   ? about"),
                Line::from(second),
            ]
        }
        InputMode::Manage => vec![
            Line::from("Esc back   j/k select   Space toggle"),
            Line::from("d delete   a add   s browse   M move   h hide-toggle"),
        ],
        InputMode::ManageMove => vec![
            Line::from("j/k move row   Enter confirm"),
            Line::from("Esc cancel"),
        ],
        InputMode::ManageAddSearch => vec![
            Line::from("Enter search   Up/Down select   a add"),
            Line::from("Esc back   type to edit query"),
        ],
        InputMode::ManageBrowseSearch => vec![
            Line::from("Enter search   Up/Down select   a preview"),
            Line::from("Esc back   type to edit query"),
        ],
        InputMode::About => vec![
            Line::from("r refresh status"),
            Line::from("any other key close"),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Keys"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_logs(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let visible_lines = area.height.saturating_sub(2) as usize;
    let lines = app
        .logs
        .iter()
        .rev()
        .take(visible_lines)
        .map(|message| Line::from(message.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Log"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_manage(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Clear, area);
    let title = if app.manage_move_from.is_some() {
        "Manage Stations [moving]"
    } else {
        "Manage Stations"
    };
    let items = app
        .config
        .stations
        .iter()
        .enumerate()
        .map(|(index, station)| {
            let marker = if station.enabled { "[x]" } else { "[ ]" };
            let hidden = app
                .hidden_stations
                .get(&station.name)
                .copied()
                .unwrap_or(false);
            let hide_tag = if hidden { " (hidden)" } else { "" };
            let moving = app.manage_move_from.is_some_and(|f| f == index);
            let prefix = if moving { ">>" } else { "  " };
            ListItem::new(format!(
                "{prefix}{:>2}. {marker} {}{hide_tag}",
                index + 1,
                station.name
            ))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.manage_selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow)),
        area,
        &mut state,
    );
}

fn draw_search(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Clear, area);
    let mode_label = match app.mode {
        InputMode::ManageAddSearch => "[Add]",
        InputMode::ManageBrowseSearch => "[Browse]",
        _ => "",
    };
    let loading_label = if app.search_loading {
        " loading…"
    } else {
        ""
    };

    let show_preview = app.mode == InputMode::ManageBrowseSearch && app.preview_snapshot.is_some();

    let constraints = if show_preview {
        vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(8),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    frame.render_widget(
        Paragraph::new(app.search_query.clone()).block(
            Block::default().borders(Borders::ALL).title(format!(
                "Search {mode_label} (page {}{loading_label})",
                app.search_next_offset / SEARCH_PAGE_SIZE
            )),
        ),
        chunks[0],
    );

    let items = app
        .search_results
        .iter()
        .map(|station| ListItem::new(station.name.clone()))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.selected_result));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Results"))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Green)),
        chunks[1],
        &mut state,
    );

    if show_preview {
        if let Some(snap) = app.preview_snapshot.as_ref() {
            draw_current(frame, chunks[2], Some(snap), PanelFocus::Alert);
        }
    } else {
        let hint = match app.mode {
            InputMode::ManageAddSearch => "Enter search | Up/Down select | a add | Esc back",
            InputMode::ManageBrowseSearch => "Enter search | Up/Down select | a preview | Esc back",
            _ => "",
        };
        frame.render_widget(Paragraph::new(hint), chunks[2]);
    }
}

fn draw_about(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("About")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = about_sections(app.about_status.as_ref());
    let mut constraints = Vec::new();
    for section in &sections {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(section.rows.len() as u16));
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let mut chunk_index = 0;
    for section in sections {
        frame.render_widget(
            Paragraph::new(section.title).style(Style::default().fg(section.color)),
            chunks[chunk_index],
        );
        chunk_index += 1;
        let rows = section.rows.into_iter().map(|row| {
            Row::new(vec![
                Cell::from(row.label).style(Style::default().fg(section.color)),
                Cell::from(row.value),
            ])
        });
        frame.render_widget(
            Table::new(rows, [Constraint::Length(16), Constraint::Min(10)]),
            chunks[chunk_index],
        );
        chunk_index += 2;
    }
    frame.render_widget(
        Paragraph::new("r refresh | any other key close"),
        chunks[chunk_index],
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AboutRow {
    label: &'static str,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AboutSection {
    title: &'static str,
    color: Color,
    rows: Vec<AboutRow>,
}

fn about_sections(status: Option<&EngineStatus>) -> Vec<AboutSection> {
    let mut sections = vec![AboutSection {
        title: "TUI",
        color: Color::Yellow,
        rows: vec![
            AboutRow {
                label: "Version",
                value: env!("CARGO_PKG_VERSION").to_string(),
            },
            AboutRow {
                label: "Build",
                value: env!("BUILD_VERSION").to_string(),
            },
        ],
    }];

    if let Some(status) = status {
        sections.push(AboutSection {
            title: "Engine",
            color: Color::Cyan,
            rows: vec![
                AboutRow {
                    label: "Version",
                    value: status.engine_version.clone(),
                },
                AboutRow {
                    label: "Build",
                    value: status.build_version.clone(),
                },
                AboutRow {
                    label: "Schema",
                    value: status.schema_version.clone(),
                },
            ],
        });
        sections.push(AboutSection {
            title: "Runtime",
            color: Color::Green,
            rows: vec![
                AboutRow {
                    label: "Mode",
                    value: status.mode.clone(),
                },
                AboutRow {
                    label: "Ready",
                    value: status.ready.to_string(),
                },
                AboutRow {
                    label: "RPC endpoint",
                    value: status.rpc_endpoint.clone(),
                },
                AboutRow {
                    label: "PUB endpoint",
                    value: status.pub_endpoint.clone(),
                },
                AboutRow {
                    label: "Config",
                    value: status.config_path.clone(),
                },
                AboutRow {
                    label: "Last cfg error",
                    value: status
                        .last_config_error
                        .clone()
                        .unwrap_or_else(|| "none".to_string()),
                },
            ],
        });
    } else {
        sections.push(AboutSection {
            title: "Engine",
            color: Color::Cyan,
            rows: vec![AboutRow {
                label: "Status",
                value: "unavailable".to_string(),
            }],
        });
    }

    sections
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> EngineStatus {
        EngineStatus {
            ready: true,
            mode: "foreground".to_string(),
            rpc_endpoint: "tcp://127.0.0.1:44445".to_string(),
            pub_endpoint: "tcp://127.0.0.1:44444".to_string(),
            config_path: "/tmp/weather/config/weather.toml".to_string(),
            last_config_error: None,
            message: None,
            engine_version: "0.1.0".to_string(),
            schema_version: "v1".to_string(),
            build_version: "dev".to_string(),
            instance_id: "test-instance".to_string(),
        }
    }

    #[test]
    fn about_sections_group_table_rows() {
        let sections = about_sections(Some(&status()));

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].title, "TUI");
        assert_eq!(sections[0].rows[0].label, "Version");
        assert_eq!(sections[1].title, "Engine");
        assert_eq!(sections[1].rows[2].label, "Schema");
        assert_eq!(sections[2].title, "Runtime");
        assert_eq!(sections[2].rows[4].label, "Config");
        assert_eq!(sections[2].rows[5].value, "none");
    }

    #[test]
    fn about_sections_show_unavailable_status() {
        let sections = about_sections(None);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].title, "Engine");
        assert_eq!(sections[1].rows[0].label, "Status");
        assert_eq!(sections[1].rows[0].value, "unavailable");
    }

    #[test]
    fn main_layout_reserves_two_content_lines_for_footer() {
        let chunks = main_layout(Rect::new(0, 0, 100, 24));

        assert_eq!(chunks[0].height, 3);
        assert_eq!(chunks[2].height, 4);
    }

    #[test]
    fn body_layout_compresses_forecast_to_two_lines_first() {
        let chunks = right_panel_layout(Rect::new(0, 0, 80, 13), true);

        assert_eq!(chunks[0].height, 8);
        assert_eq!(chunks[1].height, 2);
        assert_eq!(chunks[2].height, 3);
    }

    #[test]
    fn current_rows_are_label_value_pairs() {
        let rows = current_rows(Some(&WeatherSnapshot {
            station: None,
            real: Some(ObservedWeather {
                info: Some("Sunny".to_string()),
                temperature: Some(12.0),
                feel_temperature: Some(10.0),
                humidity: Some(55.0),
                rain: Some(0.0),
                wind_direct: Some("North".to_string()),
                wind_power: Some("Light".to_string()),
                wind_speed: Some(3.0),
                sunrise: Some("06:00".to_string()),
                sunset: Some("18:00".to_string()),
                publish_time: Some("12:00".to_string()),
                alert: None,
                temperature_diff: Some(1.0),
                air_pressure: Some(1001.0),
                comfort_index: Some("3".to_string()),
                comfort_label: Some("Comfortable".to_string()),
                weather_icon: Some("0".to_string()),
                wind_degree: Some(10.0),
            }),
            predict: Some(ForecastReport {
                publish_time: Some("11:00".to_string()),
                days: vec![ForecastDay {
                    date: "2026-06-29".to_string(),
                    day_info: Some("Cloudy".to_string()),
                    night_info: Some("Clear".to_string()),
                    day_temperature: Some("18".to_string()),
                    night_temperature: Some("8".to_string()),
                    wind_direct: Some("North".to_string()),
                    wind_power: Some("Light".to_string()),
                    precipitation: Some(0.0),
                    publish_time: Some("11:00".to_string()),
                    day_weather_icon: Some("Cloudy".to_string()),
                    night_weather_icon: Some("Clear".to_string()),
                    day_wind_direct: Some("North".to_string()),
                    day_wind_power: Some("Light".to_string()),
                    night_wind_direct: Some("North".to_string()),
                    night_wind_power: Some("Light".to_string()),
                }],
            }),
            air: None,
            tempchart: Vec::new(),
            passedchart: Vec::new(),
            climate: None,
            radar: None,
            stale: false,
            debug: None,
        }));

        assert_eq!(rows[0].0, "Weather");
        assert_eq!(rows[0].1, "Sunny");
        assert_eq!(rows[1].0, "Temperature");
        assert!(rows[1].1.contains("12"));
        assert!(rows[1].1.contains("18"));
        assert!(rows[1].1.contains("8"));
        assert_eq!(rows[2], ("Feels like", "10℃".to_string()));
        assert!(rows[3].1.contains("1001hPa"));
        assert!(rows[4].1.contains("Comfortable"));
        assert!(rows[5].1.contains("Light"));
        assert_eq!(rows[6], ("Sunrise", "06:00".to_string()));
    }

    #[test]
    fn current_rows_fall_back_to_charts_for_high_and_pressure() {
        let rows = current_rows(Some(&WeatherSnapshot {
            station: None,
            real: Some(ObservedWeather {
                info: Some("Cloudy".to_string()),
                temperature: Some(27.0),
                feel_temperature: Some(30.0),
                humidity: Some(68.0),
                rain: Some(0.0),
                wind_direct: Some("North".to_string()),
                wind_power: Some("Light".to_string()),
                wind_speed: Some(3.0),
                sunrise: None,
                sunset: None,
                publish_time: Some("18:25".to_string()),
                alert: None,
                temperature_diff: None,
                air_pressure: None,
                comfort_index: None,
                comfort_label: None,
                weather_icon: None,
                wind_degree: None,
            }),
            predict: Some(ForecastReport {
                publish_time: Some("20:00".to_string()),
                days: vec![ForecastDay {
                    date: "2026-06-29".to_string(),
                    day_info: None,
                    night_info: Some("Rain".to_string()),
                    day_temperature: None,
                    night_temperature: Some("21".to_string()),
                    wind_direct: None,
                    wind_power: None,
                    precipitation: Some(35.0),
                    publish_time: Some("20:00".to_string()),
                    day_weather_icon: None,
                    night_weather_icon: Some("9".to_string()),
                    day_wind_direct: None,
                    day_wind_power: None,
                    night_wind_direct: Some("North".to_string()),
                    night_wind_power: Some("Light".to_string()),
                }],
            }),
            tempchart: vec![TemperatureChart {
                date: Some("2026/06/29".to_string()),
                max_temperature: Some(27.1),
                min_temperature: Some(21.0),
                day_info: None,
                day_icon: None,
                night_info: Some("Rain".to_string()),
                night_icon: Some("9".to_string()),
            }],
            passedchart: vec![PassedWeatherChart {
                pressure: Some(998.0),
                ..Default::default()
            }],
            ..Default::default()
        }));

        assert!(rows[1].1.contains("high 27.1℃"));
        assert!(rows[1].1.contains("low 21℃"));
        assert!(rows[3].1.contains("pressure 998hPa"));
    }

    #[test]
    fn alert_text_lines_only_include_alert_details() {
        let lines = alert_text_lines(Some(&WeatherSnapshot {
            real: Some(ObservedWeather {
                alert: Some(WeatherAlert {
                    alert: Some("雷电黄色预警".to_string()),
                    signal_level: Some("黄色".to_string()),
                    issue_content: Some("雷阵雨天气".to_string()),
                    prevention: Some("注意防范".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            radar: Some(RadarInfo {
                title: Some("华北".to_string()),
                image_url: Some("https://www.nmc.cn/radar.png".to_string()),
                ..Default::default()
            }),
            passedchart: vec![PassedWeatherChart {
                rain_1h: Some(0.0),
                temperature: Some(27.2),
                wind_speed: Some(2.9),
                ..Default::default()
            }],
            ..Default::default()
        }));

        let joined = lines.join("\n");
        assert!(joined.contains("雷电黄色预警"));
        assert!(joined.contains("雷阵雨天气"));
        assert!(!joined.contains("Radar"));
        assert!(!joined.contains("Recent rain"));
    }

    #[test]
    fn alert_text_lines_split_full_width_semicolon() {
        let lines = alert_text_lines(Some(&WeatherSnapshot {
            real: Some(ObservedWeather {
                alert: Some(WeatherAlert {
                    alert: Some("雷电黄色预警".to_string()),
                    signal_level: Some("黄色".to_string()),
                    issue_content: Some("预计有雷阵雨；伴有阵风；请注意防范。".to_string()),
                    prevention: Some("减少户外活动；远离高处。".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }));

        assert_eq!(
            lines,
            vec![
                "雷电黄色预警 黄色",
                "预计有雷阵雨",
                "伴有阵风",
                "请注意防范。",
                "减少户外活动",
                "远离高处。",
            ]
        );
    }

    #[test]
    fn weather_refresh_errors_are_added_to_logs() {
        let mut app = TuiApp::empty_for_test();

        app.record_weather_refresh_error(anyhow::anyhow!(
            "WEATHER: table upstream_fetch_log has no column named unified_uuid"
        ));

        assert_eq!(
            app.logs.back().map(String::as_str),
            Some(
                "refresh failed: WEATHER: table upstream_fetch_log has no column named unified_uuid"
            )
        );
    }

    #[test]
    fn fetch_log_events_use_station_name_and_uuid_in_logs() {
        let mut app = TuiApp::empty_for_test();
        let uuid = weather_schema::unified_station_uuid("北京-北京市-朝阳");
        app.config.stations.push(StationConfig {
            name: "北京-北京市-朝阳".to_string(),
            enabled: true,
        });
        let payload = FetchLogEvent {
            unified_uuid: Some(uuid.clone()),
            endpoint: "rest/weather".to_string(),
            ok: true,
            message: None,
            timestamp_unix_ms: 0,
        }
        .encode_to_vec();
        let event = EngineEvent {
            topic: TOPIC_ENGINE_LOG.to_string(),
            envelope: EventEnvelope {
                schema_version: SCHEMA_VERSION.to_string(),
                event_id: "event-id".to_string(),
                kind: EventKind::FetchLog as i32,
                timestamp_unix_ms: 0,
                hmac_sha256: Vec::new(),
                payload,
            },
        };
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        tx.send(event).unwrap();

        app.drain_events(&mut rx);

        let expected = format!("engine.log 北京-北京市-朝阳({uuid}) ok: rest/weather");
        assert_eq!(app.logs.back().map(String::as_str), Some(expected.as_str()));
    }

    #[test]
    fn refresh_events_use_station_name_and_uuid_in_logs() {
        let mut app = TuiApp::empty_for_test();
        let uuid = weather_schema::unified_station_uuid("北京-北京市-朝阳");
        app.config.stations.push(StationConfig {
            name: "北京-北京市-朝阳".to_string(),
            enabled: true,
        });
        let payload = RefreshEvent {
            unified_uuid: Some(uuid.clone()),
            started: false,
            completed: true,
            message: None,
        }
        .encode_to_vec();
        let event = EngineEvent {
            topic: TOPIC_ENGINE_REFRESH.to_string(),
            envelope: EventEnvelope {
                schema_version: SCHEMA_VERSION.to_string(),
                event_id: "event-id".to_string(),
                kind: EventKind::Refresh as i32,
                timestamp_unix_ms: 0,
                hmac_sha256: Vec::new(),
                payload,
            },
        };
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        tx.send(event).unwrap();

        app.drain_events(&mut rx);

        let expected = format!("engine.refresh 北京-北京市-朝阳({uuid}) completed");
        assert_eq!(app.logs.back().map(String::as_str), Some(expected.as_str()));
    }
}
