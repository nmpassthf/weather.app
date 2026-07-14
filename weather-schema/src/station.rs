/// Canonicalize a user-facing station path while preserving its components.
pub fn normalize_station_name(name: &str) -> String {
    name.split('-')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Remove one supported Chinese administrative suffix without trimming or
/// otherwise changing the input.
pub fn short_region_name(value: &str) -> &str {
    value
        .strip_suffix('市')
        .or_else(|| value.strip_suffix('省'))
        .or_else(|| value.strip_suffix("自治区"))
        .or_else(|| value.strip_suffix("特别行政区"))
        .unwrap_or(value)
}

/// Build the stable public station path used by providers, the database and
/// engine requests.
pub fn canonical_station_name(province: &str, city: &str) -> String {
    let region = short_region_name(province);
    let city_level = province;
    let core = short_region_name(city_level);
    if city == core || city == province {
        format!("{region}-{city_level}")
    } else {
        format!("{region}-{city_level}-{city}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_path_normalization_keeps_exact_component_semantics() {
        assert_eq!(normalize_station_name(" 北京 -  - 朝阳 "), "北京-朝阳");
        assert_eq!(
            normalize_station_name("\u{3000}湖北-湖北省-武汉\u{3000}"),
            "湖北-湖北省-武汉"
        );
        assert_eq!(normalize_station_name("---"), "");
        assert_eq!(normalize_station_name("南宁"), "南宁");
    }

    #[test]
    fn short_region_name_preserves_suffix_and_whitespace_rules() {
        assert_eq!(short_region_name("北京市"), "北京");
        assert_eq!(short_region_name("湖北省"), "湖北");
        assert_eq!(short_region_name("广西壮族自治区"), "广西壮族");
        assert_eq!(short_region_name("香港特别行政区"), "香港");
        assert_eq!(short_region_name("重庆"), "重庆");
        assert_eq!(short_region_name("市"), "");
        assert_eq!(short_region_name("北京市 "), "北京市 ");
    }

    #[test]
    fn canonical_station_name_keeps_redundant_city_rules() {
        assert_eq!(canonical_station_name("北京市", "北京"), "北京-北京市");
        assert_eq!(canonical_station_name("北京市", "北京市"), "北京-北京市");
        assert_eq!(canonical_station_name("北京市", "朝阳"), "北京-北京市-朝阳");
        assert_eq!(
            canonical_station_name("广西壮族自治区", "南宁"),
            "广西壮族-广西壮族自治区-南宁"
        );
        assert_eq!(canonical_station_name("", ""), "-");
    }
}
