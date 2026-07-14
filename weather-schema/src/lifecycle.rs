use serde::{Deserialize, Serialize};

pub const ENGINE_LOCK_METADATA_VERSION: u32 = 1;

/// Versioned identity written into the stable engine lock file while held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineLockMetadata {
    pub version: u32,
    pub pid: u32,
    pub instance_id: String,
    pub owner_token: Option<String>,
    pub started_at_unix_ms: u64,
    pub config_path: String,
}

impl EngineLockMetadata {
    pub fn is_supported(&self) -> bool {
        self.version == ENGINE_LOCK_METADATA_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_metadata_json_round_trip_preserves_launch_identity() {
        let metadata = EngineLockMetadata {
            version: ENGINE_LOCK_METADATA_VERSION,
            pid: 42,
            instance_id: "engine-instance".to_string(),
            owner_token: Some("owner-token".to_string()),
            started_at_unix_ms: 1_788_000_000_000,
            config_path: "/tmp/weather.toml".to_string(),
        };

        let encoded = serde_json::to_vec(&metadata).unwrap();
        let decoded: EngineLockMetadata = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, metadata);
        assert!(decoded.is_supported());
    }
}
