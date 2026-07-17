use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use weather_configure::ProviderConfig;

use super::{NmcProvider, WeatherProvider, dto, mapper};

const FULL: &str = include_str!("fixtures/weather_full.json");
const NONZERO_NULL: &str = include_str!("fixtures/weather_nonzero_null.json");
const SUCCESS_NULL: &str = include_str!("fixtures/weather_success_null.json");
const MINIMAL_REAL: &str = include_str!("fixtures/weather_minimal_real.json");
const MALFORMED_OPTIONAL: &str = include_str!("fixtures/weather_malformed_optional.json");
const ALIAS_FALLBACK: &str = include_str!("fixtures/weather_alias_fallback.json");
const CATALOG_URLS: &str = include_str!("fixtures/catalog_urls.json");

fn map_fixture(
    fixture: &str,
    base_url: &str,
) -> mapper::MapOutcome<weather_schema::WeatherSnapshot> {
    let body: Value = serde_json::from_str(fixture).expect("valid JSON fixture");
    let data = dto::decode_weather_response(body).expect("successful fixture envelope");
    mapper::map_weather(data, &Url::parse(base_url).expect("valid fixture base URL"))
}

#[test]
fn full_response_preserves_the_existing_structured_mapping_baseline() {
    let mapped = map_fixture(FULL, "https://configured.example/nmc/");
    assert!(mapped.warnings.is_empty(), "{:?}", mapped.warnings);
    let snapshot = mapped.value;

    let station = snapshot.station.as_ref().expect("station");
    assert_eq!(station.name, "北京-北京市-朝阳");
    let real = snapshot.real.as_ref().expect("real weather");
    assert_eq!(real.info.as_deref(), Some("晴"));
    assert_eq!(real.temperature, Some(30.0));
    assert_eq!(real.temperature_diff, Some(1.2));
    assert_eq!(real.air_pressure, Some(1001.0));
    assert_eq!(real.comfort_label.as_deref(), Some("较舒适"));
    assert_eq!(real.comfort_index.as_deref(), Some("3"));
    assert_eq!(real.weather_icon.as_deref(), Some("0"));
    assert_eq!(real.wind_degree, Some(45.0));
    assert_eq!(real.sunrise.as_deref(), Some("04:50"));
    assert_eq!(real.sunset.as_deref(), Some("19:47"));
    let alert = real.alerts.first().expect("weather alert");
    assert!(!alert.inherited);
    assert_eq!(
        alert.url.as_deref(),
        Some("https://configured.example/publish/alarm/ABJ.html")
    );
    assert_eq!(
        alert.icon_url.as_deref(),
        Some("https://configured.example/site4/images/alarm/yellow.png")
    );

    let forecast = snapshot.predict.as_ref().expect("forecast");
    assert_eq!(forecast.publish_time.as_deref(), Some("2026-06-29 08:00"));
    let day = &forecast.days[0];
    assert_eq!(day.publish_time.as_deref(), Some("2026-06-29 08:00"));
    assert_eq!(day.day_weather_icon.as_deref(), Some("0"));
    assert_eq!(day.night_weather_icon.as_deref(), Some("1"));
    assert_eq!(day.day_wind_direct.as_deref(), Some("东北风"));
    assert_eq!(day.night_wind_power.as_deref(), Some("2级"));

    let air = snapshot.air.as_ref().expect("air quality");
    assert_eq!(air.aqi, Some(66.0));
    assert_eq!(air.primary_pollutant.as_deref(), Some("O3"));
    assert_eq!(snapshot.tempchart[0].max_temperature, Some(34.0));
    assert_eq!(snapshot.passedchart[0].rain_24h, Some(0.4));
    assert_eq!(snapshot.passedchart[0].wind_direction_degree, Some(45.0));
    assert_eq!(
        snapshot.climate.as_ref().unwrap().month[0].precipitation,
        Some(78.9)
    );
    assert_eq!(
        snapshot.radar.as_ref().unwrap().image_url.as_deref(),
        Some(
            "https://configured.example/product/SEVP_AOC_RDCP_SLDAS_EBREF_ACHN_L88_PI_20260629100000000.PNG"
        )
    );
    assert!(snapshot.debug.is_none());
}

