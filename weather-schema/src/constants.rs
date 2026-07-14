//! Schema 版本与 ZMQ endpoint / PUB topic 常量。
//!
//! 这些常量在 engine、daemon、renderer 之间共享，避免硬编码不一致。
//! 当默认 endpoint 或 topic 命名规则变化时，只需修改本模块。

/// 当前 wire schema 版本字符串，所有 envelope 都会校验该字段。
pub const SCHEMA_VERSION: &str = "weather.schema.v1";

/// DEALER/ROUTER RPC socket 默认 endpoint。
pub const DEFAULT_ZMQ_RPC_ENDPOINT: &str = "tcp://127.0.0.1:44445";

/// PUB/SUB 广播 socket 默认 endpoint。
pub const DEFAULT_ZMQ_PUB_ENDPOINT: &str = "tcp://127.0.0.1:44444";

/// RPC 列表/搜索接口允许的最大单页大小。
pub const MAX_RPC_PAGE_SIZE: u32 = 256;

/// RPC 列表/搜索接口允许的最大页偏移量。
pub const MAX_RPC_PAGE_OFFSET: u32 = 100_000;

const _: () = assert!(MAX_RPC_PAGE_SIZE > 0);
const _: () = assert!(MAX_RPC_PAGE_OFFSET >= MAX_RPC_PAGE_SIZE);

/// 天气快照广播 topic（单一）。
///
/// 所有站点共用一个 topic，订阅方按 `WeatherSnapshot.station.unified_uuid`
/// 过滤自己关心的站点，而非依赖 per-station topic。这样 topic 维度不绑定
/// 到具体站点实体，切换/合并 provider 时订阅键保持稳定。
pub const TOPIC_WEATHER_SNAPSHOT: &str = "weather.snapshot";
/// 引擎状态变更 topic。
pub const TOPIC_ENGINE_STATUS: &str = "engine.status";
/// 上游 fetch 日志 topic。
pub const TOPIC_ENGINE_LOG: &str = "engine.log";
/// 刷新触发/完成 topic。
pub const TOPIC_ENGINE_REFRESH: &str = "engine.refresh";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_topic_is_single_const() {
        assert_eq!(TOPIC_WEATHER_SNAPSHOT, "weather.snapshot");
    }

    #[test]
    fn default_endpoints_are_loopback_tcp() {
        assert!(DEFAULT_ZMQ_RPC_ENDPOINT.starts_with("tcp://127.0.0.1:"));
        assert!(DEFAULT_ZMQ_PUB_ENDPOINT.starts_with("tcp://127.0.0.1:"));
        assert_ne!(DEFAULT_ZMQ_RPC_ENDPOINT, DEFAULT_ZMQ_PUB_ENDPOINT);
    }

    #[test]
    fn topics_are_distinct() {
        let topics = [
            TOPIC_WEATHER_SNAPSHOT,
            TOPIC_ENGINE_STATUS,
            TOPIC_ENGINE_LOG,
            TOPIC_ENGINE_REFRESH,
        ];
        let dedup: std::collections::HashSet<_> = topics.iter().collect();
        assert_eq!(dedup.len(), topics.len());
    }
}
