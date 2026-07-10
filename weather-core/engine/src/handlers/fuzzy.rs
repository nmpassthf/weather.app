use anyhow::{Result, bail};
use weather_db::{ProviderCity, ProviderProvince};
use weather_schema::*;

use crate::{
    limits::{DEFAULT_FUZZY_PAGE_SIZE, normalize_pagination},
    runtime::Engine,
    station::{
        canonical_station_name, city_to_provider_station, push_matching_provinces, station_names,
    },
};

impl Engine {
    pub(super) async fn handle_fuzzy(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<FuzzyMatchStationsRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let (offset, page_size) =
            match normalize_pagination(req.page_offset, req.page_size, DEFAULT_FUZZY_PAGE_SIZE) {
                Ok(page) => page,
                Err(err) => {
                    return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err);
                }
            };
        match self.fuzzy(req, offset, page_size).await {
            Ok(resp) => self.ok(&request.request_id, resp),
            Err(err) => Self::rpc_error_response(&request.request_id, "FUZZY", err.to_string()),
        }
    }

    async fn fuzzy(
        &self,
        req: FuzzyMatchStationsRequest,
        offset: usize,
        page_size: usize,
    ) -> Result<FuzzyMatchStationsResponse> {
        let scan_limit = offset.saturating_add(page_size).saturating_add(1);
        let query = req.query;
        let matching_provider_provinces = self
            .provider_provinces()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.name.contains(&query))
            .skip(offset)
            .take(page_size)
            .collect::<Vec<_>>();
        let provinces = matching_provider_provinces
            .into_iter()
            .map(|province| province.public_ref())
            .collect::<Vec<_>>();
        let mut cities = Vec::<ProviderCity>::new();
        if let Some(province) = req.province {
            cities = self
                .provider_cities_by_province_name(&province)
                .await?
                .into_iter()
                .filter(|c| c.city.contains(&query) || c.province.contains(&query))
                .take(scan_limit)
                .collect();
        } else {
            for station in &self.config.get().stations {
                if station.name.contains(&query)
                    && let Ok(resolved) = self.station_by_name(&station.name).await
                {
                    cities.push(ProviderCity {
                        provider_code: resolved.provider_station_id,
                        provider_province_code: resolved.provider_province_code,
                        province: resolved.province,
                        city: resolved.city,
                        url: resolved.url,
                    });
                }
            }
            if cities.len() < scan_limit {
                let provinces = self.prioritized_search_provinces(&query).await?;
                for province in provinces {
                    let province_cities = self
                        .provider_cities_by_code(&province.provider_code)
                        .await?;
                    for city in province_cities.into_iter().filter(|c| {
                        c.city.contains(&query)
                            || c.province.contains(&query)
                            || canonical_station_name(&c.province, &c.city).contains(&query)
                    }) {
                        cities.push(city);
                        if cities.len() >= scan_limit {
                            break;
                        }
                    }
                    if cities.len() >= scan_limit {
                        break;
                    }
                }
            }
        }
        let all_stations = cities
            .iter()
            .map(|city| {
                city_to_provider_station(
                    self.provider.provider_name(),
                    &city.provider_province_code,
                    city,
                )
            })
            .collect::<Vec<_>>();
        let has_more = all_stations.len() > offset.saturating_add(page_size);
        let stations = all_stations
            .into_iter()
            .skip(offset)
            .take(page_size)
            .collect::<Vec<_>>();
        for station in &stations {
            self.cache_station_mapping(station).await?;
        }
        let stations = stations
            .into_iter()
            .map(|station| station.public_ref())
            .collect::<Vec<_>>();
        let cities = cities
            .into_iter()
            .skip(offset)
            .take(page_size)
            .map(|city| city.public_ref())
            .collect::<Vec<_>>();
        Ok(FuzzyMatchStationsResponse {
            stations,
            cities,
            provinces,
            has_more,
            next_offset: offset.saturating_add(page_size) as u32,
        })
    }

    async fn prioritized_search_provinces(&self, query: &str) -> Result<Vec<ProviderProvince>> {
        let provinces = self.provider_provinces().await?;
        let mut ordered = Vec::<ProviderProvince>::new();

        for station in &self.config.get().stations {
            if let Some(region) = station.name.split('-').next() {
                push_matching_provinces(&mut ordered, &provinces, region);
            }
        }

        push_matching_provinces(&mut ordered, &provinces, query);

        for direct_city in ["北京", "上海", "天津", "重庆"] {
            push_matching_provinces(&mut ordered, &provinces, direct_city);
        }

        for province in provinces {
            if !ordered
                .iter()
                .any(|item| item.provider_code == province.provider_code)
            {
                ordered.push(province);
            }
        }
        Ok(ordered)
    }

    pub(super) async fn resolve_station_name_from_targeted_index(
        &self,
        name: &str,
    ) -> Result<weather_db::ProviderStation> {
        let province_hint = name.split('-').next().unwrap_or(name);
        let provinces = self.provider_provinces().await?;
        let mut matched_provinces = provinces
            .iter()
            .filter(|province| {
                province.name == province_hint
                    || province.name.contains(province_hint)
                    || name.starts_with(&province.name)
            })
            .collect::<Vec<_>>();
        if matched_provinces.is_empty() {
            matched_provinces = provinces.iter().collect();
        }

        for province in matched_provinces {
            for city in self
                .provider_cities_by_code(&province.provider_code)
                .await?
            {
                let station = city_to_provider_station(
                    self.provider.provider_name(),
                    &province.provider_code,
                    &city,
                );
                if station_names(&station)
                    .iter()
                    .any(|candidate| candidate == name)
                {
                    self.cache_station_mapping(&station).await?;
                    return Ok(station);
                }
            }
        }
        bail!("station `{name}` was not found in provider station index")
    }

    async fn cache_station_mapping(&self, station: &weather_db::ProviderStation) -> Result<()> {
        for name in station_names(station) {
            let mut station = station.clone();
            station.display_name = name;
            self.db.put_provider_station_mapping(station).await?;
        }
        Ok(())
    }
}
