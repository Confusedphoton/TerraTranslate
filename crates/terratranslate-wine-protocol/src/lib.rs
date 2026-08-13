//! Authenticated IPC shared by TerraTranslate and platform text-hook clients.
//!
//! The historical crate name is retained so existing Wine-side build tooling does not need to
//! change. The protocol itself is platform-neutral and is also used by native Linux preload
//! clients.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 2;
pub const AUTHENTICATION_TOKEN_BYTES: usize = 32;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_SAMPLE_BYTES: usize = 4 * 1024;
pub const MAX_IDENTITY_BYTES: usize = 4 * 1024;
pub const MAX_ADAPTERS: usize = 32;
pub const MAX_METADATA_ENTRIES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPlatform {
    Linux,
    Windows,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRuntime {
    Native,
    Wine,
    Proton,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessArchitecture {
    X86,
    X86_64,
    Aarch64,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableIdentity {
    /// Guest-visible or host-visible executable path, depending on the hook runtime.
    pub path: String,
    /// Optional build ID, PE identity, or other content-derived image identifier.
    pub image_id: Option<String>,
}

impl ExecutableIdentity {
    pub fn stable_id(&self, platform: &HookPlatform) -> String {
        self.image_id
            .as_ref()
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| normalize_module(&self.path, platform))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeHello {
    pub protocol_version: u32,
    pub authentication_token: [u8; AUTHENTICATION_TOKEN_BYTES],
    /// Unique for this client connection. It is not persisted as hook identity.
    pub bridge_id: Uuid,
    pub platform: HookPlatform,
    pub runtime: HookRuntime,
    pub process_id: u32,
    pub architecture: ProcessArchitecture,
    pub executable: ExecutableIdentity,
    pub adapters: Vec<String>,
}

/// Stable persisted identity for a semantic text source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StableCandidateKey(String);

impl StableCandidateKey {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityError::InvalidStableKey);
        }
        Ok(Self(value))
    }

    /// Derives a key that remains stable across PID, connection UUID, address, and ASLR changes.
    pub fn derive(
        platform: &HookPlatform,
        executable: &ExecutableIdentity,
        adapter_id: &str,
        caller_module: Option<&str>,
        module_offset: Option<u64>,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"terratranslate-hook-candidate-v1\0");
        hash_field(&mut hasher, platform_tag(platform));
        hash_field(&mut hasher, &executable.stable_id(platform));
        hash_field(&mut hasher, adapter_id);
        hash_field(
            &mut hasher,
            &caller_module
                .map(|module| normalize_module(module, platform))
                .unwrap_or_default(),
        );
        match module_offset {
            Some(offset) => {
                hasher.update(&[1]);
                hasher.update(&offset.to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        Self(hasher.finalize().to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StableCandidateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookCandidate {
    /// Connection-local identifier used by enable/disable commands.
    pub candidate_id: Uuid,
    /// Persisted identity; clients derive this without PID or absolute load address.
    pub stable_key: StableCandidateKey,
    pub adapter_id: String,
    /// Human-readable intercepted API, for example `pango_layout_set_text`.
    pub api: String,
    pub caller_module: Option<String>,
    pub module_offset: Option<u64>,
    pub sample: String,
    pub embeddable: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookTextEvent {
    pub sequence: u64,
    pub candidate_id: Uuid,
    pub stable_key: StableCandidateKey,
    pub thread_id: u32,
    pub timestamp_ms: i64,
    pub text: String,
    pub speaker: Option<String>,
    pub replacement_capacity_utf16: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    pub sequence: u64,
    pub translated_text: String,
    pub overflow: OverflowPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    Reject,
    TruncateAtGrapheme,
    OverlayFallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostMessage {
    Accept { protocol_version: u32 },
    Reject { reason: String },
    EnableCandidate(Uuid),
    DisableCandidate(Uuid),
    Replace(Replacement),
    Ping(u64),
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// On-disk configuration passed to native preload clients and Wine injectors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookBridgeConfig {
    pub socket_path: String,
    pub authentication_token_hex: String,
}

impl HookBridgeConfig {
    pub fn authentication_token(&self) -> Result<[u8; AUTHENTICATION_TOKEN_BYTES], ConfigError> {
        if self.authentication_token_hex.len() != AUTHENTICATION_TOKEN_BYTES * 2 {
            return Err(ConfigError::AuthenticationTokenLength);
        }
        let mut token = [0; AUTHENTICATION_TOKEN_BYTES];
        let encoded = self.authentication_token_hex.as_bytes();
        for (index, byte) in token.iter_mut().enumerate() {
            let start = index * 2;
            let high = hex_value(encoded[start]).ok_or(ConfigError::AuthenticationTokenHex)?;
            let low = hex_value(encoded[start + 1]).ok_or(ConfigError::AuthenticationTokenHex)?;
            *byte = (high << 4) | low;
        }
        Ok(token)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("authentication token must contain exactly 64 hexadecimal characters")]
    AuthenticationTokenLength,
    #[error("authentication token contains a non-hexadecimal character")]
    AuthenticationTokenHex,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("stable candidate key is empty or exceeds its size limit")]
    InvalidStableKey,
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
    let encoded = rmp_serde::to_vec_named(message)?;
    if encoded.len() > MAX_WIRE_MESSAGE_BYTES {
        return Err(WireError::TooLarge);
    }
    Ok(encoded)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8], maximum: usize) -> Result<T, WireError> {
    if bytes.len() > maximum.min(MAX_WIRE_MESSAGE_BYTES) {
        return Err(WireError::TooLarge);
    }
    Ok(rmp_serde::from_slice(bytes)?)
}

fn platform_tag(platform: &HookPlatform) -> &'static str {
    match platform {
        HookPlatform::Linux => "linux",
        HookPlatform::Windows => "windows",
    }
}

fn normalize_module(module: &str, platform: &HookPlatform) -> String {
    let normalized = module.replace('\\', "/");
    match platform {
        HookPlatform::Linux => normalized,
        HookPlatform::Windows => normalized.to_lowercase(),
    }
}

fn hash_field(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable() -> ExecutableIdentity {
        ExecutableIdentity {
            path: "C:\\Games\\Story.exe".into(),
            image_id: None,
        }
    }

    #[test]
    fn bridge_messages_round_trip() {
        let stable_key = StableCandidateKey::derive(
            &HookPlatform::Windows,
            &executable(),
            "gdi",
            Some("C:\\Games\\Story.exe"),
            Some(0x1234),
        );
        let message = BridgeMessage::Text(HookTextEvent {
            sequence: 7,
            candidate_id: Uuid::nil(),
            stable_key,
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

    #[test]
    fn candidate_key_ignores_pid_address_and_windows_path_case() {
        let first = StableCandidateKey::derive(
            &HookPlatform::Windows,
            &executable(),
            "gdi",
            Some("C:\\Games\\Story.exe"),
            Some(0x1234),
        );
        let second = StableCandidateKey::derive(
            &HookPlatform::Windows,
            &ExecutableIdentity {
                path: "c:/games/story.exe".into(),
                image_id: None,
            },
            "gdi",
            Some("c:/games/story.exe"),
            Some(0x1234),
        );
        assert_eq!(first, second);
    }

    #[test]
    fn config_rejects_malformed_tokens() {
        let config = HookBridgeConfig {
            socket_path: "/tmp/hook.sock".into(),
            authentication_token_hex: "zz".repeat(AUTHENTICATION_TOKEN_BYTES),
        };
        assert_eq!(
            config.authentication_token(),
            Err(ConfigError::AuthenticationTokenHex)
        );
        let unicode = HookBridgeConfig {
            socket_path: "/tmp/hook.sock".into(),
            authentication_token_hex: format!("é{}", "0".repeat(62)),
        };
        assert_eq!(
            unicode.authentication_token(),
            Err(ConfigError::AuthenticationTokenHex)
        );
    }

    #[test]
    fn encode_enforces_the_global_wire_limit() {
        let message = BridgeMessage::Diagnostic {
            level: "info".into(),
            message: "x".repeat(MAX_WIRE_MESSAGE_BYTES),
        };
        assert!(matches!(encode(&message), Err(WireError::TooLarge)));
    }
}
