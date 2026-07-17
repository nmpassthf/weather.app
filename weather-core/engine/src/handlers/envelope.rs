use prost::Message;
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
                RpcErrorCode::SchemaVersion,
                "unsupported schema version",
            );
        }
        let config = self.config.get();
        if let Some(key) = match weather_configure::resolve_hmac_key(&config) {
            Ok(opt) => opt,
            Err(err) => {
                return Self::rpc_error_response(
                    &request.request_id,
                    RpcErrorCode::Auth,
                    err.to_string(),
                );
            }
        } {
            match weather_schema::verify_rpc_request_hmac(&request, &key) {
                Ok(true) => {}
                Ok(false) => {
                    return Self::rpc_error_response(
                        &request.request_id,
                        RpcErrorCode::Auth,
                        "invalid hmac",
                    );
                }
                Err(err) => {
                    return Self::rpc_error_response(
                        &request.request_id,
                        RpcErrorCode::Auth,
                        err.to_string(),
                    );
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
            RpcKind::GetResource => self.handle_get_resource(&request).await,
            RpcKind::GetTemperatureHistory => self.handle_get_temperature_history(&request).await,
            RpcKind::FuzzyMatchStations => self.handle_fuzzy(&request).await,
            RpcKind::TriggerRefresh => self.handle_trigger_refresh(&request).await,
            RpcKind::RestartEngine => self.accepted(&request.request_id, Empty {}),
            RpcKind::Shutdown => self.handle_shutdown(&request),
            RpcKind::Unspecified => Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::BadRequest,
                "rpc kind is unspecified",
            ),
        }
    }

    fn handle_shutdown(&self, request: &RpcRequest) -> RpcResponse {
        let shutdown = match ShutdownRequest::decode(request.payload.as_slice()) {
            Ok(shutdown) => shutdown,
            Err(error) => {
                return Self::rpc_error_response(
                    &request.request_id,
                    RpcErrorCode::BadRequest,
                    format!("invalid shutdown payload: {error}"),
                );
            }
        };
        if !shutdown_owner_authorized(
            self.launch.owner_token.as_deref(),
            shutdown.owner_token.as_deref(),
        ) {
            return Self::rpc_error_response(
                &request.request_id,
                RpcErrorCode::OwnerMismatch,
                "engine ownership changed before conditional shutdown",
            );
        }
        self.accepted(&request.request_id, Empty {})
    }
}

fn shutdown_owner_authorized(actual: Option<&str>, requested: Option<&str>) -> bool {
    requested.is_none_or(|requested| actual == Some(requested))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_legacy_shutdown_payload_is_unconditional() {
        let request = ShutdownRequest::decode(Empty {}.encode_to_vec().as_slice()).unwrap();

        assert_eq!(request.owner_token, None);
        assert!(shutdown_owner_authorized(Some("owner"), None));
        assert!(shutdown_owner_authorized(None, None));
    }

    #[test]
    fn conditional_shutdown_requires_the_current_owner() {
        assert!(shutdown_owner_authorized(Some("owner"), Some("owner")));
        assert!(!shutdown_owner_authorized(Some("winner"), Some("loser")));
        assert!(!shutdown_owner_authorized(None, Some("owner")));
    }
}
