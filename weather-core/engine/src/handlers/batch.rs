use weather_schema::*;

use crate::{
    handlers::catalog::normalize_page_size, handlers::response::paginate, runtime::Engine,
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
        let mut pages = Vec::with_capacity(req.queries.len());
        for query in req.queries {
            let page = self
                .fetch_region_page(
                    &query.province,
                    query.page_offset as usize,
                    normalize_page_size(query.page_size),
                )
                .await;
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
