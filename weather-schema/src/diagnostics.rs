use std::fmt;

/// Stable error codes carried by the v1 `EngineError.code` string field.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RpcErrorCode {
    Engine,
    BadRequest,
    SchemaVersion,
    Auth,
    OwnerMismatch,
    Updater,
    Database,
    PayloadTooLarge,
    Busy,
    Timeout,
    Fuzzy,
    RestartRequired,
    Config,
    Weather,
}

impl RpcErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "ENGINE",
            Self::BadRequest => "BAD_REQUEST",
            Self::SchemaVersion => "SCHEMA_VERSION",
            Self::Auth => "AUTH",
            Self::OwnerMismatch => "OWNER_MISMATCH",
            Self::Updater => "UPDATER",
            Self::Database => "DB",
            Self::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            Self::Busy => "BUSY",
            Self::Timeout => "TIMEOUT",
            Self::Fuzzy => "FUZZY",
            Self::RestartRequired => "RESTART_REQUIRED",
            Self::Config => "CONFIG",
            Self::Weather => "WEATHER",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "ENGINE" => Self::Engine,
            "BAD_REQUEST" => Self::BadRequest,
            "SCHEMA_VERSION" => Self::SchemaVersion,
            "AUTH" => Self::Auth,
            "OWNER_MISMATCH" => Self::OwnerMismatch,
            "UPDATER" => Self::Updater,
            "DB" => Self::Database,
            "PAYLOAD_TOO_LARGE" => Self::PayloadTooLarge,
            "BUSY" => Self::Busy,
            "TIMEOUT" => Self::Timeout,
            "FUZZY" => Self::Fuzzy,
            "RESTART_REQUIRED" => Self::RestartRequired,
            "CONFIG" => Self::Config,
            "WEATHER" => Self::Weather,
            _ => return None,
        })
    }
}

