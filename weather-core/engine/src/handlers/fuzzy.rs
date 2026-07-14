use std::{cmp::Ordering, collections::HashSet};

use anyhow::{Result, bail};
use weather_db::{ProviderCity, ProviderProvince, ProviderStation};
use weather_schema::*;

use crate::{
    catalog::ProviderCatalog,
    handlers::response::paginate,
    limits::{DEFAULT_FUZZY_PAGE_SIZE, normalize_pagination},
    runtime::Engine,
    station::{city_to_provider_station, station_names},
};

#[derive(Clone)]
enum SearchCandidate {
    Station(ProviderStation),
    City(ProviderCity),
    Province(ProviderProvince),
}

#[derive(Hash, PartialEq, Eq)]
enum SearchCandidateKey {
    Station(String),
    City(String, String),
    Province(String),
}

impl Engine {
    pub(super) async fn handle_fuzzy(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<FuzzyMatchStationsRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                decoded.unwrap_err().to_string(),
            );
        };
        let (offset, page_size) =
            match normalize_pagination(req.page_offset, req.page_size, DEFAULT_FUZZY_PAGE_SIZE) {
                Ok(page) => page,
                Err(err) => {
                    return Self::rpc_error_response(
                        &request.request_id,
                        RpcErrorCode::BadRequest,
                        err,
                    );
                }
            };
        match self.fuzzy(req, offset, page_size).await {
            Ok(resp) => self.ok(&request.request_id, resp),
            Err(err) => {
                Self::rpc_error_response(&request.request_id, RpcErrorCode::Fuzzy, err.to_string())
            }
        }
    }

    async fn fuzzy(
        &self,
        req: FuzzyMatchStationsRequest,
        offset: usize,
        page_size: usize,
    ) -> Result<FuzzyMatchStationsResponse> {
        let catalog = match req.province.as_deref() {
            Some(province) => self.provider_catalog_for_province(province).await?,
            None => self.provider_catalog().await?.as_ref().clone(),
        };
        let candidates =
            search_candidates(self.provider.provider_name(), &catalog, req.query.as_str());
        let (page, has_more, next_offset) =
            paginate(&candidates, offset, page_size, |slice| slice.to_vec());

        let mut provider_stations = Vec::new();
        let mut cities = Vec::new();
        let mut provinces = Vec::new();
        for candidate in page {
            match candidate {
                SearchCandidate::Station(station) => provider_stations.push(station),
                SearchCandidate::City(city) => cities.push(city.public_ref()),
                SearchCandidate::Province(province) => provinces.push(province.public_ref()),
            }
        }
        for station in &provider_stations {
            self.cache_station_mapping(station).await?;
        }
        let stations = provider_stations
            .into_iter()
            .map(|station| station.public_ref())
            .collect();

        Ok(FuzzyMatchStationsResponse {
            stations,
            cities,
            provinces,
            has_more,
            next_offset,
        })
    }

    pub(super) async fn resolve_station_name_from_targeted_index(
        &self,
        name: &str,
    ) -> Result<ProviderStation> {
        let province_hint = name.split('-').next().unwrap_or(name);
        let provinces = self.provider_provinces().await?;
        let matched_provinces = provinces
            .iter()
            .filter(|province| province_matches_hint(province, province_hint, name))
            .cloned()
            .collect::<Vec<_>>();
        let cities = if matched_provinces.is_empty() {
            self.provider_catalog_from_provinces(provinces)
                .await?
                .cities
                .clone()
        } else {
            self.provider_catalog_for_provinces(matched_provinces)
                .await?
                .cities
        };

        let mut matches = cities
            .iter()
            .map(|city| {
                city_to_provider_station(
                    self.provider.provider_name(),
                    &city.provider_province_code,
                    city,
                )
            })
            .filter(|station| {
                station_names(station)
                    .iter()
                    .any(|candidate| candidate == name)
            })
            .collect::<Vec<_>>();
        matches.sort_by(station_stable_cmp);
        let Some(station) = matches.into_iter().next() else {
            bail!("station `{name}` was not found in provider station index");
        };
        self.cache_station_mapping(&station).await?;
        Ok(station)
    }

    async fn cache_station_mapping(&self, station: &ProviderStation) -> Result<()> {
        for name in station_names(station) {
            let mut station = station.clone();
            station.display_name = name;
            self.db.put_provider_station_mapping(station).await?;
        }
        Ok(())
    }
}

fn search_candidates(
    provider_name: &str,
    catalog: &ProviderCatalog,
    query: &str,
) -> Vec<SearchCandidate> {
    let mut candidates = Vec::new();
    for city in catalog
        .cities
        .iter()
        .filter(|city| city_matches(city, query))
    {
        candidates.push(SearchCandidate::Station(city_to_provider_station(
            provider_name,
            &city.provider_province_code,
            city,
        )));
        candidates.push(SearchCandidate::City(city.clone()));
    }
    candidates.extend(
        catalog
            .provinces
            .iter()
            .filter(|province| province.name.contains(query))
            .cloned()
            .map(SearchCandidate::Province),
    );

    candidates.sort_by(|left, right| candidate_cmp(left, right, query));
    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate_key(candidate)));
    candidates
}

