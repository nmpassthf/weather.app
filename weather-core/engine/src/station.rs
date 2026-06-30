use weather_db::{ProviderCity, ProviderProvince, ProviderStation};
use weather_schema::StationRef;

pub(crate) fn merge_station(
    upstream: Option<StationRef>,
    requested: &ProviderStation,
) -> StationRef {
    let mut station = upstream.unwrap_or_else(|| requested.public_ref());
    if station.province.is_empty() {
        station.province = requested.province.clone();
    }
    if station.city.is_empty() {
        station.city = requested.city.clone();
    }
    if station.name.is_empty() {
        station.name = requested.name.clone();
    }
    if station.unified_uuid.is_empty() {
        station.unified_uuid = requested.unified_uuid.clone();
    }
    station
}

pub(crate) fn city_to_provider_station(
    provider_name: &str,
    provider_province_code: &str,
    city: &ProviderCity,
) -> ProviderStation {
    let name = canonical_station_name(&city.province, &city.city);
    ProviderStation {
        provider_name: provider_name.to_string(),
        display_name: name.clone(),
        provider_station_id: city.provider_code.clone(),
        provider_province_code: provider_province_code.to_string(),
        province: city.province.clone(),
        city: city.city.clone(),
        url: city.url.clone(),
        unified_uuid: weather_schema::unified_station_uuid(&name),
        name,
    }
}

pub(crate) fn push_matching_provinces(
    target: &mut Vec<ProviderProvince>,
    source: &[ProviderProvince],
    hint: &str,
) {
    for province in source.iter().filter(|province| {
        province.name == hint
            || short_region_name(&province.name) == hint
            || province.name.contains(hint)
            || hint.contains(short_region_name(&province.name))
    }) {
        if !target
            .iter()
            .any(|item| item.provider_code == province.provider_code)
        {
            target.push(province.clone());
        }
    }
}

pub(crate) fn station_names(station: &ProviderStation) -> Vec<String> {
    vec![canonical_station_name(&station.province, &station.city)]
}

pub(crate) fn canonical_station_name(province: &str, city: &str) -> String {
    let region = short_region_name(province);
    let city_level = province;
    let core = short_region_name(city_level);
    if city == core || city == province {
        format!("{region}-{city_level}")
    } else {
        format!("{region}-{city_level}-{city}")
    }
}

pub(crate) fn short_region_name(value: &str) -> &str {
    value
        .strip_suffix('市')
        .or_else(|| value.strip_suffix('省'))
        .or_else(|| value.strip_suffix("自治区"))
        .or_else(|| value.strip_suffix("特别行政区"))
        .unwrap_or(value)
}

pub(crate) fn normalize_station_name(name: &str) -> String {
    name.split('-')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_region_name_strips_suffixes() {
        assert_eq!(short_region_name("北京市"), "北京");
        assert_eq!(short_region_name("湖北省"), "湖北");
        assert_eq!(short_region_name("广西壮族自治区"), "广西壮族");
        assert_eq!(short_region_name("香港特别行政区"), "香港");
        assert_eq!(short_region_name("重庆"), "重庆");
    }

    #[test]
    fn canonical_name_omits_redundant_city() {
        // city 等于 province 短名时退化为 `<region>-<province>`。
        assert_eq!(canonical_station_name("北京市", "北京"), "北京-北京市");
        assert_eq!(canonical_station_name("北京市", "朝阳"), "北京-北京市-朝阳");
    }

    #[test]
    fn station_names_returns_single_canonical() {
        let station = StationRef {
            province: "北京市".to_string(),
            city: "朝阳".to_string(),
            name: String::new(),
            unified_uuid: String::new(),
        };
        assert_eq!(
            station_names(&ProviderStation {
                provider_name: "nmc".to_string(),
                display_name: station.name.clone(),
                provider_station_id: "X".to_string(),
                provider_province_code: "ABJ".to_string(),
                province: station.province,
                city: station.city,
                url: String::new(),
                name: station.name,
                unified_uuid: station.unified_uuid,
            }),
            vec!["北京-北京市-朝阳".to_string()]
        );
    }

    #[test]
    fn normalize_station_name_drops_empty_parts() {
        assert_eq!(normalize_station_name(" 北京 -  - 朝阳 "), "北京-朝阳");
        assert_eq!(normalize_station_name("北京-北京市"), "北京-北京市");
    }
}
