use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn request_id() -> String {
    format!("daemon-{}", now_ms())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}