#[test]
fn nonzero_code_is_reported_before_null_data_is_considered() {
    let body = serde_json::from_str(NONZERO_NULL).unwrap();
    let error = dto::decode_weather_response(body).unwrap_err().to_string();
    assert!(
        error.contains("returned code 503: upstream busy"),
        "{error}"
    );
    assert!(!error.contains("success with null data"), "{error}");
}

#[test]
fn successful_envelope_requires_non_null_object_data() {
    let body = serde_json::from_str(SUCCESS_NULL).unwrap();
    assert!(
        dto::decode_weather_response(body)
            .unwrap_err()
            .to_string()
            .contains("success with null data")
    );
    let scalar = serde_json::json!({ "code": 0, "msg": "ok", "data": 7 });
    assert!(
        dto::decode_weather_response(scalar)
            .unwrap_err()
            .to_string()
            .contains("weather data object")
    );
    let positional_data = serde_json::json!({
        "code": 0,
        "msg": "ok",
        "data": [null, null, null, null, null, null, null]
    });
    assert!(
        dto::decode_weather_response(positional_data)
            .unwrap_err()
            .to_string()
            .contains("weather data object")
    );

    let positional_envelope = serde_json::json!([0, "ok", {}]);
    assert!(
        dto::decode_weather_response(positional_envelope)
            .unwrap_err()
            .to_string()
            .contains("envelope must be an object")
    );
}

#[test]
fn absent_optional_sections_preserve_current_weather_without_warnings() {
    let mapped = map_fixture(MINIMAL_REAL, "https://configured.example/");
    assert!(mapped.warnings.is_empty(), "{:?}", mapped.warnings);
    assert_eq!(mapped.value.real.unwrap().temperature, Some(30.0));
    assert!(mapped.value.predict.is_none());
    assert!(mapped.value.air.is_none());
    assert!(mapped.value.tempchart.is_empty());
}

#[test]
fn malformed_sections_and_entries_are_isolated_with_stable_warnings() {
    let mapped = map_fixture(MALFORMED_OPTIONAL, "https://configured.example/");
    let snapshot = mapped.value;
    let real = snapshot.real.expect("valid current weather must survive");
    assert_eq!(real.temperature, Some(30.0));
    assert_eq!(real.wind_speed, Some(2.5));
    assert!(real.alerts.is_empty());
    assert_eq!(snapshot.predict.unwrap().days.len(), 1);
    assert_eq!(snapshot.tempchart.len(), 1);
    assert_eq!(snapshot.climate.unwrap().month.len(), 1);
    assert!(snapshot.air.is_none());
    assert!(snapshot.passedchart.is_empty());
    assert!(snapshot.radar.is_none());
    assert_eq!(
        mapped.warnings,
        [
            "real.warn: malformed object; ignored",
            "real.sunriseSunset: malformed object; ignored",
            "predict.detail[0].night: malformed object; ignored",
            "predict.detail[1]: malformed object; ignored",
            "predict.detail[2]: missing usable date; entry ignored",
            "air: expected object; ignored",
            "tempchart[0]: expected object; ignored",
            "passedchart: expected array or object; ignored",
            "climate.month[0]: expected object; ignored",
            "radar: expected object; ignored",
        ]
    );
}

#[test]
fn aliases_try_each_complete_conversion_and_filter_every_sentinel_shape() {
    let mapped = map_fixture(ALIAS_FALLBACK, "http://proxy.test/nmc/");
    assert!(mapped.warnings.is_empty(), "{:?}", mapped.warnings);
    let snapshot = mapped.value;
    let real = snapshot.real.unwrap();
    assert_eq!(real.temperature, None);
    assert_eq!(real.humidity, None);
    assert_eq!(real.rain, None);
    assert_eq!(real.comfort_index.as_deref(), Some("1"));
    assert_eq!(real.comfort_label.as_deref(), Some("71"));
    let air = snapshot.air.unwrap();
    assert_eq!(air.publish_time.as_deref(), Some("2026-06-29 09:00"));
    assert_eq!(air.aqi, Some(66.0));
    assert_eq!(air.level.as_deref(), Some("二级"));
    assert_eq!(air.pm2_5, Some(22.0));
    assert_eq!(snapshot.tempchart[0].min_temperature, Some(24.0));
    assert_eq!(snapshot.passedchart[0].pressure, Some(1001.0));
    assert_eq!(snapshot.climate.unwrap().month[0].month, Some(6));
    let radar = snapshot.radar.unwrap();
    assert_eq!(
        radar.image_url.as_deref(),
        Some("http://proxy.test/radar.png")
    );
    assert_eq!(radar.page_url.as_deref(), Some("http://cdn.example/radar"));
}

