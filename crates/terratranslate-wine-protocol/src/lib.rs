//! Authenticated IPC messages shared by the Linux host and Wine-side hook bridge.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeHello {
    pub protocol_version: u32,
    pub authentication_token: [u8; 32],
    pub bridge_id: Uuid,
    pub process_id: u32,
    pub pointer_width: u8,
    pub executable: String,
    pub adapters: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookCandidate {
    pub candidate_id: Uuid,
    pub adapter_id: String,
    pub address: u64,
    pub sample: String,
    pub embeddable: bool,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookTextEvent {
    pub sequence: u64,
    pub candidate_id: Uuid,
    pub thread_id: u32,
    pub timestamp_ms: i64,
    pub text: String,
    pub speaker: Option<String>,
    pub replacement_capacity_utf16: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Replacement {
    pub sequence: u64,
    pub translated_text: String,
    pub overflow: OverflowPolicy,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    Reject,
    TruncateAtGrapheme,
    OverlayFallback,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostMessage {
    Accept { protocol_version: u32 },
    Reject { reason: String },
    EnableCandidate(Uuid),
    DisableCandidate(Uuid),
    Replace(Replacement),
    Ping(u64),
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BridgeMessage {
    Hello(BridgeHello),
    Candidate(HookCandidate),
    Text(HookTextEvent),
    ReplacementResult {
        sequence: u64,
        applied: bool,
        reason: Option<String>,
    },
    Pong(u64),
    Diagnostic {
        level: String,
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("wire message exceeds limit")]
    TooLarge,
    #[error("message encoding failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("message decoding failed: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, WireError> {
    Ok(rmp_serde::to_vec_named(message)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8], maximum: usize) -> Result<T, WireError> {
    if bytes.len() > maximum {
        return Err(WireError::TooLarge);
    }
    Ok(rmp_serde::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_messages_round_trip() {
        let message = BridgeMessage::Text(HookTextEvent {
            sequence: 7,
            candidate_id: Uuid::nil(),
            thread_id: 3,
            timestamp_ms: 100,
            text: "あのね".into(),
            speaker: Some("栞".into()),
            replacement_capacity_utf16: Some(32),
        });
        let bytes = encode(&message).unwrap();
        let decoded: BridgeMessage = decode(&bytes, 4096).unwrap();
        assert!(matches!(decoded, BridgeMessage::Text(event) if event.sequence == 7));
    }
}
