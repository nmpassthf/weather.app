use prost::Message;
use weather_schema::*;

use crate::{handlers::RpcFailure, runtime::Engine};

impl Engine {
    pub(crate) fn status(
        &self,
        mode: &str,
        rpc_endpoint: &str,
        pub_endpoint: &str,
    ) -> EngineStatus {
        self.lifecycle_status(
            mode,
            rpc_endpoint,
            pub_endpoint,
            LifecycleState::Ready,
            None,
        )
    }

    pub(crate) fn lifecycle_status(
        &self,
        mode: &str,
        rpc_endpoint: &str,
        pub_endpoint: &str,
        lifecycle_state: LifecycleState,
        message: Option<String>,
    ) -> EngineStatus {
        EngineStatus {
            ready: lifecycle_state == LifecycleState::Ready,
            mode: mode.to_string(),
            rpc_endpoint: rpc_endpoint.to_string(),
            pub_endpoint: pub_endpoint.to_string(),
            config_path: self.config_path.display().to_string(),
            last_config_error: self.config.last_error(),
            message,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: SCHEMA_VERSION.to_string(),
            build_version: env!("BUILD_VERSION").to_string(),
            instance_id: self.launch.instance_id.clone(),
            lifecycle_state: lifecycle_state as i32,
        }
    }

    pub(crate) fn configured_stations_page(
        &self,
        offset: usize,
        page_size: usize,
    ) -> ListConfiguredStationsResponse {
        let all = self
            .config
            .get()
            .stations
            .iter()
            .map(|station| ConfiguredStation {
                name: station.name.clone(),
                enabled: station.enabled,
            })
            .collect::<Vec<_>>();
        let (stations, has_more, next_offset) =
            paginate(&all, offset, page_size, |slice| slice.to_vec());
        ListConfiguredStationsResponse {
            stations,
            has_more,
            next_offset,
        }
    }

    pub(crate) fn ok(&self, request_id: &str, payload: impl Message) -> RpcResponse {
        self.build_response(request_id, ResponseStatus::Ok, payload, None)
    }

    pub(crate) fn accepted(&self, request_id: &str, payload: impl Message) -> RpcResponse {
        self.build_response(request_id, ResponseStatus::Accepted, payload, None)
    }

    fn build_response(
        &self,
        request_id: &str,
        status: ResponseStatus,
        payload: impl Message,
        error: Option<EngineError>,
    ) -> RpcResponse {
        let mut response = RpcResponse {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: request_id.to_string(),
            status: status as i32,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
            hmac_sha256: Vec::new(),
            payload: payload.encode_to_vec(),
            error,
        };
        if let Some(sig) = self.response_signature(&response) {
            response.hmac_sha256 = sig;
        }
        response
    }

    pub(crate) fn rpc_error_response(
        request_id: &str,
        code: RpcErrorCode,
        message: impl Into<String>,
    ) -> RpcResponse {
        let failure = RpcFailure::new(code, message).into_engine_error();
        RpcResponse {
            schema_version: SCHEMA_VERSION.to_string(),
            request_id: request_id.to_string(),
            status: ResponseStatus::Error as i32,
            timestamp_unix_ms: unix_timestamp_ms().unwrap_or_default(),
            hmac_sha256: Vec::new(),
            payload: Vec::new(),
            error: Some(failure),
        }
    }

    fn response_signature(&self, response: &RpcResponse) -> Option<Vec<u8>> {
        let config = self.config.get();
        let key = weather_configure::resolve_hmac_key(&config).ok()??;
        weather_schema::rpc_response_hmac(response, &key).ok()
    }
}

/// 对 `all` 切片按 `offset` / `page_size` 分页，返回 `(页内元素, has_more, next_offset)`。
///
/// `collect` 用于把切片转换为最终容器，避免重复 clone 逻辑。
pub(crate) fn paginate<T: Clone>(
    all: &[T],
    offset: usize,
    page_size: usize,
    collect: impl FnOnce(&[T]) -> Vec<T>,
) -> (Vec<T>, bool, u32) {
    let total = all.len();
    let start = offset.min(total);
    let end = start.saturating_add(page_size).min(total);
    let slice = &all[start..end];
    let has_more = end < total;
    let next_offset = end as u32;
    (collect(slice), has_more, next_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_vec<T: Clone>(slice: &[T]) -> Vec<T> {
        slice.to_vec()
    }

    #[test]
    fn paginate_returns_first_page_with_more() {
        let items = vec![1, 2, 3, 4, 5];
        let (page, has_more, next) = paginate(&items, 0, 2, collect_vec);
        assert_eq!(page, vec![1, 2]);
        assert!(has_more);
        assert_eq!(next, 2);
    }

    #[test]
    fn paginate_returns_last_page_without_more() {
        let items = vec![1, 2, 3, 4, 5];
        let (page, has_more, next) = paginate(&items, 4, 10, collect_vec);
        assert_eq!(page, vec![5]);
        assert!(!has_more);
        assert_eq!(next, 5);
    }

    #[test]
    fn paginate_clamps_offset_beyond_end() {
        let items = vec![1, 2, 3];
        let (page, has_more, next) = paginate(&items, 10, 2, collect_vec);
        assert!(page.is_empty());
        assert!(!has_more);
        assert_eq!(next, 3);
    }

    #[test]
    fn paginate_handles_empty_input() {
        let items: Vec<i32> = vec![];
        let (page, has_more, next) = paginate(&items, 0, 5, collect_vec);
        assert!(page.is_empty());
        assert!(!has_more);
        assert_eq!(next, 0);
    }

    #[test]
    fn paginate_zero_page_size_yields_empty_page() {
        let items = vec![1, 2, 3];
        let (page, has_more, next) = paginate(&items, 0, 0, collect_vec);
        assert!(page.is_empty());
        assert!(has_more);
        assert_eq!(next, 0);
    }
}
