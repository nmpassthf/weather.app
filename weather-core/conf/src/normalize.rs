use crate::AppConfig;
pub use weather_schema::normalize_station_name;

/// Normalize every configured station in place, retaining order and enabled state.
pub fn normalize_config_stations(config: &mut AppConfig) -> bool {
    let mut changed = false;
    for station in &mut config.stations {
        let normalized = normalize_station_name(&station.name);
        if normalized != station.name {
            station.name = normalized;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StationConfig;

    #[test]
    fn normalizes_all_stations_without_changing_order_or_state() {
        let mut config = AppConfig {
            stations: vec![
                StationConfig {
                    name: " 北京 -  - 朝阳 ".to_string(),
                    enabled: false,
                },
                StationConfig {
                    name: "湖北-湖北省-武汉".to_string(),
                    enabled: true,
                },
            ],
            ..Default::default()
        };

        assert!(normalize_config_stations(&mut config));
        assert_eq!(config.stations[0].name, "北京-朝阳");
        assert!(!config.stations[0].enabled);
        assert_eq!(config.stations[1].name, "湖北-湖北省-武汉");
        assert!(config.stations[1].enabled);
        assert!(!normalize_config_stations(&mut config));
    }
}
