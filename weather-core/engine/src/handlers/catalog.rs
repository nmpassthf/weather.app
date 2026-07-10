use anyhow::Result;
use weather_db::{ProviderCity, ProviderProvince};
use weather_schema::*;

use crate::{
    handlers::response::paginate,
    limits::{DEFAULT_PAGE_SIZE, normalize_pagination},
    runtime::Engine,
};

impl Engine {
    pub(super) async fn handle_list_provinces(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<ListProvincesRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let (offset, page_size) =
            match normalize_pagination(req.page_offset, req.page_size, DEFAULT_PAGE_SIZE) {
                Ok(page) => page,
                Err(err) => {
                    return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err);
                }
            };
        match self.list_provinces().await {
            Ok(provinces) => {
                let (provinces, has_more, next_offset) =
                    paginate(&provinces, offset, page_size, |slice| slice.to_vec());
                self.ok(
                    &request.request_id,
                    ListProvincesResponse {
                        provinces,
                        has_more,
                        next_offset,
                    },
                )
            }
            Err(err) => Self::rpc_error_response(&request.request_id, "UPDATER", err.to_string()),
        }
    }

    pub(super) async fn list_provinces(&self) -> Result<Vec<Province>> {
        Ok(self
            .provider_provinces()
            .await?
            .into_iter()
            .map(|province| province.public_ref())
            .collect())
    }

    pub(super) async fn provider_provinces(&self) -> Result<Vec<ProviderProvince>> {
        let provider = self.provider.provider_name();
        if let Some(cache) = self.db.get_provider_provinces(provider).await? {
            return Ok(cache.items);
        }
        let provinces = self
            .provider
            .provinces()
            .await?
            .into_iter()
            .map(db_provider_province)
            .collect::<Vec<_>>();
        self.db
            .replace_provider_provinces(provider, provinces.clone())
            .await?;
        self.db
            .log_fetch(None, "rest/province/all".to_string(), true, None)
            .await?;
        self.publish_fetch_log(None, "rest/province/all", true, None);
        Ok(provinces)
    }

    pub(super) async fn handle_list_cities(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<ListCitiesRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let (offset, page_size) =
            match normalize_pagination(req.page_offset, req.page_size, DEFAULT_PAGE_SIZE) {
                Ok(page) => page,
                Err(err) => {
                    return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err);
                }
            };
        match self.list_cities(&req.province).await {
            Ok(cities) => {
                let (cities, has_more, next_offset) =
                    paginate(&cities, offset, page_size, |slice| slice.to_vec());
                self.ok(
                    &request.request_id,
                    ListCitiesResponse {
                        cities,
                        has_more,
                        next_offset,
                    },
                )
            }
            Err(err) => Self::rpc_error_response(&request.request_id, "UPDATER", err.to_string()),
        }
    }

    pub(super) async fn list_cities(&self, province: &str) -> Result<Vec<City>> {
        Ok(self
            .provider_cities_by_province_name(province)
            .await?
            .into_iter()
            .map(|city| city.public_ref())
            .collect())
    }

    pub(super) async fn provider_cities_by_province_name(
        &self,
        province: &str,
    ) -> Result<Vec<ProviderCity>> {
        let provider_province_code = self.resolve_provider_province_code(province).await?;
        self.provider_cities_by_code(&provider_province_code).await
    }

    pub(super) async fn resolve_provider_province_code(&self, province: &str) -> Result<String> {
        let _ = self.provider_provinces().await?;
        let provider = self.provider.provider_name();
        self.db
            .resolve_provider_province_code(provider, province)
            .await
    }

    pub(super) async fn provider_cities_by_code(
        &self,
        provider_province_code: &str,
    ) -> Result<Vec<ProviderCity>> {
        let provider = self.provider.provider_name();
        if let Some(cache) = self
            .db
            .get_provider_cities(provider, provider_province_code)
            .await?
        {
            return Ok(cache.items);
        }
        let cities = self
            .provider
            .cities(provider_province_code)
            .await?
            .into_iter()
            .map(db_provider_city)
            .collect::<Vec<_>>();
        self.db
            .replace_provider_cities(provider, provider_province_code, cities.clone())
            .await?;
        self.db
            .log_fetch(
                None,
                format!("rest/province/{provider_province_code}"),
                true,
                None,
            )
            .await?;
        self.publish_fetch_log(
            None,
            &format!("rest/province/{provider_province_code}"),
            true,
            None,
        );
        Ok(cities)
    }

    pub(super) async fn handle_list_configured_stations(
        &self,
        request: &RpcRequest,
    ) -> RpcResponse {
        let decoded = decode_message::<ListConfiguredStationsRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let (offset, page_size) =
            match normalize_pagination(req.page_offset, req.page_size, DEFAULT_PAGE_SIZE) {
                Ok(page) => page,
                Err(err) => {
                    return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err);
                }
            };
        let page = self.configured_stations_page(offset, page_size);
        self.ok(&request.request_id, page)
    }
}

fn db_provider_province(province: weather_updater::ProviderProvince) -> ProviderProvince {
    ProviderProvince {
        provider_code: province.provider_code,
        name: province.name,
        url: province.url,
    }
}

fn db_provider_city(city: weather_updater::ProviderCity) -> ProviderCity {
    ProviderCity {
        provider_code: city.provider_code,
        provider_province_code: city.provider_province_code,
        province: city.province,
        city: city.city,
        url: city.url,
    }
}
