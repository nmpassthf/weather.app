//! 处理 `RESOLVE_STATION_UUID` RPC：把规范化站点名映射为内部 unified UUID。
//!
//! UUID 由 [`weather_schema::unified_station_uuid`] 从 `name` 纯函数推导，
//! 因此本 handler 无 IO、无 await，仅做编解码与响应包装。

use weather_schema::*;

use crate::runtime::Engine;

impl Engine {
    /// 解析 `name` 对应的 unified UUID，用于客户端订阅 PUB/SUB snapshot topic。
    ///
    /// 空名也合法——会得到一个确定性的 UUID，订阅方应自行决定是否接受。
    pub(crate) async fn handle_resolve_station_uuid(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<ResolveStationUuidRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        let uuid = unified_station_uuid(&req.name);
        self.ok(
            &request.request_id,
            ResolveStationUuidResponse {
                name: req.name,
                unified_uuid: uuid,
            },
        )
    }
}
