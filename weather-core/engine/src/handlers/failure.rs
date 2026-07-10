use weather_schema::{EngineError, RpcErrorCode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpcFailure {
    code: RpcErrorCode,
    message: String,
}

impl RpcFailure {
    pub(crate) fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn into_engine_error(self) -> EngineError {
        EngineError {
            code: self.code.as_str().to_string(),
            message: self.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_failure_preserves_typed_code_and_wire_message() {
        let error = RpcFailure::new(RpcErrorCode::Busy, "try later").into_engine_error();

        assert_eq!(error.code, "BUSY");
        assert_eq!(error.message, "try later");
    }
}
