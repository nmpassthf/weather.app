use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use tokio::task::JoinSet;
use weather_db::{
    ProviderCity, ProviderProvince, validate_provider_city_catalog,
    validate_provider_province_catalog,
};
use weather_schema::*;

use crate::{
    catalog::{CityCatalogKey, ProviderCatalog, catalog_cache_is_fresh},
    handlers::response::paginate,
    limits::{DEFAULT_PAGE_SIZE, MAX_CONCURRENT_CATALOG_FETCHES, normalize_pagination},
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
        let provinces = self.provider_provinces().await?;
        Ok(resolve_provider_province(&provinces, provider, province)?.provider_code)
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

    pub(super) async fn provider_catalog(&self) -> Result<Arc<ProviderCatalog>> {
        let provider = self.provider.provider_name().to_string();
        self.catalog
            .population_flights
            .run(provider.clone(), || async {
                let provinces = self.provider_provinces().await?;
                self.populate_provider_catalog(&provider, provinces).await
            })
            .await
    }

    pub(super) async fn provider_catalog_from_provinces(
        &self,
        provinces: Vec<ProviderProvince>,
    ) -> Result<Arc<ProviderCatalog>> {
        let provider = self.provider.provider_name().to_string();
        self.catalog
            .population_flights
            .run(provider.clone(), || async {
                self.populate_provider_catalog(&provider, provinces).await
            })
            .await
    }

    async fn populate_provider_catalog(
        &self,
        provider: &str,
        provinces: Vec<ProviderProvince>,
    ) -> Result<ProviderCatalog> {
        let ttl_seconds = self.config.get().updater.province_ttl_seconds;
        let now = now_ms();
        let expected_codes = provinces
            .iter()
            .map(|province| province.provider_code.clone())
            .collect::<Vec<_>>();
        let mut city_scopes = BTreeMap::<String, Vec<ProviderCity>>::new();
        let mut cached_scopes = self
            .db
            .get_all_provider_city_scopes(provider)
            .await?
            .into_iter()
            .map(|scope| (scope.provider_province_code.clone(), scope))
            .collect::<BTreeMap<_, _>>();
        let mut pending = Vec::new();

        for code in &expected_codes {
            match cached_scopes.remove(code) {
                Some(scope)
                    if catalog_cache_is_fresh(scope.fetched_at_unix_ms, now, ttl_seconds) =>
                {
                    city_scopes.insert(code.clone(), scope.items);
                }
                _ => pending.push(code.clone()),
            }
        }

        city_scopes.extend(self.load_provider_city_scopes(pending).await?);
        let cities = flatten_city_scopes(&expected_codes, city_scopes)?;
        validate_population_city_codes(&cities)?;
        Ok(ProviderCatalog { provinces, cities })
    }

    pub(super) async fn provider_catalog_for_provinces(
        &self,
        provinces: Vec<ProviderProvince>,
    ) -> Result<ProviderCatalog> {
        let expected_codes = provinces
            .iter()
            .map(|province| province.provider_code.clone())
            .collect::<Vec<_>>();
        let scopes = self
            .load_provider_city_scopes(expected_codes.clone())
            .await?;
        let cities = flatten_city_scopes(&expected_codes, scopes)?;
        validate_population_city_codes(&cities)?;
        Ok(ProviderCatalog { provinces, cities })
    }

    pub(super) async fn provider_catalog_for_province(
        &self,
        province: &str,
    ) -> Result<ProviderCatalog> {
        let provider = self.provider.provider_name();
        let provinces = self.provider_provinces().await?;
        let province = resolve_provider_province(&provinces, provider, province)?;
        self.provider_catalog_for_provinces(vec![province]).await
    }

    pub(super) async fn load_provider_city_scopes(
        &self,
        province_codes: Vec<String>,
    ) -> Result<BTreeMap<String, Vec<ProviderCity>>> {
        let mut pending = province_codes.into_iter();
        let mut tasks = JoinSet::<(String, Result<Vec<ProviderCity>>)>::new();
        let mut loaded = BTreeMap::new();

        while tasks.len() < MAX_CONCURRENT_CATALOG_FETCHES {
            let Some(code) = pending.next() else {
                break;
            };
            spawn_city_scope(&mut tasks, self.clone(), code);
        }

        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((code, Ok(mut cities))) => {
                    sort_provider_cities(&mut cities);
                    loaded.insert(code, cities);
                    if let Some(code) = pending.next() {
                        spawn_city_scope(&mut tasks, self.clone(), code);
                    }
                }
                Ok((code, Err(err))) => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return Err(
                        err.context(format!("failed to populate provider city scope `{code}`"))
                    );
                }
                Err(err) => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return Err(anyhow!("provider city population task failed: {err}"));
                }
            }
        }
        Ok(loaded)
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

