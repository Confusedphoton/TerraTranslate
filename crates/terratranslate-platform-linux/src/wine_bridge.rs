use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use terratranslate_wine_protocol::{
    BridgeHello, BridgeMessage, HostMessage, PROTOCOL_VERSION, decode, encode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const MAX_WIRE_MESSAGE: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BridgeServerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire protocol error: {0}")]
    Wire(#[from] terratranslate_wine_protocol::WireError),
    #[error("first bridge message was not a hello")]
    MissingHello,
    #[error("bridge authentication failed")]
    Authentication,
    #[error("bridge protocol version {0} is unsupported")]
    Version(u32),
}

pub struct WineBridgeServer {
    listener: UnixListener,
    authentication_token: [u8; 32],
}

impl WineBridgeServer {
    pub fn bind(
        path: impl AsRef<Path>,
        authentication_token: [u8; 32],
    ) -> Result<Self, BridgeServerError> {
        Ok(Self {
            listener: UnixListener::bind(path)?,
            authentication_token,
        })
    }

    pub async fn accept(&self) -> Result<WineBridgeConnection, BridgeServerError> {
        let (mut stream, _) = self.listener.accept().await?;
        let hello = match read_message::<BridgeMessage>(&mut stream).await? {
            BridgeMessage::Hello(hello) => hello,
            _ => return Err(BridgeServerError::MissingHello),
        };
        if hello.authentication_token != self.authentication_token {
            let _ = write_message(
                &mut stream,
                &HostMessage::Reject {
                    reason: "authentication failed".into(),
                },
            )
            .await;
            return Err(BridgeServerError::Authentication);
        }
        if hello.protocol_version != PROTOCOL_VERSION {
            let _ = write_message(
                &mut stream,
                &HostMessage::Reject {
                    reason: format!("protocol {} is unsupported", hello.protocol_version),
                },
            )
            .await;
            return Err(BridgeServerError::Version(hello.protocol_version));
        }
        write_message(
            &mut stream,
            &HostMessage::Accept {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;
        Ok(WineBridgeConnection { stream, hello })
    }
}

pub struct WineBridgeConnection {
    stream: UnixStream,
    hello: BridgeHello,
}

/// Events emitted by authenticated Wine hook bridges.
///
/// The portal can authorize video capture, but it intentionally cannot disclose a process's
/// in-memory text. Wine clients therefore connect to this separate authenticated endpoint.
#[derive(Clone, Debug)]
pub enum WineHookEvent {
    Connected {
        process_id: u32,
        executable: String,
    },
    Text {
        process_id: u32,
        executable: String,
        event: terratranslate_wine_protocol::HookTextEvent,
    },
    Diagnostic {
        process_id: u32,
        executable: String,
        level: String,
        message: String,
    },
    Disconnected {
        process_id: u32,
        executable: String,
    },
    Error(String),
}

/// Owns a local Wine hook socket and exposes received text to the GUI/application thread.
///
/// The socket path and its 256-bit token must be supplied to the injected Wine bridge. The
/// service does not infer that a portal-selected window belongs to a particular Wine process:
/// portals deliberately do not provide that identity.
pub struct WineHookService {
    events: Receiver<WineHookEvent>,
    socket_path: PathBuf,
}

impl WineHookService {
    pub fn bind(
        path: impl Into<PathBuf>,
        authentication_token: [u8; 32],
    ) -> Result<Self, BridgeServerError> {
        let socket_path = path.into();
        let listener = StdUnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        let (events_tx, events) = mpsc::channel();
        thread::Builder::new()
            .name("terratranslate-wine-hook".into())
            .spawn(move || run_hook_server(listener, authentication_token, events_tx))
            .map_err(BridgeServerError::Io)?;
        Ok(Self {
            events,
            socket_path,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn try_recv(&self) -> Result<WineHookEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for WineHookService {
    fn drop(&mut self) {
        // The background listener is terminated with the application process. Removing the path
        // here ensures a normal shutdown does not leave a stale endpoint behind.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn run_hook_server(
    listener: StdUnixListener,
    authentication_token: [u8; 32],
    events: mpsc::Sender<WineHookEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(WineHookEvent::Error(format!(
                "start Wine hook runtime: {error}"
            )));
            return;
        }
    };
    runtime.block_on(async move {
        let listener = match UnixListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = events.send(WineHookEvent::Error(format!(
                    "register Wine hook socket with runtime: {error}"
                )));
                return;
            }
        };
        let server = WineBridgeServer {
            listener,
            authentication_token,
        };
        loop {
            match server.accept().await {
                Ok(connection) => {
                    let events = events.clone();
                    tokio::spawn(receive_bridge_events(connection, events));
                }
                Err(error) => {
                    let _ = events.send(WineHookEvent::Error(error.to_string()));
                }
            }
        }
    });
}

async fn receive_bridge_events(
    mut connection: WineBridgeConnection,
    events: mpsc::Sender<WineHookEvent>,
) {
    let process_id = connection.hello().process_id;
    let executable = connection.hello().executable.clone();
    let _ = events.send(WineHookEvent::Connected {
        process_id,
        executable: executable.clone(),
    });
    loop {
        match connection.receive().await {
            Ok(BridgeMessage::Candidate(candidate)) => {
                if let Err(error) = connection
                    .send(&HostMessage::EnableCandidate(candidate.candidate_id))
                    .await
                {
                    let _ = events.send(WineHookEvent::Error(format!(
                        "could not enable hook candidate from {executable} (PID {process_id}): {error}"
                    )));
                    break;
                }
                if events
                    .send(WineHookEvent::Diagnostic {
                        process_id,
                        executable: executable.clone(),
                        level: "info".into(),
                        message: format!(
                            "enabled {} hook candidate at 0x{:x}",
                            candidate.adapter_id, candidate.address
                        ),
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(BridgeMessage::Text(event)) => {
                if events
                    .send(WineHookEvent::Text {
                        process_id,
                        executable: executable.clone(),
                        event,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(BridgeMessage::Diagnostic { level, message }) => {
                if events
                    .send(WineHookEvent::Diagnostic {
                        process_id,
                        executable: executable.clone(),
                        level,
                        message,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(error) => {
                let _ = events.send(WineHookEvent::Error(format!(
                    "Wine bridge {executable} (PID {process_id}): {error}"
                )));
                break;
            }
        }
    }
    let _ = events.send(WineHookEvent::Disconnected {
        process_id,
        executable,
    });
}

impl WineBridgeConnection {
    pub fn hello(&self) -> &BridgeHello {
        &self.hello
    }

    pub async fn receive(&mut self) -> Result<BridgeMessage, BridgeServerError> {
        read_message(&mut self.stream).await
    }

    pub async fn send(&mut self, message: &HostMessage) -> Result<(), BridgeServerError> {
        write_message(&mut self.stream, message).await
    }
}

async fn read_message<T>(stream: &mut UnixStream) -> Result<T, BridgeServerError>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let length = stream.read_u32_le().await? as usize;
    if length > MAX_WIRE_MESSAGE {
        return Err(terratranslate_wine_protocol::WireError::TooLarge.into());
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(decode(&bytes, MAX_WIRE_MESSAGE)?)
}

async fn write_message<T: serde::Serialize>(
    stream: &mut UnixStream,
    message: &T,
) -> Result<(), BridgeServerError> {
    let bytes = encode(message)?;
    if bytes.len() > MAX_WIRE_MESSAGE {
        return Err(terratranslate_wine_protocol::WireError::TooLarge.into());
    }
    stream.write_u32_le(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use terratranslate_wine_protocol::PROTOCOL_VERSION;
    use uuid::Uuid;

    use super::*;

    fn socket_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "terratranslate-{}-{nonce}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn authenticates_bridge_before_accepting_events() {
        let path = socket_path();
        let token = [7; 32];
        let server = match WineBridgeServer::bind(&path, token) {
            Ok(server) => server,
            Err(BridgeServerError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                // Some CI sandboxes prohibit creating any socket, including local Unix sockets.
                return;
            }
            Err(error) => panic!("could not bind test bridge: {error}"),
        };
        let accept = tokio::spawn(async move { server.accept().await });
        let mut client = UnixStream::connect(&path).await.unwrap();
        write_message(
            &mut client,
            &BridgeMessage::Hello(BridgeHello {
                protocol_version: PROTOCOL_VERSION,
                authentication_token: token,
                bridge_id: Uuid::nil(),
                process_id: 10,
                pointer_width: 64,
                executable: "game.exe".into(),
                adapters: vec!["gdi".into()],
            }),
        )
        .await
        .unwrap();
        let response: HostMessage = read_message(&mut client).await.unwrap();
        assert!(matches!(response, HostMessage::Accept { .. }));
        let connection = accept.await.unwrap().unwrap();
        assert_eq!(connection.hello().process_id, 10);
        drop(connection);
        drop(client);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn forwards_hook_text_from_an_authenticated_bridge() {
        let path = socket_path();
        let token = [9; 32];
        let service = match WineHookService::bind(&path, token) {
            Ok(service) => service,
            Err(BridgeServerError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            Err(error) => panic!("could not bind test bridge service: {error}"),
        };
        let mut client = UnixStream::connect(&path).await.unwrap();
        write_message(
            &mut client,
            &BridgeMessage::Hello(BridgeHello {
                protocol_version: PROTOCOL_VERSION,
                authentication_token: token,
                bridge_id: Uuid::nil(),
                process_id: 42,
                pointer_width: 64,
                executable: "game.exe".into(),
                adapters: vec!["gdi".into()],
            }),
        )
        .await
        .unwrap();
        let _: HostMessage = read_message(&mut client).await.unwrap();
        let candidate_id = Uuid::new_v4();
        write_message(
            &mut client,
            &BridgeMessage::Candidate(terratranslate_wine_protocol::HookCandidate {
                candidate_id,
                adapter_id: "gdi".into(),
                address: 0x1234,
                sample: "こんにちは".into(),
                embeddable: true,
                metadata: Default::default(),
            }),
        )
        .await
        .unwrap();
        let enabled: HostMessage = read_message(&mut client).await.unwrap();
        assert!(matches!(enabled, HostMessage::EnableCandidate(id) if id == candidate_id));
        write_message(
            &mut client,
            &BridgeMessage::Text(terratranslate_wine_protocol::HookTextEvent {
                sequence: 3,
                candidate_id,
                thread_id: 7,
                timestamp_ms: 100,
                text: "こんにちは".into(),
                speaker: Some("栞".into()),
                replacement_capacity_utf16: None,
            }),
        )
        .await
        .unwrap();

        let event = loop {
            match service
                .events
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
            {
                WineHookEvent::Text { event, .. } => break event,
                _ => continue,
            }
        };
        assert_eq!(event.sequence, 3);
        assert_eq!(event.text, "こんにちは");
    }
}
