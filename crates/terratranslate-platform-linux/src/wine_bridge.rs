use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use terratranslate_wine_protocol::{
    BridgeHello, BridgeMessage, ExecutableIdentity, HookCandidate, HookPlatform, HookRuntime,
    HookTextEvent, HostMessage, MAX_ADAPTERS, MAX_IDENTITY_BYTES, MAX_METADATA_ENTRIES,
    MAX_SAMPLE_BYTES, MAX_TEXT_BYTES, MAX_WIRE_MESSAGE_BYTES, PROTOCOL_VERSION,
    ProcessArchitecture, StableCandidateKey, decode, encode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc as tokio_mpsc;

const EVENT_QUEUE_CAPACITY: usize = 256;
const CONNECTION_COMMAND_CAPACITY: usize = 64;
const MAX_CANDIDATES_PER_CONNECTION: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum BridgeServerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire protocol error: {0}")]
    Wire(#[from] terratranslate_wine_protocol::WireError),
    #[error("first hook-client message was not a hello")]
    MissingHello,
    #[error("hook-client authentication failed")]
    Authentication,
    #[error("hook protocol version {0} is unsupported")]
    Version(u32),
    #[error("hook client did not authenticate before the handshake deadline")]
    HandshakeTimeout,
    #[error("invalid hook message: {0}")]
    InvalidMessage(&'static str),
}

pub struct HookBridgeServer {
    listener: UnixListener,
    authentication_token: [u8; 32],
}

impl HookBridgeServer {
    pub fn bind(
        path: impl AsRef<Path>,
        authentication_token: [u8; 32],
    ) -> Result<Self, BridgeServerError> {
        Ok(Self {
            listener: UnixListener::bind(path)?,
            authentication_token,
        })
    }

    pub async fn accept(&self) -> Result<HookBridgeConnection, BridgeServerError> {
        let (mut stream, _) = self.listener.accept().await?;
        let first_message = tokio::time::timeout(
            Duration::from_secs(2),
            read_message::<BridgeMessage>(&mut stream),
        )
        .await
        .map_err(|_| BridgeServerError::HandshakeTimeout)??;
        let hello = match first_message {
            BridgeMessage::Hello(hello) => hello,
            _ => return Err(BridgeServerError::MissingHello),
        };
        if !tokens_equal(&hello.authentication_token, &self.authentication_token) {
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
        validate_hello(&hello)?;
        write_message(
            &mut stream,
            &HostMessage::Accept {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;
        Ok(HookBridgeConnection { stream, hello })
    }
}

pub struct HookBridgeConnection {
    stream: UnixStream,
    hello: BridgeHello,
}

impl HookBridgeConnection {
    pub fn hello(&self) -> &BridgeHello {
        &self.hello
    }

    pub async fn receive(&mut self) -> Result<BridgeMessage, BridgeServerError> {
        let message = read_message(&mut self.stream).await?;
        validate_bridge_message(&message)?;
        Ok(message)
    }

    pub async fn send(&mut self, message: &HostMessage) -> Result<(), BridgeServerError> {
        write_message(&mut self.stream, message).await
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookBridgeIdentity {
    pub bridge_id: uuid::Uuid,
    pub platform: HookPlatform,
    pub runtime: HookRuntime,
    pub process_id: u32,
    pub architecture: ProcessArchitecture,
    pub executable: ExecutableIdentity,
    pub adapters: Vec<String>,
}

impl From<&BridgeHello> for HookBridgeIdentity {
    fn from(hello: &BridgeHello) -> Self {
        Self {
            bridge_id: hello.bridge_id,
            platform: hello.platform.clone(),
            runtime: hello.runtime.clone(),
            process_id: hello.process_id,
            architecture: hello.architecture.clone(),
            executable: hello.executable.clone(),
            adapters: hello.adapters.clone(),
        }
    }
}

/// Events from authenticated native Linux or Wine/Proton hook clients.
#[derive(Clone, Debug)]
pub enum HookEvent {
    Connected {
        bridge: HookBridgeIdentity,
    },
    Candidate {
        bridge: HookBridgeIdentity,
        candidate: HookCandidate,
    },
    Text {
        bridge: HookBridgeIdentity,
        event: HookTextEvent,
    },
    Diagnostic {
        bridge: HookBridgeIdentity,
        level: String,
        message: String,
    },
    Disconnected {
        bridge: HookBridgeIdentity,
    },
    Error(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HookControlError {
    #[error("hook bridge {0} is no longer connected")]
    NotConnected(uuid::Uuid),
    #[error("hook bridge {0} command queue is full")]
    QueueFull(uuid::Uuid),
    #[error("candidate {candidate_id} is not known on hook bridge {bridge_id}")]
    UnknownCandidate {
        bridge_id: uuid::Uuid,
        candidate_id: uuid::Uuid,
    },
}

/// Owns a bounded, authenticated local hook socket and routes control to exact connections.
pub struct HookService {
    events: Receiver<HookEvent>,
    socket_path: PathBuf,
    connections: ConnectionRegistry,
    stopping: Arc<AtomicBool>,
}

#[derive(Clone)]
struct ConnectionControl {
    commands: tokio_mpsc::Sender<ConnectionCommand>,
    candidates: Arc<Mutex<HashMap<uuid::Uuid, StableCandidateKey>>>,
    enabled: Arc<Mutex<HashSet<uuid::Uuid>>>,
}

type ConnectionRegistry = Arc<Mutex<HashMap<uuid::Uuid, ConnectionControl>>>;

#[derive(Clone, Copy, Debug)]
enum ConnectionCommand {
    Enable(uuid::Uuid),
    Disable(uuid::Uuid),
    Shutdown,
}

impl HookService {
    pub fn bind(
        path: impl Into<PathBuf>,
        authentication_token: [u8; 32],
    ) -> Result<Self, BridgeServerError> {
        let socket_path = path.into();
        let listener = StdUnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        let (events_tx, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let connections = Arc::new(Mutex::new(HashMap::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("terratranslate-hook-service".into())
            .spawn({
                let connections = Arc::clone(&connections);
                let stopping = Arc::clone(&stopping);
                move || {
                    run_hook_server(
                        listener,
                        authentication_token,
                        events_tx,
                        connections,
                        stopping,
                    )
                }
            })
            .map_err(BridgeServerError::Io)?;
        Ok(Self {
            events,
            socket_path,
            connections,
            stopping,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn try_recv(&self) -> Result<HookEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    pub fn enable_candidate(
        &self,
        bridge_id: uuid::Uuid,
        candidate_id: uuid::Uuid,
    ) -> Result<(), HookControlError> {
        let control = self.connection(bridge_id)?;
        if !control
            .candidates
            .lock()
            .expect("hook candidate registry poisoned")
            .contains_key(&candidate_id)
        {
            return Err(HookControlError::UnknownCandidate {
                bridge_id,
                candidate_id,
            });
        }
        control
            .enabled
            .lock()
            .expect("enabled candidate registry poisoned")
            .insert(candidate_id);
        if let Err(error) =
            send_command(&control, bridge_id, ConnectionCommand::Enable(candidate_id))
        {
            control
                .enabled
                .lock()
                .expect("enabled candidate registry poisoned")
                .remove(&candidate_id);
            return Err(error);
        }
        Ok(())
    }

    pub fn disable_candidate(
        &self,
        bridge_id: uuid::Uuid,
        candidate_id: uuid::Uuid,
    ) -> Result<(), HookControlError> {
        let control = self.connection(bridge_id)?;
        // Stop local routing synchronously; producer notification may follow on the I/O task.
        control
            .enabled
            .lock()
            .expect("enabled candidate registry poisoned")
            .remove(&candidate_id);
        send_command(
            &control,
            bridge_id,
            ConnectionCommand::Disable(candidate_id),
        )
    }

    pub fn shutdown(&self, bridge_id: uuid::Uuid) -> Result<(), HookControlError> {
        let control = self.connection(bridge_id)?;
        control
            .enabled
            .lock()
            .expect("enabled candidate registry poisoned")
            .clear();
        send_command(&control, bridge_id, ConnectionCommand::Shutdown)
    }

    fn connection(&self, bridge_id: uuid::Uuid) -> Result<ConnectionControl, HookControlError> {
        self.connections
            .lock()
            .expect("hook connection registry poisoned")
            .get(&bridge_id)
            .cloned()
            .ok_or(HookControlError::NotConnected(bridge_id))
    }
}

impl Drop for HookService {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        for control in self
            .connections
            .lock()
            .expect("hook connection registry poisoned")
            .values()
        {
            control
                .enabled
                .lock()
                .expect("enabled candidate registry poisoned")
                .clear();
            let _ = control.commands.try_send(ConnectionCommand::Shutdown);
        }
        // Wake the blocking async accept so the server thread can observe `stopping`.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn run_hook_server(
    listener: StdUnixListener,
    authentication_token: [u8; 32],
    events: SyncSender<HookEvent>,
    connections: ConnectionRegistry,
    stopping: Arc<AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            send_event(
                &events,
                HookEvent::Error(format!("start hook runtime: {error}")),
            );
            return;
        }
    };
    runtime.block_on(async move {
        let listener = match UnixListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                send_event(
                    &events,
                    HookEvent::Error(format!("register hook socket with runtime: {error}")),
                );
                return;
            }
        };
        let server = HookBridgeServer {
            listener,
            authentication_token,
        };
        while !stopping.load(Ordering::Acquire) {
            match server.accept().await {
                Ok(connection) => {
                    let events = events.clone();
                    let connections = Arc::clone(&connections);
                    tokio::spawn(receive_hook_events(connection, events, connections));
                }
                Err(_) if stopping.load(Ordering::Acquire) => break,
                Err(error) => send_event(&events, HookEvent::Error(error.to_string())),
            }
        }
    });
}

async fn receive_hook_events(
    mut connection: HookBridgeConnection,
    events: SyncSender<HookEvent>,
    connections: ConnectionRegistry,
) {
    let bridge = HookBridgeIdentity::from(connection.hello());
    let bridge_id = bridge.bridge_id;
    let (commands_tx, mut commands_rx) = tokio_mpsc::channel(CONNECTION_COMMAND_CAPACITY);
    let candidates = Arc::new(Mutex::new(HashMap::<uuid::Uuid, StableCandidateKey>::new()));
    let enabled = Arc::new(Mutex::new(HashSet::<uuid::Uuid>::new()));
    {
        let mut registered = connections
            .lock()
            .expect("hook connection registry poisoned");
        match registered.entry(bridge_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(ConnectionControl {
                    commands: commands_tx,
                    candidates: Arc::clone(&candidates),
                    enabled: Arc::clone(&enabled),
                });
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                send_event(
                    &events,
                    HookEvent::Error(format!("duplicate hook bridge connection ID {bridge_id}")),
                );
                return;
            }
        }
    }
    send_event(
        &events,
        HookEvent::Connected {
            bridge: bridge.clone(),
        },
    );

    loop {
        tokio::select! {
            command = commands_rx.recv() => {
                let Some(command) = command else { break };
                let result = match command {
                    ConnectionCommand::Enable(candidate_id) => {
                        connection.send(&HostMessage::EnableCandidate(candidate_id)).await
                    }
                    ConnectionCommand::Disable(candidate_id) => {
                        connection.send(&HostMessage::DisableCandidate(candidate_id)).await
                    }
                    ConnectionCommand::Shutdown => {
                        let _ = connection.send(&HostMessage::Shutdown).await;
                        break;
                    }
                };
                if let Err(error) = result {
                    send_event(&events, HookEvent::Error(format!(
                        "hook bridge {} (PID {}): {error}",
                        bridge.executable.path, bridge.process_id
                    )));
                    break;
                }
            }
            message = connection.receive() => match message {
                Ok(BridgeMessage::Candidate(candidate)) => {
                    let (over_limit, identity_changed) = {
                        let candidates = candidates.lock().expect("hook candidate registry poisoned");
                        (
                            candidates.len() >= MAX_CANDIDATES_PER_CONNECTION
                                && !candidates.contains_key(&candidate.candidate_id),
                            candidates
                                .get(&candidate.candidate_id)
                                .is_some_and(|key| key != &candidate.stable_key),
                        )
                    };
                    if over_limit {
                        send_event(&events, HookEvent::Diagnostic {
                            bridge: bridge.clone(),
                            level: "warning".into(),
                            message: "candidate limit reached; ignoring additional candidates".into(),
                        });
                        continue;
                    }
                    if identity_changed {
                        send_event(&events, HookEvent::Diagnostic {
                            bridge: bridge.clone(),
                            level: "warning".into(),
                            message: format!(
                                "candidate {} attempted to change stable identity",
                                candidate.candidate_id
                            ),
                        });
                        continue;
                    }
                    candidates
                        .lock()
                        .expect("hook candidate registry poisoned")
                        .insert(candidate.candidate_id, candidate.stable_key.clone());
                    send_event(&events, HookEvent::Candidate {
                        bridge: bridge.clone(),
                        candidate,
                    });
                }
                Ok(BridgeMessage::Text(event)) => {
                    let identity_matches = candidates
                        .lock()
                        .expect("hook candidate registry poisoned")
                        .get(&event.candidate_id)
                        .is_some_and(|stable_key| stable_key == &event.stable_key);
                    let is_enabled = enabled
                        .lock()
                        .expect("enabled candidate registry poisoned")
                        .contains(&event.candidate_id);
                    if identity_matches && is_enabled {
                        send_event(&events, HookEvent::Text {
                            bridge: bridge.clone(),
                            event,
                        });
                    }
                }
                Ok(BridgeMessage::Diagnostic { level, message }) => {
                    send_event(&events, HookEvent::Diagnostic {
                        bridge: bridge.clone(),
                        level,
                        message,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    send_event(&events, HookEvent::Error(format!(
                        "hook bridge {} (PID {}): {error}",
                        bridge.executable.path, bridge.process_id
                    )));
                    break;
                }
            }
        }
    }
    connections
        .lock()
        .expect("hook connection registry poisoned")
        .remove(&bridge_id);
    enabled
        .lock()
        .expect("enabled candidate registry poisoned")
        .clear();
    send_event(&events, HookEvent::Disconnected { bridge });
}

fn send_event(events: &SyncSender<HookEvent>, event: HookEvent) {
    // Discovery and text are observational. A slow GUI must not backpressure hook clients.
    let _ = events.try_send(event);
}

fn send_command(
    control: &ConnectionControl,
    bridge_id: uuid::Uuid,
    command: ConnectionCommand,
) -> Result<(), HookControlError> {
    control
        .commands
        .try_send(command)
        .map_err(|error| match error {
            tokio_mpsc::error::TrySendError::Full(_) => HookControlError::QueueFull(bridge_id),
            tokio_mpsc::error::TrySendError::Closed(_) => HookControlError::NotConnected(bridge_id),
        })
}

fn validate_hello(hello: &BridgeHello) -> Result<(), BridgeServerError> {
    if hello.executable.path.is_empty() || hello.executable.path.len() > MAX_IDENTITY_BYTES {
        return Err(BridgeServerError::InvalidMessage(
            "invalid executable identity",
        ));
    }
    let runtime_is_invalid = match &hello.runtime {
        HookRuntime::Other(runtime) => runtime.is_empty() || runtime.len() > MAX_IDENTITY_BYTES,
        _ => false,
    };
    let architecture_is_invalid = match &hello.architecture {
        ProcessArchitecture::Other(architecture) => {
            architecture.is_empty() || architecture.len() > MAX_IDENTITY_BYTES
        }
        _ => false,
    };
    if runtime_is_invalid
        || architecture_is_invalid
        || hello.adapters.len() > MAX_ADAPTERS
        || hello
            .adapters
            .iter()
            .any(|adapter| adapter.is_empty() || adapter.len() > MAX_IDENTITY_BYTES)
    {
        return Err(BridgeServerError::InvalidMessage("invalid adapter list"));
    }
    Ok(())
}

fn validate_bridge_message(message: &BridgeMessage) -> Result<(), BridgeServerError> {
    match message {
        BridgeMessage::Hello(_) => Err(BridgeServerError::InvalidMessage(
            "hello is only valid as the first message",
        )),
        BridgeMessage::Candidate(candidate) => validate_candidate(candidate),
        BridgeMessage::Text(event) => {
            if event.text.len() > MAX_TEXT_BYTES
                || event
                    .speaker
                    .as_ref()
                    .is_some_and(|speaker| speaker.len() > MAX_SAMPLE_BYTES)
            {
                Err(BridgeServerError::InvalidMessage("text event is too large"))
            } else {
                Ok(())
            }
        }
        BridgeMessage::Diagnostic { level, message } => {
            if level.len() > MAX_SAMPLE_BYTES || message.len() > MAX_TEXT_BYTES {
                Err(BridgeServerError::InvalidMessage("diagnostic is too large"))
            } else {
                Ok(())
            }
        }
        BridgeMessage::ReplacementResult { reason, .. } => {
            if reason
                .as_ref()
                .is_some_and(|reason| reason.len() > MAX_TEXT_BYTES)
            {
                Err(BridgeServerError::InvalidMessage(
                    "replacement result is too large",
                ))
            } else {
                Ok(())
            }
        }
        BridgeMessage::Pong(_) => Ok(()),
    }
}

fn validate_candidate(candidate: &HookCandidate) -> Result<(), BridgeServerError> {
    if candidate.stable_key.as_str().is_empty()
        || candidate.stable_key.as_str().len() > MAX_IDENTITY_BYTES
        || candidate.adapter_id.is_empty()
        || candidate.adapter_id.len() > MAX_IDENTITY_BYTES
        || candidate.api.is_empty()
        || candidate.api.len() > MAX_IDENTITY_BYTES
        || candidate.sample.len() > MAX_SAMPLE_BYTES
        || candidate
            .caller_module
            .as_ref()
            .is_some_and(|module| module.len() > MAX_IDENTITY_BYTES)
        || candidate.metadata.len() > MAX_METADATA_ENTRIES
        || candidate
            .metadata
            .iter()
            .any(|(key, value)| key.len() > MAX_IDENTITY_BYTES || value.len() > MAX_SAMPLE_BYTES)
    {
        Err(BridgeServerError::InvalidMessage("invalid hook candidate"))
    } else {
        Ok(())
    }
}

fn tokens_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn read_message<T>(stream: &mut UnixStream) -> Result<T, BridgeServerError>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let length = stream.read_u32_le().await? as usize;
    if length > MAX_WIRE_MESSAGE_BYTES {
        return Err(terratranslate_wine_protocol::WireError::TooLarge.into());
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(decode(&bytes, MAX_WIRE_MESSAGE_BYTES)?)
}

async fn write_message<T: serde::Serialize>(
    stream: &mut UnixStream,
    message: &T,
) -> Result<(), BridgeServerError> {
    let bytes = encode(message)?;
    stream.write_u32_le(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

// Source-compatible names for downstream Wine integration while it migrates to generic naming.
pub type WineBridgeServer = HookBridgeServer;
pub type WineBridgeConnection = HookBridgeConnection;
pub type WineHookService = HookService;
pub type WineHookEvent = HookEvent;

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use uuid::Uuid;

    use super::*;

    fn socket_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "terratranslate-{label}-{}-{nonce}.sock",
            std::process::id()
        ))
    }

    fn hello(token: [u8; 32], bridge_id: Uuid) -> BridgeHello {
        BridgeHello {
            protocol_version: PROTOCOL_VERSION,
            authentication_token: token,
            bridge_id,
            platform: HookPlatform::Windows,
            runtime: HookRuntime::Wine,
            process_id: 42,
            architecture: ProcessArchitecture::X86_64,
            executable: ExecutableIdentity {
                path: "game.exe".into(),
                image_id: Some("pe:1234".into()),
            },
            adapters: vec!["gdi".into()],
        }
    }

    fn candidate(id: Uuid) -> HookCandidate {
        let executable = ExecutableIdentity {
            path: "game.exe".into(),
            image_id: Some("pe:1234".into()),
        };
        HookCandidate {
            candidate_id: id,
            stable_key: StableCandidateKey::derive(
                &HookPlatform::Windows,
                &executable,
                "gdi",
                Some("game.exe"),
                Some(0x1234),
            ),
            adapter_id: "gdi".into(),
            api: "TextOutW".into(),
            caller_module: Some("game.exe".into()),
            module_offset: Some(0x1234),
            sample: "こんにちは".into(),
            embeddable: false,
            metadata: Default::default(),
        }
    }

    async fn connect(path: &Path, hello: BridgeHello) -> UnixStream {
        let mut client = UnixStream::connect(path).await.unwrap();
        write_message(&mut client, &BridgeMessage::Hello(hello))
            .await
            .unwrap();
        let response: HostMessage = read_message(&mut client).await.unwrap();
        assert_eq!(
            response,
            HostMessage::Accept {
                protocol_version: PROTOCOL_VERSION
            }
        );
        client
    }

    fn bind_service(path: &Path, token: [u8; 32]) -> Option<HookService> {
        match HookService::bind(path, token) {
            Ok(service) => Some(service),
            Err(BridgeServerError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                None
            }
            Err(error) => panic!("could not bind test hook service: {error}"),
        }
    }

    fn recv_until(service: &HookService, predicate: impl Fn(&HookEvent) -> bool) -> HookEvent {
        loop {
            let event = service.events.recv_timeout(Duration::from_secs(2)).unwrap();
            if predicate(&event) {
                return event;
            }
        }
    }

    #[tokio::test]
    async fn rejects_bad_authentication_and_protocol_versions() {
        let path = socket_path("auth");
        let token = [7; 32];
        let server = match HookBridgeServer::bind(&path, token) {
            Ok(server) => server,
            Err(BridgeServerError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            Err(error) => panic!("could not bind test bridge: {error}"),
        };

        let accept = tokio::spawn(async move { server.accept().await });
        let mut client = UnixStream::connect(&path).await.unwrap();
        write_message(
            &mut client,
            &BridgeMessage::Hello(hello([8; 32], Uuid::new_v4())),
        )
        .await
        .unwrap();
        assert!(matches!(
            read_message::<HostMessage>(&mut client).await.unwrap(),
            HostMessage::Reject { .. }
        ));
        assert!(matches!(
            accept.await.unwrap(),
            Err(BridgeServerError::Authentication)
        ));
        drop(client);
        std::fs::remove_file(path).unwrap();

        let path = socket_path("version");
        let server = HookBridgeServer::bind(&path, token).unwrap();
        let accept = tokio::spawn(async move { server.accept().await });
        let mut client = UnixStream::connect(&path).await.unwrap();
        let mut versioned = hello(token, Uuid::new_v4());
        versioned.protocol_version += 1;
        write_message(&mut client, &BridgeMessage::Hello(versioned))
            .await
            .unwrap();
        assert!(matches!(
            read_message::<HostMessage>(&mut client).await.unwrap(),
            HostMessage::Reject { .. }
        ));
        assert!(matches!(
            accept.await.unwrap(),
            Err(BridgeServerError::Version(_))
        ));
        drop(client);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn requires_explicit_enable_and_routes_commands_to_the_right_connection() {
        let path = socket_path("routing");
        let token = [9; 32];
        let Some(service) = bind_service(&path, token) else {
            return;
        };
        let first_bridge = Uuid::new_v4();
        let second_bridge = Uuid::new_v4();
        let mut first = connect(&path, hello(token, first_bridge)).await;
        let mut second = connect(&path, hello(token, second_bridge)).await;
        let candidate_id = Uuid::new_v4();
        let candidate = candidate(candidate_id);
        write_message(&mut first, &BridgeMessage::Candidate(candidate.clone()))
            .await
            .unwrap();
        write_message(&mut second, &BridgeMessage::Candidate(candidate.clone()))
            .await
            .unwrap();
        let mut discovered = HashSet::new();
        while discovered.len() < 2 {
            if let HookEvent::Candidate { bridge, .. } =
                service.events.recv_timeout(Duration::from_secs(2)).unwrap()
            {
                discovered.insert(bridge.bridge_id);
            }
        }
        assert_eq!(discovered, HashSet::from([first_bridge, second_bridge]));

        let text = BridgeMessage::Text(HookTextEvent {
            sequence: 1,
            candidate_id,
            stable_key: candidate.stable_key.clone(),
            thread_id: 7,
            timestamp_ms: 100,
            text: "disabled".into(),
            speaker: None,
            replacement_capacity_utf16: None,
        });
        write_message(&mut first, &text).await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(service.events.try_recv().is_err());

        service
            .enable_candidate(first_bridge, candidate_id)
            .unwrap();
        assert!(matches!(
            read_message::<HostMessage>(&mut first).await.unwrap(),
            HostMessage::EnableCandidate(id) if id == candidate_id
        ));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                read_message::<HostMessage>(&mut second)
            )
            .await
            .is_err()
        );

        write_message(
            &mut first,
            &BridgeMessage::Text(HookTextEvent {
                sequence: 2,
                candidate_id,
                stable_key: candidate.stable_key,
                thread_id: 7,
                timestamp_ms: 101,
                text: "enabled".into(),
                speaker: None,
                replacement_capacity_utf16: None,
            }),
        )
        .await
        .unwrap();
        assert!(matches!(
            recv_until(&service, |event| matches!(event, HookEvent::Text { .. })),
            HookEvent::Text { bridge, event }
                if bridge.bridge_id == first_bridge && event.text == "enabled"
        ));
    }

    #[tokio::test]
    async fn disable_is_immediate_and_disconnect_removes_command_routing() {
        let path = socket_path("disconnect");
        let token = [11; 32];
        let Some(service) = bind_service(&path, token) else {
            return;
        };
        let bridge_id = Uuid::new_v4();
        let mut client = connect(&path, hello(token, bridge_id)).await;
        let candidate_id = Uuid::new_v4();
        let candidate = candidate(candidate_id);
        write_message(&mut client, &BridgeMessage::Candidate(candidate))
            .await
            .unwrap();
        recv_until(&service, |event| {
            matches!(event, HookEvent::Candidate { .. })
        });
        service.enable_candidate(bridge_id, candidate_id).unwrap();
        let _: HostMessage = read_message(&mut client).await.unwrap();
        service.disable_candidate(bridge_id, candidate_id).unwrap();
        assert!(matches!(
            read_message::<HostMessage>(&mut client).await.unwrap(),
            HostMessage::DisableCandidate(id) if id == candidate_id
        ));

        service.shutdown(bridge_id).unwrap();
        assert!(matches!(
            read_message::<HostMessage>(&mut client).await.unwrap(),
            HostMessage::Shutdown
        ));
        recv_until(
            &service,
            |event| matches!(event, HookEvent::Disconnected { bridge } if bridge.bridge_id == bridge_id),
        );
        assert_eq!(
            service.enable_candidate(bridge_id, candidate_id),
            Err(HookControlError::NotConnected(bridge_id))
        );
    }

    #[tokio::test]
    async fn stable_candidate_identity_survives_reconnection() {
        let path = socket_path("reconnect");
        let token = [13; 32];
        let Some(service) = bind_service(&path, token) else {
            return;
        };
        let first_bridge = Uuid::new_v4();
        let mut first = connect(&path, hello(token, first_bridge)).await;
        let first_candidate = candidate(Uuid::new_v4());
        let expected_key = first_candidate.stable_key.clone();
        write_message(&mut first, &BridgeMessage::Candidate(first_candidate))
            .await
            .unwrap();
        let first_key = match recv_until(
            &service,
            |event| matches!(event, HookEvent::Candidate { bridge, .. } if bridge.bridge_id == first_bridge),
        ) {
            HookEvent::Candidate { candidate, .. } => candidate.stable_key,
            _ => unreachable!(),
        };
        drop(first);
        recv_until(
            &service,
            |event| matches!(event, HookEvent::Disconnected { bridge } if bridge.bridge_id == first_bridge),
        );

        let second_bridge = Uuid::new_v4();
        let mut second = connect(&path, hello(token, second_bridge)).await;
        let second_candidate = candidate(Uuid::new_v4());
        write_message(&mut second, &BridgeMessage::Candidate(second_candidate))
            .await
            .unwrap();
        let second_key = match recv_until(
            &service,
            |event| matches!(event, HookEvent::Candidate { bridge, .. } if bridge.bridge_id == second_bridge),
        ) {
            HookEvent::Candidate { candidate, .. } => candidate.stable_key,
            _ => unreachable!(),
        };
        assert_eq!(first_key, expected_key);
        assert_eq!(second_key, expected_key);
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocation() {
        let path = socket_path("limit");
        let server = HookBridgeServer::bind(&path, [3; 32]).unwrap();
        let accept = tokio::spawn(async move { server.accept().await });
        let mut client = UnixStream::connect(&path).await.unwrap();
        client
            .write_u32_le((MAX_WIRE_MESSAGE_BYTES + 1) as u32)
            .await
            .unwrap();
        assert!(matches!(
            accept.await.unwrap(),
            Err(BridgeServerError::Wire(
                terratranslate_wine_protocol::WireError::TooLarge
            ))
        ));
        drop(client);
        std::fs::remove_file(path).unwrap();
    }
}
