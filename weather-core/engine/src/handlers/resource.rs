use weather_schema::{
    GetResourceRequest, GetResourceResponse, ResourceTransferState, RpcErrorCode, RpcRequest,
    RpcResponse, decode_message,
};

use crate::{resource_cache::ResourceFetchPlan, runtime::Engine};

const DEFAULT_RESOURCE_CHUNK_BYTES: u32 = 256 * 1024;
const MAX_RESOURCE_CHUNK_BYTES: u32 = 512 * 1024;
const ASYNC_RESOURCE_RETRY_AFTER_MS: u32 = 75;

impl Engine {
    pub(super) async fn handle_get_resource(&self, request: &RpcRequest) -> RpcResponse {
        let req = match decode_message::<GetResourceRequest>(&request.payload) {
            Ok(req) => req,
            Err(error) => {
                return Self::rpc_error_response(
                    &request.request_id,
                    RpcErrorCode::BadRequest,
                    error.to_string(),
                );
            }
        };
        if req.resource_id.trim().is_empty() {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                "resource_id must not be empty",
            );
        }
        let max_bytes = if req.max_bytes == 0 {
            DEFAULT_RESOURCE_CHUNK_BYTES
        } else {
            req.max_bytes
        };
        if max_bytes > MAX_RESOURCE_CHUNK_BYTES {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                format!(
                    "resource max_bytes {max_bytes} exceeds maximum {MAX_RESOURCE_CHUNK_BYTES}"
                ),
            );
        }

        match self.resources.begin_fetch(&req.resource_id) {
            ResourceFetchPlan::Ready(resource) => match resource_chunk(
                &req,
                &resource.content_type,
                resource.bytes.as_ref(),
                max_bytes,
            ) {
                Ok(response) => {
                    log::debug!(
                        "resource cache hit resource_id={} offset={} chunk_bytes={} total_bytes={} complete={}",
                        req.resource_id,
                        req.offset,
                        response.data.len(),
                        response.total_size,
                        response.complete
                    );
                    self.ok(
                        &request.request_id,
                        GetResourceResponse {
                            cache_hit: true,
                            ..response
                        },
                    )
                }
                Err(message) => {
                    log::warn!(
                        "resource chunk request rejected resource_id={} offset={}: {}",
                        req.resource_id,
                        req.offset,
                        message
                    );
                    Self::rpc_error_response(&request.request_id, RpcErrorCode::BadRequest, message)
                }
            },
            ResourceFetchPlan::Start { source_url } => {
                log::debug!(
                    "resource upstream fetch scheduled resource_id={}",
                    req.resource_id
                );
                self.spawn_resource_fetch(req.resource_id.clone(), source_url);
                self.accepted(
                    &request.request_id,
                    pending_resource_response(&req, ASYNC_RESOURCE_RETRY_AFTER_MS),
                )
            }
            ResourceFetchPlan::Pending => {
                log::trace!(
                    "resource fetch still pending resource_id={}",
                    req.resource_id
                );
                self.accepted(
                    &request.request_id,
                    pending_resource_response(&req, ASYNC_RESOURCE_RETRY_AFTER_MS),
                )
            }
            ResourceFetchPlan::Failed(message) => {
                log::warn!(
                    "resource fetch unavailable resource_id={}: {}",
                    req.resource_id,
                    message
                );
                Self::rpc_error_response(&request.request_id, RpcErrorCode::Resource, message)
            }
            ResourceFetchPlan::Missing => {
                log::warn!(
                    "resource request is not registered resource_id={}",
                    req.resource_id
                );
                Self::rpc_error_response(
                    &request.request_id,
                    RpcErrorCode::Resource,
                    format!("resource `{}` is not registered", req.resource_id),
                )
            }
        }
    }

    fn spawn_resource_fetch(&self, resource_id: String, source_url: String) {
        let provider = self.provider.clone();
        let singleflight = self.resource_singleflight.clone();
        let resources = self.resources.clone();
        let cache_resources = resources.clone();
        let cache_resource_id = resource_id.clone();
        let task = tokio::spawn(async move {
            log::debug!("resource upstream fetch started resource_id={resource_id}");
            let fetched = singleflight
                .run(resource_id.clone(), || async move {
                    let resource = provider.resource(&source_url).await?;
                    cache_resources
                        .cache(&cache_resource_id, resource.clone())
                        .map_err(anyhow::Error::msg)?;
                    Ok(resource)
                })
                .await;
            match fetched {
                Ok(resource) => {
                    log::debug!(
                        "resource upstream fetch completed resource_id={} content_type={} bytes={}",
                        resource_id,
                        resource.content_type,
                        resource.bytes.len()
                    );
                    resources.finish_fetch(&resource_id, None);
                }
                Err(error) => {
                    log::warn!(
                        "resource upstream fetch failed resource_id={}: {error:#}",
                        resource_id
                    );
                    resources.finish_fetch(&resource_id, Some(error.to_string()));
                }
            }
        });
        drop(task);
    }
}

