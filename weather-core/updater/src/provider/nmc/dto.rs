use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct EnvelopeDto {
    code: i32,
    #[serde(default)]
    msg: Value,
    #[serde(default)]
    data: Value,
}

#[derive(Debug, Deserialize)]
pub(super) struct ProvinceDto {
    pub(super) code: String,
    pub(super) name: String,
    pub(super) url: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct CityDto {
    pub(super) code: String,
    pub(super) province: String,
    pub(super) city: String,
    pub(super) url: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct WeatherDataDto {
    #[serde(default)]
    pub(super) real: Option<Value>,
    #[serde(default)]
    pub(super) predict: Option<Value>,
    #[serde(default)]
    pub(super) air: Option<Value>,
    #[serde(default)]
    pub(super) tempchart: Option<Value>,
    #[serde(default)]
    pub(super) passedchart: Option<Value>,
    #[serde(default)]
    pub(super) climate: Option<Value>,
    #[serde(default)]
    pub(super) radar: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RealDto {
    #[serde(default)]
    pub(super) station: Option<Value>,
    #[serde(default)]
    pub(super) publish_time: Option<Value>,
    #[serde(default)]
    pub(super) weather: Option<Value>,
    #[serde(default)]
    pub(super) wind: Option<Value>,
    #[serde(default)]
    pub(super) warn: Option<Value>,
    #[serde(default, rename = "sunriseSunset")]
    pub(super) sunrise_sunset: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct StationDto {
    #[serde(default)]
    pub(super) province: Option<Value>,
    #[serde(default)]
    pub(super) city: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RealWeatherDto {
    #[serde(default)]
    pub(super) temperature: Option<Value>,
    #[serde(default)]
    pub(super) humidity: Option<Value>,
    #[serde(default)]
    pub(super) rain: Option<Value>,
    #[serde(default)]
    pub(super) info: Option<Value>,
    #[serde(default)]
    pub(super) feelst: Option<Value>,
    #[serde(default, rename = "temperatureDiff")]
    pub(super) temperature_diff: Option<Value>,
    #[serde(default, rename = "airpressure")]
    pub(super) air_pressure: Option<Value>,
    #[serde(default, rename = "icomfort")]
    pub(super) comfort_index: Option<Value>,
    #[serde(default, rename = "rcomfort")]
    pub(super) comfort_label: Option<Value>,
    #[serde(default, rename = "img")]
    pub(super) weather_icon: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WindDto {
    #[serde(default)]
    pub(super) direct: Option<Value>,
    #[serde(default)]
    pub(super) power: Option<Value>,
    #[serde(default)]
    pub(super) speed: Option<Value>,
    #[serde(default)]
    pub(super) degree: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WarnDto {
    #[serde(default)]
    pub(super) alert: Option<Value>,
    #[serde(default)]
    pub(super) province: Option<Value>,
    #[serde(default)]
    pub(super) city: Option<Value>,
    #[serde(default)]
    pub(super) url: Option<Value>,
    #[serde(default)]
    pub(super) issuecontent: Option<Value>,
    #[serde(default)]
    pub(super) fmeans: Option<Value>,
    #[serde(default)]
    pub(super) signaltype: Option<Value>,
    #[serde(default)]
    pub(super) signallevel: Option<Value>,
    #[serde(default)]
    pub(super) pic: Option<Value>,
    #[serde(default)]
    pub(super) pic2: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SunriseSunsetDto {
    #[serde(default)]
    pub(super) sunrise: Option<Value>,
    #[serde(default)]
    pub(super) sunset: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PredictDto {
    #[serde(default)]
    pub(super) station: Option<Value>,
    #[serde(default)]
    pub(super) publish_time: Option<Value>,
    #[serde(default)]
    pub(super) detail: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ForecastDayDto {
    #[serde(default)]
    pub(super) date: Option<Value>,
    #[serde(default)]
    pub(super) pt: Option<Value>,
    #[serde(default)]
    pub(super) day: Option<Value>,
    #[serde(default)]
    pub(super) night: Option<Value>,
    #[serde(default)]
    pub(super) precipitation: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ForecastPartDto {
    #[serde(default)]
    pub(super) weather: Option<Value>,
    #[serde(default)]
    pub(super) wind: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ForecastWeatherDto {
    #[serde(default)]
    pub(super) info: Option<Value>,
    #[serde(default)]
    pub(super) temperature: Option<Value>,
    #[serde(default, rename = "img")]
    pub(super) icon: Option<Value>,
}

pub(super) fn decode_weather_response(body: Value) -> Result<WeatherDataDto> {
    if !body.is_object() {
        bail!("NMC response envelope must be an object");
    }
    let envelope: EnvelopeDto =
        serde_json::from_value(body).context("failed to decode NMC response envelope")?;
    if envelope.code != 0 {
        let message = envelope_message(&envelope.msg);
        bail!("NMC weather API returned code {}: {message}", envelope.code);
    }
    if envelope.data.is_null() {
        bail!("NMC weather API returned success with null data");
    }
    if !envelope.data.is_object() {
        bail!("NMC weather API returned success without a weather data object");
    }
    serde_json::from_value(envelope.data).context("failed to decode NMC weather data object")
}

fn envelope_message(value: &Value) -> String {
    match value {
        Value::String(message) if !message.trim().is_empty() => message.trim().to_string(),
        Value::Null => "unknown provider error".to_string(),
        other => other.to_string(),
    }
}
