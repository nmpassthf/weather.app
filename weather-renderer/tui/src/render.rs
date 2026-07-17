use std::fmt::Write as _;

use weather_renderer_common::{local_today, multi_day_datetime_label};
use weather_schema::*;

use crate::{
    presentation::WeatherView,
    util::{degrees, format_index, meter_per_second, mm, percent, text},
};

pub(crate) fn render_status(status: &EngineStatus) -> String {
    let mut out = String::new();
    writeln!(out, "ready: {}", status.ready).ok();
    writeln!(out, "mode: {}", status.mode).ok();
    writeln!(out, "rpc: {}", status.rpc_endpoint).ok();
    writeln!(out, "pub: {}", status.pub_endpoint).ok();
    writeln!(out, "config: {}", status.config_path).ok();
    writeln!(out, "engine version: {}", status.engine_version).ok();
    writeln!(out, "schema version: {}", status.schema_version).ok();
    writeln!(out, "build: {}", status.build_version).ok();
    if let Some(err) = &status.last_config_error {
        writeln!(out, "config error: {err}").ok();
    }
    out.trim_end().to_string()
}

pub(crate) fn render_weather(snapshot: &WeatherSnapshot) -> String {
    let weather = WeatherView::new(snapshot);
    let today = local_today();
    let mut out = String::new();
    if let Some(station) = &snapshot.station {
        writeln!(out, "{}", station_label(station)).ok();
    }
    if snapshot.stale {
        writeln!(out, "数据状态: stale").ok();
    }
    writeln!(out, "{}", "-".repeat(64)).ok();
    if let Some(current) = &weather.current {
        writeln!(out, "实况发布时间: {}", current.publish_time).ok();
        writeln!(
            out,
            "当前天气: {}  气温: {}  体感: {}  湿度: {}  降水: {}  气压: {}  温差: {}",
            current.info,
            current.temperature,
            current.feel_temperature,
            current.humidity,
            current.rain,
            current.observed_pressure,
            current.temperature_diff
        )
        .ok();
        if current.has_comfort {
            writeln!(
                out,
                "舒适度: {}  指数: {}",
                current.comfort_label, current.comfort_index
            )
            .ok();
        }
        writeln!(
            out,
            "风: {} {}  风速: {}  风向角: {}",
            current.wind_direct, current.wind_power, current.wind_speed, current.wind_degree
        )
        .ok();
        writeln!(out, "日出/日落: {} / {}", current.sunrise, current.sunset).ok();
        for (index, alert) in weather.alerts.iter().enumerate() {
            writeln!(out).ok();
            writeln!(
                out,
                "预警 {}（{}）: {}",
                index + 1,
                if alert.inherited {
                    "父级站点"
                } else {
                    "当前站点"
                },
                alert.alert
            )
            .ok();
            if let Some(content) = &alert.issue_content {
                writeln!(out, "内容: {content}").ok();
            }
            if let Some(prevention) = &alert.prevention {
                writeln!(out, "防御: {prevention}").ok();
            }
        }
    }
    if let Some(forecast) = &weather.forecast {
        writeln!(out).ok();
        writeln!(out, "未来预报: {} 发布", forecast.publish_time).ok();
        writeln!(out, "{}", "-".repeat(64)).ok();
        writeln!(
            out,
            "{:<12} {:<12} {:<12} {:<11} {:<14} 降水",
            "日期", "白天", "夜间", "高/低温", "风"
        )
        .ok();
        for day in &forecast.days {
            writeln!(
                out,
                "{:<12} {:<12} {:<12} {:<11} {:<14} {}",
                day.date,
                day.day_info,
                day.night_info,
                format!("{}/{}℃", day.day_temperature, day.night_temperature),
                day.wind,
                day.precipitation
            )
            .ok();
        }
    }
    if let Some(air) = &snapshot.air
        && air_has_data(air)
    {
        writeln!(out).ok();
        writeln!(
            out,
            "空气质量: AQI: {}  等级: {}  类别: {}  首要污染物: {}  发布时间: {}",
            number(air.aqi),
            text(air.level.as_deref()),
            text(air.category.as_deref()),
            text(air.primary_pollutant.as_deref()),
            text(air.publish_time.as_deref())
        )
        .ok();
        writeln!(
            out,
            "污染物: PM2.5 {}  PM10 {}  NO2 {}  SO2 {}  CO {}  O3 {}",
            number(air.pm2_5),
            number(air.pm10),
            number(air.no2),
            number(air.so2),
            number(air.co),
            number(air.o3)
        )
        .ok();
    }
    let passed = snapshot
        .passedchart
        .iter()
        .filter(|chart| passed_chart_has_data(chart))
        .take(6)
        .collect::<Vec<_>>();
    if !passed.is_empty() {
        writeln!(out).ok();
        writeln!(out, "历史观测:").ok();
        for chart in passed {
            writeln!(
                out,
                "{}  温度 {}  湿度 {}  1h雨量 {}  24h雨量 {}  风速 {}",
                multi_day_datetime_label(text(chart.time.as_deref()), today),
                degrees(chart.temperature),
                percent(chart.humidity),
                mm(chart.rain_1h),
                mm(chart.rain_24h),
                meter_per_second(chart.wind_speed)
            )
            .ok();
        }
    }
    if let Some(climate) = &snapshot.climate
        && (!climate.month.is_empty() || climate.period.is_some())
    {
        writeln!(out).ok();
        writeln!(out, "气候常年值: {}", text(climate.period.as_deref())).ok();
        for month in climate.month.iter().take(12) {
            writeln!(
                out,
                "{}月  平均高温 {}  平均低温 {}  降水 {}",
                month
                    .month
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                degrees(month.average_max_temperature),
                degrees(month.average_min_temperature),
                mm(month.precipitation)
            )
            .ok();
        }
    }
    if let Some(radar) = &snapshot.radar
        && (radar.title.is_some() || radar.image_url.is_some() || radar.page_url.is_some())
    {
        writeln!(out).ok();
        writeln!(out, "雷达: {}", text(radar.title.as_deref())).ok();
        if let Some(image_url) = &radar.image_url {
            writeln!(out, "图片: {image_url}").ok();
        }
        if let Some(page_url) = &radar.page_url {
            writeln!(out, "页面: {page_url}").ok();
        }
    }
    out.trim_end().to_string()
}

