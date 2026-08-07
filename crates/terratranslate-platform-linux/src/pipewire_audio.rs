use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;

use crate::pipewire_video::initialize_pipewire;

#[derive(Clone, Debug, serde::Serialize)]
pub struct AudioTarget {
    pub object_id: u32,
    pub node_name: String,
    pub description: String,
    pub application_name: Option<String>,
    pub process_id: Option<u32>,
}

pub fn list_application_audio_targets() -> Result<Vec<AudioTarget>, PipeWireAudioError> {
    initialize_pipewire();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(setup_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(setup_error)?;
    let core = context.connect_rc(None).map_err(setup_error)?;
    let registry = core.get_registry().map_err(setup_error)?;
    let targets = Rc::new(RefCell::new(Vec::new()));
    let callback_targets = Rc::clone(&targets);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(properties) = global.props else {
                return;
            };
            let media_class = properties.get(*pw::keys::MEDIA_CLASS).unwrap_or_default();
            if media_class != "Stream/Output/Audio" {
                return;
            }
            let Some(node_name) = properties.get(*pw::keys::NODE_NAME) else {
                return;
            };
            callback_targets.borrow_mut().push(AudioTarget {
                object_id: global.id,
                node_name: node_name.to_owned(),
                description: properties
                    .get(*pw::keys::NODE_DESCRIPTION)
                    .or_else(|| properties.get(*pw::keys::MEDIA_NAME))
                    .unwrap_or(node_name)
                    .to_owned(),
                application_name: properties.get(*pw::keys::APP_NAME).map(str::to_owned),
                process_id: properties
                    .get(*pw::keys::APP_PROCESS_ID)
                    .and_then(|id| id.parse().ok()),
            });
        })
        .register();
    let complete = Rc::new(Cell::new(false));
    let callback_complete = Rc::clone(&complete);
    let callback_loop = mainloop.clone();
    let pending = core.sync(0).map_err(setup_error)?;
    let _core_listener = core
        .add_listener_local()
        .done(move |id, sequence| {
            if id == pw::core::PW_ID_CORE && sequence == pending {
                callback_complete.set(true);
                callback_loop.quit();
            }
        })
        .register();
    while !complete.get() {
        mainloop.run();
    }
    let mut result = targets.borrow().clone();
    result.sort_by(|left, right| left.description.cmp(&right.description));
    Ok(result)
}

#[derive(Clone, Debug)]
pub struct AudioChunk {
    pub sample_rate: u32,
    pub channels: u32,
    pub samples: Vec<f32>,
}

impl AudioChunk {
    pub fn mono_i16(&self) -> Vec<i16> {
        if self.channels == 0 {
            return Vec::new();
        }
        self.samples
            .chunks(self.channels as usize)
            .map(|frame| {
                let average = frame.iter().copied().sum::<f32>() / frame.len() as f32;
                (average.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            })
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PipeWireAudioError {
    #[error("PipeWire audio setup failed: {0}")]
    Setup(String),
    #[error("PipeWire audio capture thread stopped")]
    Stopped,
}

#[derive(Debug)]
pub struct ApplicationAudioReceiver {
    chunks: mpsc::Receiver<AudioChunk>,
    stop: Arc<AtomicBool>,
}

impl ApplicationAudioReceiver {
    /// Capture an audio output node. `target_object` is the PipeWire node name or serial selected
    /// from the registry for the target Wine/application process.
    pub fn spawn(target_object: impl Into<String>) -> Result<Self, PipeWireAudioError> {
        initialize_pipewire();
        let target_object = target_object.into();
        let (chunks_tx, chunks_rx) = mpsc::sync_channel(8);
        let (setup_tx, setup_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("terratranslate-pipewire-audio".into())
            .spawn(move || {
                if let Err(error) =
                    run_capture(target_object, chunks_tx, thread_stop, setup_tx.clone())
                {
                    let _ = setup_tx.send(Err(error.to_string()));
                }
            })
            .map_err(|error| PipeWireAudioError::Setup(error.to_string()))?;
        setup_rx
            .recv()
            .map_err(|_| PipeWireAudioError::Stopped)?
            .map_err(PipeWireAudioError::Setup)?;
        Ok(Self {
            chunks: chunks_rx,
            stop,
        })
    }

    pub fn try_recv(&self) -> Result<AudioChunk, mpsc::TryRecvError> {
        self.chunks.try_recv()
    }

    pub fn recv(&self) -> Result<AudioChunk, PipeWireAudioError> {
        self.chunks.recv().map_err(|_| PipeWireAudioError::Stopped)
    }
}

impl Drop for ApplicationAudioReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct AudioState {
    format: spa::param::audio::AudioInfoRaw,
    chunks: mpsc::SyncSender<AudioChunk>,
}

fn run_capture(
    target_object: String,
    chunks: mpsc::SyncSender<AudioChunk>,
    stop: Arc<AtomicBool>,
    setup: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), PipeWireAudioError> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(setup_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(setup_error)?;
    let core = context.connect_rc(None).map_err(setup_error)?;
    let mut properties = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    properties.insert("target.object", target_object);
    let stream = pw::stream::StreamBox::new(&core, "terratranslate-application-audio", properties)
        .map_err(setup_error)?;

    let loop_for_process = mainloop.clone();
    let state = AudioState {
        format: Default::default(),
        chunks,
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type == spa::param::format::MediaType::Audio
                && media_subtype == spa::param::format::MediaSubtype::Raw
            {
                let _ = state.format.parse(param);
            }
        })
        .process(move |stream, state| {
            if stop.load(Ordering::Acquire) {
                loop_for_process.quit();
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let offset = chunk.offset() as usize;
            let size = chunk.size() as usize;
            let Some(mapped) = data.data() else { return };
            let Some(bytes) = mapped.get(offset..offset.saturating_add(size)) else {
                return;
            };
            let samples = bytes
                .chunks_exact(std::mem::size_of::<f32>())
                .map(|sample| f32::from_le_bytes(sample.try_into().expect("four-byte chunk")))
                .collect::<Vec<_>>();
            let _ = state.chunks.try_send(AudioChunk {
                sample_rate: state.format.rate(),
                channels: state.format.channels(),
                samples,
            });
        })
        .register()
        .map_err(setup_error)?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let format = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let serialized = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format),
    )
    .map_err(setup_error)?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&serialized)
        .ok_or_else(|| PipeWireAudioError::Setup("could not construct audio format pod".into()))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(setup_error)?;
    setup
        .send(Ok(()))
        .map_err(|_| PipeWireAudioError::Stopped)?;
    mainloop.run();
    Ok(())
}

fn setup_error(error: impl std::fmt::Display) -> PipeWireAudioError {
    PipeWireAudioError::Setup(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_chunk_downmixes_and_clamps() {
        let chunk = AudioChunk {
            sample_rate: 48_000,
            channels: 2,
            samples: vec![1.0, 0.0, -2.0, -2.0],
        };
        let mono = chunk.mono_i16();
        assert_eq!(mono.len(), 2);
        assert!(mono[0] > 16_000);
        assert_eq!(mono[1], -i16::MAX);
    }
}
