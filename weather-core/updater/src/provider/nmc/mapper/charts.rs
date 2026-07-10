use serde_json::Value;
use weather_schema::{PassedWeatherChart, TemperatureChart};

use super::{MappingContext, optional::warn_if_unusable, value};

pub(super) fn map_temperature_charts(
    raw: Option<Value>,
    context: &mut MappingContext<'_>,
) -> Vec<TemperatureChart> {
    value::sequence(raw, "tempchart", context)
        .into_iter()
        .filter_map(|(index, raw)| map_temperature_chart(raw, index, context))
        .collect()
}

pub(super) fn map_passed_charts(
    raw: Option<Value>,
    context: &mut MappingContext<'_>,
) -> Vec<PassedWeatherChart> {
    value::sequence(raw, "passedchart", context)
        .into_iter()
        .filter_map(|(index, raw)| map_passed_chart(raw, index, context))
        .collect()
}

fn map_temperature_chart(
    raw: Value,
    index: usize,
    context: &mut MappingContext<'_>,
) -> Option<TemperatureChart> {
    let path = format!("tempchart[{index}]");
    let map = value::section_object(Some(raw), &path, context)?;
    let before = context.warning_count();
    let chart = TemperatureChart {
        date: value::alias_string(&map, &["date", "time"], &format!("{path}.date"), context),
        max_temperature: value::alias_f64(
            &map,
            &["max_temperature", "maxTemperature", "max_temp", "max"],
            &format!("{path}.max_temperature"),
            context,
        ),
        min_temperature: value::alias_f64(
            &map,
            &["min_temperature", "minTemperature", "min_temp", "min"],
            &format!("{path}.min_temperature"),
            context,
        ),
        day_info: value::alias_string(
            &map,
            &["day_info", "dayInfo", "day_weather", "dayWeather"],
            &format!("{path}.day_info"),
            context,
        ),
        day_icon: value::alias_string(
            &map,
            &["day_icon", "dayIcon", "day_img", "dayImg"],
            &format!("{path}.day_icon"),
            context,
        ),
        night_info: value::alias_string(
            &map,
            &["night_info", "nightInfo", "night_weather", "nightWeather"],
            &format!("{path}.night_info"),
            context,
        ),
        night_icon: value::alias_string(
            &map,
            &["night_icon", "nightIcon", "night_img", "nightImg"],
            &format!("{path}.night_icon"),
            context,
        ),
    };
    if has_temperature_chart_fields(&chart) {
        Some(chart)
    } else {
        warn_if_unusable(&path, &map, before, context);
        None
    }
}

fn map_passed_chart(
    raw: Value,
    index: usize,
    context: &mut MappingContext<'_>,
) -> Option<PassedWeatherChart> {
    let path = format!("passedchart[{index}]");
    let map = value::section_object(Some(raw), &path, context)?;
    let before = context.warning_count();
    let chart = PassedWeatherChart {
        time: value::alias_string(
            &map,
            &["time", "publish_time", "publishTime"],
            &format!("{path}.time"),
            context,
        ),
        rain_1h: value::alias_f64(
            &map,
            &["rain_1h", "rain1h", "rain1H"],
            &format!("{path}.rain_1h"),
            context,
        ),
        rain_6h: value::alias_f64(
            &map,
            &["rain_6h", "rain6h", "rain6H"],
            &format!("{path}.rain_6h"),
            context,
        ),
        rain_12h: value::alias_f64(
            &map,
            &["rain_12h", "rain12h", "rain12H"],
            &format!("{path}.rain_12h"),
            context,
        ),
        rain_24h: value::alias_f64(
            &map,
            &["rain_24h", "rain24h", "rain24H"],
            &format!("{path}.rain_24h"),
            context,
        ),
        temperature: value::alias_f64(
            &map,
            &["temperature", "temp"],
            &format!("{path}.temperature"),
            context,
        ),
        temperature_diff: value::alias_f64(
            &map,
            &["temperature_diff", "temperatureDiff", "tempDiff"],
            &format!("{path}.temperature_diff"),
            context,
        ),
        humidity: value::alias_f64(&map, &["humidity"], &format!("{path}.humidity"), context),
        pressure: value::alias_f64(
            &map,
            &["pressure", "airpressure", "air_pressure"],
            &format!("{path}.pressure"),
            context,
        ),
        wind_direction_degree: value::alias_f64(
            &map,
            &["wind_direction_degree", "windDirectionDegree", "winddegree"],
            &format!("{path}.wind_direction_degree"),
            context,
        ),
        wind_speed: value::alias_f64(
            &map,
            &["wind_speed", "windSpeed", "windspeed"],
            &format!("{path}.wind_speed"),
            context,
        ),
    };
    if has_passed_chart_fields(&chart) {
        Some(chart)
    } else {
        warn_if_unusable(&path, &map, before, context);
        None
    }
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