fn number(value: Option<f64>) -> String {
    match value {
        Some(value) if value.fract().abs() < f64::EPSILON => format!("{value:.0}"),
        Some(value) => format!("{value:.1}"),
        None => "-".to_string(),
    }
}

fn air_has_data(air: &AirQuality) -> bool {
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

fn passed_chart_has_data(chart: &PassedWeatherChart) -> bool {
    chart.time.is_some()
        || chart.rain_1h.is_some()
        || chart.rain_24h.is_some()
        || chart.temperature.is_some()
        || chart.humidity.is_some()
        || chart.wind_speed.is_some()
}

pub(crate) fn render_search_results(resp: &FuzzyMatchStationsResponse) -> String {
    let mut out = String::new();
    let mut index = 1usize;
    for station in &resp.stations {
        writeln!(
            out,
            "[{}] {}\t({} {} {})",
            format_index(index),
            station.name,
            station.province,
            station.city,
            station.unified_uuid
        )
        .ok();
        index += 1;
    }
    for city in &resp.cities {
        writeln!(
            out,
            "[{}] {}-{}\t({})",
            format_index(index),
            city.province,
            city.city,
            city.province
        )
        .ok();
        index += 1;
    }
    for province in &resp.provinces {
        writeln!(out, "[{}] {}", format_index(index), province.name).ok();
        index += 1;
    }
    out.trim_end().to_string()
}

pub(crate) fn render_configured_stations(stations: &[ConfiguredStation]) -> String {
    if stations.is_empty() {
        return "未配置站点。".to_string();
    }

    let mut out = String::new();
    writeln!(out, "配置站点:").ok();
    for (index, station) in stations.iter().enumerate() {
        let state = if station.enabled { "启用" } else { "停用" };
        writeln!(
            out,
            "[{}] {:<4} {}",
            format_index(index + 1),
            state,
            station.name
        )
        .ok();
    }
    out.trim_end().to_string()
}

pub(crate) fn render_station_change(message: &str, stations: &[ConfiguredStation]) -> String {
    let mut out = String::new();
    writeln!(out, "{message}").ok();
    writeln!(out, "engine 已热加载新配置。").ok();
    if !stations.is_empty() {
        writeln!(out).ok();
        writeln!(out, "{}", render_configured_stations(stations)).ok();
    }
    out.trim_end().to_string()
}

pub(crate) fn render_station_candidates(results: &FuzzyMatchStationsResponse) -> String {
    if results.stations.is_empty() {
        return "未命中可写入的站点目标。".to_string();
    }
    let mut out = String::from("候选站点:\n");
    for (index, station) in results.stations.iter().enumerate() {
        writeln!(
            out,
            "[{}] {}\t({} {} {})",
            format_index(index + 1),
            station.name,
            station.province,
            station.city,
            station.unified_uuid
        )
        .ok();
    }
    out.trim_end().to_string()
}

fn station_label(station: &StationRef) -> String {
    let name = if station.name.is_empty() {
        format!("{}-{}", station.province, station.city)
    } else {
        station.name.clone()
    };
    if station.unified_uuid.is_empty() {
        name
    } else {
        format!("{name}({})", station.unified_uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station() -> StationRef {
        StationRef {
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            name: "北京-北京市-朝阳".to_string(),
            unified_uuid: "uuid-1".to_string(),
        }
    }

    #[test]
    fn weather_output_uses_public_uuid() {
        let rendered = render_weather(&WeatherSnapshot {
            station: Some(station()),
            ..Default::default()
        });

        assert!(rendered.contains("北京-北京市-朝阳"));
        assert!(rendered.contains("uuid-1"));
        assert!(!rendered.contains("PROVIDER-ID"));
    }

    #[test]
    fn search_output_hides_internal_identity() {
        let rendered = render_search_results(&FuzzyMatchStationsResponse {
            stations: vec![station()],
            ..Default::default()
        });

        assert!(rendered.contains("北京-北京市-朝阳"));
        assert!(rendered.contains("uuid-1"));
        assert!(!rendered.contains("PROVIDER-ID"));
    }

    #[test]
    fn station_candidate_output_hides_internal_identity() {
        let rendered = render_station_candidates(&FuzzyMatchStationsResponse {
            stations: vec![station()],
            ..Default::default()
        });

        assert!(rendered.contains("北京-北京市-朝阳"));
        assert!(rendered.contains("uuid-1"));
        assert!(!rendered.contains("PROVIDER-ID"));
    }

    #[test]
    fn weather_output_includes_structured_auxiliary_sections() {
        let rendered = render_weather(&WeatherSnapshot {
            station: Some(station()),
            real: Some(ObservedWeather {
                publish_time: Some("2026-06-29 10:00".to_string()),
                info: Some("晴".to_string()),
                temperature: Some(30.0),
                feel_temperature: Some(32.0),
                humidity: Some(45.0),
                rain: Some(0.0),
                wind_direct: Some("东北风".to_string()),
                wind_power: Some("3级".to_string()),
                wind_speed: Some(5.1),
                sunrise: Some("04:50".to_string()),
                sunset: Some("19:47".to_string()),
                alerts: Vec::new(),
                temperature_diff: Some(1.2),
                air_pressure: Some(1001.0),
                comfort_index: Some("3".to_string()),
                comfort_label: Some("较舒适".to_string()),
                weather_icon: Some("0".to_string()),
                wind_degree: Some(45.0),
            }),
            predict: Some(ForecastReport {
                publish_time: Some("2026-06-29 08:00".to_string()),
                days: vec![ForecastDay {
                    date: "2026-06-29".to_string(),
                    day_info: Some("晴".to_string()),
                    night_info: Some("多云".to_string()),
                    day_temperature: Some("34".to_string()),
                    night_temperature: Some("24".to_string()),
                    wind_direct: Some("东北风".to_string()),
                    wind_power: Some("3级".to_string()),
                    precipitation: Some(7.5),
                    publish_time: Some("2026-06-29 08:00".to_string()),
                    day_weather_icon: Some("晴".to_string()),
                    night_weather_icon: Some("多云".to_string()),
                    day_wind_direct: Some("东北风".to_string()),
                    day_wind_power: Some("3级".to_string()),
                    night_wind_direct: Some("北风".to_string()),
                    night_wind_power: Some("2级".to_string()),
                }],
            }),
            air: Some(AirQuality {
                publish_time: Some("2026-06-29 09:00".to_string()),
                aqi: Some(66.0),
                level: Some("二级".to_string()),
                category: Some("良".to_string()),
                primary_pollutant: Some("O3".to_string()),
                pm2_5: Some(22.0),
                pm10: Some(55.0),
                no2: Some(18.0),
                so2: Some(5.0),
                co: Some(0.7),
                o3: Some(140.0),
            }),
            tempchart: vec![TemperatureChart {
                date: Some("2026-06-29".to_string()),
                max_temperature: Some(34.0),
                min_temperature: Some(24.0),
                day_info: Some("晴".to_string()),
                day_icon: Some("晴".to_string()),
                night_info: Some("多云".to_string()),
                night_icon: Some("多云".to_string()),
            }],
            passedchart: vec![PassedWeatherChart {
                time: Some("10:00".to_string()),
                rain_1h: Some(0.1),
                rain_6h: Some(0.2),
                rain_12h: Some(0.3),
                rain_24h: Some(0.4),
                temperature: Some(30.0),
                temperature_diff: Some(1.2),
                humidity: Some(45.0),
                pressure: Some(1001.0),
                wind_direction_degree: Some(45.0),
                wind_speed: Some(5.1),
            }],
            climate: Some(ClimateSummary {
                period: Some("1991-2020".to_string()),
                month: vec![ClimateMonth {
                    month: Some(6),
                    average_max_temperature: Some(30.1),
                    average_min_temperature: Some(20.2),
                    precipitation: Some(78.9),
                }],
            }),
            radar: Some(RadarInfo {
                title: Some("华北雷达".to_string()),
                image_url: Some("https://www.nmc.cn/radar.png".to_string()),
                page_url: Some("https://www.nmc.cn/radar.html".to_string()),
                image_resource_id: None,
            }),
            stale: false,
            debug: None,
        });

        assert!(rendered.contains("气压: 1001hPa"));
        assert!(rendered.contains("舒适度: 较舒适"));
        assert!(rendered.contains("温差: 1.2℃"));
        assert!(rendered.contains("34/24℃"));
        assert!(rendered.contains("东北风3级/北风2级"));
        assert!(rendered.contains("空气质量"));
        assert!(rendered.contains("AQI: 66"));
        assert!(rendered.contains("历史观测"));
        assert!(rendered.contains("10:00"));
        assert!(rendered.contains("气候常年值"));
        assert!(rendered.contains("雷达"));
        assert!(rendered.contains("https://www.nmc.cn/radar.png"));
        assert!(!rendered.contains("raw_json"));
    }

    #[test]
    fn weather_output_keeps_alert_text_unsplit() {
        let rendered = render_weather(&WeatherSnapshot {
            real: Some(ObservedWeather {
                alerts: vec![WeatherAlert {
                    alert: Some("雷电黄色预警".to_string()),
                    issue_content: Some("预计有雷阵雨；伴有阵风".to_string()),
                    prevention: Some("减少户外活动；远离高处".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(
            rendered
                .lines()
                .any(|line| line == "内容: 预计有雷阵雨；伴有阵风")
        );
        assert!(
            rendered
                .lines()
                .any(|line| line == "防御: 减少户外活动；远离高处")
        );
    }
}
