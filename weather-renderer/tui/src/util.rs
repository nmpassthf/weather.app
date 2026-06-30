use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn format_index(index: usize) -> String {
    format!("{index:>2}")
}

pub(crate) fn short_region_name(value: &str) -> &str {
    value
        .strip_suffix('市')
        .or_else(|| value.strip_suffix('省'))
        .or_else(|| value.strip_suffix("自治区"))
        .or_else(|| value.strip_suffix("特别行政区"))
        .unwrap_or(value)
}

pub(crate) fn text(value: Option<&str>) -> &str {
    match value {
        Some("") | Some("9999") | None => "-",
        Some(value) => value,
    }
}

pub(crate) fn wind_summary(direct: Option<&str>, power: Option<&str>) -> String {
    match (text(direct), text(power)) {
        ("-", "-") => "-".to_string(),
        ("-", power) => power.to_string(),
        (direct, "-") => direct.to_string(),
        (direct, power) => format!("{direct}{power}"),
    }
}

pub(crate) fn degrees(value: Option<f64>) -> String {
    number_with_unit(value, "℃")
}
pub(crate) fn percent(value: Option<f64>) -> String {
    number_with_unit(value, "%")
}
pub(crate) fn mm(value: Option<f64>) -> String {
    number_with_unit(value, "mm")
}
pub(crate) fn meter_per_second(value: Option<f64>) -> String {
    number_with_unit(value, "m/s")
}
pub(crate) fn hectopascal(value: Option<f64>) -> String {
    number_with_unit(value, "hPa")
}

fn number_with_unit(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(value) if value.fract().abs() < f64::EPSILON => format!("{value:.0}{unit}"),
        Some(value) => format!("{value:.1}{unit}"),
        None => "-".to_string(),
    }
}

pub(crate) fn request_id() -> String {
    format!("{}-{}", std::process::id(), now_ms())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
