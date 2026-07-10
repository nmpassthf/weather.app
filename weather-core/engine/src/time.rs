use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    local_date(unix_ms, tz)
}

/// Returns the current local date and the exact duration until that date
/// changes. The binary search also handles DST days that are not 24 hours.
pub(crate) fn local_date_and_next_change(
    unix_ms: i64,
    timezone: &str,
) -> Result<(String, Duration)> {
    const SEARCH_WINDOW_MS: i64 = 48 * 60 * 60 * 1000;

    let tz = chrono_tz::Tz::from_str(timezone)
        .map_err(|_| anyhow::anyhow!("invalid timezone `{timezone}`"))?;
    let current = local_date(unix_ms, tz)?;
    let mut low = unix_ms;
    let mut high = unix_ms
        .checked_add(SEARCH_WINDOW_MS)
        .context("timestamp overflow while locating next local date")?;
    if local_date(high, tz)? == current {
        anyhow::bail!("local date did not change within 48 hours for `{timezone}`");
    }

    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if local_date(middle, tz)? == current {
            low = middle;
        } else {
            high = middle;
        }
    }
    let wait_ms = high.saturating_sub(unix_ms).max(1) as u64;
    Ok((current, Duration::from_millis(wait_ms)))
}

fn local_date(unix_ms: i64, tz: chrono_tz::Tz) -> Result<String> {
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

    #[test]
    fn locates_next_local_date_boundary() {
        let ts = DateTime::parse_from_rfc3339("2026-06-23T15:59:30Z")
            .unwrap()
            .timestamp_millis();
        let (date, wait) = local_date_and_next_change(ts, "Asia/Shanghai").unwrap();

        assert_eq!(date, "2026-06-23");
        assert_eq!(wait, Duration::from_secs(30));
    }
}
