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

pub(crate) fn clean(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty() && v != "9999")
}

pub(crate) fn clean_num(value: Option<f64>) -> Option<f64> {
    value.filter(|v| (*v - 9999.0).abs() > f64::EPSILON)
}
