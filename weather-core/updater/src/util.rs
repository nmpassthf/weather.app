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

fn short_region_name(value: &str) -> &str {
    value
        .strip_suffix('市')
        .or_else(|| value.strip_suffix('省'))
        .or_else(|| value.strip_suffix("自治区"))
        .or_else(|| value.strip_suffix("特别行政区"))
        .unwrap_or(value)
}
