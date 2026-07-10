use anyhow::{Result, bail};
use weather_db::{
    ProviderCity, ProviderProvince, validate_provider_city_catalog,
    validate_provider_province_catalog,
};
use weather_schema::*;

use crate::{
    catalog::{CityCatalogKey, catalog_cache_is_fresh},
    handlers::response::paginate,
    limits::{DEFAULT_PAGE_SIZE, normalize_pagination},
    runtime::Engine,
    time::now_ms,
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
        let provider = self.provider.provider_name().to_string();
        if let Some(cache) = self.db.get_provider_provinces(&provider).await?
            && catalog_cache_is_fresh(
                cache.fetched_at_unix_ms,
                now_ms(),
                self.config.get().updater.province_ttl_seconds,
            )
        {
            return Ok(cache.items);
        }

        let result = self
            .catalog
            .province_flights
            .run(provider.clone(), || async {
                self.refresh_provider_provinces(&provider).await
            })
            .await?;
        Ok(result.as_ref().clone())
    }

    async fn refresh_provider_provinces(&self, provider: &str) -> Result<Vec<ProviderProvince>> {
        let ttl_seconds = self.config.get().updater.province_ttl_seconds;
        let cached = self.db.get_provider_provinces(provider).await?;
        if cached.as_ref().is_some_and(|cache| {
            catalog_cache_is_fresh(cache.fetched_at_unix_ms, now_ms(), ttl_seconds)
        }) {
            return Ok(cached.expect("fresh cache was present").items);
        }
        let stale = cached.map(|cache| cache.items);
        let endpoint = "rest/province/all";
        let fetched = match self.catalog.acquire_upstream_permit().await {
            Ok(permit) => {
                let result = self.provider.provinces().await;
                drop(permit);
                match result {
                    Ok(provinces) => {
                        let provinces = provinces
                            .into_iter()
                            .map(db_provider_province)
                            .collect::<Vec<_>>();
                        validate_provider_province_catalog(&provinces).map(|()| provinces)
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };

        match fetched {
            Ok(mut upstream) => {
                sort_provider_provinces(&mut upstream);
                let (result, warning) = self.persist_provider_provinces(provider, upstream).await;
                self.record_catalog_fetch(endpoint, true, warning.clone(), true, warning)
                    .await;
                Ok(result)
            }
            Err(err) => {
                let message = format!("{err:#}");
                match stale {
                    Some(items) => {
                        self.record_catalog_fetch(
                            endpoint,
                            false,
                            Some(message.clone()),
                            false,
                            Some(format!("using stale catalog after {message}")),
                        )
                        .await;
                        Ok(items)
                    }
                    None => {
                        self.record_catalog_fetch(
                            endpoint,
                            false,
                            Some(message.clone()),
                            false,
                            Some(message),
                        )
                        .await;
                        Err(err)
                    }
                }
            }
        }
    }

    async fn persist_provider_provinces(
        &self,
        provider: &str,
        upstream: Vec<ProviderProvince>,
    ) -> (Vec<ProviderProvince>, Option<String>) {
        if let Err(err) = self
            .db
            .replace_provider_provinces(provider, upstream.clone())
            .await
        {
            return (
                upstream,
                Some(format!("catalog cache write failed: {err:#}")),
            );
        }
        match self.db.get_provider_provinces(provider).await {
            Ok(Some(cache)) => (cache.items, None),
            Ok(None) => (
                upstream,
                Some("catalog cache re-read returned no value after replacement".to_string()),
            ),
            Err(err) => (
                upstream,
                Some(format!("catalog cache re-read failed: {err:#}")),
            ),
        }
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
        let provider = self.provider.provider_name();
        let codes = self
            .provider_provinces()
            .await?
            .into_iter()
            .filter(|candidate| candidate.name == province)
            .map(|candidate| candidate.provider_code)
            .collect::<Vec<_>>();
        match codes.as_slice() {
            [code] => Ok(code.clone()),
            [] => bail!("provider province `{province}` not found for `{provider}`"),
            codes => bail!(
                "provider province `{province}` is ambiguous for `{provider}`: {}",
                codes.join(", ")
            ),
        }
    }

    pub(super) async fn provider_cities_by_code(
        &self,
        provider_province_code: &str,
    ) -> Result<Vec<ProviderCity>> {
        let provider = self.provider.provider_name().to_string();
        if let Some(cache) = self
            .db
            .get_provider_cities(&provider, provider_province_code)
            .await?
            && catalog_cache_is_fresh(
                cache.fetched_at_unix_ms,
                now_ms(),
                self.config.get().updater.province_ttl_seconds,
            )
        {
            return Ok(cache.items);
        }

        let key = CityCatalogKey {
            provider: provider.clone(),
            province_code: provider_province_code.to_string(),
        };
        let result = self
            .catalog
            .city_flights
            .run(key, || async {
                self.refresh_provider_cities(&provider, provider_province_code)
                    .await
            })
            .await?;
        Ok(result.as_ref().clone())
    }

    async fn refresh_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
    ) -> Result<Vec<ProviderCity>> {
        let ttl_seconds = self.config.get().updater.province_ttl_seconds;
        let cached = self
            .db
            .get_provider_cities(provider, provider_province_code)
            .await?;
        if cached.as_ref().is_some_and(|cache| {
            catalog_cache_is_fresh(cache.fetched_at_unix_ms, now_ms(), ttl_seconds)
        }) {
            return Ok(cached.expect("fresh cache was present").items);
        }
        let stale = cached.map(|cache| cache.items);
        let endpoint = format!("rest/province/{provider_province_code}");
        let fetched = match self.catalog.acquire_upstream_permit().await {
            Ok(permit) => {
                let result = self.provider.cities(provider_province_code).await;
                drop(permit);
                match result {
                    Ok(cities) => {
                        let cities = cities.into_iter().map(db_provider_city).collect::<Vec<_>>();
                        validate_provider_city_catalog(provider_province_code, &cities)
                            .map(|()| cities)
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };

        match fetched {
            Ok(mut upstream) => {
                sort_provider_cities(&mut upstream);
                let (result, warning) = self
                    .persist_provider_cities(provider, provider_province_code, upstream)
                    .await;
                self.record_catalog_fetch(&endpoint, true, warning.clone(), true, warning)
                    .await;
                Ok(result)
            }
            Err(err) => {
                let message = format!("{err:#}");
                match stale {
                    Some(items) => {
                        self.record_catalog_fetch(
                            &endpoint,
                            false,
                            Some(message.clone()),
                            false,
                            Some(format!("using stale catalog after {message}")),
                        )
                        .await;
                        Ok(items)
                    }
                    None => {
                        self.record_catalog_fetch(
                            &endpoint,
                            false,
                            Some(message.clone()),
                            false,
                            Some(message),
                        )
                        .await;
                        Err(err)
                    }
                }
            }
        }
    }

    async fn persist_provider_cities(
        &self,
        provider: &str,
        provider_province_code: &str,
        upstream: Vec<ProviderCity>,
    ) -> (Vec<ProviderCity>, Option<String>) {
        if let Err(err) = self
            .db
            .replace_provider_cities(provider, provider_province_code, upstream.clone())
            .await
        {
            return (
                upstream,
                Some(format!("catalog cache write failed: {err:#}")),
            );
        }
        match self
            .db
            .get_provider_cities(provider, provider_province_code)
            .await
        {
            Ok(Some(cache)) => (cache.items, None),
            Ok(None) => (
                upstream,
                Some("catalog cache re-read returned no value after replacement".to_string()),
            ),
            Err(err) => (
                upstream,
                Some(format!("catalog cache re-read failed: {err:#}")),
            ),
        }
    }

    async fn record_catalog_fetch(
        &self,
        endpoint: &str,
        persisted_ok: bool,
        persisted_message: Option<String>,
        event_ok: bool,
        mut event_message: Option<String>,
    ) {
        if let Err(err) = self
            .db
            .log_fetch(None, endpoint.to_string(), persisted_ok, persisted_message)
            .await
        {
            append_warning(
                &mut event_message,
                format!("fetch log write failed: {err:#}"),
            );
        }
        self.publish_fetch_log(None, endpoint, event_ok, event_message);
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

fn sort_provider_provinces(provinces: &mut [ProviderProvince]) {
    provinces.sort_by(|left, right| left.provider_code.cmp(&right.provider_code));
}

fn sort_provider_cities(cities: &mut [ProviderCity]) {
    cities.sort_by(|left, right| {
        left.city
            .cmp(&right.city)
            .then_with(|| left.provider_code.cmp(&right.provider_code))
    });
}

fn append_warning(message: &mut Option<String>, warning: String) {
    match message {
        Some(message) => {
            message.push_str("; ");
            message.push_str(&warning);
        }
        None => *message = Some(warning),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Result, anyhow};
    use rusqlite::Connection;
    use tokio::{sync::Semaphore, time::timeout};
    use weather_configure::{AppConfig, write_config_atomic};
    use weather_schema::{EventEnvelope, FetchLogEvent, WeatherSnapshot, decode_message};
    use weather_updater::{
        ProviderCity as UpstreamCity, ProviderFuture, ProviderProvince as UpstreamProvince,
        WeatherProvider,
    };

    use crate::runtime::EngineRuntime;

    enum Reply<T> {
        Success(Vec<T>),
        Failure(&'static str),
    }

    impl<T> Reply<T> {
        fn into_result(self) -> Result<Vec<T>> {
            match self {
                Self::Success(items) => Ok(items),
                Self::Failure(message) => Err(anyhow!(message)),
            }
        }
    }

    struct CatalogProvider {
        provinces: Mutex<VecDeque<Reply<UpstreamProvince>>>,
        cities: Mutex<HashMap<String, VecDeque<Reply<UpstreamCity>>>>,
        province_calls: AtomicUsize,
        city_calls: Mutex<HashMap<String, usize>>,
        total_city_calls: AtomicUsize,
        active_calls: AtomicUsize,
        peak_active_calls: AtomicUsize,
        gate: Mutex<Option<Arc<Semaphore>>>,
    }

    impl CatalogProvider {
        fn new() -> Self {
            Self {
                provinces: Mutex::new(VecDeque::new()),
                cities: Mutex::new(HashMap::new()),
                province_calls: AtomicUsize::new(0),
                city_calls: Mutex::new(HashMap::new()),
                total_city_calls: AtomicUsize::new(0),
                active_calls: AtomicUsize::new(0),
                peak_active_calls: AtomicUsize::new(0),
                gate: Mutex::new(None),
            }
        }

        fn queue_provinces(&self, reply: Reply<UpstreamProvince>) {
            self.provinces
                .lock()
                .expect("province replies lock")
                .push_back(reply);
        }

        fn queue_cities(&self, province_code: &str, reply: Reply<UpstreamCity>) {
            self.cities
                .lock()
                .expect("city replies lock")
                .entry(province_code.to_string())
                .or_default()
                .push_back(reply);
        }

        fn block_calls(&self) -> Arc<Semaphore> {
            let gate = Arc::new(Semaphore::new(0));
            *self.gate.lock().expect("catalog gate lock") = Some(gate.clone());
            gate
        }

        fn province_calls(&self) -> usize {
            self.province_calls.load(Ordering::SeqCst)
        }

        fn city_calls(&self, province_code: &str) -> usize {
            self.city_calls
                .lock()
                .expect("city calls lock")
                .get(province_code)
                .copied()
                .unwrap_or_default()
        }

        fn begin_call(&self) -> ActiveCall<'_> {
            let active = self.active_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_active_calls.fetch_max(active, Ordering::SeqCst);
            ActiveCall { provider: self }
        }

        async fn wait_on_gate(&self, gate: Option<Arc<Semaphore>>) {
            if let Some(gate) = gate {
                gate.acquire()
                    .await
                    .expect("catalog test gate closed")
                    .forget();
            }
        }
    }

    struct ActiveCall<'a> {
        provider: &'a CatalogProvider,
    }

    impl Drop for ActiveCall<'_> {
        fn drop(&mut self) {
            self.provider.active_calls.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl WeatherProvider for CatalogProvider {
        fn provider_name(&self) -> &str {
            "catalog-test"
        }

        fn provinces(&self) -> ProviderFuture<'_, Vec<UpstreamProvince>> {
            self.province_calls.fetch_add(1, Ordering::SeqCst);
            let reply = self
                .provinces
                .lock()
                .expect("province replies lock")
                .pop_front()
                .unwrap_or(Reply::Failure("unexpected province fetch"));
            let gate = self.gate.lock().expect("catalog gate lock").clone();
            Box::pin(async move {
                let _active = self.begin_call();
                self.wait_on_gate(gate).await;
                reply.into_result()
            })
        }

        fn cities<'a>(
            &'a self,
            provider_province_code: &'a str,
        ) -> ProviderFuture<'a, Vec<UpstreamCity>> {
            self.total_city_calls.fetch_add(1, Ordering::SeqCst);
            *self
                .city_calls
                .lock()
                .expect("city calls lock")
                .entry(provider_province_code.to_string())
                .or_default() += 1;
            let reply = self
                .cities
                .lock()
                .expect("city replies lock")
                .get_mut(provider_province_code)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Reply::Failure("unexpected city fetch"));
            let gate = self.gate.lock().expect("catalog gate lock").clone();
            Box::pin(async move {
                let _active = self.begin_call();
                self.wait_on_gate(gate).await;
                reply.into_result()
            })
        }

        fn weather<'a>(
            &'a self,
            _provider_station_id: &'a str,
            _include_debug: bool,
        ) -> ProviderFuture<'a, WeatherSnapshot> {
            Box::pin(async { Ok(WeatherSnapshot::default()) })
        }
    }

    fn upstream_province(code: &str, name: &str) -> UpstreamProvince {
        UpstreamProvince {
            provider_code: code.to_string(),
            name: name.to_string(),
            url: format!("/{code}"),
        }
    }

    fn upstream_city(code: &str, province_code: &str, city: &str) -> UpstreamCity {
        UpstreamCity {
            provider_code: code.to_string(),
            provider_province_code: province_code.to_string(),
            province: format!("province-{province_code}"),
            city: city.to_string(),
            url: format!("/{code}"),
        }
    }

    async fn start_runtime(
        provider: Arc<CatalogProvider>,
        ttl_seconds: u64,
    ) -> (tempfile::TempDir, EngineRuntime, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("weather.toml");
        let db_path = directory.path().join("weather.db");
        let mut config = AppConfig::default();
        config.db.path = db_path.display().to_string();
        config.updater.default_provider = "catalog-test".to_string();
        config.updater.provider[0].name = "catalog-test".to_string();
        config.updater.province_ttl_seconds = ttl_seconds;
        write_config_atomic(&config_path, &config).unwrap();
        let provider: Arc<dyn WeatherProvider> = provider;
        let runtime = EngineRuntime::start_with_provider(config_path, provider)
            .await
            .unwrap();
        (directory, runtime, db_path)
    }

    async fn wait_for(mut condition: impl FnMut() -> bool) {
        timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not met before timeout");
    }

    async fn next_fetch_event(
        receiver: &mut tokio::sync::broadcast::Receiver<(String, EventEnvelope)>,
    ) -> FetchLogEvent {
        let (_, envelope) = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("fetch event timeout")
            .expect("fetch event channel closed");
        decode_message(&envelope.payload).expect("decode fetch event")
    }

    fn latest_persisted_fetch(path: &Path) -> (bool, Option<String>) {
        let connection = Connection::open(path).unwrap();
        connection
            .query_row(
                "SELECT ok, message FROM upstream_fetch_log ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, bool>(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    async fn shutdown(runtime: &EngineRuntime) {
        runtime.test_engine().db.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn missing_catalogs_refresh_once_and_warm_hits_keep_database_order() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "Beta"),
            upstream_province("A", "Alpha"),
        ]));
        provider.queue_cities(
            "A",
            Reply::Success(vec![
                upstream_city("Z", "A", "Zulu"),
                upstream_city("A", "A", "Alpha"),
            ]),
        );
        provider.queue_cities("EMPTY", Reply::Success(Vec::new()));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        for _ in 0..2 {
            let provinces = engine.provider_provinces().await.unwrap();
            assert_eq!(
                provinces
                    .iter()
                    .map(|province| province.provider_code.as_str())
                    .collect::<Vec<_>>(),
                ["A", "B"]
            );
            let cities = engine.provider_cities_by_code("A").await.unwrap();
            assert_eq!(
                cities
                    .iter()
                    .map(|city| city.provider_code.as_str())
                    .collect::<Vec<_>>(),
                ["A", "Z"]
            );
        }

        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.city_calls("A"), 1);
        for _ in 0..2 {
            assert!(
                engine
                    .provider_cities_by_code("EMPTY")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(provider.city_calls("EMPTY"), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zero_ttl_refreshes_and_failures_fall_back_to_stale_and_empty_caches() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("A", "Old")]));
        provider.queue_provinces(Reply::Success(vec![upstream_province("B", "New")]));
        provider.queue_provinces(Reply::Failure("province unavailable"));
        provider.queue_cities("E", Reply::Success(Vec::new()));
        provider.queue_cities("E", Reply::Failure("empty scope unavailable"));
        provider.queue_cities(
            "M",
            Reply::Success(vec![upstream_city("GOOD", "M", "Good")]),
        );
        provider.queue_cities(
            "M",
            Reply::Success(vec![upstream_city("", "M", "Malformed")]),
        );
        provider.queue_cities("C", Reply::Failure("cold scope unavailable"));
        let (_directory, runtime, db_path) = start_runtime(provider.clone(), 0).await;
        let engine = runtime.test_engine();

        assert_eq!(
            engine.provider_provinces().await.unwrap()[0].provider_code,
            "A"
        );
        assert_eq!(
            engine.provider_provinces().await.unwrap()[0].provider_code,
            "B"
        );
        let mut events = engine.sink.subscribe();
        assert_eq!(
            engine.provider_provinces().await.unwrap()[0].provider_code,
            "B"
        );
        let event = next_fetch_event(&mut events).await;
        assert!(!event.ok);
        assert!(event.message.as_deref().is_some_and(|message| {
            message.contains("using stale catalog after province unavailable")
        }));
        let persisted = latest_persisted_fetch(&db_path);
        assert!(!persisted.0);
        assert_eq!(persisted.1.as_deref(), Some("province unavailable"));

        assert!(
            engine
                .provider_cities_by_code("E")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            engine
                .provider_cities_by_code("E")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            engine.provider_cities_by_code("M").await.unwrap()[0].provider_code,
            "GOOD"
        );
        assert_eq!(
            engine.provider_cities_by_code("M").await.unwrap()[0].provider_code,
            "GOOD"
        );
        let cold = engine.provider_cities_by_code("C").await.unwrap_err();
        assert!(cold.to_string().contains("cold scope unavailable"));
        assert_eq!(provider.province_calls(), 3);
        assert_eq!(provider.city_calls("E"), 2);
        assert_eq!(provider.city_calls("M"), 2);
        assert_eq!(provider.city_calls("C"), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_city_refreshes_share_one_provider_call() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_cities("P", Reply::Success(vec![upstream_city("S", "P", "Shared")]));
        let gate = provider.block_calls();
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        let first = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_cities_by_code("P").await })
        };
        wait_for(|| provider.city_calls("P") == 1).await;
        let second = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_cities_by_code("P").await })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(provider.city_calls("P"), 1);
        gate.add_permits(1);

        assert_eq!(first.await.unwrap().unwrap()[0].provider_code, "S");
        assert_eq!(second.await.unwrap().unwrap()[0].provider_code, "S");
        assert_eq!(provider.city_calls("P"), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_catalog_leader_is_replaced_by_its_waiter() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_cities(
            "P",
            Reply::Success(vec![upstream_city("FIRST", "P", "First")]),
        );
        provider.queue_cities(
            "P",
            Reply::Success(vec![upstream_city("SECOND", "P", "Second")]),
        );
        let gate = provider.block_calls();
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        let leader = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_cities_by_code("P").await })
        };
        wait_for(|| provider.city_calls("P") == 1).await;
        let waiter = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_cities_by_code("P").await })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        wait_for(|| provider.city_calls("P") == 2).await;
        gate.add_permits(1);

        let cities = waiter.await.unwrap().unwrap();
        assert_eq!(cities[0].provider_code, "SECOND");
        wait_for(|| provider.active_calls.load(Ordering::SeqCst) == 0).await;
        assert_eq!(
            engine.provider_cities_by_code("P").await.unwrap()[0].provider_code,
            "SECOND"
        );
        assert_eq!(provider.city_calls("P"), 2);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_city_scopes_share_the_global_eight_request_limit() {
        let provider = Arc::new(CatalogProvider::new());
        for index in 0..9 {
            let code = format!("P{index}");
            provider.queue_cities(
                &code,
                Reply::Success(vec![upstream_city(&format!("S{index}"), &code, "City")]),
            );
        }
        let gate = provider.block_calls();
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();
        let mut tasks = Vec::new();
        for index in 0..9 {
            let engine = engine.clone();
            tasks.push(tokio::spawn(async move {
                engine.provider_cities_by_code(&format!("P{index}")).await
            }));
        }

        wait_for(|| provider.active_calls.load(Ordering::SeqCst) == 8).await;
        assert_eq!(provider.total_city_calls.load(Ordering::SeqCst), 8);
        assert_eq!(provider.active_calls.load(Ordering::SeqCst), 8);
        assert_eq!(provider.peak_active_calls.load(Ordering::SeqCst), 8);
        gate.add_permits(8);
        wait_for(|| provider.total_city_calls.load(Ordering::SeqCst) == 9).await;
        gate.add_permits(1);
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        assert_eq!(provider.peak_active_calls.load(Ordering::SeqCst), 8);
        assert_eq!(provider.active_calls.load(Ordering::SeqCst), 0);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_write_failure_returns_sorted_upstream_with_a_warning() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "Beta"),
            upstream_province("A", "Alpha"),
        ]));
        let (_directory, runtime, db_path) = start_runtime(provider, 3_600).await;
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER fail_catalog_write
                   BEFORE INSERT ON provider_provinces
                   BEGIN SELECT RAISE(FAIL, 'catalog write blocked'); END;"#,
            )
            .unwrap();
        let engine = runtime.test_engine();
        let mut events = engine.sink.subscribe();

        let provinces = engine.provider_provinces().await.unwrap();
        assert_eq!(
            provinces
                .iter()
                .map(|province| province.provider_code.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        let event = next_fetch_event(&mut events).await;
        assert!(event.ok);
        assert!(
            event
                .message
                .as_deref()
                .is_some_and(|message| message.contains("catalog cache write failed"))
        );
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_reread_failure_returns_sorted_upstream_with_a_warning() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "Beta"),
            upstream_province("A", "Alpha"),
        ]));
        let (_directory, runtime, db_path) = start_runtime(provider, 3_600).await;
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER corrupt_catalog_state
                   AFTER INSERT ON catalog_cache_state
                   WHEN NEW.catalog_kind = 'provinces'
                   BEGIN
                     UPDATE catalog_cache_state
                     SET row_count = row_count + 1
                     WHERE provider = NEW.provider
                       AND catalog_kind = NEW.catalog_kind
                       AND scope = NEW.scope;
                   END;"#,
            )
            .unwrap();
        let engine = runtime.test_engine();
        let mut events = engine.sink.subscribe();

        let provinces = engine.provider_provinces().await.unwrap();
        assert_eq!(
            provinces
                .iter()
                .map(|province| province.provider_code.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        let event = next_fetch_event(&mut events).await;
        assert!(event.ok);
        assert!(
            event
                .message
                .as_deref()
                .is_some_and(|message| message.contains("catalog cache re-read failed"))
        );
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fetch_log_failure_only_adds_a_warning() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "Beta"),
            upstream_province("A", "Alpha"),
        ]));
        let (_directory, runtime, db_path) = start_runtime(provider, 3_600).await;
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER fail_fetch_log
                   BEFORE INSERT ON upstream_fetch_log
                   BEGIN SELECT RAISE(FAIL, 'fetch log blocked'); END;"#,
            )
            .unwrap();
        let engine = runtime.test_engine();
        let mut events = engine.sink.subscribe();

        let provinces = engine.provider_provinces().await.unwrap();
        assert_eq!(
            provinces
                .iter()
                .map(|province| province.provider_code.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        let event = next_fetch_event(&mut events).await;
        assert!(event.ok);
        assert!(
            event
                .message
                .as_deref()
                .is_some_and(|message| message.contains("fetch log write failed"))
        );
        shutdown(&runtime).await;
    }
}