fn pending_resource_response(
    request: &GetResourceRequest,
    retry_after_ms: u32,
) -> GetResourceResponse {
    GetResourceResponse {
        resource_id: request.resource_id.clone(),
        content_type: String::new(),
        data: Vec::new(),
        total_size: 0,
        next_offset: request.offset,
        complete: false,
        cache_hit: false,
        transfer_state: ResourceTransferState::Pending as i32,
        retry_after_ms,
    }
}

fn resource_chunk(
    request: &GetResourceRequest,
    content_type: &str,
    bytes: &[u8],
    max_bytes: u32,
) -> Result<GetResourceResponse, String> {
    let total_size = u64::try_from(bytes.len())
        .map_err(|_| "resource length does not fit in u64".to_string())?;
    if request.offset > total_size {
        return Err(format!(
            "resource offset {} exceeds total size {total_size}",
            request.offset
        ));
    }
    let start = usize::try_from(request.offset)
        .map_err(|_| "resource offset does not fit in memory".to_string())?;
    let end = start.saturating_add(max_bytes as usize).min(bytes.len());
    let next_offset =
        u64::try_from(end).map_err(|_| "resource chunk offset does not fit in u64".to_string())?;
    Ok(GetResourceResponse {
        resource_id: request.resource_id.clone(),
        content_type: content_type.to_string(),
        data: bytes[start..end].to_vec(),
        total_size,
        next_offset,
        complete: end == bytes.len(),
        cache_hit: false,
        transfer_state: ResourceTransferState::Ready as i32,
        retry_after_ms: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use prost::Message;
    use tokio::{sync::Semaphore, time::timeout};
    use weather_configure::{AppConfig, write_config_atomic};
    use weather_schema::{ResponseStatus, WeatherSnapshot};
    use weather_updater::{
        ProviderCity, ProviderFuture, ProviderProvince, ProviderResource, WeatherFetch,
        WeatherProvider,
    };

    use super::*;
    use crate::runtime::EngineRuntime;

    fn request(offset: u64, max_bytes: u32) -> GetResourceRequest {
        GetResourceRequest {
            resource_id: "resource-id".to_string(),
            offset,
            max_bytes,
        }
    }

    #[test]
    fn chunks_report_offsets_and_completion() {
        let first = resource_chunk(&request(0, 3), "image/png", b"abcdef", 3).unwrap();
        assert_eq!(first.data, b"abc");
        assert_eq!(first.total_size, 6);
        assert_eq!(first.next_offset, 3);
        assert!(!first.complete);
        assert_eq!(
            ResourceTransferState::try_from(first.transfer_state),
            Ok(ResourceTransferState::Ready)
        );

        let second = resource_chunk(&request(3, 3), "image/png", b"abcdef", 3).unwrap();
        assert_eq!(second.data, b"def");
        assert_eq!(second.next_offset, 6);
        assert!(second.complete);
    }

    #[test]
    fn exact_end_is_an_empty_complete_chunk() {
        let response = resource_chunk(&request(3, 8), "image/png", b"abc", 8).unwrap();
        assert!(response.data.is_empty());
        assert_eq!(response.next_offset, 3);
        assert!(response.complete);
    }

    #[test]
    fn offset_beyond_the_resource_is_rejected() {
        assert!(resource_chunk(&request(4, 8), "image/png", b"abc", 8).is_err());
    }

    #[test]
    fn pending_responses_do_not_advance_the_requested_offset() {
        let response = pending_resource_response(&request(17, 8), 75);
        assert_eq!(response.next_offset, 17);
        assert!(response.data.is_empty());
        assert!(!response.complete);
        assert_eq!(response.retry_after_ms, 75);
        assert_eq!(
            ResourceTransferState::try_from(response.transfer_state),
            Ok(ResourceTransferState::Pending)
        );
    }

    struct ResourceProvider {
        calls: AtomicUsize,
        release: Option<Arc<Semaphore>>,
    }

    impl WeatherProvider for ResourceProvider {
        fn provider_name(&self) -> &str {
            "resource-test"
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<ProviderProvince>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn cities<'a>(
            &'a self,
            _provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<ProviderCity>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn weather<'a>(
            &'a self,
            _provider_station_id: &'a str,
            _include_debug: bool,
        ) -> ProviderFuture<'a, WeatherFetch> {
            Box::pin(async {
                Ok(WeatherFetch {
                    snapshot: WeatherSnapshot::default(),
                    warnings: Vec::new(),
                })
            })
        }

        fn resource<'a>(&'a self, _source_url: &'a str) -> ProviderFuture<'a, ProviderResource> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let release = self.release.clone();
            Box::pin(async move {
                if let Some(release) = release {
                    release.acquire().await.unwrap().forget();
                }
                Ok(ProviderResource {
                    content_type: "image/png".to_string(),
                    bytes: Arc::from(&b"abcdef"[..]),
                })
            })
        }
    }

    async fn resource_engine(
        release: Option<Arc<Semaphore>>,
    ) -> (
        tempfile::TempDir,
        EngineRuntime,
        Engine,
        Arc<ResourceProvider>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let mut config = AppConfig::default();
        config.db.path = directory.path().join("weather.db").display().to_string();
        config.updater.default_provider = "resource-test".to_string();
        config.updater.provider[0].name = "resource-test".to_string();
        write_config_atomic(&config_path, &config).unwrap();
        let provider = Arc::new(ResourceProvider {
            calls: AtomicUsize::new(0),
            release,
        });
        let runtime = EngineRuntime::start_with_provider(config_path, provider.clone())
            .await
            .unwrap();
        let engine = runtime.test_engine();
        (directory, runtime, engine, provider)
    }

    fn rpc_request(resource_id: String) -> RpcRequest {
        RpcRequest {
            schema_version: weather_schema::SCHEMA_VERSION.to_string(),
            request_id: "resource-request".to_string(),
            kind: weather_schema::RpcKind::GetResource as i32,
            timestamp_unix_ms: 0,
            hmac_sha256: Vec::new(),
            payload: GetResourceRequest {
                resource_id,
                offset: 0,
                max_bytes: 3,
            }
            .encode_to_vec(),
        }
    }

    #[tokio::test]
    async fn handler_fetches_once_then_reports_a_cache_hit() {
        let (_directory, _runtime, engine, provider) = resource_engine(None).await;
        let resource_id = engine
            .resources
            .register("https://provider.example/radar.png")
            .unwrap();
        let request = rpc_request(resource_id);

        let first = engine.handle_get_resource(&request).await;
        assert_eq!(first.status, ResponseStatus::Accepted as i32);
        let first: GetResourceResponse = decode_message(&first.payload).unwrap();
        assert_eq!(
            ResourceTransferState::try_from(first.transfer_state),
            Ok(ResourceTransferState::Pending)
        );

        let second = timeout(Duration::from_secs(5), async {
            loop {
                let response = engine.handle_get_resource(&request).await;
                if response.status == ResponseStatus::Ok as i32 {
                    break response;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background resource fetch did not complete");
        let second: GetResourceResponse = decode_message(&second.payload).unwrap();
        assert_eq!(second.data, b"abc");
        assert!(second.cache_hit);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn slow_binary_fetch_returns_pending_without_consuming_rpc_timeout() {
        let release = Arc::new(Semaphore::new(0));
        let (_directory, _runtime, engine, provider) = resource_engine(Some(release.clone())).await;
        let resource_id = engine
            .resources
            .register("https://provider.example/radar.png")
            .unwrap();
        let request = rpc_request(resource_id);

        let pending = timeout(
            Duration::from_millis(100),
            engine.handle_get_resource(&request),
        )
        .await
        .expect("pending response waited for upstream binary data");
        assert_eq!(pending.status, ResponseStatus::Accepted as i32);

        timeout(Duration::from_secs(5), async {
            while provider.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background resource fetch did not start");
        let overlapping = engine.handle_get_resource(&request).await;
        assert_eq!(overlapping.status, ResponseStatus::Accepted as i32);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        release.add_permits(1);
        let ready = timeout(Duration::from_secs(5), async {
            loop {
                let response = engine.handle_get_resource(&request).await;
                if response.status == ResponseStatus::Ok as i32 {
                    break response;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released resource fetch did not become ready");
        let ready: GetResourceResponse = decode_message(&ready.payload).unwrap();
        assert_eq!(ready.data, b"abc");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        engine.db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn handler_rejects_unregistered_resource_ids_without_calling_provider() {
        let (_directory, _runtime, engine, provider) = resource_engine(None).await;

        let response = engine
            .handle_get_resource(&rpc_request("resource-unknown".to_string()))
            .await;

        assert_eq!(response.status, ResponseStatus::Error as i32);
        assert_eq!(
            response.error.unwrap().code,
            RpcErrorCode::Resource.as_str()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        engine.db.shutdown().await.unwrap();
    }
}
