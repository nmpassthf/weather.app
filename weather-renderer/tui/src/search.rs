use std::collections::HashSet;

use anyhow::Result;
use weather_schema::*;

use crate::{client::EngineClient, util::short_region_name};

pub(crate) async fn execute_search(
    client: &EngineClient,
    query: Option<&String>,
    province: &Option<String>,
    city: &Option<String>,
    station: &Option<String>,
    limit: u32,
) -> Result<FuzzyMatchStationsResponse> {
    let raw_search_text = search_text_from_filters(query, province, city, station);
    let (search_text, selector) = split_result_selector(&raw_search_text);
    let provider_query = provider_query_from_search_text(&search_text);
    let request_limit = selector.unwrap_or(limit as usize).max(1) as u32;
    let resp = client
        .request::<FuzzyMatchStationsRequest, FuzzyMatchStationsResponse>(
            RpcKind::FuzzyMatchStations,
            FuzzyMatchStationsRequest {
                query: provider_query,
                province: province.clone(),
                page_offset: 0,
                page_size: request_limit,
            },
        )
        .await?;
    Ok(filter_search_results(
        resp,
        province,
        city,
        station,
        &search_text,
        selector,
        limit as usize,
    ))
}

fn search_text_from_filters(
    query: Option<&String>,
    province: &Option<String>,
    city: &Option<String>,
    station: &Option<String>,
) -> String {
    station
        .as_ref()
        .or(city.as_ref())
        .or(query)
        .or(province.as_ref())
        .cloned()
        .unwrap_or_default()
}

fn filter_search_results(
    mut resp: FuzzyMatchStationsResponse,
    province: &Option<String>,
    city: &Option<String>,
    station: &Option<String>,
    search_text: &str,
    selector: Option<usize>,
    limit: usize,
) -> FuzzyMatchStationsResponse {
    let exact_path = search_text.contains('-').then_some(search_text);
    resp.stations.retain(|item| {
        province
            .as_ref()
            .is_none_or(|value| station_province_matches(item, value))
            && city.as_ref().is_none_or(|value| item.city == *value)
            && station
                .as_ref()
                .is_none_or(|value| station_name_matches(item, value))
            && exact_path.is_none_or(|value| item.name == value)
    });
    resp.cities.retain(|item| {
        if exact_path.is_some() {
            return false;
        }
        province
            .as_ref()
            .is_none_or(|value| city_province_matches(item, value))
            && city.as_ref().is_none_or(|value| item.city == *value)
    });
    resp.provinces.retain(|item| {
        if exact_path.is_some() {
            return false;
        }
        province.as_ref().is_none_or(|value| item.name == *value)
    });
    dedup_search_results(&mut resp);
    if let Some(selector) = selector
        && selector > 0
        && selector <= resp.stations.len()
    {
        let selected = resp.stations[selector - 1].clone();
        resp.stations = vec![selected];
        resp.cities.clear();
        resp.provinces.clear();
        return resp;
    }
    resp.stations.truncate(limit);
    resp.cities
        .truncate(limit.saturating_sub(resp.stations.len()));
    resp.provinces
        .truncate(limit.saturating_sub(resp.stations.len() + resp.cities.len()));
    resp
}

fn split_result_selector(value: &str) -> (String, Option<usize>) {
    let Some((prefix, suffix)) = value.rsplit_once('.') else {
        return (value.to_string(), None);
    };
    match suffix.parse::<usize>() {
        Ok(index) if index > 0 => (prefix.to_string(), Some(index)),
        _ => (value.to_string(), None),
    }
}

fn provider_query_from_search_text(value: &str) -> String {
    value
        .split('-')
        .rfind(|part| !part.is_empty())
        .unwrap_or(value)
        .to_string()
}

fn station_name_matches(station: &StationRef, value: &str) -> bool {
    let (value, _) = split_result_selector(value);
    station.name == value || station.city == value
}

fn station_province_matches(station: &StationRef, value: &str) -> bool {
    station.province == value || short_region_name(&station.province) == value
}

fn city_province_matches(city: &City, value: &str) -> bool {
    city.province == value || short_region_name(&city.province) == value
}

