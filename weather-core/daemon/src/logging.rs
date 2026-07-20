use std::{
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::{Result, anyhow};
use chrono::{Local, SecondsFormat};
use log::{LevelFilter, Log, Metadata, Record};

static LOGGER: WeatherLogger = WeatherLogger {
    level: AtomicU8::new(INFO),
};
static INSTALLED: AtomicBool = AtomicBool::new(false);

const OFF: u8 = 0;
const ERROR: u8 = 1;
const WARN: u8 = 2;
const INFO: u8 = 3;
const DEBUG: u8 = 4;
const TRACE: u8 = 5;

struct WeatherLogger {
    level: AtomicU8,
}

impl WeatherLogger {
    fn set_level(&self, level: LevelFilter) {
        self.level.store(encode_level(level), Ordering::Release);
    }

    fn level(&self) -> LevelFilter {
        decode_level(self.level.load(Ordering::Acquire))
    }
}

impl Log for WeatherLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        weather_target(metadata.target())
            && self
                .level()
                .to_level()
                .is_some_and(|maximum| metadata.level() <= maximum)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let timestamp = Local::now().to_rfc3339_opts(SecondsFormat::Millis, false);
        eprintln!(
            "{timestamp} {:<5} {} — {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

pub(crate) fn configure(level: &str) -> Result<()> {
    let level =
        LevelFilter::from_str(level).map_err(|_| anyhow!("invalid daemon log level `{level}`"))?;
    LOGGER.set_level(level);
    if !INSTALLED.swap(true, Ordering::AcqRel) {
        log::set_logger(&LOGGER).map_err(|_| anyhow!("failed to install daemon logger"))?;
    }
    log::set_max_level(level);
    Ok(())
}

fn weather_target(target: &str) -> bool {
    target
        .split("::")
        .next()
        .is_some_and(|root| root.starts_with("weather_"))
}

fn encode_level(level: LevelFilter) -> u8 {
    match level {
        LevelFilter::Off => OFF,
        LevelFilter::Error => ERROR,
        LevelFilter::Warn => WARN,
        LevelFilter::Info => INFO,
        LevelFilter::Debug => DEBUG,
        LevelFilter::Trace => TRACE,
    }
}

fn decode_level(level: u8) -> LevelFilter {
    match level {
        OFF => LevelFilter::Off,
        ERROR => LevelFilter::Error,
        WARN => LevelFilter::Warn,
        INFO => LevelFilter::Info,
        DEBUG => LevelFilter::Debug,
        TRACE => LevelFilter::Trace,
        _ => LevelFilter::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_weather_targets_are_enabled() {
        assert!(weather_target("weather_engine::server"));
        assert!(weather_target("weather_daemon::run"));
        assert!(!weather_target("tokio::runtime"));
        assert!(!weather_target("zeromq::router"));
        assert!(!weather_target("hyper::client"));
    }

    #[test]
    fn level_encoding_round_trips() {
        for level in [
            LevelFilter::Off,
            LevelFilter::Error,
            LevelFilter::Warn,
            LevelFilter::Info,
            LevelFilter::Debug,
            LevelFilter::Trace,
        ] {
            assert_eq!(decode_level(encode_level(level)), level);
        }
    }
}
