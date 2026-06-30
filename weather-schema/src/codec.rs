use anyhow::{Context, Result};
use prost::Message;
use serde::Serialize;

pub fn encode_message<M: Message>(message: &M) -> Vec<u8> {
    message.encode_to_vec()
}

pub fn decode_message<M: Message + Default>(bytes: &[u8]) -> Result<M> {
    M::decode(bytes).context("failed to decode protobuf payload")
}

pub fn json_pretty<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value).context("failed to serialize schema JSON")
}
