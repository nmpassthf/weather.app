use prost::Message;
use weather_schema::*;

use crate::runtime::Engine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshTerminal {
    Success,
    Stale,
    Failure(String),
}

impl RefreshTerminal {
    fn message(self) -> String {
        match self {
            Self::Success => "success".to_string(),
            Self::Stale => "stale".to_string(),
            Self::Failure(message) => format!("failure: {message}"),
        }
    }
}

impl Engine {
    pub(crate) fn publish_event(&self, topic: &str, kind: EventKind, payload: Vec<u8>) {
        let mut envelope = EventEnvelope {
            schema_version: SCHEMA_VERSION.to_string(),
            event_id: correlation_id("engine-event"),
            kind: kind as i32,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
            hmac_sha256: Vec::new(),
            payload,
        };
        if let Some(sig) = self.event_signature(&envelope) {
            envelope.hmac_sha256 = sig;
        }
        let _ = self.sink.send((topic.to_string(), envelope));
    }

    /// 广播天气快照。所有站点共用单一 topic `weather.snapshot`，
    /// 订阅方按 `snapshot.station.unified_uuid` 过滤自己关心的站点。
    pub(crate) fn publish_snapshot(&self, snapshot: &WeatherSnapshot) {
        let mut snapshot = snapshot.clone();
        snapshot.debug = None;
        let payload = WeatherSnapshotEvent {
            snapshot: Some(snapshot),
        }
        .encode_to_vec();
        self.publish_event(TOPIC_WEATHER_SNAPSHOT, EventKind::WeatherSnapshot, payload);
    }

    pub(crate) fn publish_status(&self, mode: &str, rpc_endpoint: &str, pub_endpoint: &str) {
        let status = self.status(mode, rpc_endpoint, pub_endpoint);
        let payload = status.encode_to_vec();
        let mut envelope = EventEnvelope {
            schema_version: SCHEMA_VERSION.to_string(),
            event_id: correlation_id("engine-event"),
            kind: EventKind::EngineStatus as i32,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
            hmac_sha256: Vec::new(),
            payload,
        };
        if let Some(sig) = self.event_signature(&envelope) {
            envelope.hmac_sha256 = sig;
        }
        let _ = self.sink.send((TOPIC_ENGINE_STATUS.to_string(), envelope));
    }

    pub(crate) fn publish_fetch_log(
        &self,
        unified_uuid: Option<&str>,
        endpoint: &str,
        ok: bool,
        message: Option<String>,
    ) {
        if let Some(line) = fetch_log_output_line(unified_uuid, endpoint, ok, message.as_deref()) {
            println!("{line}");
        }
        let payload = FetchLogEvent {
            unified_uuid: unified_uuid.map(str::to_string),
            endpoint: endpoint.to_string(),
            ok,
            message,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
        }
        .encode_to_vec();
        self.publish_event(TOPIC_ENGINE_LOG, EventKind::FetchLog, payload);
    }

    pub(crate) fn publish_refresh_started(&self, unified_uuid: Option<&str>) {
        self.publish_refresh(unified_uuid, true, false, None);
    }

    /// Every started refresh is followed by one completed terminal event.
    /// `message` is a stable `success` / `stale` / `failure: ...` discriminator
    /// while preserving the existing protobuf wire shape.
    pub(crate) fn publish_refresh_terminal(
        &self,
        unified_uuid: Option<&str>,
        outcome: RefreshTerminal,
    ) {
        self.publish_refresh(unified_uuid, false, true, Some(outcome.message()));
    }

    fn publish_refresh(
        &self,
        unified_uuid: Option<&str>,
        started: bool,
        completed: bool,
        message: Option<String>,
    ) {
        let payload = refresh_event(unified_uuid, started, completed, message).encode_to_vec();
        self.publish_event(TOPIC_ENGINE_REFRESH, EventKind::Refresh, payload);
    }

    fn event_signature(&self, envelope: &EventEnvelope) -> Option<Vec<u8>> {
        let config = self.config.get();
        let key = weather_configure::resolve_hmac_key(&config).ok()??;
        weather_schema::event_hmac(envelope, &key).ok()
    }
}

fn refresh_event(
    unified_uuid: Option<&str>,
    started: bool,
    completed: bool,
    message: Option<String>,
) -> RefreshEvent {
    RefreshEvent {
        unified_uuid: unified_uuid.map(str::to_string),
        started,
        completed,
        message,
    }
}

fn fetch_log_output_line(
    unified_uuid: Option<&str>,
    endpoint: &str,
    ok: bool,
    message: Option<&str>,
) -> Option<String> {
    if ok && message.is_none() {
        return None;
    }
    let level = if ok { "warn" } else { "error" };
    let station = unified_uuid
        .filter(|value| !value.is_empty())
        .map(|value| format!(" station={value}"))
        .unwrap_or_default();
    let message = message
        .filter(|value| !value.is_empty())
        .map(|value| format!(" message={value}"))
        .unwrap_or_default();
    Some(format!(
        "weather-engine {level}: endpoint={endpoint}{station}{message}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_log_output_skips_plain_success() {
        assert!(fetch_log_output_line(Some("uuid"), "rest/weather", true, None).is_none());
    }

    #[test]
    fn fetch_log_output_includes_failure_context() {
        let line = fetch_log_output_line(
            Some("uuid-1"),
            "rest/weather",
            false,
            Some("failed to decode NMC weather response"),
        )
        .expect("failure should produce output");

        assert!(line.contains("weather-engine error"));
        assert!(line.contains("endpoint=rest/weather"));
        assert!(line.contains("station=uuid-1"));
        assert!(line.contains("failed to decode NMC weather response"));
    }

    #[test]
    fn fetch_log_output_includes_warning_context() {
        let line = fetch_log_output_line(None, "rest/weather", true, Some("using stale data"))
            .expect("message should produce output");

        assert!(line.contains("weather-engine warn"));
        assert!(line.contains("endpoint=rest/weather"));
        assert!(line.contains("using stale data"));
    }

    #[test]
    fn refresh_terminal_messages_distinguish_outcomes() {
        assert_eq!(RefreshTerminal::Success.message(), "success");
        assert_eq!(RefreshTerminal::Stale.message(), "stale");
        assert_eq!(
            RefreshTerminal::Failure("upstream timeout".to_string()).message(),
            "failure: upstream timeout"
        );
    }

    #[test]
    fn terminal_refresh_event_is_completed_and_not_started() {
        let event = refresh_event(
            Some("uuid"),
            false,
            true,
            Some(RefreshTerminal::Stale.message()),
        );

        assert_eq!(event.unified_uuid.as_deref(), Some("uuid"));
        assert!(!event.started);
        assert!(event.completed);
        assert_eq!(event.message.as_deref(), Some("stale"));
    }
}
