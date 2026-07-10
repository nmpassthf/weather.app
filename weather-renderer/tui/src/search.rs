use std::{collections::HashSet, future::Future};

use anyhow::Result;
use weather_schema::*;

use crate::{
    client::EngineClient,
    pagination::{PageCursor, page_size_for_target},
    util::short_region_name,
};

pub(crate) async fn execute_search(
    client: &EngineClient,
    query: Option<&String>,
    province: &Option<String>,
    city: &Option<String>,
    station: &Option<String>,
    limit: u32,
) -> Result<FuzzyMatchStationsResponse> {
    execute_search_with(query, province, city, station, limit, |request| {
        client.request::<FuzzyMatchStationsRequest, FuzzyMatchStationsResponse>(
            RpcKind::FuzzyMatchStations,
            request,
        )
    })
    .await
}

async fn execute_search_with<Fetch, FetchFuture>(
    query: Option<&String>,
    province: &Option<String>,
    city: &Option<String>,
    station: &Option<String>,
    limit: u32,
    mut fetch: Fetch,
) -> Result<FuzzyMatchStationsResponse>
where
    Fetch: FnMut(FuzzyMatchStationsRequest) -> FetchFuture,
    FetchFuture: Future<Output = Result<FuzzyMatchStationsResponse>>,
{
    let raw_search_text = search_text_from_filters(query, province, city, station);
    let (search_text, selector) = split_result_selector(&raw_search_text);
    let provider_query = provider_query_from_search_text(&search_text);
    let target = selector.unwrap_or(limit as usize);
    if target == 0 {
        return Ok(FuzzyMatchStationsResponse::default());
    }

    let mut cursor = PageCursor::default();
    let mut combined = FuzzyMatchStationsResponse::default();
    let mut page_size = page_size_for_target(target);
    loop {
        let (page_offset, bounded_page_size) = cursor.request(page_size)?;
        let mut page = fetch(FuzzyMatchStationsRequest {
            query: provider_query.clone(),
            province: province.clone(),
            page_offset,
            page_size: bounded_page_size,
        })
        .await?;
        let has_more = page.has_more;
        let next_offset = page.next_offset;
        retain_matching_search_results(&mut page, province, city, station, &search_text);
        append_search_results(&mut combined, &mut page);
        dedup_search_results(&mut combined);
        combined.has_more = has_more;
        combined.next_offset = next_offset;

        let can_continue = cursor.advance(has_more, next_offset)?;
        if search_target_reached(&combined, selector, target) || !can_continue {
            break;
        }
        // Once client-side filtering makes another request necessary, scan at
        // the largest legal page size so sparse matches cannot degenerate into
        // thousands of one-item RPCs.
        page_size = MAX_RPC_PAGE_SIZE;
    }

    Ok(finalize_search_results(combined, selector, limit as usize))
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

fn retain_matching_search_results(
    resp: &mut FuzzyMatchStationsResponse,
    province: &Option<String>,
    city: &Option<String>,
    station: &Option<String>,
    search_text: &str,
) {
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
}

fn append_search_results(
    combined: &mut FuzzyMatchStationsResponse,
    page: &mut FuzzyMatchStationsResponse,
) {
    combined.stations.append(&mut page.stations);
    combined.cities.append(&mut page.cities);
    combined.provinces.append(&mut page.provinces);
}

fn search_target_reached(
    response: &FuzzyMatchStationsResponse,
    selector: Option<usize>,
    target: usize,
) -> bool {
    if selector.is_some() {
        response.stations.len() >= target
    } else {
        response
            .stations
            .len()
            .saturating_add(response.cities.len())
            .saturating_add(response.provinces.len())
            >= target
    }
}

fn finalize_search_results(
    mut resp: FuzzyMatchStationsResponse,
    selector: Option<usize>,
    limit: usize,
) -> FuzzyMatchStationsResponse {
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
    use std::{cell::RefCell, collections::VecDeque};

    use super::*;
    use weather_schema::{City, Province};

    fn station(index: usize, city: &str) -> StationRef {
        StationRef {
            province: "province".to_string(),
            city: city.to_string(),
            name: format!("station-{index}"),
            unified_uuid: format!("uuid-{index}"),
        }
    }

    fn station_page(
        range: std::ops::Range<usize>,
        has_more: bool,
        next_offset: u32,
    ) -> FuzzyMatchStationsResponse {
        FuzzyMatchStationsResponse {
            stations: range.map(|index| station(index, "city")).collect(),
            has_more,
            next_offset,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn search_limit_600_is_collected_without_oversized_pages() {
        let pages = RefCell::new(VecDeque::from([
            station_page(0..256, true, 256),
            station_page(256..512, true, 512),
            station_page(512..600, false, 600),
        ]));
        let requests = RefCell::new(Vec::new());

        let response = execute_search_with(None, &None, &None, &None, 600, |request| {
            requests
                .borrow_mut()
                .push((request.page_offset, request.page_size));
            std::future::ready(Ok(pages.borrow_mut().pop_front().unwrap()))
        })
        .await
        .unwrap();

        assert_eq!(response.stations.len(), 600);
        assert_eq!(
            requests.into_inner(),
            vec![
                (0, MAX_RPC_PAGE_SIZE),
                (256, MAX_RPC_PAGE_SIZE),
                (512, MAX_RPC_PAGE_SIZE),
            ]
        );
    }

    #[tokio::test]
    async fn search_selector_can_resolve_across_pages() {
        let pages = RefCell::new(VecDeque::from([
            station_page(0..256, true, 256),
            station_page(256..257, false, 257),
        ]));
        let requests = RefCell::new(Vec::new());
        let query = "target.257".to_string();

        let response = execute_search_with(Some(&query), &None, &None, &None, 10, |request| {
            requests
                .borrow_mut()
                .push((request.page_offset, request.page_size));
            std::future::ready(Ok(pages.borrow_mut().pop_front().unwrap()))
        })
        .await
        .unwrap();

        assert_eq!(response.stations.len(), 1);
        assert_eq!(response.stations[0].name, "station-256");
        assert_eq!(
            requests.into_inner(),
            vec![(0, MAX_RPC_PAGE_SIZE), (256, MAX_RPC_PAGE_SIZE)]
        );
    }

    #[tokio::test]
    async fn search_continues_when_client_filter_removes_the_first_page() {
        let pages = RefCell::new(VecDeque::from([
            FuzzyMatchStationsResponse {
                stations: vec![station(0, "other")],
                has_more: true,
                next_offset: 1,
                ..Default::default()
            },
            FuzzyMatchStationsResponse {
                stations: vec![station(1, "wanted")],
                has_more: false,
                next_offset: 2,
                ..Default::default()
            },
        ]));
        let requests = RefCell::new(Vec::new());
        let city = Some("wanted".to_string());

        let response = execute_search_with(None, &None, &city, &None, 1, |request| {
            requests
                .borrow_mut()
                .push((request.page_offset, request.page_size));
            std::future::ready(Ok(pages.borrow_mut().pop_front().unwrap()))
        })
        .await
        .unwrap();

        assert_eq!(response.stations.len(), 1);
        assert_eq!(response.stations[0].city, "wanted");
        assert_eq!(requests.into_inner(), vec![(0, 1), (1, MAX_RPC_PAGE_SIZE)]);
    }

    #[tokio::test]
    async fn search_rejects_a_non_advancing_server_cursor() {
        let error = execute_search_with(None, &None, &None, &None, 10, |_| {
            std::future::ready(Ok(FuzzyMatchStationsResponse {
                has_more: true,
                next_offset: 0,
                ..Default::default()
            }))
        })
        .await
        .unwrap_err();

        assert!(error.to_string().contains("did not advance"));
    }

    #[tokio::test]
    async fn search_honors_an_early_end_before_the_requested_limit() {
        let requests = RefCell::new(0);

        let response = execute_search_with(None, &None, &None, &None, 600, |_| {
            *requests.borrow_mut() += 1;
            std::future::ready(Ok(station_page(0..3, false, 3)))
        })
        .await
        .unwrap();

        assert_eq!(response.stations.len(), 3);
        assert_eq!(requests.into_inner(), 1);
    }

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