fn city_matches(city: &ProviderCity, query: &str) -> bool {
    city.city.contains(query)
        || city.province.contains(query)
        || canonical_station_name(&city.province, &city.city).contains(query)
}

fn candidate_cmp(left: &SearchCandidate, right: &SearchCandidate, query: &str) -> Ordering {
    candidate_kind_rank(left)
        .cmp(&candidate_kind_rank(right))
        .then_with(|| candidate_match_rank(left, query).cmp(&candidate_match_rank(right, query)))
        .then_with(|| candidate_stable_cmp(left, right))
}

fn candidate_kind_rank(candidate: &SearchCandidate) -> u8 {
    match candidate {
        SearchCandidate::Station(_) => 0,
        SearchCandidate::City(_) => 1,
        SearchCandidate::Province(_) => 2,
    }
}

fn candidate_match_rank(candidate: &SearchCandidate, query: &str) -> u8 {
    match candidate {
        SearchCandidate::Station(station) => {
            match_rank(&[&station.name, &station.province, &station.city], query)
        }
        SearchCandidate::City(city) => {
            let canonical = canonical_station_name(&city.province, &city.city);
            match_rank(&[&city.city, &city.province, &canonical], query)
        }
        SearchCandidate::Province(province) => match_rank(&[&province.name], query),
    }
}

fn match_rank(values: &[&str], query: &str) -> u8 {
    values
        .iter()
        .map(|value| {
            if *value == query {
                0
            } else if value.starts_with(query) {
                1
            } else {
                2
            }
        })
        .min()
        .unwrap_or(2)
}

fn candidate_stable_cmp(left: &SearchCandidate, right: &SearchCandidate) -> Ordering {
    match (left, right) {
        (SearchCandidate::Station(left), SearchCandidate::Station(right)) => {
            station_stable_cmp(left, right)
        }
        (SearchCandidate::City(left), SearchCandidate::City(right)) => left
            .province
            .cmp(&right.province)
            .then_with(|| left.city.cmp(&right.city))
            .then_with(|| {
                left.provider_province_code
                    .cmp(&right.provider_province_code)
            })
            .then_with(|| left.provider_code.cmp(&right.provider_code)),
        (SearchCandidate::Province(left), SearchCandidate::Province(right)) => left
            .name
            .cmp(&right.name)
            .then_with(|| left.provider_code.cmp(&right.provider_code)),
        _ => Ordering::Equal,
    }
}

fn station_stable_cmp(left: &ProviderStation, right: &ProviderStation) -> Ordering {
    left.name
        .cmp(&right.name)
        .then_with(|| left.province.cmp(&right.province))
        .then_with(|| left.city.cmp(&right.city))
        .then_with(|| left.provider_station_id.cmp(&right.provider_station_id))
}

fn candidate_key(candidate: &SearchCandidate) -> SearchCandidateKey {
    match candidate {
        SearchCandidate::Station(station) => {
            SearchCandidateKey::Station(station.unified_uuid.clone())
        }
        SearchCandidate::City(city) => {
            SearchCandidateKey::City(city.province.clone(), city.city.clone())
        }
        SearchCandidate::Province(province) => SearchCandidateKey::Province(province.name.clone()),
    }
}

fn province_matches_hint(province: &ProviderProvince, hint: &str, name: &str) -> bool {
    province.name == hint
        || short_region_name(&province.name) == hint
        || province.name.contains(hint)
        || name.starts_with(&province.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn city(code: &str, scope: &str) -> ProviderCity {
        ProviderCity {
            provider_code: code.to_string(),
            provider_province_code: scope.to_string(),
            province: "Same省".to_string(),
            city: "SameCity".to_string(),
            url: format!("/{code}"),
        }
    }

    #[test]
    fn candidates_deduplicate_public_identities_after_stable_sorting() {
        let catalog = ProviderCatalog {
            provinces: vec![
                ProviderProvince {
                    provider_code: "B".to_string(),
                    name: "Same省".to_string(),
                    url: "/B".to_string(),
                },
                ProviderProvince {
                    provider_code: "A".to_string(),
                    name: "Same省".to_string(),
                    url: "/A".to_string(),
                },
            ],
            cities: vec![city("B1", "B"), city("A1", "A")],
        };

        let candidates = search_candidates("provider", &catalog, "");

        assert_eq!(candidates.len(), 3);
        match &candidates[0] {
            SearchCandidate::Station(station) => assert_eq!(station.provider_station_id, "A1"),
            _ => panic!("first candidate was not a station"),
        }
        match &candidates[1] {
            SearchCandidate::City(city) => assert_eq!(city.provider_code, "A1"),
            _ => panic!("second candidate was not a city"),
        }
        match &candidates[2] {
            SearchCandidate::Province(province) => assert_eq!(province.provider_code, "A"),
            _ => panic!("third candidate was not a province"),
        }
    }
}