impl fmt::Display for RpcErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;
    use crate::{
        EngineStatus, FetchLogEvent, FetchOutcome, LifecycleState, RefreshEvent, RefreshOutcome,
        RefreshPhase,
    };

    #[derive(Clone, PartialEq, prost::Message)]
    struct LegacyEngineStatus {
        #[prost(bool, tag = "1")]
        ready: bool,
        #[prost(string, optional, tag = "7")]
        message: Option<String>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct LegacyFetchLogEvent {
        #[prost(string, optional, tag = "1")]
        unified_uuid: Option<String>,
        #[prost(string, tag = "2")]
        endpoint: String,
        #[prost(bool, tag = "3")]
        ok: bool,
        #[prost(string, optional, tag = "4")]
        message: Option<String>,
        #[prost(int64, tag = "5")]
        timestamp_unix_ms: i64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct LegacyRefreshEvent {
        #[prost(string, optional, tag = "1")]
        unified_uuid: Option<String>,
        #[prost(bool, tag = "2")]
        started: bool,
        #[prost(bool, tag = "3")]
        completed: bool,
        #[prost(string, optional, tag = "4")]
        message: Option<String>,
    }

    #[test]
    fn known_rpc_error_codes_round_trip_without_accepting_future_values() {
        let known = [
            RpcErrorCode::Engine,
            RpcErrorCode::BadRequest,
            RpcErrorCode::SchemaVersion,
            RpcErrorCode::Auth,
            RpcErrorCode::OwnerMismatch,
            RpcErrorCode::Updater,
            RpcErrorCode::Database,
            RpcErrorCode::PayloadTooLarge,
            RpcErrorCode::Busy,
            RpcErrorCode::Timeout,
            RpcErrorCode::Fuzzy,
            RpcErrorCode::RestartRequired,
            RpcErrorCode::Config,
            RpcErrorCode::Weather,
        ];

        for code in known {
            assert_eq!(RpcErrorCode::from_wire(code.as_str()), Some(code));
            assert_eq!(code.to_string(), code.as_str());
        }
        assert_eq!(RpcErrorCode::from_wire("FUTURE_CODE"), None);
    }

    #[test]
    fn diagnostic_enum_numbers_are_frozen() {
        let lifecycle = [
            (LifecycleState::Unspecified, 0),
            (LifecycleState::Starting, 1),
            (LifecycleState::Ready, 2),
            (LifecycleState::Stopping, 3),
            (LifecycleState::Failed, 4),
        ];
        let fetch = [
            (FetchOutcome::Unspecified, 0),
            (FetchOutcome::Success, 1),
            (FetchOutcome::Warning, 2),
            (FetchOutcome::Failure, 3),
        ];
        let phase = [
            (RefreshPhase::Unspecified, 0),
            (RefreshPhase::Started, 1),
            (RefreshPhase::Completed, 2),
        ];
        let refresh = [
            (RefreshOutcome::Unspecified, 0),
            (RefreshOutcome::Success, 1),
            (RefreshOutcome::Stale, 2),
            (RefreshOutcome::Failure, 3),
        ];

        for (value, number) in lifecycle {
            assert_eq!(value as i32, number);
            assert_eq!(LifecycleState::try_from(number).unwrap(), value);
        }
        for (value, number) in fetch {
            assert_eq!(value as i32, number);
            assert_eq!(FetchOutcome::try_from(number).unwrap(), value);
        }
        for (value, number) in phase {
            assert_eq!(value as i32, number);
            assert_eq!(RefreshPhase::try_from(number).unwrap(), value);
        }
        for (value, number) in refresh {
            assert_eq!(value as i32, number);
            assert_eq!(RefreshOutcome::try_from(number).unwrap(), value);
        }
    }

    #[test]
    fn additive_diagnostics_preserve_legacy_wire_fields_both_directions() {
        let legacy_status = LegacyEngineStatus {
            ready: true,
            message: Some("ready".to_string()),
        };
        let decoded_status =
            EngineStatus::decode(legacy_status.encode_to_vec().as_slice()).unwrap();
        assert!(decoded_status.ready);
        assert_eq!(decoded_status.message.as_deref(), Some("ready"));
        assert_eq!(
            decoded_status.lifecycle_state,
            LifecycleState::Unspecified as i32
        );

        let new_status = EngineStatus {
            ready: false,
            message: Some("stopping".to_string()),
            lifecycle_state: LifecycleState::Stopping as i32,
            ..Default::default()
        };
        let decoded_legacy_status =
            LegacyEngineStatus::decode(new_status.encode_to_vec().as_slice()).unwrap();
        assert!(!decoded_legacy_status.ready);
        assert_eq!(decoded_legacy_status.message.as_deref(), Some("stopping"));

        let legacy_fetch = LegacyFetchLogEvent {
            unified_uuid: Some("uuid".to_string()),
            endpoint: "rest/weather".to_string(),
            ok: false,
            message: Some("offline".to_string()),
            timestamp_unix_ms: 7,
        };
        let decoded_fetch = FetchLogEvent::decode(legacy_fetch.encode_to_vec().as_slice()).unwrap();
        assert!(!decoded_fetch.ok);
        assert_eq!(decoded_fetch.message.as_deref(), Some("offline"));
        assert_eq!(decoded_fetch.outcome, FetchOutcome::Unspecified as i32);

        let new_fetch = FetchLogEvent {
            unified_uuid: Some("uuid".to_string()),
            endpoint: "rest/weather".to_string(),
            ok: true,
            message: Some("cache warning".to_string()),
            timestamp_unix_ms: 8,
            outcome: FetchOutcome::Warning as i32,
        };
        let decoded_legacy_fetch =
            LegacyFetchLogEvent::decode(new_fetch.encode_to_vec().as_slice()).unwrap();
        assert!(decoded_legacy_fetch.ok);
        assert_eq!(
            decoded_legacy_fetch.message.as_deref(),
            Some("cache warning")
        );

        let legacy_refresh = LegacyRefreshEvent {
            unified_uuid: Some("uuid".to_string()),
            started: false,
            completed: true,
            message: Some("stale".to_string()),
        };
        let decoded_refresh =
            RefreshEvent::decode(legacy_refresh.encode_to_vec().as_slice()).unwrap();
        assert!(decoded_refresh.completed);
        assert_eq!(decoded_refresh.phase, RefreshPhase::Unspecified as i32);
        assert_eq!(decoded_refresh.outcome, RefreshOutcome::Unspecified as i32);

        let new_refresh = RefreshEvent {
            unified_uuid: Some("uuid".to_string()),
            started: false,
            completed: true,
            message: Some("stale".to_string()),
            phase: RefreshPhase::Completed as i32,
            outcome: RefreshOutcome::Stale as i32,
        };
        let decoded_legacy_refresh =
            LegacyRefreshEvent::decode(new_refresh.encode_to_vec().as_slice()).unwrap();
        assert!(!decoded_legacy_refresh.started);
        assert!(decoded_legacy_refresh.completed);
        assert_eq!(decoded_legacy_refresh.message.as_deref(), Some("stale"));
    }
}
