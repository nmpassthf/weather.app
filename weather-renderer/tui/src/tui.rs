use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
use tokio::sync::broadcast;
use weather_schema::*;

use crate::{
    cli::Cli,
    client::{EngineClient, EngineEvent, require_config},
    terminal::TerminalGuard,
    util::{degrees, hectopascal, meter_per_second, mm, percent, text, wind_summary},
};

mod effects;
mod input;
mod state;
mod view;

use self::{
    effects::{Effect, EffectResult, EffectRunner},
    input::TerminalInput,
    state::{
        InputMode, MoveDraft, PanelFocus, SearchIntent, SearchState, SearchStatus,
        move_station_selection, moved_index, normalize_station_selection,
    },
};

const EVENT_TICK: Duration = Duration::from_millis(200);
const WEATHER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const SEARCH_PAGE_SIZE: u32 = 12;
const MAX_LOG_LINES: usize = 64;

pub(crate) async fn run_interactive(client: &EngineClient, _cli: &Cli) -> Result<()> {
    let mut app = TuiApp::load(client).await?;
    let mut terminal = TerminalGuard::new()?;
    let mut events = client.subscribe_events();
    let (input, mut input_events) = TerminalInput::spawn();
    let (mut effects, mut effect_results) = EffectRunner::new(client.clone());
    if let Some(effect) = app.request_selected_weather(false) {
        effects.dispatch(effect);
    }
    let mut tick = tokio::time::interval(EVENT_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut exiting = false;

    let run_result = async {
        while !exiting {
            terminal.draw(|frame| view::render(frame, &mut app))?;
            let pending_effects = tokio::select! {
                input = input_events.recv() => match input {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let (exit, effects) = app.reduce_key(key);
                        exiting = exit;
                        effects
                    }
                    Some(Ok(_)) => Vec::new(),
                    Some(Err(error)) => {
                        return Err(anyhow::anyhow!("terminal input failed: {error}"));
                    }
                    None => return Err(anyhow::anyhow!("terminal input task stopped")),
                },
                result = effect_results.recv() => match result {
                    Some(result) => app.apply_effect_result(result),
                    None => return Err(anyhow::anyhow!("TUI effect result channel stopped")),
                },
                event = events.recv() => {
                    match event {
                        Ok(event) => app.apply_engine_event(event),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            app.push_log(format!("engine events lagged by {skipped}"));
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(anyhow::anyhow!("engine event stream closed"));
                        }
                    }
                    Vec::new()
                },
                _ = tick.tick() => app.tick(),
            };
            for effect in pending_effects {
                effects.dispatch(effect);
            }
            effects.reap_completed();
        }
        Ok(())
    }
    .await;

    input.stop().await;
    effects.shutdown().await;
    run_result
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

struct PendingConfig {
    token: u64,
    message: String,
    select_name: Option<String>,
}

struct TuiApp {
    config: weather_schema::AppConfig,
    stations: Vec<ConfiguredStation>,
    selected_station: Option<usize>,
    snapshot: Option<WeatherSnapshot>,
    mode: InputMode,
    focus: PanelFocus,
    forecast_scroll: usize,
    alert_scroll: usize,
    logs: VecDeque<String>,
    last_weather_refresh: Option<Instant>,
    manage_selected: usize,
    move_draft: Option<MoveDraft>,
    hidden_stations: HashSet<String>,
    preview_snapshot: Option<WeatherSnapshot>,
    search: SearchState,
    about_status: Option<EngineStatus>,
    weather_token: u64,
    weather_pending: bool,
    config_token: u64,
    pending_config: Option<PendingConfig>,
}

impl TuiApp {
    #[cfg(test)]
    fn empty_for_test() -> Self {
        Self {
            config: weather_schema::AppConfig::default(),
            stations: Vec::new(),
            selected_station: None,
            snapshot: None,
            mode: InputMode::Normal,
            focus: PanelFocus::Stations,
            forecast_scroll: 0,
            alert_scroll: 0,
            logs: VecDeque::from(["ready".to_string()]),
            last_weather_refresh: None,
            manage_selected: 0,
            move_draft: None,
            hidden_stations: HashSet::new(),
            preview_snapshot: None,
            search: SearchState::default(),
            about_status: None,
            weather_token: 0,
            weather_pending: false,
            config_token: 0,
            pending_config: None,
        }
    }

