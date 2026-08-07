use std::path::Path;

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
}
