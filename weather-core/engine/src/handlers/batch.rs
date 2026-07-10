use weather_schema::*;

use crate::{
    handlers::response::paginate,
    limits::{DEFAULT_PAGE_SIZE, normalize_pagination, validate_batch_size},
    runtime::Engine,
};

impl Engine {
    pub(super) async fn handle_batch_list_regions(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<BatchListRegionsRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        if let Err(err) = validate_batch_size(req.queries.len()) {
            return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err);
        }
        let mut queries = Vec::with_capacity(req.queries.len());
        for query in req.queries {
            let (offset, page_size) =
                match normalize_pagination(query.page_offset, query.page_size, DEFAULT_PAGE_SIZE) {
                    Ok(page) => page,
                    Err(err) => {
                        return Self::rpc_error_response(&request.request_id, "BAD_REQUEST", err);
                    }
                };
            queries.push((query.province, offset, page_size));
        }
        let mut pages = Vec::with_capacity(queries.len());
        for (province, offset, page_size) in queries {
            let page = self.fetch_region_page(&province, offset, page_size).await;
            pages.push(page);
        }
        self.ok(&request.request_id, BatchListRegionsResponse { pages })
    }

    async fn fetch_region_page(
        &self,
        province: &str,
        offset: usize,
        page_size: usize,
    ) -> BatchRegionPage {
        match self.list_cities(province).await {
            Ok(cities) => {
                let (cities, has_more, next_offset) =
                    paginate(&cities, offset, page_size, |slice| slice.to_vec());
                BatchRegionPage {
                    province: province.to_string(),
                    cities,
                    has_more,
                    next_offset,
                    error: None,
                }
            }
            Err(err) => BatchRegionPage {
                province: province.to_string(),
                cities: Vec::new(),
                has_more: false,
                next_offset: offset as u32,
                error: Some(err.to_string()),
            },
        }
    }
}
