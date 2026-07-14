//! Unified station UUID 生成。
//!
//! PUB/SUB 的 snapshot topic 需要一个与上游 provider 解耦的稳定订阅键。
//! 直接用 provider 自身的 `provider_station_id`（如 NMC 的 city provider code）会带来两个问题：
//! 1. 上游 code 对客户端不透明、不稳定，切换 provider 时无法保持订阅；
//! 2. 同一物理站点在不同 provider 下 `provider_station_id` 不同，无法合并到同一 topic。
//!
//! 本模块用固定 seed + 规范化站点名（`StationRef.name`，如 `北京-北京市-朝阳`）
//! 做确定性哈希，得到标准 8-4-4-4-12 UUID 字符串。相同输入永远映射到相同 UUID，
//! 与 provider 无关。`unified_station_uuid` 是同步纯函数，无 IO、无 await、无锁，
//! 可在任意 publish 路径直接调用。
//!
//! # Seed 升级策略
//!
//! seed 为编译期常量字符串。若未来需要重置 UUID 空间（例如规范化命名规则变更），
//! 将 `UNIFIED_UUID_SEED` 从 `v1` 升到 `v2` 即可让所有 UUID 重新生成，
//! 旧订阅方因 topic 不匹配自然失联，无需额外迁移逻辑。

use sha2::{Digest, Sha256};

/// 固定 seed，参与哈希以隔离命名空间。升级时改版本号。
const UNIFIED_UUID_SEED: &str = "weather.app/unified-station/v1";

/// 由规范化站点名确定性生成 unified UUID。
///
/// 算法：`SHA-256(seed ‖ 0x00 ‖ canonical_name)` 取前 16 字节，按 RFC 4122
/// 设置 version(5) 与 variant(10) 位，格式化为标准 8-4-4-4-12 hex 字符串。
/// 相同输入永远得到相同输出，不同输入碰撞概率可忽略。
///
/// # 示例
///
/// ```
/// use weather_schema::unified_station_uuid;
/// let a = unified_station_uuid("北京-北京市-朝阳");
/// let b = unified_station_uuid("北京-北京市-朝阳");
/// assert_eq!(a, b);
/// assert_eq!(a.len(), 36);
/// ```
pub fn unified_station_uuid(canonical_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(UNIFIED_UUID_SEED.as_bytes());
    hasher.update([0x00]);
    hasher.update(canonical_name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 4122: version 5 (SHA-1 based 名空间 + 名字)，这里用 SHA-256 截断，
    // 仍标记为 version 5 以符合"基于哈希的命名 UUID"语义。
    bytes[6] = (bytes[6] & 0x0F) | 0x50;
    // variant: 10xx xxxx
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format_uuid(&bytes)
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_input() {
        assert_eq!(
            unified_station_uuid("北京-北京市-朝阳"),
            unified_station_uuid("北京-北京市-朝阳")
        );
    }

    #[test]
    fn fixed_outputs_preserve_the_v1_station_identity_space() {
        assert_eq!(
            unified_station_uuid("北京-北京市-朝阳"),
            "4e77ce74-4c4d-5eb4-b81f-073d1f1b8979"
        );
        assert_eq!(
            unified_station_uuid("湖北-湖北省-武汉"),
            "29481a49-a728-53c8-9a4e-49bc3b138c89"
        );
        assert_eq!(
            unified_station_uuid(""),
            "f4c6ef55-abd4-5015-b1f4-7175ce636a9d"
        );
        assert_eq!(
            unified_station_uuid("fixture"),
            "3c78571a-ca24-58e6-a804-266dbda1eaa8"
        );
    }

    #[test]
    fn distinct_for_different_input() {
        assert_ne!(
            unified_station_uuid("北京-北京市-朝阳"),
            unified_station_uuid("湖北-湖北省-武汉")
        );
    }

    #[test]
    fn distinct_from_empty() {
        assert_ne!(
            unified_station_uuid("北京-北京市-朝阳"),
            unified_station_uuid("")
        );
    }

    #[test]
    fn format_matches_rfc4122() {
        let uuid = unified_station_uuid("北京-北京市-朝阳");
        assert!(
            matches_uuid_format(&uuid),
            "uuid `{uuid}` does not match RFC 4122 format"
        );
    }

    #[test]
    fn version_and_variant_bits_set() {
        let uuid = unified_station_uuid("test");
        let bytes: Vec<&str> = uuid.split('-').collect();
        assert_eq!(bytes.len(), 5);
        // 第三段以 5 开头（version 5）。
        assert!(bytes[2].starts_with('5'), "version bit not set");
        // 第四段首位为 8/9/a/b（variant 10）。
        let first = bytes[3].chars().next().unwrap();
        assert!(
            matches!(first, '8' | '9' | 'a' | 'b'),
            "variant bit not set"
        );
    }

    #[test]
    fn length_is_36() {
        assert_eq!(unified_station_uuid("").len(), 36);
        assert_eq!(unified_station_uuid("x").len(), 36);
    }

    fn matches_uuid_format(value: &str) -> bool {
        if value.len() != 36 {
            return false;
        }
        let bytes = value.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            match i {
                8 | 13 | 18 | 23 => {
                    if *b != b'-' {
                        return false;
                    }
                }
                _ => {
                    if !b.is_ascii_hexdigit() {
                        return false;
                    }
                }
            }
        }
        true
    }
}