    async fn load(client: &EngineClient) -> Result<Self> {
        let config = require_config(client.get_config(false).await?.config, "get-config")?;
        let stations = configured_stations(&config);
        let about_status = client.status().await.ok();
        let selected_station = normalize_station_selection(&stations, &HashSet::new(), None);
        Ok(Self {
            config,
            stations,
            selected_station,
            snapshot: None,
            mode: InputMode::Normal,
            focus: PanelFocus::Stations,
            forecast_scroll: 0,
            alert_scroll: 0,
            logs: VecDeque::from(["ready".to_string()]),
            last_weather_refresh: None,
            manage_selected: 0,
            move_draft: None,
            hidden_stations: HashSet::new(),
            preview_snapshot: None,
            search: SearchState::default(),
            about_status,
            weather_token: 0,
            weather_pending: false,
            config_token: 0,
            pending_config: None,
        })
    }

    fn reduce_key(&mut self, key: KeyEvent) -> (bool, Vec<Effect>) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return (true, Vec::new());
        }
        match self.mode {
            InputMode::Normal => self.handle_normal_key(key.code),
            InputMode::Manage => self.handle_manage_key(key.code),
            InputMode::ManageMove => self.handle_move_key(key.code),
            InputMode::ManageAddSearch | InputMode::ManageBrowseSearch => {
                self.handle_search_key(key)
            }
            InputMode::About => self.handle_about_key(key.code),
        }
    }

    fn handle_normal_key(&mut self, code: KeyCode) -> (bool, Vec<Effect>) {
        let mut effects = Vec::new();
        match code {
            KeyCode::Char('q') => return (true, effects),
            KeyCode::Char('r') => {
                if let Some(effect) = self.request_selected_weather(true) {
                    effects.push(effect);
                }
            }
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
                    if self.select_station(1)
                        && let Some(effect) = self.request_selected_weather(false)
                    {
                        effects.push(effect);
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
                    if self.select_station(-1)
                        && let Some(effect) = self.request_selected_weather(false)
                    {
                        effects.push(effect);
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
                    .unwrap_or_default()
                    .min(self.config.stations.len().saturating_sub(1));
                self.push_log("manage mode");
            }
            KeyCode::Char('?') => {
                self.mode = InputMode::About;
                effects.push(Effect::LoadAbout);
            }
            _ => {}
        }
        (false, effects)
    }

    fn handle_about_key(&mut self, code: KeyCode) -> (bool, Vec<Effect>) {
        match code {
            KeyCode::Char('r') => return (false, vec![Effect::LoadAbout]),
            _ => {
                self.mode = InputMode::Normal;
            }
        }
        (false, Vec::new())
    }

    fn handle_manage_key(&mut self, code: KeyCode) -> (bool, Vec<Effect>) {
        let mut effects = Vec::new();
        match code {
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                let previous = self.selected_station;
                self.normalize_selected_station();
                self.push_log("manage closed");
                if self.selected_station != previous
                    && let Some(effect) = self.request_selected_weather(false)
                {
                    effects.push(effect);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.manage_selected =
                    moved_index(self.manage_selected, self.config.stations.len(), 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.manage_selected =
                    moved_index(self.manage_selected, self.config.stations.len(), -1);
            }
            KeyCode::Char(' ') => {
                if let Some(effect) = self.toggle_selected() {
                    effects.push(effect);
                }
            }
            KeyCode::Char('d') => {
                if let Some(effect) = self.delete_selected() {
                    effects.push(effect);
                }
            }
            KeyCode::Char('a') => self.enter_search(InputMode::ManageAddSearch),
            KeyCode::Char('s') => self.enter_search(InputMode::ManageBrowseSearch),
            KeyCode::Char('M') => {
                if self.pending_config.is_some() {
                    self.push_log("configuration update already in progress");
                    return (false, effects);
                }
                if let Some(draft) = MoveDraft::new(&self.config.stations, self.manage_selected) {
                    self.move_draft = Some(draft);
                    self.mode = InputMode::ManageMove;
                    self.push_log("move mode: j/k to move, Enter to confirm");
                }
            }
            KeyCode::Char('h') => {
                let Some(station) = self.config.stations.get(self.manage_selected) else {
                    return (false, effects);
                };
                let name = station.name.clone();
                let previous = self.selected_station;
                let new_hidden = if self.hidden_stations.contains(&name) {
                    self.hidden_stations.remove(&name);
                    false
                } else {
                    self.hidden_stations.insert(name.clone());
                    true
                };
                self.normalize_selected_station();
                if self.selected_station != previous
                    && let Some(effect) = self.request_selected_weather(false)
                {
                    effects.push(effect);
                }
                self.push_log(if new_hidden {
                    format!("hid {name}")
                } else {
                    format!("showed {name}")
                });
            }
            _ => {}
        }
        (false, effects)
    }

    fn handle_move_key(&mut self, code: KeyCode) -> (bool, Vec<Effect>) {
        let Some(draft) = self.move_draft.as_mut() else {
            self.mode = InputMode::Manage;
            return (false, Vec::new());
        };
        match code {
            KeyCode::Esc => {
                self.manage_selected = draft.origin;
                self.move_draft = None;
                self.mode = InputMode::Manage;
                self.push_log("move cancelled");
            }
            KeyCode::Enter => {
                let selected_name = draft
                    .stations
                    .get(draft.selected)
                    .map(|station| station.name.clone());
                let mut candidate = self.config.clone();
                candidate.stations = draft.stations.clone();
                self.manage_selected = draft.selected;
                self.move_draft = None;
                self.mode = InputMode::Manage;
                let effect = self.begin_config_update(
                    candidate,
                    "move confirmed".to_string(),
                    selected_name,
                );
                return (false, effect.into_iter().collect());
            }
            KeyCode::Char('j') | KeyCode::Down => {
                draft.move_by(1);
                self.manage_selected = draft.selected;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                draft.move_by(-1);
                self.manage_selected = draft.selected;
            }
            _ => {}
        }
        (false, Vec::new())
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> (bool, Vec<Effect>) {
        let intent = self.search.handle_key(key);
        match intent {
            SearchIntent::Exit => {
                self.exit_search_to_manage();
                (false, vec![Effect::CancelSearch])
            }
            SearchIntent::Submit => {
                let (token, query) = self.search.start();
                self.preview_snapshot = None;
                (false, vec![Effect::Search { token, query }])
            }
            SearchIntent::QueryChanged => {
                self.preview_snapshot = None;
                (false, vec![Effect::CancelSearch])
            }
            SearchIntent::Action => {
                let Some(station) = self.search.selected_result().cloned() else {
                    self.push_log("no selected search result");
                    return (false, Vec::new());
                };
                if self.mode == InputMode::ManageAddSearch {
                    let effect = self.add_selected_result(station);
                    let mut effects = vec![Effect::CancelSearch];
                    effects.extend(effect);
                    (false, effects)
                } else {
                    (
                        false,
                        vec![Effect::Preview {
                            token: self.search.token(),
                            station,
                        }],
                    )
                }
            }
            SearchIntent::None => (false, Vec::new()),
        }
    }

    fn enter_search(&mut self, mode: InputMode) {
        self.mode = mode;
        self.search.enter();
        self.preview_snapshot = None;
        self.push_log(match mode {
            InputMode::ManageAddSearch => "add search: type query, Enter to search, Ctrl+A to add",
            InputMode::ManageBrowseSearch => {
                "browse search: type query, Enter to search, Ctrl+A to preview"
            }
            _ => "",
        });
    }

    fn exit_search_to_manage(&mut self) {
        self.search.cancel_and_clear();
        self.mode = InputMode::Manage;
        self.preview_snapshot = None;
        self.push_log("search closed");
    }

    fn select_station(&mut self, delta: isize) -> bool {
        self.normalize_selected_station();
        let next = move_station_selection(
            &self.stations,
            &self.hidden_stations,
            self.selected_station,
            delta,
        );
        if next == self.selected_station {
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

    fn record_weather_refresh_error(&mut self, err: impl std::fmt::Display) {
        self.push_log(format!("refresh failed: {err}"));
    }

    fn toggle_selected(&mut self) -> Option<Effect> {
        let station = self.config.stations.get(self.manage_selected)?;
        let mut candidate = self.config.clone();
        candidate.stations[self.manage_selected].enabled = !station.enabled;
        let new_enabled = !station.enabled;
        let name = station.name.clone();
        self.begin_config_update(
            candidate,
            format!(
                "{} {name}",
                if new_enabled { "enabled" } else { "disabled" }
            ),
            Some(name),
        )
    }

    fn delete_selected(&mut self) -> Option<Effect> {
        if self.config.stations.is_empty() {
            return None;
        }
        let mut candidate = self.config.clone();
        let name = candidate.stations.remove(self.manage_selected).name;
        let select_name = candidate
            .stations
            .get(
                self.manage_selected
                    .min(candidate.stations.len().saturating_sub(1)),
            )
            .map(|station| station.name.clone());
        self.begin_config_update(candidate, format!("removed {name}"), select_name)
    }

    fn add_selected_result(&mut self, station: StationRef) -> Option<Effect> {
        let name = station.name.clone();
        let mut candidate = self.config.clone();
        let already = candidate.stations.iter().any(|item| item.name == name);
        let message = if already {
            if let Some(existing) = candidate.stations.iter_mut().find(|item| item.name == name) {
                existing.enabled = true;
            }
            format!("enabled existing {name}")
        } else {
            candidate.stations.push(StationConfig {
                name: name.clone(),
                enabled: true,
            });
            format!("added {name}")
        };
        self.exit_search_to_manage();
        self.begin_config_update(candidate, message, Some(name))
    }

    fn begin_config_update(
        &mut self,
        config: AppConfig,
        message: String,
        select_name: Option<String>,
    ) -> Option<Effect> {
        if self.pending_config.is_some() {
            self.push_log("configuration update already in progress");
            return None;
        }
        self.config_token = self
            .config_token
            .checked_add(1)
            .expect("TUI config token exhausted");
        let token = self.config_token;
        self.pending_config = Some(PendingConfig {
            token,
            message,
            select_name,
        });
        Some(Effect::UpdateConfig {
            token,
            config: Box::new(config),
        })
    }

    fn request_selected_weather(&mut self, refresh: bool) -> Option<Effect> {
        self.normalize_selected_station();
        let Some(index) = self.selected_station else {
            self.snapshot = None;
            self.weather_pending = false;
            self.last_weather_refresh = Some(Instant::now());
            self.push_log("no enabled visible stations");
            return None;
        };
        let name = self.stations.get(index)?.name.clone();
        self.weather_token = self
            .weather_token
            .checked_add(1)
            .expect("TUI weather token exhausted");
        self.weather_pending = true;
        Some(Effect::LoadWeather {
            token: self.weather_token,
            name,
            unified_uuid: None,
            refresh,
        })
    }

    fn tick(&mut self) -> Vec<Effect> {
        let due = self
            .last_weather_refresh
            .is_none_or(|last| last.elapsed() >= WEATHER_REFRESH_INTERVAL);
        if due
            && !self.weather_pending
            && let Some(effect) = self.request_selected_weather(false)
        {
            return vec![effect];
        }
        Vec::new()
    }

    fn normalize_selected_station(&mut self) {
        self.selected_station = normalize_station_selection(
            &self.stations,
            &self.hidden_stations,
            self.selected_station,
        );
    }

    fn apply_effect_result(&mut self, result: EffectResult) -> Vec<Effect> {
        match result {
            EffectResult::SearchPage(page) => {
                self.search.apply_page(page);
            }
            EffectResult::SearchFailed { token, message } => {
                if self.search.apply_error(token, message.clone()) {
                    self.push_log(format!("search failed: {message}"));
                }
            }
            EffectResult::Weather {
                token,
                name,
                result,
            } => {
                if token != self.weather_token {
                    return Vec::new();
                }
                self.weather_pending = false;
                self.last_weather_refresh = Some(Instant::now());
                match result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        if self.focus.hidden(self) {
                            self.focus = PanelFocus::Stations;
                        }
                        self.push_log(format!("loaded {name}"));
                    }
                    Err(error) => self.record_weather_refresh_error(error),
                }
            }
            EffectResult::Preview {
                token,
                station_name,
                result,
            } => {
                let selected_matches = self
                    .search
                    .selected_result()
                    .is_some_and(|station| station.name == station_name);
                if token == self.search.token()
                    && self.mode == InputMode::ManageBrowseSearch
                    && selected_matches
                {
                    match result {
                        Ok(snapshot) => {
                            self.preview_snapshot = Some(snapshot);
                            self.push_log(format!("preview {station_name}"));
                        }
                        Err(error) => self.push_log(format!("preview failed: {error}")),
                    }
                }
            }
            EffectResult::Config { token, result } => {
                let Some(pending) = self.pending_config.take() else {
                    return Vec::new();
                };
                if token != pending.token {
                    self.pending_config = Some(pending);
                    return Vec::new();
                }
                match result {
                    Ok(config) => {
                        let old_selected_name = self
                            .selected_station
                            .and_then(|index| self.stations.get(index))
                            .map(|station| station.name.clone());
                        self.config = config;
                        self.stations = configured_stations(&self.config);
                        self.hidden_stations.retain(|name| {
                            self.stations.iter().any(|station| station.name == *name)
                        });
                        let preferred = old_selected_name.as_ref().and_then(|name| {
                            self.stations
                                .iter()
                                .position(|station| station.name == *name)
                        });
                        self.selected_station = normalize_station_selection(
                            &self.stations,
                            &self.hidden_stations,
                            preferred,
                        );
                        if let Some(name) = pending.select_name {
                            self.manage_selected = self
                                .config
                                .stations
                                .iter()
                                .position(|station| station.name == name)
                                .unwrap_or_else(|| self.config.stations.len().saturating_sub(1));
                        } else {
                            self.manage_selected = self
                                .manage_selected
                                .min(self.config.stations.len().saturating_sub(1));
                        }
                        self.push_log(pending.message);
                        let selected_name = self
                            .selected_station
                            .and_then(|index| self.stations.get(index))
                            .map(|station| station.name.clone());
                        if selected_name != old_selected_name
                            && let Some(effect) = self.request_selected_weather(false)
                        {
                            return vec![effect];
                        }
                    }
                    Err(error) => {
                        if let Some(name) = pending.select_name {
                            self.manage_selected = self
                                .config
                                .stations
                                .iter()
                                .position(|station| station.name == name)
                                .unwrap_or_else(|| {
                                    self.manage_selected
                                        .min(self.config.stations.len().saturating_sub(1))
                                });
                        } else {
                            self.manage_selected = self
                                .manage_selected
                                .min(self.config.stations.len().saturating_sub(1));
                        }
                        self.push_log(format!("configuration update failed: {error}"));
                    }
                }
            }
            EffectResult::About(result) => match result {
                Ok(status) => self.about_status = Some(status),
                Err(error) => self.push_log(format!("about refresh failed: {error}")),
            },
            EffectResult::TaskFailed(error) => self.push_log(error),
        }
        Vec::new()
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
            .search
            .results
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
    #[cfg(test)]
    fn drain_events(&mut self, events: &mut broadcast::Receiver<EngineEvent>) {
        while let Ok(event) = events.try_recv() {
            self.apply_engine_event(event);
        }
    }

    fn apply_engine_event(&mut self, event: EngineEvent) {
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
                    let station = self.log_station_label(&refresh.unified_uuid.unwrap_or_default());
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

fn configured_stations(config: &AppConfig) -> Vec<ConfiguredStation> {
    config
        .stations
        .iter()
        .map(|station| ConfiguredStation {
            name: station.name.clone(),
            enabled: station.enabled,
        })
        .collect()
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
        .filter(|(_, station)| !app.hidden_stations.contains(&station.name))
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
        .position(|(index, _)| Some(*index) == app.selected_station);
    let mut state = ListState::default();
    state.select(visible_pos);
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
            Line::from("Enter search   Up/Down select   Ctrl+A add"),
            Line::from("Esc back   type to edit query"),
        ],
        InputMode::ManageBrowseSearch => vec![
            Line::from("Enter search   Up/Down select   Ctrl+A preview"),
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
    let title = if app.move_draft.is_some() {
        "Manage Stations [moving]"
    } else {
        "Manage Stations"
    };
    let stations = app
        .move_draft
        .as_ref()
        .map(|draft| draft.stations.as_slice())
        .unwrap_or(&app.config.stations);
    let items = stations
        .iter()
        .enumerate()
        .map(|(index, station)| {
            let marker = if station.enabled { "[x]" } else { "[ ]" };
            let hidden = app.hidden_stations.contains(&station.name);
            let hide_tag = if hidden { " (hidden)" } else { "" };
            let moving = app
                .move_draft
                .as_ref()
                .is_some_and(|draft| draft.selected == index);
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
    let loading_label = if app.search.status == SearchStatus::Loading {
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
        Paragraph::new(app.search.query.clone()).block(
            Block::default().borders(Borders::ALL).title(format!(
                "Search {mode_label} (page {}{loading_label})",
                app.search.next_offset / SEARCH_PAGE_SIZE
            )),
        ),
        chunks[0],
    );

    let items = app
        .search
        .results
        .iter()
        .map(|station| ListItem::new(station.name.clone()))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.search.selected));
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
        let hint = match (&app.search.status, app.mode) {
            (SearchStatus::Failed(error), _) => error.as_str(),
            (_, InputMode::ManageAddSearch) => {
                "Enter search | Up/Down select | Ctrl+A add | Esc back"
            }
            (_, InputMode::ManageBrowseSearch) => {
                "Enter search | Up/Down select | Ctrl+A preview | Esc back"
            }
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
    fn failed_move_restores_authoritative_order_and_selection() {
        let mut app = TuiApp::empty_for_test();
        app.config.stations = vec![
            StationConfig {
                name: "a".to_string(),
                enabled: true,
            },
            StationConfig {
                name: "b".to_string(),
                enabled: true,
            },
        ];
        app.stations = configured_stations(&app.config);
        app.manage_selected = 0;
        app.move_draft = MoveDraft::new(&app.config.stations, 0);
        app.move_draft.as_mut().unwrap().move_by(1);
        app.manage_selected = 1;
        app.mode = InputMode::ManageMove;

        let (_, effects) = app.handle_move_key(KeyCode::Enter);
        let Effect::UpdateConfig { token, config } = effects.into_iter().next().unwrap() else {
            panic!("move confirmation must request a config update");
        };
        assert_eq!(config.stations[0].name, "b");
        assert_eq!(app.config.stations[0].name, "a");

        app.apply_effect_result(EffectResult::Config {
            token,
            result: Err("injected".to_string()),
        });

        assert_eq!(app.config.stations[0].name, "a");
        assert_eq!(app.config.stations[1].name, "b");
        assert_eq!(app.manage_selected, 0);
    }

    #[test]
    fn cancelling_move_discards_the_draft_without_touching_config() {
        let mut app = TuiApp::empty_for_test();
        app.config.stations = vec![
            StationConfig {
                name: "a".to_string(),
                enabled: true,
            },
            StationConfig {
                name: "b".to_string(),
                enabled: true,
            },
        ];
        app.manage_selected = 0;
        app.move_draft = MoveDraft::new(&app.config.stations, 0);
        app.move_draft.as_mut().unwrap().move_by(1);
        app.manage_selected = 1;
        app.mode = InputMode::ManageMove;

        let (_, effects) = app.handle_move_key(KeyCode::Esc);

        assert!(effects.is_empty());
        assert!(app.move_draft.is_none());
        assert_eq!(app.mode, InputMode::Manage);
        assert_eq!(app.manage_selected, 0);
        assert_eq!(app.config.stations[0].name, "a");
        assert_eq!(app.config.stations[1].name, "b");
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
