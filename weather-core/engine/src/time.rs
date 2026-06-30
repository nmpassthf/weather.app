use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) fn request_id() -> String {
    format!("{}-{}", std::process::id(), now_ms())
}

/// 把 `unix_ms` 在 `timezone`（IANA 名，如 `Asia/Shanghai`）下格式化为 `YYYY-MM-DD`。
pub(crate) fn date_for_tz(unix_ms: i64, timezone: &str) -> Result<String> {
    let tz = chrono_tz::Tz::from_str(timezone)
        .map_err(|_| anyhow::anyhow!("invalid timezone `{timezone}`"))?;
    let dt =
        DateTime::<Utc>::from_timestamp_millis(unix_ms).context("invalid fetched_at_unix_ms")?;
    Ok(dt.with_timezone(&tz).format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_for_shanghai_crosses_day_at_16_utc() {
        // UTC 2026-06-23 16:00 = Shanghai 2026-06-24 00:00
        let ts = DateTime::<Utc>::from_timestamp(1_782_252_000, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(date_for_tz(ts, "Asia/Shanghai").unwrap(), "2026-06-24");
        assert_eq!(date_for_tz(ts, "UTC").unwrap(), "2026-06-23");
    }

    #[test]
    fn rejects_invalid_timezone() {
        assert!(date_for_tz(0, "Not/A/Zone").is_err());
    }
}
