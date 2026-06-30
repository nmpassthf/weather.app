//! 处理 `MIGRATE_DB_TIMEZONE` RPC：把 DB 历史表 date 列迁移到新时区。

use std::str::FromStr;

use weather_schema::*;

use crate::runtime::Engine;

impl Engine {
    pub(crate) async fn handle_migrate_db_timezone(&self, request: &RpcRequest) -> RpcResponse {
        let decoded = decode_message::<MigrateDbTimezoneRequest>(&request.payload);
        let Ok(req) = decoded else {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                decoded.unwrap_err().to_string(),
            );
        };
        if chrono_tz::Tz::from_str(&req.new_timezone).is_err() {
            return Self::rpc_error_response(
                &request.request_id,
                "BAD_REQUEST",
                format!("invalid timezone `{}`", req.new_timezone),
            );
        }
        let old = match self.db.get_db_timezone().await {
            Ok(Some(tz)) => tz,
            Ok(None) => String::new(),
            Err(err) => {
                return Self::rpc_error_response(&request.request_id, "DB", err.to_string());
            }
        };
        match self
            .db
            .migrate_timezone(old.clone(), req.new_timezone.clone())
            .await
        {
            Ok(rows) => self.ok(
                &request.request_id,
                MigrateDbTimezoneResponse {
                    old_timezone: old,
                    new_timezone: req.new_timezone,
                    rows_rewritten: rows,
                },
            ),
            Err(err) => Self::rpc_error_response(&request.request_id, "DB", err.to_string()),
        }
    }
}
