use std::{error::Error, fmt, sync::Arc};

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
