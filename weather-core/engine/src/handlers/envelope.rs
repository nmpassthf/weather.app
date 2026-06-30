use std::sync::atomic::Ordering;

use weather_schema::*;

use crate::runtime::Engine;

impl Engine {
    pub(crate) async fn handle_rpc_request(
        &self,
        request: RpcRequest,
        mode: &str,
        rpc_endpoint: &str,
        pub_endpoint: &str,
    ) -> RpcResponse {
        if request.schema_version != SCHEMA_VERSION {
            return Self::rpc_error_response(
                &request.request_id,
                "SCHEMA_VERSION",
                "unsupported schema version",
            );
        }
        let config = self.config.get();
        if let Some(key) = match weather_configure::resolve_hmac_key(&config) {
            Ok(opt) => opt,
            Err(err) => {
                return Self::rpc_error_response(&request.request_id, "AUTH", err.to_string());
            }
        } {
            match weather_schema::verify_rpc_request_hmac(&request, &key) {
                Ok(true) => {}
                Ok(false) => {
                    return Self::rpc_error_response(&request.request_id, "AUTH", "invalid hmac");
                }
                Err(err) => {
                    return Self::rpc_error_response(&request.request_id, "AUTH", err.to_string());
                }
            }
        }

        let kind = RpcKind::try_from(request.kind).unwrap_or(RpcKind::Unspecified);
        match kind {
            RpcKind::Ping => self.ok(&request.request_id, Empty {}),
            RpcKind::GetEngineStatus => self.ok(
                &request.request_id,
                self.status(mode, rpc_endpoint, pub_endpoint),
            ),
            RpcKind::GetConfig => self.handle_get_config(&request).await,
            RpcKind::UpdateConfig => self.handle_update_config(&request).await,
            RpcKind::ListProvinces => self.handle_list_provinces(&request).await,
            RpcKind::ListCities => self.handle_list_cities(&request).await,
            RpcKind::ListConfiguredStations => self.handle_list_configured_stations(&request).await,
            RpcKind::BatchListRegions => self.handle_batch_list_regions(&request).await,
            RpcKind::ResolveStationUuid => self.handle_resolve_station_uuid(&request).await,
            RpcKind::MigrateDbTimezone => self.handle_migrate_db_timezone(&request).await,
            RpcKind::GetWeather => self.handle_get_weather(&request).await,
            RpcKind::FuzzyMatchStations => self.handle_fuzzy(&request).await,
            RpcKind::TriggerRefresh => self.handle_trigger_refresh(&request).await,
            RpcKind::RestartEngine => {
                self.restart.store(true, Ordering::SeqCst);
                self.stop.store(true, Ordering::SeqCst);
                self.accepted(&request.request_id, Empty {})
            }
            RpcKind::Shutdown => {
                self.stop.store(true, Ordering::SeqCst);
                self.accepted(&request.request_id, Empty {})
            }
            RpcKind::Unspecified => Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                "rpc kind is unspecified",
            ),
        }
    }
}
