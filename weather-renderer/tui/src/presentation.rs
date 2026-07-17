use chrono::NaiveDate;
use weather_renderer_common::{local_today, multi_day_date_label};
use weather_schema::{ForecastDay, TemperatureChart, WeatherAlert, WeatherSnapshot};

use crate::util::{degrees, hectopascal, meter_per_second, mm, percent, text, wind_summary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WeatherView {
    pub current: Option<CurrentWeatherView>,
    pub forecast: Option<ForecastView>,
    pub alerts: Vec<AlertView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentWeatherView {
    pub publish_time: String,
    pub info: String,
    pub temperature: String,
    pub high_temperature: Option<String>,
    pub low_temperature: Option<String>,
    pub feel_temperature: String,
    pub humidity: String,
    pub rain: String,
    pub observed_pressure: String,
    pub pressure_with_history_fallback: String,
    pub temperature_diff: String,
    pub comfort_label: String,
    pub comfort_index: String,
    pub has_comfort: bool,
    pub wind_direct: String,
    pub wind_power: String,
    pub wind: String,
    pub wind_speed: String,
    pub wind_degree: String,
    pub sunrise: String,
    pub sunset: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForecastView {
    pub publish_time: String,
    pub days: Vec<ForecastDayView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForecastDayView {
    pub date: String,
    pub day_info: String,
    pub night_info: String,
    pub day_temperature: String,
    pub night_temperature: String,
    pub wind: String,
    pub precipitation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlertView {
    pub alert: String,
    pub inherited: bool,
    pub issue_content: Option<String>,
    pub prevention: Option<String>,
    pub lines: Vec<String>,
}

impl WeatherView {
    pub(crate) fn new(snapshot: &WeatherSnapshot) -> Self {
        Self::new_at(snapshot, local_today())
    }

    fn new_at(snapshot: &WeatherSnapshot, calendar_today: NaiveDate) -> Self {
        let forecast_today = snapshot
            .predict
            .as_ref()
            .and_then(|predict| predict.days.first());
        let temperature_chart = matching_temperature_chart(snapshot, forecast_today);
        let current = snapshot.real.as_ref().map(|real| {
            let high_temperature = forecast_today
                .map(|day| forecast_degrees(day.day_temperature.as_deref()))
                .filter(|value| value != "-")
                .or_else(|| {
                    temperature_chart
                        .and_then(|chart| chart.max_temperature.map(|value| degrees(Some(value))))
                });
            let low_temperature = forecast_today
                .map(|day| forecast_degrees(day.night_temperature.as_deref()))
                .filter(|value| value != "-")
                .or_else(|| {
                    temperature_chart
                        .and_then(|chart| chart.min_temperature.map(|value| degrees(Some(value))))
                });
            let pressure = real
                .air_pressure
                .or_else(|| snapshot.passedchart.iter().find_map(|chart| chart.pressure));

            CurrentWeatherView {
                publish_time: text(real.publish_time.as_deref()).to_string(),
                info: text(real.info.as_deref()).to_string(),
                temperature: degrees(real.temperature),
                high_temperature,
                low_temperature,
                feel_temperature: degrees(real.feel_temperature),
                humidity: percent(real.humidity),
                rain: mm(real.rain),
                observed_pressure: hectopascal(real.air_pressure),
                pressure_with_history_fallback: hectopascal(pressure),
                temperature_diff: degrees(real.temperature_diff),
                comfort_label: text(real.comfort_label.as_deref()).to_string(),
                comfort_index: text(real.comfort_index.as_deref()).to_string(),
                has_comfort: real.comfort_label.is_some() || real.comfort_index.is_some(),
                wind_direct: text(real.wind_direct.as_deref()).to_string(),
                wind_power: text(real.wind_power.as_deref()).to_string(),
                wind: wind_summary(real.wind_direct.as_deref(), real.wind_power.as_deref()),
                wind_speed: meter_per_second(real.wind_speed),
                wind_degree: real
                    .wind_degree
                    .map(|value| format!("{value:.0}°"))
                    .unwrap_or_else(|| "-".to_string()),
                sunrise: text(real.sunrise.as_deref()).to_string(),
                sunset: text(real.sunset.as_deref()).to_string(),
            }
        });
        let forecast = snapshot.predict.as_ref().map(|predict| ForecastView {
            publish_time: text(predict.publish_time.as_deref()).to_string(),
            days: predict
                .days
                .iter()
                .map(|day| ForecastDayView::new(day, calendar_today))
                .collect(),
        });
        let alerts = snapshot
            .real
            .as_ref()
            .map(|real| real.alerts.iter().map(AlertView::new).collect())
            .unwrap_or_default();

        Self {
            current,
            forecast,
            alerts,
        }
    }
}

impl ForecastDayView {
    fn new(day: &ForecastDay, today: NaiveDate) -> Self {
        Self {
            date: multi_day_date_label(&day.date, today),
            day_info: text(day.day_info.as_deref()).to_string(),
            night_info: text(day.night_info.as_deref()).to_string(),
            day_temperature: text(day.day_temperature.as_deref()).to_string(),
            night_temperature: text(day.night_temperature.as_deref()).to_string(),
            wind: forecast_wind_summary(day),
            precipitation: mm(day.precipitation),
        }
    }
}

impl AlertView {
    fn new(alert: &WeatherAlert) -> Self {
        let title = text(alert.alert.as_deref()).to_string();
        let signal_level = text(alert.signal_level.as_deref()).to_string();
        let source = if alert.inherited {
            "父级站点"
        } else {
            "当前站点"
        };
        let mut lines = vec![format!("[{source}] {title} {signal_level}")];
        lines.extend(split_alert_lines(alert.issue_content.as_deref()));
        lines.extend(split_alert_lines(alert.prevention.as_deref()));

        Self {
            alert: title,
            inherited: alert.inherited,
            issue_content: alert.issue_content.clone(),
            prevention: alert.prevention.clone(),
            lines,
        }
    }
}

fn forecast_wind_summary(day: &ForecastDay) -> String {
    let day_wind = wind_summary(
        day.day_wind_direct.as_deref(),
        day.day_wind_power.as_deref(),
    );
    let night_wind = wind_summary(
        day.night_wind_direct.as_deref(),
        day.night_wind_power.as_deref(),
    );
    if day_wind == "-" && night_wind == "-" {
        wind_summary(day.wind_direct.as_deref(), day.wind_power.as_deref())
    } else if night_wind == "-" || day_wind == night_wind {
        day_wind
    } else {
        format!("{day_wind}/{night_wind}")
    }
}

fn matching_temperature_chart<'a>(
    snapshot: &'a WeatherSnapshot,
    today: Option<&ForecastDay>,
) -> Option<&'a TemperatureChart> {
    let today_key = today.map(|day| date_key(&day.date));
    if let Some(today_key) = today_key {
        snapshot
            .tempchart
            .iter()
            .find(|chart| chart.date.as_deref().map(date_key).as_deref() == Some(&today_key))
            .or_else(|| snapshot.tempchart.first())
    } else {
        snapshot.tempchart.first()
    }
}

fn date_key(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

fn forecast_degrees(value: Option<&str>) -> String {
    let value = text(value);
    if value == "-" || value.ends_with("℃") {
        value.to_string()
    } else {
        format!("{value}℃")
    }
}

fn split_alert_lines(value: Option<&str>) -> Vec<String> {
    text(value)
        .split('；')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use weather_schema::{
        ForecastReport, ObservedWeather, PassedWeatherChart, TemperatureChart, WeatherAlert,
    };

    use super::*;

    #[test]
    fn forecast_wind_prefers_day_and_night_then_legacy_values() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        let combined = ForecastDayView::new(
            &ForecastDay {
                day_wind_direct: Some("North".to_string()),
                day_wind_power: Some("3".to_string()),
                night_wind_direct: Some("West".to_string()),
                night_wind_power: Some("2".to_string()),
                ..Default::default()
            },
            today,
        );
        assert_eq!(combined.wind, "North3/West2");

        let legacy = ForecastDayView::new(
            &ForecastDay {
                wind_direct: Some("South".to_string()),
                wind_power: Some("1".to_string()),
                ..Default::default()
            },
            today,
        );
        assert_eq!(legacy.wind, "South1");
    }

    #[test]
    fn forecast_dates_use_relative_weekday_and_concrete_labels() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        let snapshot = WeatherSnapshot {
            predict: Some(ForecastReport {
                days: [
                    "2026-07-15",
                    "2026-07-16",
                    "2026-07-17",
                    "2026-07-18",
                    "2026-07-19",
                    "2026-07-24",
                    "2026-07-25",
                ]
                .into_iter()
                .map(|date| ForecastDay {
                    date: date.to_string(),
                    ..Default::default()
                })
                .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let labels = WeatherView::new_at(&snapshot, today)
            .forecast
            .unwrap()
            .days
            .into_iter()
            .map(|day| day.date)
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            [
                "2026-07-15",
                "昨天",
                "今天",
                "明天",
                "星期日",
                "星期五",
                "2026-07-25",
            ]
        );
    }

    #[test]
    fn current_values_fall_back_to_matching_charts_and_history_pressure() {
        let view = WeatherView::new(&WeatherSnapshot {
            real: Some(ObservedWeather {
                temperature: Some(27.0),
                air_pressure: None,
                ..Default::default()
            }),
            predict: Some(ForecastReport {
                days: vec![ForecastDay {
                    date: "2026-06-29".to_string(),
                    night_temperature: Some("21".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            tempchart: vec![TemperatureChart {
                date: Some("2026/06/29".to_string()),
                max_temperature: Some(27.1),
                min_temperature: Some(19.0),
                ..Default::default()
            }],
            passedchart: vec![PassedWeatherChart {
                pressure: Some(998.0),
                ..Default::default()
            }],
            ..Default::default()
        });

        let current = view.current.unwrap();
        assert_eq!(current.high_temperature.as_deref(), Some("27.1℃"));
        assert_eq!(current.low_temperature.as_deref(), Some("21℃"));
        assert_eq!(current.observed_pressure, "-");
        assert_eq!(current.pressure_with_history_fallback, "998hPa");
    }

    #[test]
    fn alert_lines_split_full_width_semicolons_and_keep_raw_text() {
        let issue_content = "预计有雷阵雨；伴有阵风；请注意防范。";
        let prevention = "减少户外活动；远离高处。";
        let view = WeatherView::new(&WeatherSnapshot {
            real: Some(ObservedWeather {
                alerts: vec![WeatherAlert {
                    alert: Some("雷电黄色预警".to_string()),
                    signal_level: Some("黄色".to_string()),
                    issue_content: Some(issue_content.to_string()),
                    prevention: Some(prevention.to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });

        let alert = view.alerts.into_iter().next().unwrap();
        assert_eq!(alert.issue_content.as_deref(), Some(issue_content));
        assert_eq!(alert.prevention.as_deref(), Some(prevention));
        assert_eq!(
            alert.lines,
            vec![
                "[当前站点] 雷电黄色预警 黄色",
                "预计有雷阵雨",
                "伴有阵风",
                "请注意防范。",
                "减少户外活动",
                "远离高处。",
            ]
        );
    }
}
