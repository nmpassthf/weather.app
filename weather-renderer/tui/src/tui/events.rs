use std::time::Instant;

use prost::Message as _;
use weather_schema::{
    EngineStatus, EventKind, FetchLogEvent, FetchOutcome, LifecycleState, RefreshEvent,
    RefreshOutcome, RefreshPhase, WeatherSnapshotEvent,
};

use crate::client::EngineEvent;

use super::{
    TuiApp, configured_stations,
    effects::{Effect, EffectResult},
    state::{InputMode, PanelFocus, normalize_station_selection},
};

impl TuiApp {
    pub(super) fn apply_effect_result(&mut self, result: EffectResult) -> Vec<Effect> {
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

    fn record_weather_refresh_error(&mut self, error: impl std::fmt::Display) {
        self.push_log(format!("refresh failed: {error}"));
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

    #[cfg(test)]
    fn drain_events(&mut self, events: &mut tokio::sync::broadcast::Receiver<EngineEvent>) {
        while let Ok(event) = events.try_recv() {
            self.apply_engine_event(event);
        }
    }

    pub(super) fn apply_engine_event(&mut self, event: EngineEvent) {
        let kind = event.envelope.kind;
        let payload = event.envelope.payload;
        match kind {
            kind if kind == EventKind::WeatherSnapshot as i32 => {
                if let Ok(decoded) = WeatherSnapshotEvent::decode(payload.as_slice()) {
                    let incoming_uuid = decoded
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.station.as_ref())
                        .map(|station| station.unified_uuid.clone());
                    let current_uuid = self
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.station.as_ref())
                        .map(|station| station.unified_uuid.clone());
                    if incoming_uuid
                        .as_deref()
                        .is_some_and(|uuid| !uuid.is_empty())
                        && incoming_uuid == current_uuid
                    {
                        self.snapshot = decoded.snapshot;
                        self.last_weather_refresh = Some(Instant::now());
                    }
                }
            }
            kind if kind == EventKind::EngineStatus as i32 => {
                if let Ok(status) = EngineStatus::decode(payload.as_slice()) {
                    let state = match LifecycleState::try_from(status.lifecycle_state)
                        .unwrap_or(LifecycleState::Unspecified)
                    {
                        LifecycleState::Starting => "starting",
                        LifecycleState::Ready => "ready",
                        LifecycleState::Stopping => "stopping",
                        LifecycleState::Failed => "failed",
                        LifecycleState::Unspecified if status.ready => "ready",
                        LifecycleState::Unspecified => "not-ready",
                    };
                    let detail = status
                        .message
                        .as_deref()
                        .filter(|message| !message.is_empty())
                        .map(|message| format!(": {message}"))
                        .unwrap_or_default();
                    self.push_log(format!("engine {} {state}{detail}", status.mode));
                }
            }
            kind if kind == EventKind::FetchLog as i32 => {
                if let Ok(log) = FetchLogEvent::decode(payload.as_slice()) {
                    let station = self.log_station_label(&log.unified_uuid.unwrap_or_default());
                    let state = match FetchOutcome::try_from(log.outcome)
                        .unwrap_or(FetchOutcome::Unspecified)
                    {
                        FetchOutcome::Success => "ok",
                        FetchOutcome::Warning => "warn",
                        FetchOutcome::Failure => "fail",
                        FetchOutcome::Unspecified if log.ok => "ok",
                        FetchOutcome::Unspecified => "fail",
                    };
                    self.push_log(format!(
                        "{} {station} {state}: {}",
                        event.topic, log.endpoint
                    ));
                }
            }
            kind if kind == EventKind::Refresh as i32 => {
                if let Ok(refresh) = RefreshEvent::decode(payload.as_slice()) {
                    let station = self.log_station_label(&refresh.unified_uuid.unwrap_or_default());
                    match RefreshPhase::try_from(refresh.phase).unwrap_or(RefreshPhase::Unspecified)
                    {
                        RefreshPhase::Started => {
                            self.push_log(format!("{} {station} started", event.topic));
                        }
                        RefreshPhase::Completed => {
                            let outcome = RefreshOutcome::try_from(refresh.outcome)
                                .unwrap_or(RefreshOutcome::Unspecified);
                            let state = match outcome {
                                RefreshOutcome::Success => "success",
                                RefreshOutcome::Stale => "stale",
                                RefreshOutcome::Failure => "failure",
                                RefreshOutcome::Unspecified => "completed",
                            };
                            let detail = match outcome {
                                RefreshOutcome::Failure => refresh
                                    .message
                                    .as_deref()
                                    .and_then(|message| message.strip_prefix("failure: ")),
                                RefreshOutcome::Unspecified => refresh.message.as_deref(),
                                RefreshOutcome::Success | RefreshOutcome::Stale => None,
                            }
                            .filter(|message| !message.is_empty())
                            .map(|message| format!(": {message}"))
                            .unwrap_or_default();
                            self.push_log(format!("{} {station} {state}{detail}", event.topic));
                        }
                        RefreshPhase::Unspecified if refresh.started => {
                            self.push_log(format!("{} {station} started", event.topic));
                        }
                        RefreshPhase::Unspecified if refresh.completed => {
                            self.push_log(format!("{} {station} completed", event.topic));
                        }
                        RefreshPhase::Unspecified => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use prost::Message as _;
    use weather_schema::{
        EventEnvelope, RefreshEvent, SCHEMA_VERSION, StationConfig, TOPIC_ENGINE_LOG,
        TOPIC_ENGINE_REFRESH,
    };

    use super::*;

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
            ..Default::default()
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
        let (sender, mut receiver) = tokio::sync::broadcast::channel(1);
        sender.send(event).unwrap();

        app.drain_events(&mut receiver);

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
            ..Default::default()
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
        let (sender, mut receiver) = tokio::sync::broadcast::channel(1);
        sender.send(event).unwrap();

        app.drain_events(&mut receiver);

        let expected = format!("engine.refresh 北京-北京市-朝阳({uuid}) completed");
        assert_eq!(app.logs.back().map(String::as_str), Some(expected.as_str()));
    }

    #[test]
    fn structured_refresh_outcome_does_not_duplicate_legacy_message() {
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
            message: Some("failure: upstream timeout".to_string()),
            phase: RefreshPhase::Completed as i32,
            outcome: RefreshOutcome::Failure as i32,
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
        let (sender, mut receiver) = tokio::sync::broadcast::channel(1);
        sender.send(event).unwrap();

        app.drain_events(&mut receiver);

        let expected = format!("engine.refresh 北京-北京市-朝阳({uuid}) failure: upstream timeout");
        assert_eq!(app.logs.back().map(String::as_str), Some(expected.as_str()));
    }
}