fn resolve_provider_province(
    provinces: &[ProviderProvince],
    provider: &str,
    province: &str,
) -> Result<ProviderProvince> {
    let matches = provinces
        .iter()
        .filter(|candidate| candidate.name == province)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [province] => Ok((*province).clone()),
        [] => bail!("provider province `{province}` not found for `{provider}`"),
        matches => bail!(
            "provider province `{province}` is ambiguous for `{provider}`: {}",
            matches
                .iter()
                .map(|candidate| candidate.provider_code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn spawn_city_scope(
    tasks: &mut JoinSet<(String, Result<Vec<ProviderCity>>)>,
    engine: Engine,
    code: String,
) {
    tasks.spawn(async move {
        let result = engine.provider_cities_by_code(&code).await;
        (code, result)
    });
}

fn flatten_city_scopes(
    expected_codes: &[String],
    mut scopes: BTreeMap<String, Vec<ProviderCity>>,
) -> Result<Vec<ProviderCity>> {
    let mut cities = Vec::new();
    for code in expected_codes {
        let scope = scopes
            .remove(code)
            .with_context(|| format!("provider city scope `{code}` was not populated"))?;
        cities.extend(scope);
    }
    Ok(cities)
}

fn validate_population_city_codes(cities: &[ProviderCity]) -> Result<()> {
    let mut owners = BTreeMap::<&str, &str>::new();
    for city in cities {
        if let Some(previous) = owners.insert(
            city.provider_code.as_str(),
            city.provider_province_code.as_str(),
        ) {
            bail!(
                "duplicate provider city code `{}` across province scopes `{previous}` and `{}`",
                city.provider_code,
                city.provider_province_code
            );
        }
    }
    Ok(())
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
    use weather_schema::{
        EventEnvelope, FetchLogEvent, FuzzyMatchStationsRequest, FuzzyMatchStationsResponse,
        ResponseStatus, RpcRequest, SCHEMA_VERSION, WeatherSnapshot, decode_message,
        encode_message,
    };
    use weather_updater::{
        ProviderCity as UpstreamCity, ProviderFuture, ProviderProvince as UpstreamProvince,
        WeatherProvider,
    };

    use crate::{runtime::EngineRuntime, station::canonical_station_name};

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

    async fn fuzzy_page(
        engine: &crate::runtime::Engine,
        query: &str,
        province: Option<&str>,
        page_offset: u32,
        page_size: u32,
    ) -> FuzzyMatchStationsResponse {
        let response = engine
            .handle_fuzzy(&RpcRequest {
                schema_version: SCHEMA_VERSION.to_string(),
                request_id: "fuzzy-test".to_string(),
                payload: encode_message(&FuzzyMatchStationsRequest {
                    query: query.to_string(),
                    province: province.map(str::to_string),
                    page_offset,
                    page_size,
                }),
                ..Default::default()
            })
            .await;
        assert_eq!(
            response.status,
            ResponseStatus::Ok as i32,
            "{:?}",
            response.error
        );
        decode_message(&response.payload).unwrap()
    }

    fn fuzzy_keys(response: &FuzzyMatchStationsResponse) -> Vec<String> {
        response
            .stations
            .iter()
            .map(|station| format!("station:{}", station.unified_uuid))
            .chain(
                response
                    .cities
                    .iter()
                    .map(|city| format!("city:{}:{}", city.province, city.city)),
            )
            .chain(
                response
                    .provinces
                    .iter()
                    .map(|province| format!("province:{}", province.name)),
            )
            .collect()
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
    async fn provider_population_uses_one_province_snapshot_at_zero_ttl() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "Beta"),
            upstream_province("A", "Alpha"),
            upstream_province("E", "Empty"),
        ]));
        provider.queue_cities("A", Reply::Success(vec![upstream_city("A1", "A", "Alpha")]));
        provider.queue_cities("B", Reply::Success(vec![upstream_city("B1", "B", "Beta")]));
        provider.queue_cities("E", Reply::Success(Vec::new()));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 0).await;
        let engine = runtime.test_engine();

        let catalog = engine.provider_catalog().await.unwrap();

        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.total_city_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            catalog
                .provinces
                .iter()
                .map(|province| province.provider_code.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "E"]
        );
        assert_eq!(
            catalog
                .cities
                .iter()
                .map(|city| city.provider_province_code.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warm_provider_population_is_local_and_keeps_scope_order() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "Beta"),
            upstream_province("A", "Alpha"),
        ]));
        provider.queue_cities(
            "A",
            Reply::Success(vec![
                upstream_city("A2", "A", "Zulu"),
                upstream_city("A1", "A", "Alpha"),
            ]),
        );
        provider.queue_cities("B", Reply::Success(vec![upstream_city("B1", "B", "Beta")]));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        let cold = engine.provider_catalog().await.unwrap();
        let warm = engine.provider_catalog().await.unwrap();

        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.total_city_calls.load(Ordering::SeqCst), 2);
        assert_eq!(cold.cities, warm.cities);
        assert_eq!(
            warm.cities
                .iter()
                .map(|city| city.provider_code.as_str())
                .collect::<Vec<_>>(),
            ["A1", "A2", "B1"]
        );
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn provider_population_has_a_rolling_eight_scope_window() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(
            (0..9)
                .rev()
                .map(|index| upstream_province(&format!("P{index}"), "Province"))
                .collect(),
        ));
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
        let task = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_catalog().await })
        };

        wait_for(|| provider.province_calls() == 1).await;
        gate.add_permits(1);
        wait_for(|| provider.active_calls.load(Ordering::SeqCst) == 8).await;
        assert_eq!(provider.total_city_calls.load(Ordering::SeqCst), 8);
        gate.add_permits(8);
        wait_for(|| provider.total_city_calls.load(Ordering::SeqCst) == 9).await;
        gate.add_permits(1);
        let catalog = task.await.unwrap().unwrap();

        assert_eq!(provider.peak_active_calls.load(Ordering::SeqCst), 8);
        assert_eq!(provider.active_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            catalog
                .cities
                .iter()
                .map(|city| city.provider_province_code.as_str())
                .collect::<Vec<_>>(),
            ["P0", "P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8"]
        );
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_provider_populations_share_one_flight() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Province")]));
        provider.queue_cities("P", Reply::Success(vec![upstream_city("S", "P", "Shared")]));
        let gate = provider.block_calls();
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();
        let first = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_catalog().await })
        };
        wait_for(|| provider.province_calls() == 1).await;
        let second = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_catalog().await })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(provider.province_calls(), 1);
        gate.add_permits(1);
        wait_for(|| provider.city_calls("P") == 1).await;
        gate.add_permits(1);

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.city_calls("P"), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_population_leader_is_replaced_without_leaking_tasks() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "First")]));
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Second")]));
        provider.queue_cities("P", Reply::Success(vec![upstream_city("S", "P", "City")]));
        let gate = provider.block_calls();
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 0).await;
        let engine = runtime.test_engine();
        let leader = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_catalog().await })
        };
        wait_for(|| provider.province_calls() == 1).await;
        let waiter = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.provider_catalog().await })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        wait_for(|| provider.province_calls() == 2).await;
        gate.add_permits(1);
        wait_for(|| provider.city_calls("P") == 1).await;
        gate.add_permits(1);
        let catalog = waiter.await.unwrap().unwrap();

        assert_eq!(catalog.provinces[0].name, "Second");
        wait_for(|| provider.active_calls.load(Ordering::SeqCst) == 0).await;
        assert_eq!(provider.province_calls(), 2);
        assert_eq!(provider.city_calls("P"), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn population_deadline_aborts_all_owned_provider_calls() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Province")]));
        provider.queue_cities("P", Reply::Success(vec![upstream_city("S", "P", "City")]));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();
        engine.provider_provinces().await.unwrap();
        let _gate = provider.block_calls();

        let population = {
            let engine = engine.clone();
            tokio::spawn(async move {
                timeout(Duration::from_millis(50), engine.provider_catalog()).await
            })
        };
        wait_for(|| provider.city_calls("P") == 1).await;
        let expired = population.await.unwrap();

        assert!(expired.is_err());
        wait_for(|| provider.active_calls.load(Ordering::SeqCst) == 0).await;
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn population_first_error_aborts_and_drains_other_scopes() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("A", "Alpha"),
            upstream_province("B", "Beta"),
        ]));
        provider.queue_cities("A", Reply::Failure("cold scope unavailable"));
        provider.queue_cities("B", Reply::Success(vec![upstream_city("B1", "B", "Beta")]));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        let error = engine.provider_catalog().await.unwrap_err().to_string();

        assert!(error.contains("failed to populate provider city scope `A`"));
        assert!(error.contains("cold scope unavailable"));
        assert_eq!(provider.active_calls.load(Ordering::SeqCst), 0);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn population_uses_stale_scopes_when_zero_ttl_refreshes_fail() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Province")]));
        provider.queue_cities("P", Reply::Success(vec![upstream_city("OLD", "P", "Old")]));
        provider.queue_provinces(Reply::Failure("province unavailable"));
        provider.queue_cities("P", Reply::Failure("city unavailable"));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 0).await;
        let engine = runtime.test_engine();

        let first = engine.provider_catalog().await.unwrap();
        let stale = engine.provider_catalog().await.unwrap();

        assert_eq!(first.cities, stale.cities);
        assert_eq!(stale.cities[0].provider_code, "OLD");
        assert_eq!(provider.province_calls(), 2);
        assert_eq!(provider.city_calls("P"), 2);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn population_preserves_valid_upstream_when_city_cache_write_fails() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Province")]));
        provider.queue_cities("P", Reply::Success(vec![upstream_city("S", "P", "City")]));
        let (_directory, runtime, db_path) = start_runtime(provider, 3_600).await;
        let engine = runtime.test_engine();
        engine.provider_provinces().await.unwrap();
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER fail_city_cache_write
                   BEFORE INSERT ON provider_cities
                   BEGIN SELECT RAISE(FAIL, 'city cache blocked'); END;"#,
            )
            .unwrap();

        let catalog = engine.provider_catalog().await.unwrap();

        assert_eq!(catalog.cities[0].provider_code, "S");
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn population_reuses_city_state_when_province_cache_writes_fail() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Province")]));
        provider.queue_provinces(Reply::Success(vec![upstream_province("P", "Province")]));
        provider.queue_cities("P", Reply::Success(vec![upstream_city("S", "P", "City")]));
        let (_directory, runtime, db_path) = start_runtime(provider.clone(), 3_600).await;
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                r#"CREATE TRIGGER fail_province_cache_write
                   BEFORE INSERT ON provider_provinces
                   BEGIN SELECT RAISE(FAIL, 'province cache blocked'); END;"#,
            )
            .unwrap();
        let engine = runtime.test_engine();

        let first = engine.provider_catalog().await.unwrap();
        let second = engine.provider_catalog().await.unwrap();

        assert_eq!(first.cities, second.cities);
        assert_eq!(provider.province_calls(), 2);
        assert_eq!(provider.city_calls("P"), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn population_rejects_duplicate_city_codes_across_scopes() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("A", "Alpha"),
            upstream_province("B", "Beta"),
        ]));
        provider.queue_cities(
            "A",
            Reply::Success(vec![upstream_city("DUP", "A", "Alpha")]),
        );
        provider.queue_cities("B", Reply::Success(vec![upstream_city("DUP", "B", "Beta")]));
        let (_directory, runtime, _db_path) = start_runtime(provider, 3_600).await;
        let engine = runtime.test_engine();

        let error = engine.provider_catalog().await.unwrap_err().to_string();

        assert!(error.contains("duplicate provider city code `DUP`"));
        assert!(error.contains("province scopes `A` and `B`"));
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scoped_fuzzy_only_populates_the_requested_province() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("A", "province-A"),
            upstream_province("B", "province-B"),
        ]));
        provider.queue_cities("A", Reply::Success(vec![upstream_city("A1", "A", "Alpha")]));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        let cold = fuzzy_page(&engine, "", Some("province-A"), 0, 32).await;
        let warm = fuzzy_page(&engine, "", Some("province-A"), 0, 32).await;

        assert_eq!(fuzzy_keys(&cold), fuzzy_keys(&warm));
        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.city_calls("A"), 1);
        assert_eq!(provider.city_calls("B"), 0);
        assert_eq!(cold.stations.len(), 1);
        assert_eq!(cold.cities.len(), 1);
        assert_eq!(cold.provinces.len(), 1);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scoped_fuzzy_uses_stale_target_without_populating_other_provinces() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("A", "province-A"),
            upstream_province("B", "province-B"),
        ]));
        provider.queue_cities("A", Reply::Success(vec![upstream_city("A1", "A", "Alpha")]));
        provider.queue_provinces(Reply::Failure("province unavailable"));
        provider.queue_cities("A", Reply::Failure("city unavailable"));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 0).await;
        let engine = runtime.test_engine();

        let first = fuzzy_page(&engine, "", Some("province-A"), 0, 32).await;
        let stale = fuzzy_page(&engine, "", Some("province-A"), 0, 32).await;

        assert_eq!(fuzzy_keys(&first), fuzzy_keys(&stale));
        assert_eq!(provider.province_calls(), 2);
        assert_eq!(provider.city_calls("A"), 2);
        assert_eq!(provider.city_calls("B"), 0);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fuzzy_pages_form_one_stable_deduplicated_candidate_sequence() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("B", "province-B"),
            upstream_province("A", "province-A"),
        ]));
        provider.queue_cities(
            "A",
            Reply::Success(vec![
                upstream_city("A3", "A", "Three"),
                upstream_city("A1", "A", "One"),
                upstream_city("A2", "A", "Two"),
            ]),
        );
        provider.queue_cities(
            "B",
            Reply::Success(vec![
                upstream_city("B2", "B", "Five"),
                upstream_city("B1", "B", "Four"),
            ]),
        );
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();

        let default_page = fuzzy_page(&engine, "", None, 0, 0).await;
        assert!(default_page.has_more);
        assert_eq!(default_page.next_offset, 10);
        let full = fuzzy_page(&engine, "", None, 0, 256).await;
        let full_keys = fuzzy_keys(&full);
        assert_eq!(full_keys.len(), 12);
        assert_eq!(
            full_keys
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            full_keys.len()
        );

        let mut paged_keys = Vec::new();
        let mut offset = 0;
        loop {
            let page = fuzzy_page(&engine, "", None, offset, 3).await;
            paged_keys.extend(fuzzy_keys(&page));
            if !page.has_more {
                assert_eq!(page.next_offset, 12);
                break;
            }
            assert!(page.next_offset > offset);
            offset = page.next_offset;
        }
        assert_eq!(paged_keys, full_keys);

        let beyond = fuzzy_page(&engine, "", None, 100, 3).await;
        assert!(fuzzy_keys(&beyond).is_empty());
        assert!(!beyond.has_more);
        assert_eq!(beyond.next_offset, 12);
        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.total_city_calls.load(Ordering::SeqCst), 2);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn targeted_resolution_populates_only_its_matching_scope_and_reuses_mapping() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("A", "Alpha省"),
            upstream_province("B", "Beta省"),
        ]));
        let mut target = upstream_city("A1", "A", "Target");
        target.province = "Alpha省".to_string();
        provider.queue_cities("A", Reply::Success(vec![target]));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 3_600).await;
        let engine = runtime.test_engine();
        let name = canonical_station_name("Alpha省", "Target");

        let first = engine.station_by_name(&name).await.unwrap();
        let second = engine.station_by_name(&name).await.unwrap();

        assert_eq!(first.provider_station_id, "A1");
        assert_eq!(second.provider_station_id, "A1");
        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.city_calls("A"), 1);
        assert_eq!(provider.city_calls("B"), 0);
        shutdown(&runtime).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn targeted_resolution_uses_one_nationwide_population_when_hint_is_unknown() {
        let provider = Arc::new(CatalogProvider::new());
        provider.queue_provinces(Reply::Success(vec![
            upstream_province("A", "AdministrativeA"),
            upstream_province("B", "AdministrativeB"),
        ]));
        let mut target = upstream_city("A1", "A", "Target");
        target.province = "Alias省".to_string();
        provider.queue_cities("A", Reply::Success(vec![target]));
        provider.queue_cities("B", Reply::Success(vec![upstream_city("B1", "B", "Other")]));
        let (_directory, runtime, _db_path) = start_runtime(provider.clone(), 0).await;
        let engine = runtime.test_engine();
        let name = canonical_station_name("Alias省", "Target");

        let station = engine.station_by_name(&name).await.unwrap();

        assert_eq!(station.provider_station_id, "A1");
        assert_eq!(provider.province_calls(), 1);
        assert_eq!(provider.city_calls("A"), 1);
        assert_eq!(provider.city_calls("B"), 1);
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
