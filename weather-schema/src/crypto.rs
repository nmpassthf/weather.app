//! HMAC-SHA256 签名工具,覆盖 RPC 与 PUB envelope。
//!
//! - 密钥为 32 字节,配置中以 raw ASCII 字符串表示(易记),不足 32 字节末尾补 0。
//! - `timestamp` 仅作为 debug/随机值参与 HMAC 计算,不做 replay 窗口校验。

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::{EventEnvelope, RpcRequest, RpcResponse};

/// 将 ASCII 字符串解码为 32 字节 HMAC 密钥。
///
/// 取字符串的 UTF-8 字节;不足 32 字节末尾补 0,超过 32 字节报错。
///
/// # 示例
///
/// ```
/// use weather_schema::hmac_key_from_str;
/// let key = hmac_key_from_str("my-secret").unwrap();
/// assert_eq!(key.len(), 32);
/// ```
pub fn hmac_key_from_str(key: &str) -> Result<[u8; 32]> {
    let bytes = key.as_bytes();
    if bytes.len() > 32 {
        bail!("ipc.hmac_key must be at most 32 bytes");
    }
    let mut key = [0u8; 32];
    key[..bytes.len()].copy_from_slice(bytes);
    Ok(key)
}

/// 计算 RPC 请求 envelope 的 HMAC。
pub fn rpc_request_hmac(env: &RpcRequest, key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).context("failed to initialize hmac")?;
    mac.update(env.schema_version.as_bytes());
    mac.update(env.request_id.as_bytes());
    mac.update(&env.kind.to_le_bytes());
    mac.update(&env.timestamp_unix_ms.to_le_bytes());
    mac.update(&env.payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// 校验 RPC 请求 envelope 的 HMAC 是否匹配。
pub fn verify_rpc_request_hmac(env: &RpcRequest, key: &[u8; 32]) -> Result<bool> {
    Ok(rpc_request_hmac(env, key)? == env.hmac_sha256)
}

/// 计算 RPC 响应 envelope 的 HMAC（含错误字段）。
pub fn rpc_response_hmac(env: &RpcResponse, key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).context("failed to initialize hmac")?;
    mac.update(env.schema_version.as_bytes());
    mac.update(env.request_id.as_bytes());
    mac.update(&env.status.to_le_bytes());
    mac.update(&env.timestamp_unix_ms.to_le_bytes());
    mac.update(&env.payload);
    if let Some(error) = &env.error {
        mac.update(error.code.as_bytes());
        mac.update(error.message.as_bytes());
    }
    Ok(mac.finalize().into_bytes().to_vec())
}

/// 计算 PUB 事件 envelope 的 HMAC。
pub fn event_hmac(env: &EventEnvelope, key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).context("failed to initialize hmac")?;
    mac.update(env.schema_version.as_bytes());
    mac.update(env.event_id.as_bytes());
    mac.update(&env.kind.to_le_bytes());
    mac.update(&env.timestamp_unix_ms.to_le_bytes());
    mac.update(&env.payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// 校验 PUB 事件 envelope 的 HMAC 是否匹配。
pub fn verify_event_hmac(env: &EventEnvelope, key: &[u8; 32]) -> Result<bool> {
    Ok(event_hmac(env, key)? == env.hmac_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        hmac_key_from_str(&"a".repeat(32)).unwrap()
    }

    #[test]
    fn rejects_overlong_key() {
        assert!(hmac_key_from_str(&"a".repeat(33)).is_err());
    }

    #[test]
    fn pads_short_key_with_zeros() {
        let key = hmac_key_from_str("ab").unwrap();
        assert_eq!(key[0], b'a');
        assert_eq!(key[1], b'b');
        assert_eq!(key[2..], [0u8; 30]);
    }

    #[test]
    fn accepts_empty_key() {
        let key = hmac_key_from_str("").unwrap();
        assert_eq!(key, [0u8; 32]);
    }

    #[test]
    fn accepts_max_length_key() {
        assert!(hmac_key_from_str(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn rpc_request_hmac_round_trips() {
        let key = key();
        let mut env = RpcRequest {
            schema_version: "weather.schema.v1".to_string(),
            request_id: "rid".to_string(),
            kind: 1,
            timestamp_unix_ms: 42,
            hmac_sha256: Vec::new(),
            payload: vec![1, 2, 3],
        };
        env.hmac_sha256 = rpc_request_hmac(&env, &key).unwrap();
        assert!(verify_rpc_request_hmac(&env, &key).unwrap());

        let tampered = RpcRequest {
            payload: vec![1, 2, 4],
            ..env.clone()
        };
        assert!(!verify_rpc_request_hmac(&tampered, &key).unwrap());
    }

    #[test]
    fn event_hmac_round_trips() {
        let key = key();
        let mut env = EventEnvelope {
            schema_version: "weather.schema.v1".to_string(),
            event_id: "eid".to_string(),
            kind: 1,
            timestamp_unix_ms: 7,
            hmac_sha256: Vec::new(),
            payload: vec![9, 8],
        };
        env.hmac_sha256 = event_hmac(&env, &key).unwrap();
        assert!(verify_event_hmac(&env, &key).unwrap());
    }

    #[test]
    fn different_keys_produce_different_macs() {
        let key_a = key();
        let key_b = hmac_key_from_str(&"b".repeat(32)).unwrap();
        let env = RpcRequest {
            schema_version: "v".to_string(),
            request_id: "r".to_string(),
            kind: 1,
            timestamp_unix_ms: 1,
            hmac_sha256: Vec::new(),
            payload: vec![1],
        };
        assert_ne!(
            rpc_request_hmac(&env, &key_a).unwrap(),
            rpc_request_hmac(&env, &key_b).unwrap()
        );
    }
}
