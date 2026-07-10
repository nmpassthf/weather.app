use serde_json::Value;
use weather_schema::{ForecastDay, ForecastReport, ObservedWeather, StationRef, WeatherAlert};

use crate::util::canonical_station_name;

use super::{MappingContext, value};
use crate::provider::nmc::dto::{
    ForecastDayDto, ForecastPartDto, ForecastWeatherDto, PredictDto, RealDto, RealWeatherDto,
    StationDto, SunriseSunsetDto, WarnDto, WindDto,
};

pub(super) struct MappedSection<T> {
    pub(super) station: Option<StationRef>,
    pub(super) value: T,
}

pub(super) fn map_real(
    raw: Option<Value>,
    context: &mut MappingContext<'_>,
) -> Option<MappedSection<ObservedWeather>> {
    let real: RealDto = value::decode_section(raw, "real", context)?;
    let station = map_station(real.station, "real.station", context);
    let weather: Option<RealWeatherDto> =
        value::decode_section(real.weather, "real.weather", context);
    let wind: Option<WindDto> = value::decode_section(real.wind, "real.wind", context);
    let alert = map_alert(real.warn, context);
    let sun: Option<SunriseSunsetDto> =
        value::decode_section(real.sunrise_sunset, "real.sunriseSunset", context);

    let weather = weather.as_ref();
    let wind = wind.as_ref();
    let sun = sun.as_ref();
    Some(MappedSection {
        station,
        value: ObservedWeather {
            publish_time: value::fixed_string(
                real.publish_time.as_ref(),
                "real.publish_time",
                context,
            ),
            info: value::fixed_string(
                weather.and_then(|weather| weather.info.as_ref()),
                "real.weather.info",
                context,
            ),
            temperature: value::fixed_f64(
                weather.and_then(|weather| weather.temperature.as_ref()),
                "real.weather.temperature",
                context,
            ),
            feel_temperature: value::fixed_f64(
                weather.and_then(|weather| weather.feelst.as_ref()),
                "real.weather.feelst",
                context,
            ),
            humidity: value::fixed_f64(
                weather.and_then(|weather| weather.humidity.as_ref()),
                "real.weather.humidity",
                context,
            ),
            rain: value::fixed_f64(
                weather.and_then(|weather| weather.rain.as_ref()),
                "real.weather.rain",
                context,
            ),
            wind_direct: value::fixed_string(
                wind.and_then(|wind| wind.direct.as_ref()),
                "real.wind.direct",
                context,
            ),
            wind_power: value::fixed_string(
                wind.and_then(|wind| wind.power.as_ref()),
                "real.wind.power",
                context,
            ),
            wind_speed: value::fixed_f64(
                wind.and_then(|wind| wind.speed.as_ref()),
                "real.wind.speed",
                context,
            ),
            sunrise: value::fixed_string(
                sun.and_then(|sun| sun.sunrise.as_ref()),
                "real.sunriseSunset.sunrise",
                context,
            ),
            sunset: value::fixed_string(
                sun.and_then(|sun| sun.sunset.as_ref()),
                "real.sunriseSunset.sunset",
                context,
            ),
            alert,
            temperature_diff: value::fixed_f64(
                weather.and_then(|weather| weather.temperature_diff.as_ref()),
                "real.weather.temperatureDiff",
                context,
            ),
            air_pressure: value::fixed_f64(
                weather.and_then(|weather| weather.air_pressure.as_ref()),
                "real.weather.airpressure",
                context,
            ),
            comfort_index: value::fixed_string(
                weather.and_then(|weather| weather.comfort_index.as_ref()),
                "real.weather.icomfort",
                context,
            ),
            comfort_label: value::fixed_string(
                weather.and_then(|weather| weather.comfort_label.as_ref()),
                "real.weather.rcomfort",
                context,
            ),
            weather_icon: value::fixed_string(
                weather.and_then(|weather| weather.weather_icon.as_ref()),
                "real.weather.img",
                context,
            ),
            wind_degree: value::fixed_f64(
                wind.and_then(|wind| wind.degree.as_ref()),
                "real.wind.degree",
                context,
            ),
        },
    })
}

pub(super) fn map_predict(
    raw: Option<Value>,
    context: &mut MappingContext<'_>,
) -> Option<MappedSection<ForecastReport>> {
    let predict: PredictDto = value::decode_section(raw, "predict", context)?;
    let station = map_station(predict.station, "predict.station", context);
    let publish_time = value::fixed_string(
        predict.publish_time.as_ref(),
        "predict.publish_time",
        context,
    );
    let days = value::sequence(predict.detail, "predict.detail", context)
        .into_iter()
        .filter_map(|(index, raw)| map_forecast_day(raw, index, context))
        .collect();
    Some(MappedSection {
        station,
        value: ForecastReport { publish_time, days },
    })
}

