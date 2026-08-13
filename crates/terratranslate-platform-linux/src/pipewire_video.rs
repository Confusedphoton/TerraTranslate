use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, mpsc};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;

static PIPEWIRE_INIT: Once = Once::new();

pub(crate) fn initialize_pipewire() {
    PIPEWIRE_INIT.call_once(pw::init);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawFrameFormat {
    Rgb,
    Rgba,
    Rgbx,
    Bgrx,
}

#[derive(Clone, Debug)]
pub struct RawVideoFrame {
    pub width: u32,
    pub height: u32,
    pub stride: i32,
    pub format: RawFrameFormat,
    pub bytes: Vec<u8>,
}

impl RawVideoFrame {
    /// Convert a mapped raw frame into the lossless format accepted by vision APIs.
    pub fn encode_png(&self) -> Result<Vec<u8>, PipeWireVideoError> {
        let input_channels = match self.format {
            RawFrameFormat::Rgb => 3,
            RawFrameFormat::Rgba | RawFrameFormat::Rgbx | RawFrameFormat::Bgrx => 4,
        };
        let packed_stride = self.width as usize * input_channels;
        let source_stride = self.stride.unsigned_abs() as usize;
        let source_stride = if source_stride == 0 {
            packed_stride
        } else {
            source_stride
        };
        if self.bytes.len() < source_stride.saturating_mul(self.height as usize) {
            return Err(PipeWireVideoError::Setup(
                "frame buffer is shorter than its dimensions".into(),
            ));
        }
        let mut rgba = Vec::with_capacity(self.width as usize * self.height as usize * 4);
        for row in 0..self.height as usize {
            let row = if self.stride < 0 {
                self.height as usize - 1 - row
            } else {
                row
            };
            let source = &self.bytes[row * source_stride..row * source_stride + packed_stride];
            for pixel in source.chunks_exact(input_channels) {
                let (red, green, blue, alpha) = match self.format {
                    RawFrameFormat::Rgb => (pixel[0], pixel[1], pixel[2], 255),
                    RawFrameFormat::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
                    RawFrameFormat::Rgbx => (pixel[0], pixel[1], pixel[2], 255),
                    RawFrameFormat::Bgrx => (pixel[2], pixel[1], pixel[0], 255),
                };
                rgba.extend_from_slice(&[red, green, blue, alpha]);
            }
        }
        let mut output = Vec::new();
        let mut encoder = png::Encoder::new(&mut output, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| PipeWireVideoError::Setup(error.to_string()))?;
        writer
            .write_image_data(&rgba)
            .map_err(|error| PipeWireVideoError::Setup(error.to_string()))?;
        writer
            .finish()
            .map_err(|error| PipeWireVideoError::Setup(error.to_string()))?;
        Ok(output)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PipeWireVideoError {
    #[error("PipeWire setup failed: {0}")]
    Setup(String),
    #[error("PipeWire capture thread stopped")]
    Stopped,
}

/// Receives mapped raw frames from a portal-approved PipeWire node on a dedicated PipeWire loop.
#[derive(Debug)]
pub struct PortalFrameReceiver {
    frames: mpsc::Receiver<RawVideoFrame>,
    stop: Arc<AtomicBool>,
}

impl PortalFrameReceiver {
    pub fn spawn(remote: OwnedFd, node_id: u32) -> Result<Self, PipeWireVideoError> {
        initialize_pipewire();
        let (frames_tx, frames_rx) = mpsc::sync_channel(2);
        let (setup_tx, setup_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name(format!("terratranslate-pipewire-{node_id}"))
            .spawn(move || {
                if let Err(error) =
                    run_capture(remote, node_id, frames_tx, thread_stop, setup_tx.clone())
                {
                    let _ = setup_tx.send(Err(error.to_string()));
                }
            })
            .map_err(|error| PipeWireVideoError::Setup(error.to_string()))?;
        setup_rx
            .recv()
            .map_err(|_| PipeWireVideoError::Stopped)?
            .map_err(PipeWireVideoError::Setup)?;
        Ok(Self {
            frames: frames_rx,
            stop,
        })
    }

    pub fn try_recv(&self) -> Result<RawVideoFrame, mpsc::TryRecvError> {
        self.frames.try_recv()
    }

    /// Return the most recent available frame without waiting.
    ///
    /// The application only needs a current visual context. Draining older frames prevents a
    /// slow consumer from spending time encoding stale video while newer frames keep arriving.
    pub fn try_recv_latest(&self) -> Result<RawVideoFrame, mpsc::TryRecvError> {
        let mut latest = self.frames.try_recv()?;
        while let Ok(frame) = self.frames.try_recv() {
            latest = frame;
        }
        Ok(latest)
    }

    pub fn recv(&self) -> Result<RawVideoFrame, PipeWireVideoError> {
        self.frames.recv().map_err(|_| PipeWireVideoError::Stopped)
    }
}

impl Drop for PortalFrameReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct VideoState {
    format: spa::param::video::VideoInfoRaw,
    frames: mpsc::SyncSender<RawVideoFrame>,
}

fn run_capture(
    remote: OwnedFd,
    node_id: u32,
    frames: mpsc::SyncSender<RawVideoFrame>,
    stop: Arc<AtomicBool>,
    setup: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), PipeWireVideoError> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(setup_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(setup_error)?;
    let core = context.connect_fd_rc(remote, None).map_err(setup_error)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "terratranslate-window-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(setup_error)?;

    let loop_for_process = mainloop.clone();
    let state = VideoState {
        format: Default::default(),
        frames,
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
            if media_type == spa::param::format::MediaType::Video
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
            let stride = chunk.stride();
            let format = match state.format.format() {
                spa::param::video::VideoFormat::RGB => RawFrameFormat::Rgb,
                spa::param::video::VideoFormat::RGBA => RawFrameFormat::Rgba,
                spa::param::video::VideoFormat::RGBx => RawFrameFormat::Rgbx,
                spa::param::video::VideoFormat::BGRx => RawFrameFormat::Bgrx,
                _ => return,
            };
            let dimensions = state.format.size();
            let Some(mapped) = data.data() else { return };
            let Some(bytes) = mapped.get(offset..offset.saturating_add(size)) else {
                return;
            };
            let frame = RawVideoFrame {
                width: dimensions.width,
                height: dimensions.height,
                stride,
                format,
                bytes: bytes.to_vec(),
            };
            let _ = state.frames.try_send(frame);
        })
        .register()
        .map_err(setup_error)?;

    let format = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGB,
            pw::spa::param::video::VideoFormat::BGRx,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 1280,
                height: 720
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 5, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 60, denom: 1 }
        ),
    );
    let serialized = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format),
    )
    .map_err(setup_error)?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&serialized)
        .ok_or_else(|| PipeWireVideoError::Setup("could not construct video format pod".into()))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(setup_error)?;

    setup
        .send(Ok(()))
        .map_err(|_| PipeWireVideoError::Stopped)?;
    mainloop.run();
    Ok(())
}

fn setup_error(error: impl std::fmt::Display) -> PipeWireVideoError {
    PipeWireVideoError::Setup(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgrx_frame_encodes_as_png() {
        let frame = RawVideoFrame {
            width: 1,
            height: 1,
            stride: 4,
            format: RawFrameFormat::Bgrx,
            bytes: vec![30, 20, 10, 0],
        };
        let encoded = frame.encode_png().unwrap();
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn receiver_returns_the_newest_queued_frame() {
        let (sender, receiver) = mpsc::sync_channel(2);
        sender
            .send(RawVideoFrame {
                width: 1,
                height: 1,
                stride: 4,
                format: RawFrameFormat::Rgbx,
                bytes: vec![0; 4],
            })
            .unwrap();
        sender
            .send(RawVideoFrame {
                width: 2,
                height: 1,
                stride: 8,
                format: RawFrameFormat::Rgbx,
                bytes: vec![0; 8],
            })
            .unwrap();
        let receiver = PortalFrameReceiver {
            frames: receiver,
            stop: Arc::new(AtomicBool::new(false)),
        };

        assert_eq!(receiver.try_recv_latest().unwrap().width, 2);
    }
}
