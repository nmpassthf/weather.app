use std::{error::Error, fmt, sync::Arc};

use weather_schema::{EngineError, RpcErrorCode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ClientFailure {
    Closed,
    RpcSend(Arc<str>),
    RpcReceive(Arc<str>),
    EventReceive(Arc<str>),
    BackgroundTask(Arc<str>),
}

impl ClientFailure {
    pub(super) fn rpc_send(error: impl fmt::Display) -> Self {
        Self::RpcSend(Arc::from(error.to_string()))
    }

    pub(super) fn rpc_receive(error: impl fmt::Display) -> Self {
        Self::RpcReceive(Arc::from(error.to_string()))
    }

    pub(super) fn event_receive(error: impl fmt::Display) -> Self {
        Self::EventReceive(Arc::from(error.to_string()))
    }

    pub(super) fn background_task(error: impl fmt::Display) -> Self {
        Self::BackgroundTask(Arc::from(error.to_string()))
    }
}

impl fmt::Display for ClientFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("RPC client closed"),
            Self::RpcSend(error) => write!(formatter, "RPC sender stopped: {error}"),
            Self::RpcReceive(error) => write!(formatter, "RPC receiver stopped: {error}"),
            Self::EventReceive(error) => write!(formatter, "event receiver stopped: {error}"),
            Self::BackgroundTask(error) => {
                write!(formatter, "RPC client background task stopped: {error}")
            }
        }
    }
}

impl Error for ClientFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRpcError {
    wire_code: Option<String>,
    known_code: Option<RpcErrorCode>,
    message: String,
}

impl RemoteRpcError {
    pub(super) fn from_engine_error(error: EngineError) -> Self {
        Self {
            known_code: RpcErrorCode::from_wire(&error.code),
            wire_code: Some(error.code),
            message: error.message,
        }
    }

    pub(super) fn missing_engine_error() -> Self {
        Self {
            wire_code: None,
            known_code: None,
            message: "error response is missing EngineError".to_string(),
        }
    }

    #[cfg(test)]
    fn wire_code(&self) -> Option<&str> {
        self.wire_code.as_deref()
    }

    #[cfg(test)]
    fn known_code(&self) -> Option<RpcErrorCode> {
        self.known_code
    }
}

impl fmt::Display for RemoteRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display_code = self
            .known_code
            .map(RpcErrorCode::as_str)
            .or(self.wire_code.as_deref());
        match display_code {
            Some(code) => write!(formatter, "{code}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl Error for RemoteRpcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_error_retains_known_and_unknown_wire_codes() {
        let known = RemoteRpcError::from_engine_error(EngineError {
            code: "BUSY".to_string(),
            message: "try later".to_string(),
        });
        assert_eq!(known.wire_code(), Some("BUSY"));
        assert_eq!(known.known_code(), Some(RpcErrorCode::Busy));
        assert_eq!(known.to_string(), "BUSY: try later");

        let unknown = RemoteRpcError::from_engine_error(EngineError {
            code: "FUTURE_CODE".to_string(),
            message: "new server diagnostic".to_string(),
        });
        assert_eq!(unknown.wire_code(), Some("FUTURE_CODE"));
        assert_eq!(unknown.known_code(), None);
        assert_eq!(unknown.to_string(), "FUTURE_CODE: new server diagnostic");

        let missing = RemoteRpcError::missing_engine_error();
        assert_eq!(missing.wire_code(), None);
        assert_eq!(missing.known_code(), None);
        assert_eq!(missing.to_string(), "error response is missing EngineError");
    }
}
