use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::broadcast;
use weather_schema::*;

use crate::{
    cli::Cli,
    client::{EngineClient, require_config},
    terminal::TerminalGuard,
};

mod effects;
mod events;
mod input;
mod state;
mod view;

use self::{
    effects::{Effect, EffectRunner},
    input::TerminalInput,
    state::{
        InputMode, MoveDraft, PanelFocus, SearchIntent, SearchState, move_station_selection,
        moved_index, normalize_station_selection,
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

    fn push_log(&mut self, message: impl Into<String>) {
        self.logs.push_back(message.into());
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
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

#[cfg(test)]
mod tests {
    use super::effects::EffectResult;
    use super::*;

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
}
