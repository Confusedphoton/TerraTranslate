use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("desktop portal error: {0}")]
    Portal(#[from] ashpd::Error),
    #[error("the user or compositor returned no capture stream")]
    NoStream,
}

#[derive(Clone, Debug)]
pub struct PortalStream {
    pub pipewire_node_id: u32,
    pub size: Option<(i32, i32)>,
    pub position: Option<(i32, i32)>,
}

/// A live user-approved portal session. Keeping this value alive keeps permission alive.
#[derive(Debug)]
pub struct WindowCaptureSession {
    session: Session<Screencast>,
    streams: Vec<PortalStream>,
}

impl WindowCaptureSession {
    pub fn streams(&self) -> &[PortalStream] {
        &self.streams
    }

    /// Opens the PipeWire remote carrying the selected stream. Callers decode video frames and
    /// application audio directly; no OCR stage is introduced here.
    pub async fn open_pipewire_remote(&self) -> Result<std::os::fd::OwnedFd, CaptureError> {
        let portal = Screencast::new().await?;
        Ok(portal
            .open_pipe_wire_remote(&self.session, Default::default())
            .await?)
    }
}

pub async fn select_window() -> Result<WindowCaptureSession, CaptureError> {
    let portal = Screencast::new().await?;
    let session = portal.create_session(Default::default()).await?;
    portal
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Metadata)
                .set_sources(Some(SourceType::Window.into()))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await?;
    let response = portal
        .start(&session, None, Default::default())
        .await?
        .response()?;
    let streams = response
        .streams()
        .iter()
        .map(|stream| PortalStream {
            pipewire_node_id: stream.pipe_wire_node_id(),
            size: stream.size(),
            position: stream.position(),
        })
        .collect::<Vec<_>>();
    if streams.is_empty() {
        return Err(CaptureError::NoStream);
    }
    Ok(WindowCaptureSession { session, streams })
}