fn map_station(
    raw: Option<Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<StationRef> {
    let station: StationDto = value::decode_section(raw, path, context)?;
    let before = context.warning_count();
    let province = value::fixed_string(
        station.province.as_ref(),
        &format!("{path}.province"),
        context,
    );
    let city = value::fixed_string(station.city.as_ref(), &format!("{path}.city"), context);
    let (Some(province), Some(city)) = (province, city) else {
        if context.warning_count() == before {
            context.warn(format!("{path}: incomplete station; ignored"));
        }
        return None;
    };
    let name = canonical_station_name(&province, &city);
    let unified_uuid = weather_schema::unified_station_uuid(&name);
    Some(StationRef {
        province,
        city,
        name,
        unified_uuid,
    })
}

fn map_alert(raw: Option<Value>, context: &mut MappingContext<'_>) -> Option<WeatherAlert> {
    let warning: WarnDto = value::decode_section(raw, "real.warn", context)?;
    let alert = value::fixed_string(warning.alert.as_ref(), "real.warn.alert", context)?;
    Some(WeatherAlert {
        alert: Some(alert),
        province: value::fixed_string(warning.province.as_ref(), "real.warn.province", context),
        city: value::fixed_string(warning.city.as_ref(), "real.warn.city", context),
        url: value::resource_url(warning.url.as_ref(), "real.warn.url", context),
        issue_content: value::fixed_string(
            warning.issuecontent.as_ref(),
            "real.warn.issuecontent",
            context,
        ),
        prevention: value::fixed_string(warning.fmeans.as_ref(), "real.warn.fmeans", context),
        signal_type: value::fixed_string(
            warning.signaltype.as_ref(),
            "real.warn.signaltype",
            context,
        ),
        signal_level: value::fixed_string(
            warning.signallevel.as_ref(),
            "real.warn.signallevel",
            context,
        ),
        icon_url: value::resource_url(warning.pic.as_ref(), "real.warn.pic", context),
        icon_name: value::fixed_string(warning.pic2.as_ref(), "real.warn.pic2", context),
    })
}

fn map_forecast_day(
    raw: Value,
    index: usize,
    context: &mut MappingContext<'_>,
) -> Option<ForecastDay> {
    let path = format!("predict.detail[{index}]");
    let day: ForecastDayDto = value::decode_section(Some(raw), &path, context)?;
    let before = context.warning_count();
    let date = value::fixed_string(day.date.as_ref(), &format!("{path}.date"), context);
    let Some(date) = date else {
        if context.warning_count() == before {
            context.warn(format!("{path}: missing usable date; entry ignored"));
        }
        return None;
    };
    let day_part = map_forecast_part(day.day, &format!("{path}.day"), context);
    let night_part = map_forecast_part(day.night, &format!("{path}.night"), context);
    Some(ForecastDay {
        date,
        day_info: day_part.info,
        night_info: night_part.info,
        day_temperature: day_part.temperature,
        night_temperature: night_part.temperature,
        wind_direct: day_part.wind_direct.clone(),
        wind_power: day_part.wind_power.clone(),
        precipitation: value::fixed_f64(
            day.precipitation.as_ref(),
            &format!("{path}.precipitation"),
            context,
        ),
        publish_time: value::fixed_string(day.pt.as_ref(), &format!("{path}.pt"), context),
        day_weather_icon: day_part.icon,
        night_weather_icon: night_part.icon,
        day_wind_direct: day_part.wind_direct,
        day_wind_power: day_part.wind_power,
        night_wind_direct: night_part.wind_direct,
        night_wind_power: night_part.wind_power,
    })
}

#[derive(Default)]
struct ForecastPart {
    info: Option<String>,
    temperature: Option<String>,
    icon: Option<String>,
    wind_direct: Option<String>,
    wind_power: Option<String>,
}

fn map_forecast_part(
    raw: Option<Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> ForecastPart {
    let Some(part) = value::decode_section::<ForecastPartDto>(raw, path, context) else {
        return ForecastPart::default();
    };
    let weather: Option<ForecastWeatherDto> =
        value::decode_section(part.weather, &format!("{path}.weather"), context);
    let wind: Option<WindDto> = value::decode_section(part.wind, &format!("{path}.wind"), context);
    ForecastPart {
        info: value::fixed_string(
            weather.as_ref().and_then(|weather| weather.info.as_ref()),
            &format!("{path}.weather.info"),
            context,
        ),
        temperature: value::fixed_string(
            weather
                .as_ref()
                .and_then(|weather| weather.temperature.as_ref()),
            &format!("{path}.weather.temperature"),
            context,
        ),
        icon: value::fixed_string(
            weather.as_ref().and_then(|weather| weather.icon.as_ref()),
            &format!("{path}.weather.img"),
            context,
        ),
        wind_direct: value::fixed_string(
            wind.as_ref().and_then(|wind| wind.direct.as_ref()),
            &format!("{path}.wind.direct"),
            context,
        ),
        wind_power: value::fixed_string(
            wind.as_ref().and_then(|wind| wind.power.as_ref()),
            &format!("{path}.wind.power"),
            context,
        ),
    }
}
