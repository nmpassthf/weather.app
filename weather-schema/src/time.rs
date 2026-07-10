use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Return the current Unix timestamp in signed milliseconds.
pub fn unix_timestamp_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    duration_millis(elapsed)
}

fn duration_millis(elapsed: Duration) -> Result<i64> {
    elapsed
        .as_millis()
        .try_into()
        .context("Unix timestamp does not fit in i64 milliseconds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_timestamp_is_non_negative_and_checked() {
        assert!(unix_timestamp_ms().unwrap() >= 0);
    }

    #[test]
    fn signed_millisecond_bound_is_explicit() {
        let largest = Duration::from_millis(i64::MAX as u64);
        assert_eq!(duration_millis(largest).unwrap(), i64::MAX);

        let overflowing = Duration::from_millis(i64::MAX as u64 + 1);
        assert!(duration_millis(overflowing).is_err());
    }
}
