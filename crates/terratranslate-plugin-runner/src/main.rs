use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use libloading::{Library, Symbol};
use terratranslate_plugin_api::{
    ABI_VERSION, ENTRYPOINT, PluginApiV1, PluginBuffer, PluginEntrypoint, PluginManifest,
    ProcessorRequest, ProcessorResponse, RunnerRequest, RunnerResponse, decode, encode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_MESSAGE: usize = 16 * 1024 * 1024;

#[derive(Parser)]
struct Arguments {
    /// Path to a native TerraTranslate processor plugin.
    plugin: PathBuf,
}

struct LoadedPlugin {
    _library: Library,
    api: *const PluginApiV1,
}

impl LoadedPlugin {
    unsafe fn load(path: &PathBuf) -> Result<Self> {
        // SAFETY: Loading arbitrary code is the purpose of this isolated process. The ABI is
        // validated before any function other than the entrypoint is invoked.
        let library = unsafe { Library::new(path) }.context("load plugin library")?;
        let entrypoint: Symbol<'_, PluginEntrypoint> =
            unsafe { library.get(ENTRYPOINT) }.context("resolve terratranslate_plugin_v1")?;
        let api = unsafe { entrypoint() };
        if api.is_null() {
            bail!("plugin returned a null API pointer");
        }
        if unsafe { (*api).abi_version } != ABI_VERSION {
            bail!("plugin ABI does not match runner ABI {ABI_VERSION}");
        }
        Ok(Self {
            _library: library,
            api,
        })
    }

    fn call_manifest(&self) -> Result<PluginManifest> {
        let bytes =
            self.call_buffer(|api, output| unsafe { (api.manifest)(api.context, output) })?;
        decode(&bytes, MAX_MESSAGE).context("decode manifest")
    }

    fn call_process(&self, request: ProcessorRequest) -> Result<ProcessorResponse> {
        let input = encode(&request)?;
        let bytes = self.call_buffer(|api, output| unsafe {
            (api.process)(api.context, input.as_ptr(), input.len(), output)
        })?;
        decode(&bytes, MAX_MESSAGE).context("decode processor response")
    }

    fn call_buffer(
        &self,
        invoke: impl FnOnce(&PluginApiV1, *mut PluginBuffer) -> i32,
    ) -> Result<Vec<u8>> {
        let api = unsafe { &*self.api };
        let mut output = PluginBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
            capacity: 0,
        };
        let status = invoke(api, &mut output);
        if status != 0 {
            bail!("plugin returned status {status}");
        }
        if output.len > MAX_MESSAGE || (output.len > 0 && output.ptr.is_null()) {
            if !output.ptr.is_null() {
                unsafe { (api.free_buffer)(api.context, output) };
            }
            bail!("plugin returned an invalid or oversized buffer");
        }
        let bytes = if output.len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(output.ptr, output.len) }.to_vec()
        };
        unsafe { (api.free_buffer)(api.context, output) };
        Ok(bytes)
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        let api = unsafe { &*self.api };
        unsafe { (api.shutdown)(api.context) };
    }
}

async fn read_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<Vec<u8>>> {
    let length = match reader.read_u32_le().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if length > MAX_MESSAGE {
        bail!("request exceeds {MAX_MESSAGE} byte limit");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

async fn write_frame(
    writer: &mut (impl AsyncWriteExt + Unpin),
    response: &RunnerResponse,
) -> Result<()> {
    let bytes = encode(response)?;
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let plugin = unsafe { LoadedPlugin::load(&arguments.plugin) }?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    while let Some(frame) = read_frame(&mut stdin).await? {
        let request: RunnerRequest = decode(&frame, MAX_MESSAGE)?;
        let (response, stop) = match request {
            RunnerRequest::Manifest => match plugin.call_manifest() {
                Ok(manifest) => (RunnerResponse::Manifest(manifest), false),
                Err(error) => (RunnerResponse::Error(error.to_string()), false),
            },
            RunnerRequest::Process(request) => match plugin.call_process(request) {
                Ok(response) => (RunnerResponse::Processed(response), false),
                Err(error) => (RunnerResponse::Error(error.to_string()), false),
            },
            RunnerRequest::Shutdown => (RunnerResponse::Goodbye, true),
        };
        write_frame(&mut stdout, &response).await?;
        if stop {
            break;
        }
    }
    Ok(())
}
