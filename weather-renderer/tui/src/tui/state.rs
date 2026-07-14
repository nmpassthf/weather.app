use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use weather_schema::{ConfiguredStation, StationConfig, StationRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputMode {
    Normal,
    Manage,
    ManageMove,
    ManageAddSearch,
    ManageBrowseSearch,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelFocus {
    Stations,
    Current,
    Forecast,
    Alert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SearchStatus {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchIntent {
    None,
    Exit,
    Submit,
    QueryChanged,
    Action,
}

pub(super) struct SearchPage {
    pub token: u64,
    pub requested_offset: u32,
    pub stations: Vec<StationRef>,
    pub has_more: bool,
    pub next_offset: u32,
}

pub(super) struct SearchState {
    pub query: String,
    pub results: Vec<StationRef>,
    pub selected: usize,
    pub has_more: bool,
    pub next_offset: u32,
    pub status: SearchStatus,
    token: u64,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            has_more: false,
            next_offset: 0,
            status: SearchStatus::Idle,
            token: 0,
        }
    }
}

impl SearchState {
    pub fn enter(&mut self) {
        self.cancel_and_clear();
        self.query.clear();
    }

    pub fn cancel_and_clear(&mut self) {
        self.bump_token();
        self.results.clear();
        self.selected = 0;
        self.has_more = false;
        self.next_offset = 0;
        self.status = SearchStatus::Idle;
    }

    pub fn start(&mut self) -> (u64, String) {
        self.bump_token();
        self.results.clear();
        self.selected = 0;
        self.has_more = false;
        self.next_offset = 0;
        self.status = SearchStatus::Loading;
        (self.token, self.query.clone())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SearchIntent {
        match key.code {
            KeyCode::Esc => SearchIntent::Exit,
            KeyCode::Enter => SearchIntent::Submit,
            KeyCode::Backspace => {
                self.query.pop();
                self.cancel_results_for_edit();
                SearchIntent::QueryChanged
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                SearchIntent::Action
            }
            KeyCode::Down => {
                self.selected = moved_index(self.selected, self.results.len(), 1);
                SearchIntent::None
            }
            KeyCode::Up => {
                self.selected = moved_index(self.selected, self.results.len(), -1);
                SearchIntent::None
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(ch);
                self.cancel_results_for_edit();
                SearchIntent::QueryChanged
            }
            _ => SearchIntent::None,
        }
    }

    pub fn apply_page(&mut self, page: SearchPage) -> bool {
        if page.token != self.token || self.status != SearchStatus::Loading {
            return false;
        }
        if page.requested_offset != self.next_offset {
            self.status = SearchStatus::Failed(format!(
                "search page arrived out of order: expected {}, got {}",
                self.next_offset, page.requested_offset
            ));
            self.has_more = false;
            return false;
        }
        append_unique_stations(&mut self.results, page.stations);
        self.has_more = page.has_more;
        self.next_offset = page.next_offset;
        if self.selected >= self.results.len() && !self.results.is_empty() {
            self.selected = self.results.len() - 1;
        }
        if !page.has_more {
            self.status = SearchStatus::Ready;
        }
        true
    }

    pub fn apply_error(&mut self, token: u64, message: String) -> bool {
        if token != self.token || self.status != SearchStatus::Loading {
            return false;
        }
        self.has_more = false;
        self.status = SearchStatus::Failed(message);
        true
    }

    pub fn selected_result(&self) -> Option<&StationRef> {
        self.results.get(self.selected)
    }

    pub fn token(&self) -> u64 {
        self.token
    }

    fn cancel_results_for_edit(&mut self) {
        self.bump_token();
        self.results.clear();
        self.selected = 0;
        self.has_more = false;
        self.next_offset = 0;
        self.status = SearchStatus::Idle;
    }

    fn bump_token(&mut self) {
        self.token = self
            .token
            .checked_add(1)
            .expect("TUI search token exhausted");
    }
}

#[derive(Clone)]
pub(super) struct MoveDraft {
    pub stations: Vec<StationConfig>,
    pub selected: usize,
    pub origin: usize,
}

impl MoveDraft {
    pub fn new(stations: &[StationConfig], selected: usize) -> Option<Self> {
        if stations.is_empty() {
            return None;
        }
        let selected = selected.min(stations.len() - 1);
        Some(Self {
            stations: stations.to_vec(),
            selected,
            origin: selected,
        })
    }

    pub fn move_by(&mut self, delta: isize) {
        let next = moved_index(self.selected, self.stations.len(), delta);
        if next != self.selected {
            self.stations.swap(self.selected, next);
            self.selected = next;
        }
    }
}

pub(super) fn moved_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

pub(super) fn station_is_selectable(station: &ConfiguredStation, hidden: &HashSet<String>) -> bool {
    station.enabled && !hidden.contains(&station.name)
}

pub(super) fn normalize_station_selection(
    stations: &[ConfiguredStation],
    hidden: &HashSet<String>,
    selected: Option<usize>,
) -> Option<usize> {
    if selected.is_some_and(|index| {
        stations
            .get(index)
            .is_some_and(|station| station_is_selectable(station, hidden))
    }) {
        return selected;
    }
    stations
        .iter()
        .position(|station| station_is_selectable(station, hidden))
}

pub(super) fn move_station_selection(
    stations: &[ConfiguredStation],
    hidden: &HashSet<String>,
    selected: Option<usize>,
    delta: isize,
) -> Option<usize> {
    let selectable = stations
        .iter()
        .enumerate()
        .filter_map(|(index, station)| station_is_selectable(station, hidden).then_some(index))
        .collect::<Vec<_>>();
    if selectable.is_empty() {
        return None;
    }
    let position = selected
        .and_then(|selected| selectable.iter().position(|index| *index == selected))
        .unwrap_or_default();
    Some(selectable[moved_index(position, selectable.len(), delta)])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn station(name: &str, enabled: bool) -> ConfiguredStation {
        ConfiguredStation {
            name: name.to_string(),
            enabled,
        }
    }

    fn result(name: &str) -> StationRef {
        StationRef {
            name: name.to_string(),
            unified_uuid: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn ordinary_a_edits_query_while_control_a_is_an_action() {
        let mut state = SearchState::default();
        state.results.push(result("existing"));

        assert_eq!(
            state.handle_key(key(KeyCode::Char('a'))),
            SearchIntent::QueryChanged
        );
        assert_eq!(state.query, "a");
        assert!(state.results.is_empty());
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            SearchIntent::Action
        );
        assert_eq!(state.query, "a");
    }

    #[test]
    fn one_page_success_and_error_both_clear_loading() {
        let mut state = SearchState::default();
        let (token, _) = state.start();
        assert_eq!(state.status, SearchStatus::Loading);
        state.apply_page(SearchPage {
            token,
            requested_offset: 0,
            stations: vec![result("one")],
            has_more: false,
            next_offset: 1,
        });
        assert_eq!(state.status, SearchStatus::Ready);

        let (token, _) = state.start();
        state.apply_error(token, "failed".to_string());
        assert_eq!(state.status, SearchStatus::Failed("failed".to_string()));
    }

    #[test]
    fn stale_search_pages_are_ignored_after_edit_or_new_search() {
        let mut state = SearchState::default();
        let (old, _) = state.start();
        state.handle_key(key(KeyCode::Char('x')));
        let (current, _) = state.start();

        assert!(!state.apply_page(SearchPage {
            token: old,
            requested_offset: 0,
            stations: vec![result("stale")],
            has_more: false,
            next_offset: 1,
        }));
        assert_eq!(state.token(), current);
        assert!(state.results.is_empty());
    }

    #[test]
    fn out_of_order_search_page_fails_the_active_search() {
        let mut state = SearchState::default();
        let (token, _) = state.start();

        assert!(!state.apply_page(SearchPage {
            token,
            requested_offset: 12,
            stations: Vec::new(),
            has_more: true,
            next_offset: 24,
        }));
        assert!(matches!(state.status, SearchStatus::Failed(_)));
    }

    #[test]
    fn ordered_multi_page_search_stays_loading_until_the_terminal_page() {
        let mut state = SearchState::default();
        let (token, _) = state.start();

        assert!(state.apply_page(SearchPage {
            token,
            requested_offset: 0,
            stations: vec![result("first")],
            has_more: true,
            next_offset: 12,
        }));
        assert_eq!(state.status, SearchStatus::Loading);
        assert!(state.apply_page(SearchPage {
            token,
            requested_offset: 12,
            stations: vec![result("second")],
            has_more: false,
            next_offset: 13,
        }));
        assert_eq!(state.status, SearchStatus::Ready);
        assert_eq!(state.results.len(), 2);
    }

    #[test]
    fn move_draft_cancel_leaves_authoritative_order_untouched() {
        let authoritative = vec![
            StationConfig {
                name: "a".into(),
                enabled: true,
            },
            StationConfig {
                name: "b".into(),
                enabled: true,
            },
        ];
        let mut draft = MoveDraft::new(&authoritative, 0).unwrap();
        draft.move_by(1);
        assert_eq!(draft.stations[0].name, "b");
        drop(draft);
        assert_eq!(authoritative[0].name, "a");
        assert_eq!(authoritative[1].name, "b");
    }

    #[test]
    fn selection_skips_disabled_and_hidden_stations() {
        let stations = vec![
            station("first", true),
            station("disabled", false),
            station("hidden", true),
            station("last", true),
        ];
        let hidden = HashSet::from(["hidden".to_string()]);

        assert_eq!(
            normalize_station_selection(&stations, &hidden, None),
            Some(0)
        );
        assert_eq!(
            move_station_selection(&stations, &hidden, Some(0), 1),
            Some(3)
        );
        assert_eq!(
            move_station_selection(&stations, &hidden, Some(3), -1),
            Some(0)
        );
    }
}
