use anyhow::Result;
use weather_db::{ProviderCity, ProviderProvince};
use weather_schema::*;

use crate::{handlers::response::paginate, runtime::Engine};

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
        match self.list_provinces().await {
            Ok(provinces) => {
                let page_size = normalize_page_size(req.page_size);
                let offset = req.page_offset as usize;
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
        let existing = self.db.get_provider_provinces().await?;
        if !existing.is_empty() {
            return Ok(existing);
        }
        let provinces = self
            .updater
            .provinces()
            .await?
            .into_iter()
            .map(db_provider_province)
            .collect::<Vec<_>>();
        self.db.put_provider_provinces(provinces.clone()).await?;
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
        match self.list_cities(&req.province).await {
            Ok(cities) => {
                let page_size = normalize_page_size(req.page_size);
                let offset = req.page_offset as usize;
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
        self.db
            .resolve_provider_province_code(province.to_string())
            .await
    }

    pub(super) async fn provider_cities_by_code(
        &self,
        provider_province_code: &str,
    ) -> Result<Vec<ProviderCity>> {
        let existing = self
            .db
            .get_provider_cities(provider_province_code.to_string())
            .await?;
        if !existing.is_empty() {
            return Ok(existing);
        }
        let cities = self
            .updater
            .cities(provider_province_code)
            .await?
            .into_iter()
            .map(db_provider_city)
            .collect::<Vec<_>>();
        self.db
            .put_provider_cities(provider_province_code.to_string(), cities.clone())
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
        let page_size = normalize_page_size(req.page_size);
        let page = self.configured_stations_page(req.page_offset as usize, page_size);
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

/// 将客户端传入的 `page_size` 归一化：0 视为默认 32。
pub(crate) fn normalize_page_size(raw: u32) -> usize {
    if raw == 0 { 32 } else { raw as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_page_size_defaults_to_32() {
        assert_eq!(normalize_page_size(0), 32);
    }

    #[test]
    fn nonzero_page_size_passes_through() {
        assert_eq!(normalize_page_size(1), 1);
        assert_eq!(normalize_page_size(100), 100);
    }
}