fn dedup_search_results(resp: &mut FuzzyMatchStationsResponse) {
    let mut seen_stations = HashSet::new();
    resp.stations.retain(|item| {
        let public_key = if item.unified_uuid.is_empty() {
            format!("{}|{}|{}", item.name, item.province, item.city)
        } else {
            item.unified_uuid.clone()
        };
        seen_stations.insert(public_key)
    });

    let station_locations = resp
        .stations
        .iter()
        .map(|item| (item.province.clone(), item.city.clone()))
        .collect::<HashSet<_>>();
    let mut seen_cities = HashSet::new();
    resp.cities.retain(|item| {
        let location = (item.province.clone(), item.city.clone());
        !station_locations.contains(&location) && seen_cities.insert(location)
    });

    let mut seen_provinces = HashSet::new();
    resp.provinces
        .retain(|item| seen_provinces.insert(item.name.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather_schema::{City, Province};

    #[test]
    fn split_selector_extracts_trailing_index() {
        assert_eq!(
            split_result_selector("北京.2"),
            ("北京".to_string(), Some(2))
        );
        assert_eq!(
            split_result_selector("北京-朝阳.1"),
            ("北京-朝阳".to_string(), Some(1))
        );
    }

    #[test]
    fn split_selector_returns_none_for_no_index() {
        assert_eq!(split_result_selector("北京"), ("北京".to_string(), None));
        assert_eq!(
            split_result_selector("北京.0"),
            ("北京.0".to_string(), None)
        );
        assert_eq!(
            split_result_selector("北京.abc"),
            ("北京.abc".to_string(), None)
        );
    }

    #[test]
    fn provider_query_takes_last_nonempty_segment() {
        assert_eq!(provider_query_from_search_text("北京-北京市-朝阳"), "朝阳");
        assert_eq!(provider_query_from_search_text("朝阳"), "朝阳");
        assert_eq!(provider_query_from_search_text("北京-"), "北京");
    }

    #[test]
    fn explicit_query_takes_priority_over_province_filter() {
        let query = "朝阳".to_string();
        let province = Some("北京市".to_string());

        assert_eq!(
            search_text_from_filters(Some(&query), &province, &None, &None),
            "朝阳"
        );
    }

    #[test]
    fn dedup_removes_duplicates_and_overlap() {
        let mut resp = FuzzyMatchStationsResponse {
            stations: vec![
                StationRef {
                    province: "北京市".to_string(),
                    city: "朝阳".to_string(),
                    name: "北京-北京市-朝阳".to_string(),
                    unified_uuid: String::new(),
                },
                StationRef {
                    province: "北京市".to_string(),
                    city: "朝阳".to_string(),
                    name: "北京-北京市-朝阳".to_string(),
                    unified_uuid: String::new(),
                },
            ],
            cities: vec![City {
                province: "北京市".to_string(),
                city: "朝阳".to_string(),
            }],
            provinces: vec![
                Province {
                    name: "北京市".to_string(),
                },
                Province {
                    name: "北京市".to_string(),
                },
            ],
            has_more: false,
            next_offset: 0,
        };
        dedup_search_results(&mut resp);
        assert_eq!(resp.stations.len(), 1);
        assert_eq!(resp.cities.len(), 0);
        assert_eq!(resp.provinces.len(), 1);
    }

    #[test]
    fn dedup_uses_unified_uuid_for_stations() {
        let mut resp = FuzzyMatchStationsResponse {
            stations: vec![
                StationRef {
                    province: "北京市".to_string(),
                    city: "朝阳".to_string(),
                    name: "北京-北京市-朝阳".to_string(),
                    unified_uuid: "same-public-uuid".to_string(),
                },
                StationRef {
                    province: "北京市".to_string(),
                    city: "朝阳".to_string(),
                    name: "北京-北京市-朝阳".to_string(),
                    unified_uuid: "same-public-uuid".to_string(),
                },
            ],
            ..Default::default()
        };

        dedup_search_results(&mut resp);

        assert_eq!(resp.stations.len(), 1);
    }
}
