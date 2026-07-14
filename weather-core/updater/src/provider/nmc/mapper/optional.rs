use serde_json::{Map, Value};
use weather_schema::{AirQuality, ClimateMonth, ClimateSummary, RadarInfo};

use super::{MappingContext, value};

pub(super) fn map_air_quality(
    raw: Option<Value>,
    context: &mut MappingContext<'_>,
) -> Option<AirQuality> {
    let map = value::section_object(raw, "air", context)?;
    let before = context.warning_count();
    let air = AirQuality {
        publish_time: value::alias_string(
            &map,
            &["publish_time", "publishTime", "pubtime", "pub_time"],
            "air.publish_time",
            context,
        ),
        aqi: value::alias_f64(&map, &["aqi", "AQI"], "air.aqi", context),
        level: value::alias_string(
            &map,
            &["level", "aqi_level", "aqiLevel"],
            "air.level",
            context,
        ),
        category: value::alias_string(
            &map,
            &["category", "quality", "text"],
            "air.category",
            context,
        ),
        primary_pollutant: value::alias_string(
            &map,
            &[
                "primary_pollutant",
                "primaryPollutant",
                "primary",
                "main_pollutant",
            ],
            "air.primary_pollutant",
            context,
        ),
        pm2_5: value::alias_f64(
            &map,
            &["pm2_5", "pm25", "pm2.5", "PM2_5", "PM25"],
            "air.pm2_5",
            context,
        ),
        pm10: value::alias_f64(&map, &["pm10", "PM10"], "air.pm10", context),
        no2: value::alias_f64(&map, &["no2", "NO2"], "air.no2", context),
        so2: value::alias_f64(&map, &["so2", "SO2"], "air.so2", context),
        co: value::alias_f64(&map, &["co", "CO"], "air.co", context),
        o3: value::alias_f64(&map, &["o3", "O3"], "air.o3", context),
    };
    if has_air_fields(&air) {
        Some(air)
    } else {
        warn_if_unusable("air", &map, before, context);
        None
    }
}

pub(super) fn map_climate(
    raw: Option<Value>,
    context: &mut MappingContext<'_>,
) -> Option<ClimateSummary> {
    let map = value::section_object(raw, "climate", context)?;
    let before = context.warning_count();
    let period = value::alias_string(&map, &["period", "range"], "climate.period", context);
    let month = value::alias_sequence(&map, &["month", "months"], "climate.month", context)
        .into_iter()
        .filter_map(|(index, raw)| map_climate_month(raw, index, context))
        .collect::<Vec<_>>();
    if period.is_some() || !month.is_empty() {
        Some(ClimateSummary { period, month })
    } else {
        warn_if_unusable("climate", &map, before, context);
        None
    }
}

pub(super) fn map_radar(raw: Option<Value>, context: &mut MappingContext<'_>) -> Option<RadarInfo> {
    let map = value::section_object(raw, "radar", context)?;
    let before = context.warning_count();
    let radar = RadarInfo {
        title: value::alias_string(&map, &["title", "name"], "radar.title", context),
        image_url: value::alias_resource_url(
            &map,
            &["image", "image_url", "img"],
            "radar.image_url",
            context,
        ),
        page_url: value::alias_resource_url(
            &map,
            &["url", "page_url", "page"],
            "radar.page_url",
            context,
        ),
    };
    if radar.title.is_some() || radar.image_url.is_some() || radar.page_url.is_some() {
        Some(radar)
    } else {
        warn_if_unusable("radar", &map, before, context);
        None
    }
}

fn map_climate_month(
    raw: Value,
    index: usize,
    context: &mut MappingContext<'_>,
) -> Option<ClimateMonth> {
    let path = format!("climate.month[{index}]");
    let map = value::section_object(Some(raw), &path, context)?;
    let before = context.warning_count();
    let month = ClimateMonth {
        month: value::alias_u32(&map, &["month", "mon"], &format!("{path}.month"), context),
        average_max_temperature: value::alias_f64(
            &map,
            &[
                "average_max_temperature",
                "avg_max_temperature",
                "max_temperature",
                "maxTemp",
            ],
            &format!("{path}.average_max_temperature"),
            context,
        ),
        average_min_temperature: value::alias_f64(
            &map,
            &[
                "average_min_temperature",
                "avg_min_temperature",
                "min_temperature",
                "minTemp",
            ],
            &format!("{path}.average_min_temperature"),
            context,
        ),
        precipitation: value::alias_f64(
            &map,
            &["precipitation", "rain", "pre"],
            &format!("{path}.precipitation"),
            context,
        ),
    };
    if month.month.is_some()
        || month.average_max_temperature.is_some()
        || month.average_min_temperature.is_some()
        || month.precipitation.is_some()
    {
        Some(month)
    } else {
        warn_if_unusable(&path, &map, before, context);
        None
    }
}

pub(super) fn warn_if_unusable(
    path: &str,
    map: &Map<String, Value>,
    warning_count_before: usize,
    context: &mut MappingContext<'_>,
) {
    if context.warning_count() == warning_count_before && value::has_non_missing_values(map) {
        context.warn(format!("{path}: no usable known fields; ignored"));
    }
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