#[test]
fn textual_dash_placeholders_are_mapped_as_missing_values() {
    let mut fixture: Value = serde_json::from_str(FULL).unwrap();
    fixture["data"]["real"]["weather"]["info"] = Value::String("-".to_string());
    fixture["data"]["real"]["weather"]["img"] = Value::String("—".to_string());
    let data = dto::decode_weather_response(fixture).unwrap();

    let mapped = mapper::map_weather(
        data,
        &Url::parse("https://configured.example/nmc/").unwrap(),
    );

    assert!(mapped.warnings.is_empty(), "{:?}", mapped.warnings);
    let real = mapped.value.real.unwrap();
    assert!(real.info.is_none());
    assert!(real.weather_icon.is_none());
}

#[test]
fn aliases_warn_once_only_after_all_present_candidates_fail() {
    let fixture = serde_json::json!({
        "code": 0,
        "data": { "air": { "aqi": {}, "AQI": "not-a-number" } }
    });
    let data = dto::decode_weather_response(fixture).unwrap();
    let mapped = mapper::map_weather(data, &Url::parse("https://configured.example/").unwrap());
    assert_eq!(
        mapped.warnings,
        ["air.aqi: no alias contained a usable number; field ignored"]
    );
}

#[derive(Deserialize)]
struct CatalogFixture {
    provinces: Vec<dto::ProvinceDto>,
    cities: Vec<dto::CityDto>,
}

#[test]
fn every_catalog_url_form_uses_standard_configured_base_joining() {
    let fixture: CatalogFixture = serde_json::from_str(CATALOG_URLS).unwrap();
    let base = Url::parse("http://proxy.test/nmc/").unwrap();
    let provinces = mapper::map_provinces(fixture.provinces, &base).unwrap();
    assert_eq!(provinces[0].url, "https://origin.example/a");
    assert_eq!(provinces[1].url, "http://proxy.test/root");
    assert_eq!(provinces[2].url, "http://proxy.test/nmc/path/item");
    assert_eq!(provinces[3].url, "http://cdn.example/item");
    let cities = mapper::map_cities(fixture.cities, "R", &base).unwrap();
    assert_eq!(cities[0].url, "http://proxy.test/nmc/city/item");
}

#[tokio::test]
async fn debug_flag_controls_payload_but_warnings_always_reach_the_engine_boundary() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).await.expect("read request");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        MALFORMED_OPTIONAL.len(),
                        MALFORMED_OPTIONAL
                    )
                    .as_bytes(),
                )
                .await
                .expect("write response");
        }
    });
    let provider = NmcProvider::new(
        &ProviderConfig {
            name: "nmc".to_string(),
            base_url: format!("http://{addr}/nmc/"),
            request_timeout_seconds: 3,
            network: weather_configure::ProviderNetworkConfig {
                http_proxy: Some(String::new()),
                https_proxy: Some(String::new()),
                all_proxy: Some(String::new()),
                ..Default::default()
            },
        },
        &weather_configure::NetworkConfig::default(),
    )
    .unwrap();

    let without_debug = provider.weather("MjXfi", false).await.unwrap();
    assert!(!without_debug.warnings.is_empty());
    assert!(without_debug.snapshot.debug.is_none());

    let with_debug = provider.weather("MjXfi", true).await.unwrap();
    let debug = with_debug.snapshot.debug.expect("debug payload");
    assert_eq!(debug.warnings, with_debug.warnings);
    assert!(debug.endpoint.contains("nmc/rest/weather"));
    assert!(debug.endpoint.contains("stationid=MjXfi"));
    assert!(debug.raw_json.contains("\"code\":0"));
}
