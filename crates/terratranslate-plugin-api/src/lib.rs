//! Stable wire protocol and narrow C ABI for native text processor plugins.

use std::ffi::c_void;

use serde::{Deserialize, Serialize};
pub use terratranslate_core::{ProcessorRequest, ProcessorResponse, ProcessorStage};

pub const ABI_VERSION: u32 = 1;
pub const ENTRYPOINT: &[u8] = b"terratranslate_plugin_v1\0";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub abi_version: u32,
    pub stages: Vec<ProcessorStage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RunnerRequest {
    Manifest,
    Process(ProcessorRequest),
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RunnerResponse {
    Manifest(PluginManifest),
    Processed(ProcessorResponse),
    Error(String),
    Goodbye,
}

/// A buffer allocated by the plugin and released through `free_buffer`.
#[repr(C)]
pub struct PluginBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub capacity: usize,
}

#[repr(C)]
pub struct PluginApiV1 {
    pub abi_version: u32,
    pub context: *mut c_void,
    /// Writes a MessagePack-encoded `PluginManifest` to `output`.
    pub manifest: unsafe extern "C" fn(*mut c_void, *mut PluginBuffer) -> i32,
    /// Processes a MessagePack-encoded `ProcessorRequest` and writes `ProcessorResponse`.
    pub process: unsafe extern "C" fn(*mut c_void, *const u8, usize, *mut PluginBuffer) -> i32,
    pub free_buffer: unsafe extern "C" fn(*mut c_void, PluginBuffer),
    pub shutdown: unsafe extern "C" fn(*mut c_void),
}

/// Dynamic libraries export a function with this signature under `terratranslate_plugin_v1`.
pub type PluginEntrypoint = unsafe extern "C" fn() -> *const PluginApiV1;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message exceeds the configured limit")]
    TooLarge,
    #[error("invalid MessagePack payload: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("failed to encode MessagePack payload: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    Ok(rmp_serde::to_vec_named(value)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8], maximum: usize) -> Result<T, ProtocolError> {
    if bytes.len() > maximum {
        return Err(ProtocolError::TooLarge);
    }
    Ok(rmp_serde::from_slice(bytes)?)
}
