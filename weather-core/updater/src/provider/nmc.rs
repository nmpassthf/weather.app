use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Map, Value};
use weather_configure::ProviderConfig;
use weather_schema::*;
use weather_utils::JsonHttpClient;

use crate::catalog::{ProviderCity, ProviderProvince};
use crate::util::{canonical_station_name, clean, clean_num};

use super::{FetchOptions, WeatherProvider};

const USER_AGENT: &str = "weather.app/0.1";
const WEATHER_PATH: &str = "rest/weather";
const NMC_PUBLIC_BASE_URL: &str = "https://www.nmc.cn";

#[derive(Clone)]
pub(crate) struct NmcProvider {
    name: String,
    http: JsonHttpClient,
}

impl NmcProvider {
    pub(crate) fn new(config: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            name: config.name.clone(),
            http: JsonHttpClient::new(
                &config.base_url,
                Duration::from_secs(config.request_timeout_seconds),
                USER_AGENT,
            )
            .context("failed to build NMC HTTP client")?,
        })
    }
}

impl WeatherProvider for NmcProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn provinces(&self) -> Result<Vec<ProviderProvince>> {
        let rows: Vec<RawProvince> = self.http.get_json("rest/province/all", &[]).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn cities(&self, provider_province_code: &str) -> Result<Vec<ProviderCity>> {
        let rows: Vec<RawCity> = self
            .http
            .get_json(&format!("rest/province/{provider_province_code}"), &[])
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut city: ProviderCity = row.into();
                city.provider_province_code = provider_province_code.to_string();
                city
            })
            .collect())
    }

    async fn weather(
        &self,
        provider_station_id: &str,
        options: FetchOptions,
    ) -> Result<WeatherSnapshot> {
        let query = [("stationid", provider_station_id)];
        let endpoint = self.http.url_for(WEATHER_PATH, &query)?.to_string();
        let raw_value: Value = self.http.get_json(WEATHER_PATH, &query).await?;
        let raw_json = options.include_debug.then(|| raw_value.to_string());
        let env: RawEnvelope<RawWeatherData> = serde_json::from_value(raw_value)
            .with_context(|| format!("failed to decode NMC weather response from {endpoint}"))?;
        if env.code != 0 {
            return Err(anyhow!(
                "NMC weather API returned code {}: {}",
                env.code,
                env.msg
            ));
        }
        let mut snapshot = env.data.into_snapshot();
        if options.include_debug {
            snapshot.debug = Some(DebugPayload {
                provider: self.name().to_string(),
                operation: "weather".to_string(),
                endpoint,
                raw_json: raw_json.unwrap_or_default(),
                warnings: Vec::new(),
            });
        }
        Ok(snapshot)
    }

    async fn position(&self, provider_station_id: &str) -> Result<StationRef> {
        let position: RawStation = self
            .http
            .get_json("rest/position", &[("stationid", provider_station_id)])
            .await?;
        Ok(position.into())
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawEnvelope<T> {
    pub(crate) code: i32,
    pub(crate) msg: String,
    pub(crate) data: T,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawProvince {
    code: String,
    name: String,
    url: String,
}

impl From<RawProvince> for ProviderProvince {
    fn from(value: RawProvince) -> Self {
        Self {
            provider_code: value.code,
            name: value.name,
            url: value.url,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawCity {
    code: String,
    province: String,
    city: String,
    url: String,
}

impl From<RawCity> for ProviderCity {
    fn from(value: RawCity) -> Self {
        Self {
            provider_code: value.code,
            provider_province_code: String::new(),
            province: value.province,
            city: value.city,
            url: value.url,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawWeatherData {
    real: Option<RawReal>,
    predict: Option<RawPredict>,
    air: Option<Value>,
    tempchart: Option<Value>,
    passedchart: Option<Value>,
    climate: Option<Value>,
    radar: Option<Value>,
}

impl RawWeatherData {
    pub(crate) fn into_snapshot(self) -> WeatherSnapshot {
        let station = self
            .real
            .as_ref()
            .map(|real| real.station.clone())
            .or_else(|| self.predict.as_ref().map(|predict| predict.station.clone()))
            .map(Into::into);
        WeatherSnapshot {
            station,
            real: self.real.map(Into::into),
            predict: self.predict.map(Into::into),
            air: self.air.and_then(air_quality_from_value),
            tempchart: value_array(self.tempchart)
                .into_iter()
                .filter_map(temperature_chart_from_value)
                .collect(),
            passedchart: value_array(self.passedchart)
                .into_iter()
                .filter_map(passed_chart_from_value)
                .collect(),
            climate: self.climate.and_then(climate_from_value),
            radar: self.radar.and_then(radar_from_value),
            stale: false,
            debug: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawStation {
    province: String,
    city: String,
}

impl From<RawStation> for StationRef {
    fn from(value: RawStation) -> Self {
        let name = canonical_station_name(&value.province, &value.city);
        let unified_uuid = weather_schema::unified_station_uuid(&name);
        Self {
            province: value.province,
            city: value.city,
            unified_uuid,
            name,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawReal {
    station: RawStation,
    publish_time: Option<String>,
    weather: Option<RawRealWeather>,
    wind: Option<RawWind>,
    warn: Option<RawWarn>,
    #[serde(rename = "sunriseSunset")]
    sunrise_sunset: Option<RawSunriseSunset>,
}

impl From<RawReal> for ObservedWeather {
    fn from(value: RawReal) -> Self {
        let weather = value.weather.unwrap_or_default();
        let wind = value.wind.unwrap_or_default();
        let sun = value.sunrise_sunset.unwrap_or_default();
        let comfort_index = weather.comfort_index.as_ref().and_then(value_to_string);
        let comfort_label = weather.comfort_label.as_ref().and_then(value_to_string);
        Self {
            publish_time: value.publish_time,
            info: clean(weather.info),
            temperature: clean_num(weather.temperature),
            feel_temperature: clean_num(weather.feelst),
            humidity: clean_num(weather.humidity),
            rain: clean_num(weather.rain),
            wind_direct: clean(wind.direct),
            wind_power: clean(wind.power),
            wind_speed: clean_num(wind.speed),
            sunrise: clean(sun.sunrise),
            sunset: clean(sun.sunset),
            alert: value.warn.and_then(weather_alert_from_warn),
            temperature_diff: clean_num(weather.temperature_diff),
            air_pressure: clean_num(weather.air_pressure),
            comfort_index,
            comfort_label,
            weather_icon: clean(weather.weather_icon),
            wind_degree: clean_num(wind.degree),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawRealWeather {
    temperature: Option<f64>,
    humidity: Option<f64>,
    rain: Option<f64>,
    info: Option<String>,
    feelst: Option<f64>,
    #[serde(rename = "temperatureDiff")]
    temperature_diff: Option<f64>,
    #[serde(rename = "airpressure")]
    air_pressure: Option<f64>,
    #[serde(rename = "icomfort")]
    comfort_index: Option<Value>,
    #[serde(rename = "rcomfort")]
    comfort_label: Option<Value>,
    #[serde(rename = "img")]
    weather_icon: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawWind {
    direct: Option<String>,
    power: Option<String>,
    speed: Option<f64>,
    degree: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawWarn {
    alert: Option<String>,
    province: Option<String>,
    city: Option<String>,
    url: Option<String>,
    issuecontent: Option<String>,
    fmeans: Option<String>,
    signaltype: Option<String>,
    signallevel: Option<String>,
    pic: Option<String>,
    pic2: Option<String>,
}

fn weather_alert_from_warn(value: RawWarn) -> Option<WeatherAlert> {
    if value
        .alert
        .as_deref()
        .is_none_or(|v| v == "9999" || v.is_empty())
    {
        return None;
    }
    Some(WeatherAlert {
        alert: clean(value.alert),
        province: clean(value.province),
        city: clean(value.city),
        url: absolutize_nmc_url(clean(value.url)),
        issue_content: clean(value.issuecontent),
        prevention: clean(value.fmeans),
        signal_type: clean(value.signaltype),
        signal_level: clean(value.signallevel),
        icon_url: absolutize_nmc_url(clean(value.pic)),
        icon_name: clean(value.pic2),
    })
}

#[derive(Debug, Default, Deserialize)]
struct RawSunriseSunset {
    sunrise: Option<String>,
    sunset: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPredict {
    station: RawStation,
    publish_time: Option<String>,
    detail: Vec<RawForecastDay>,
}

impl From<RawPredict> for ForecastReport {
    fn from(value: RawPredict) -> Self {
        Self {
            publish_time: value.publish_time,
            days: value.detail.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawForecastDay {
    date: String,
    pt: Option<String>,
    day: RawForecastPart,
    night: RawForecastPart,
    precipitation: Option<f64>,
}

impl From<RawForecastDay> for ForecastDay {
    fn from(value: RawForecastDay) -> Self {
        let day_info = clean(value.day.weather.info);
        let night_info = clean(value.night.weather.info);
        let day_temperature = clean(value.day.weather.temperature);
        let night_temperature = clean(value.night.weather.temperature);
        let day_weather_icon = clean(value.day.weather.icon);
        let night_weather_icon = clean(value.night.weather.icon);
        let day_wind_direct = clean(value.day.wind.direct);
        let day_wind_power = clean(value.day.wind.power);
        let night_wind_direct = clean(value.night.wind.direct);
        let night_wind_power = clean(value.night.wind.power);
        Self {
            date: value.date,
            day_info,
            night_info,
            day_temperature,
            night_temperature,
            wind_direct: day_wind_direct.clone(),
            wind_power: day_wind_power.clone(),
            precipitation: clean_num(value.precipitation),
            publish_time: clean(value.pt),
            day_weather_icon,
            night_weather_icon,
            day_wind_direct,
            day_wind_power,
            night_wind_direct,
            night_wind_power,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawForecastPart {
    weather: RawForecastWeather,
    wind: RawWind,
}

#[derive(Debug, Deserialize)]
struct RawForecastWeather {
    info: Option<String>,
    temperature: Option<String>,
    #[serde(rename = "img")]
    icon: Option<String>,
}

fn value_array(value: Option<Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(values)) => values.into_iter().filter(|v| !is_empty_value(v)).collect(),
        Some(value) if !is_empty_value(&value) => vec![value],
        _ => Vec::new(),
    }
}

fn value_object(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    }
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.trim().is_empty() || value == "9999",
        Value::Object(map) => map.is_empty(),
        Value::Array(values) => values.is_empty(),
        _ => false,
    }
}

fn field_string(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(value_to_string)
}

fn field_f64(map: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(value_to_f64)
}

fn field_u32(map: &Map<String, Value>, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| map.get(*key))
        .and_then(value_to_u32)
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => clean(Some(value.trim().to_string())),
        Value::Number(value) => clean(Some(value.to_string())),
        _ => None,
    }
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(value) => value.as_f64().and_then(|v| clean_num(Some(v))),
        Value::String(value) => clean(Some(value.trim().to_string()))
            .and_then(|value| value.parse::<f64>().ok())
            .and_then(|value| clean_num(Some(value))),
        _ => None,
    }
}

fn value_to_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Number(value) => value.as_u64().and_then(|v| u32::try_from(v).ok()),
        Value::String(value) => {
            clean(Some(value.trim().to_string())).and_then(|value| value.parse::<u32>().ok())
        }
        _ => None,
    }
}

fn air_quality_from_value(value: Value) -> Option<AirQuality> {
    let map = value_object(value)?;
    let air = AirQuality {
        publish_time: field_string(
            &map,
            &["publish_time", "publishTime", "pubtime", "pub_time"],
        ),
        aqi: field_f64(&map, &["aqi", "AQI"]),
        level: field_string(&map, &["level", "aqi_level", "aqiLevel"]),
        category: field_string(&map, &["category", "quality", "text"]),
        primary_pollutant: field_string(
            &map,
            &[
                "primary_pollutant",
                "primaryPollutant",
                "primary",
                "main_pollutant",
            ],
        ),
        pm2_5: field_f64(&map, &["pm2_5", "pm25", "pm2.5", "PM2_5", "PM25"]),
        pm10: field_f64(&map, &["pm10", "PM10"]),
        no2: field_f64(&map, &["no2", "NO2"]),
        so2: field_f64(&map, &["so2", "SO2"]),
        co: field_f64(&map, &["co", "CO"]),
        o3: field_f64(&map, &["o3", "O3"]),
    };
    has_air_fields(&air).then_some(air)
}

fn has_air_fields(air: &AirQuality) -> bool {
    air.publish_time.is_some()
        || air.aqi.is_some()
        || air.level.is_some()
        || air.category.is_some()
        || air.primary_pollutant.is_some()
        || air.pm2_5.is_some()
        || air.pm10.is_some()
        || air.no2.is_some()
        || air.so2.is_some()
        || air.co.is_some()
        || air.o3.is_some()
}

fn temperature_chart_from_value(value: Value) -> Option<TemperatureChart> {
    let map = value_object(value)?;
    let chart = TemperatureChart {
        date: field_string(&map, &["date", "time"]),
        max_temperature: field_f64(
            &map,
            &["max_temperature", "maxTemperature", "max_temp", "max"],
        ),
        min_temperature: field_f64(
            &map,
            &["min_temperature", "minTemperature", "min_temp", "min"],
        ),
        day_info: field_string(&map, &["day_info", "dayInfo", "day_weather", "dayWeather"]),
        day_icon: field_string(&map, &["day_icon", "dayIcon", "day_img", "dayImg"]),
        night_info: field_string(
            &map,
            &["night_info", "nightInfo", "night_weather", "nightWeather"],
        ),
        night_icon: field_string(&map, &["night_icon", "nightIcon", "night_img", "nightImg"]),
    };
    has_temperature_chart_fields(&chart).then_some(chart)
}

fn has_temperature_chart_fields(chart: &TemperatureChart) -> bool {
    chart.date.is_some()
        || chart.max_temperature.is_some()
        || chart.min_temperature.is_some()
        || chart.day_info.is_some()
        || chart.day_icon.is_some()
        || chart.night_info.is_some()
        || chart.night_icon.is_some()
}

fn passed_chart_from_value(value: Value) -> Option<PassedWeatherChart> {
    let map = value_object(value)?;
    let chart = PassedWeatherChart {
        time: field_string(&map, &["time", "publish_time", "publishTime"]),
        rain_1h: field_f64(&map, &["rain_1h", "rain1h", "rain1H"]),
        rain_6h: field_f64(&map, &["rain_6h", "rain6h", "rain6H"]),
        rain_12h: field_f64(&map, &["rain_12h", "rain12h", "rain12H"]),
        rain_24h: field_f64(&map, &["rain_24h", "rain24h", "rain24H"]),
        temperature: field_f64(&map, &["temperature", "temp"]),
        temperature_diff: field_f64(&map, &["temperature_diff", "temperatureDiff", "tempDiff"]),
        humidity: field_f64(&map, &["humidity"]),
        pressure: field_f64(&map, &["pressure", "airpressure", "air_pressure"]),
        wind_direction_degree: field_f64(
            &map,
            &["wind_direction_degree", "windDirectionDegree", "winddegree"],
        ),
        wind_speed: field_f64(&map, &["wind_speed", "windSpeed", "windspeed"]),
    };
    has_passed_chart_fields(&chart).then_some(chart)
}

fn has_passed_chart_fields(chart: &PassedWeatherChart) -> bool {
    chart.time.is_some()
        || chart.rain_1h.is_some()
        || chart.rain_6h.is_some()
        || chart.rain_12h.is_some()
        || chart.rain_24h.is_some()
        || chart.temperature.is_some()
        || chart.temperature_diff.is_some()
        || chart.humidity.is_some()
        || chart.pressure.is_some()
        || chart.wind_direction_degree.is_some()
        || chart.wind_speed.is_some()
}

fn climate_from_value(value: Value) -> Option<ClimateSummary> {
    let map = value_object(value)?;
    let months = map
        .get("month")
        .or_else(|| map.get("months"))
        .cloned()
        .map(|value| {
            value_array(Some(value))
                .into_iter()
                .filter_map(climate_month_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let climate = ClimateSummary {
        period: field_string(&map, &["period", "range"]),
        month: months,
    };
    (climate.period.is_some() || !climate.month.is_empty()).then_some(climate)
}

fn climate_month_from_value(value: Value) -> Option<ClimateMonth> {
    let map = value_object(value)?;
    let month = ClimateMonth {
        month: field_u32(&map, &["month", "mon"]),
        average_max_temperature: field_f64(
            &map,
            &[
                "average_max_temperature",
                "avg_max_temperature",
                "max_temperature",
                "maxTemp",
            ],
        ),
        average_min_temperature: field_f64(
            &map,
            &[
                "average_min_temperature",
                "avg_min_temperature",
                "min_temperature",
                "minTemp",
            ],
        ),
        precipitation: field_f64(&map, &["precipitation", "rain", "pre"]),
    };
    (month.month.is_some()
        || month.average_max_temperature.is_some()
        || month.average_min_temperature.is_some()
        || month.precipitation.is_some())
    .then_some(month)
}

fn radar_from_value(value: Value) -> Option<RadarInfo> {
    let map = value_object(value)?;
    let radar = RadarInfo {
        title: field_string(&map, &["title", "name"]),
        image_url: absolutize_nmc_url(field_string(&map, &["image", "image_url", "img"])),
        page_url: absolutize_nmc_url(field_string(&map, &["url", "page_url", "page"])),
    };
    (radar.title.is_some() || radar.image_url.is_some() || radar.page_url.is_some())
        .then_some(radar)
}

fn absolutize_nmc_url(value: Option<String>) -> Option<String> {
    let value = clean(value)?;
    if value.starts_with("http://") || value.starts_with("https://") {
        Some(value)
    } else if value.starts_with("//") {
        Some(format!("https:{value}"))
    } else if value.starts_with('/') {
        Some(format!("{NMC_PUBLIC_BASE_URL}{value}"))
    } else {
        Some(format!("{NMC_PUBLIC_BASE_URL}/{value}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fixture() -> RawWeatherData {
        let json = serde_json::json!({
            "real": {
                "station": { "province": "北京市", "city": "朝阳" },
                "publish_time": "2026-06-29 10:00",
                "weather": {
                    "info": "晴",
                    "temperature": 30.0,
                    "temperatureDiff": 1.2,
                    "humidity": 45.0,
                    "rain": 0.0,
                    "feelst": 32.0,
                    "airpressure": 1001.0,
                    "rcomfort": "较舒适",
                    "icomfort": "3",
                    "img": "0"
                },
                "wind": {
                    "direct": "东北风",
                    "power": "3级",
                    "speed": 5.1,
                    "degree": 45.0
                },
                "warn": {
                    "alert": "高温黄色预警",
                    "province": "北京市",
                    "city": "朝阳",
                    "url": "/publish/alarm/ABJ.html",
                    "pic": "/site4/images/alarm/yellow.png",
                    "pic2": "gaowen",
                    "issuecontent": "注意防暑",
                    "fmeans": "减少户外活动",
                    "signaltype": "高温",
                    "signallevel": "黄色"
                },
                "sunriseSunset": { "sunrise": "04:50", "sunset": "19:47" }
            },
            "predict": {
                "station": { "province": "北京市", "city": "朝阳" },
                "publish_time": "2026-06-29 08:00",
                "detail": [{
                    "date": "2026-06-29",
                    "pt": "2026-06-29 08:00",
                    "day": {
                        "weather": { "info": "晴", "temperature": "34", "img": "0" },
                        "wind": { "direct": "东北风", "power": "3级" }
                    },
                    "night": {
                        "weather": { "info": "多云", "temperature": "24", "img": "1" },
                        "wind": { "direct": "北风", "power": "2级" }
                    },
                    "precipitation": 7.5
                }]
            },
            "air": {
                "publish_time": "2026-06-29 09:00",
                "aqi": 66,
                "level": "二级",
                "category": "良",
                "primary_pollutant": "O3",
                "pm2_5": 22.0,
                "pm10": 55.0,
                "no2": 18.0,
                "so2": 5.0,
                "co": 0.7,
                "o3": 140.0
            },
            "tempchart": [{
                "date": "2026-06-29",
                "max_temperature": 34.0,
                "min_temperature": 24.0,
                "day_info": "晴",
                "day_icon": "0",
                "night_info": "多云",
                "night_icon": "1"
            }],
            "passedchart": [{
                "time": "10:00",
                "rain1h": 0.1,
                "rain6h": 0.2,
                "rain12h": 0.3,
                "rain24h": 0.4,
                "temperature": 30.0,
                "temperatureDiff": 1.2,
                "humidity": 45.0,
                "pressure": 1001.0,
                "windDirectionDegree": 45.0,
                "windSpeed": 5.1
            }],
            "climate": {
                "period": "1991-2020",
                "month": [{
                    "month": 6,
                    "max_temperature": 30.1,
                    "min_temperature": 20.2,
                    "precipitation": 78.9
                }]
            },
            "radar": {
                "title": "华北雷达",
                "image": "/product/SEVP_AOC_RDCP_SLDAS_EBREF_ACHN_L88_PI_20260629100000000.PNG",
                "url": "/publish/radar.html"
            }
        });
        serde_json::from_value(json).expect("fixture should deserialize")
    }

    #[test]
    fn weather_fixture_maps_structured_sections() {
        let snapshot = fixture().into_snapshot();

        let real = snapshot.real.as_ref().expect("real weather");
        assert_eq!(real.temperature_diff, Some(1.2));
        assert_eq!(real.air_pressure, Some(1001.0));
        assert_eq!(real.comfort_label.as_deref(), Some("较舒适"));
        assert_eq!(real.comfort_index.as_deref(), Some("3"));
        assert_eq!(real.weather_icon.as_deref(), Some("0"));
        assert_eq!(real.wind_degree, Some(45.0));
        let alert = real.alert.as_ref().expect("alert");
        assert_eq!(
            alert.url.as_deref(),
            Some("https://www.nmc.cn/publish/alarm/ABJ.html")
        );
        assert_eq!(
            alert.icon_url.as_deref(),
            Some("https://www.nmc.cn/site4/images/alarm/yellow.png")
        );
        assert_eq!(alert.icon_name.as_deref(), Some("gaowen"));

        let day = &snapshot.predict.as_ref().expect("forecast").days[0];
        assert_eq!(day.publish_time.as_deref(), Some("2026-06-29 08:00"));
        assert_eq!(day.day_weather_icon.as_deref(), Some("0"));
        assert_eq!(day.night_weather_icon.as_deref(), Some("1"));
        assert_eq!(day.day_wind_direct.as_deref(), Some("东北风"));
        assert_eq!(day.day_wind_power.as_deref(), Some("3级"));
        assert_eq!(day.night_wind_direct.as_deref(), Some("北风"));
        assert_eq!(day.night_wind_power.as_deref(), Some("2级"));

        let air = snapshot.air.as_ref().expect("air quality");
        assert_eq!(air.aqi, Some(66.0));
        assert_eq!(air.category.as_deref(), Some("良"));
        assert_eq!(air.primary_pollutant.as_deref(), Some("O3"));
        assert_eq!(air.pm2_5, Some(22.0));

        assert_eq!(snapshot.tempchart[0].date.as_deref(), Some("2026-06-29"));
        assert_eq!(snapshot.tempchart[0].max_temperature, Some(34.0));
        assert_eq!(snapshot.passedchart[0].rain_24h, Some(0.4));
        assert_eq!(snapshot.passedchart[0].wind_direction_degree, Some(45.0));
        assert_eq!(
            snapshot.climate.as_ref().unwrap().month[0].precipitation,
            Some(78.9)
        );
        assert_eq!(
            snapshot.radar.as_ref().unwrap().image_url.as_deref(),
            Some(
                "https://www.nmc.cn/product/SEVP_AOC_RDCP_SLDAS_EBREF_ACHN_L88_PI_20260629100000000.PNG"
            )
        );
        assert!(snapshot.debug.is_none());
    }

    #[test]
    fn empty_and_sentinel_values_are_cleaned() {
        let json = serde_json::json!({
            "real": {
                "station": { "province": "北京市", "city": "朝阳" },
                "weather": {
                    "info": "9999",
                    "temperature": 9999.0,
                    "temperatureDiff": 9999.0,
                    "airpressure": 9999.0,
                    "rcomfort": "",
                    "icomfort": "9999",
                    "img": ""
                },
                "wind": { "direct": "", "power": "9999", "speed": 9999.0, "degree": 9999.0 },
                "warn": { "alert": "9999" }
            },
            "air": "",
            "tempchart": [""],
            "passedchart": [{}],
            "climate": "",
            "radar": {}
        });
        let snapshot = serde_json::from_value::<RawWeatherData>(json)
            .expect("fixture should deserialize")
            .into_snapshot();

        let real = snapshot.real.as_ref().expect("real weather");
        assert_eq!(real.info, None);
        assert_eq!(real.temperature, None);
        assert_eq!(real.temperature_diff, None);
        assert_eq!(real.air_pressure, None);
        assert_eq!(real.comfort_label, None);
        assert_eq!(real.comfort_index, None);
        assert_eq!(real.weather_icon, None);
        assert_eq!(real.wind_direct, None);
        assert_eq!(real.wind_power, None);
        assert_eq!(real.wind_speed, None);
        assert_eq!(real.wind_degree, None);
        assert!(real.alert.is_none());
        assert!(snapshot.air.is_none());
        assert!(snapshot.tempchart.is_empty());
        assert!(snapshot.passedchart.is_empty());
        assert!(snapshot.climate.is_none());
        assert!(snapshot.radar.is_none());
    }

    #[test]
    fn numeric_comfort_fields_decode_current_nmc_shape() {
        let json = serde_json::json!({
            "real": {
                "station": { "province": "北京市", "city": "昌平" },
                "weather": {
                    "info": "多云",
                    "rcomfort": 71,
                    "icomfort": 1
                }
            }
        });

        let snapshot = serde_json::from_value::<RawWeatherData>(json)
            .expect("current NMC numeric comfort fields should deserialize")
            .into_snapshot();

        let real = snapshot.real.expect("real weather");
        assert_eq!(real.comfort_label.as_deref(), Some("71"));
        assert_eq!(real.comfort_index.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn include_debug_controls_debug_payload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let body = r#"{"code":0,"msg":"ok","data":{"real":{"station":{"province":"北京市","city":"朝阳"}}}}"#;
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf).await.expect("read request");
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write response");
            }
        });
        let provider = NmcProvider::new(&ProviderConfig {
            name: "nmc".to_string(),
            base_url: format!("http://{addr}/"),
            request_timeout_seconds: 3,
        })
        .expect("provider");

        let without_debug = provider
            .weather("MjXfi", FetchOptions::default())
            .await
            .expect("weather");
        assert!(without_debug.debug.is_none());

        let with_debug = provider
            .weather(
                "MjXfi",
                FetchOptions {
                    include_debug: true,
                },
            )
            .await
            .expect("weather with debug");
        let debug = with_debug.debug.expect("debug payload");
        assert_eq!(debug.provider, "nmc");
        assert_eq!(debug.operation, "weather");
        assert!(debug.endpoint.contains("rest/weather"));
        assert!(debug.endpoint.contains("stationid=MjXfi"));
        assert!(debug.raw_json.contains("\"code\":0"));
    }
}
