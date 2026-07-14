mod charts;
mod core;
mod optional;
mod value;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use weather_schema::WeatherSnapshot;

use crate::{ProviderCity, ProviderProvince};

use super::dto::{CityDto, ProvinceDto, WeatherDataDto};
use charts::{map_passed_charts, map_temperature_charts};
use core::{map_predict, map_real};
use optional::{map_air_quality, map_climate, map_radar};
use value::required_resource_url;

pub(super) struct MapOutcome<T> {
    pub(super) value: T,
    pub(super) warnings: Vec<String>,
}

pub(super) struct MappingContext<'a> {
    base_url: &'a Url,
    warnings: Vec<String>,
}

impl<'a> MappingContext<'a> {
    fn new(base_url: &'a Url) -> Self {
        Self {
            base_url,
            warnings: Vec::new(),
        }
    }

    pub(super) fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    pub(super) fn warning_count(&self) -> usize {
        self.warnings.len()
    }
}

pub(super) fn map_weather(data: WeatherDataDto, base_url: &Url) -> MapOutcome<WeatherSnapshot> {
    let mut context = MappingContext::new(base_url);
    let real = map_real(data.real, &mut context);
    let predict = map_predict(data.predict, &mut context);
    let station = real
        .as_ref()
        .and_then(|mapped| mapped.station.clone())
        .or_else(|| predict.as_ref().and_then(|mapped| mapped.station.clone()));
    let snapshot = WeatherSnapshot {
        station,
        real: real.map(|mapped| mapped.value),
        predict: predict.map(|mapped| mapped.value),
        air: map_air_quality(data.air, &mut context),
        tempchart: map_temperature_charts(data.tempchart, &mut context),
        passedchart: map_passed_charts(data.passedchart, &mut context),
        climate: map_climate(data.climate, &mut context),
        radar: map_radar(data.radar, &mut context),
        stale: false,
        debug: None,
    };
    MapOutcome {
        value: snapshot,
        warnings: context.warnings,
    }
}

pub(super) fn map_provinces(
    rows: Vec<ProvinceDto>,
    base_url: &Url,
) -> Result<Vec<ProviderProvince>> {
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            validate_catalog_field(&row.code, "province", index, "code")?;
            validate_catalog_field(&row.name, "province", index, "name")?;
            let url = required_resource_url(base_url, &row.url)
                .with_context(|| format!("invalid NMC province[{index}].url"))?;
            Ok(ProviderProvince {
                provider_code: row.code,
                name: row.name,
                url,
            })
        })
        .collect()
}

pub(super) fn map_cities(
    rows: Vec<CityDto>,
    provider_province_code: &str,
    base_url: &Url,
) -> Result<Vec<ProviderCity>> {
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            validate_catalog_field(&row.code, "city", index, "code")?;
            validate_catalog_field(&row.province, "city", index, "province")?;
            validate_catalog_field(&row.city, "city", index, "city")?;
            let url = required_resource_url(base_url, &row.url)
                .with_context(|| format!("invalid NMC city[{index}].url"))?;
            Ok(ProviderCity {
                provider_code: row.code,
                provider_province_code: provider_province_code.to_string(),
                province: row.province,
                city: row.city,
                url,
            })
        })
        .collect()
}

fn validate_catalog_field(value: &str, kind: &str, index: usize, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim() == "9999" {
        bail!("NMC {kind}[{index}].{field} is empty or sentinel");
    }
    Ok(())
}
