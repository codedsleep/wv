//! Client/server wire protocol.
//!
//! Messages are length-prefixed bincode frames: a 4-byte little-endian payload
//! length followed by the encoded message. Both directions use the same
//! framing, so a single pair of helpers serves the client and the server.

use anyhow::{bail, Context};
use crossterm::event::Event;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::command::Command;

/// Upper bound on a single frame's payload.
///
/// A full repaint of a very large terminal is well under a megabyte; the cap
/// exists so a corrupt or hostile length prefix cannot make us allocate
/// unbounded memory.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const LENGTH_PREFIX_BYTES: usize = 4;

/// Message sent from an attached client to the session server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientToServer {
    /// First message on a connection: take over rendering at this size.
    Attach {
        cols: u16,
        rows: u16,
        truecolor: bool,
    },
    /// A terminal input event, forwarded verbatim from the client's `EventStream`.
    Input(Event),
    /// The client's terminal changed size.
    Resize { cols: u16, rows: u16 },
    /// Run a command as if it had been typed as a keybinding.
    Exec(Command),
    /// Leave the session running and disconnect.
    Detach,
    /// Shut the session down, killing every pane.
    Quit,
}

/// Message sent from the session server to its attached client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerToClient {
    /// Rendered terminal output to write to stdout verbatim.
    Frame(Vec<u8>),
    /// The server honored a detach; the session keeps running.
    Detached,
    /// The connection is over for the stated reason.
    Exit(ExitReason),
    /// A non-fatal server-side error worth surfacing to the user.
    Error(String),
}

/// Why the server ended a client connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// The session shut down at the user's request.
    Quit,
    /// Another client attached and took over this session.
    TakenOver,
    /// The server is going away (signal, or its last pane exited).
    ServerShutdown,
}

impl ExitReason {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Quit => "session ended",
            Self::TakenOver => "detached: another client attached to this session",
            Self::ServerShutdown => "session server shut down",
        }
    }
}

fn bincode_config() -> bincode::config::Configuration {
    bincode::config::standard()
}

/// Encode a message as a complete length-prefixed frame.
pub fn encode<T: Serialize>(message: &T) -> anyhow::Result<Vec<u8>> {
    let payload = bincode::serde::encode_to_vec(message, bincode_config())
        .context("failed to encode session message")?;
    if payload.len() > MAX_FRAME_BYTES {
        bail!(
            "session message of {} bytes exceeds the {MAX_FRAME_BYTES} byte frame limit",
            payload.len()
        );
    }

    let length = u32::try_from(payload.len())
        .context("session message length does not fit a frame prefix")?;
    let mut frame = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&payload);

    Ok(frame)
}

/// Decode a message from a frame payload (the bytes after the length prefix).
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> anyhow::Result<T> {
    let (message, _consumed) = bincode::serde::decode_from_slice(payload, bincode_config())
        .context("failed to decode session message")?;

    Ok(message)
}

/// Write one framed message.
pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode(message)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;

    Ok(())
}

/// Read one framed message.
///
/// Returns `Ok(None)` when the peer closed the connection cleanly between
/// frames, which is the normal end of a client connection.
pub async fn read_frame<R, T>(reader: &mut R) -> anyhow::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length_bytes = [0u8; LENGTH_PREFIX_BYTES];
    match reader.read_exact(&mut length_bytes).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("failed to read session frame length"),
    }

    let length = u32::from_le_bytes(length_bytes) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("session frame of {length} bytes exceeds the {MAX_FRAME_BYTES} byte frame limit");
    }

    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .context("failed to read session frame payload")?;

    decode(&payload).map(Some)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tokio::io::duplex;

    use super::{
        encode, read_frame, write_frame, ClientToServer, ExitReason, ServerToClient,
        MAX_FRAME_BYTES,
    };
    use crate::command::Command;

    #[tokio::test]
    async fn round_trips_client_messages() {
        let (mut client, mut server) = duplex(4096);
        let sent = vec![
            ClientToServer::Attach {
                cols: 120,
                rows: 40,
                truecolor: true,
            },
            ClientToServer::Input(Event::Key(KeyEvent::new(
                KeyCode::Char('v'),
                KeyModifiers::ALT,
            ))),
            ClientToServer::Resize { cols: 80, rows: 24 },
            ClientToServer::Exec(Command::SwitchWorkspace(3)),
            ClientToServer::Detach,
        ];

        for message in &sent {
            write_frame(&mut client, message)
                .await
                .expect("frame written");
        }
        drop(client);

        for expected in &sent {
            let received: Option<ClientToServer> =
                read_frame(&mut server).await.expect("frame read");
            assert_eq!(received.as_ref(), Some(expected));
        }
        let end: Option<ClientToServer> = read_frame(&mut server).await.expect("clean eof");
        assert_eq!(end, None);
    }

    #[tokio::test]
    async fn round_trips_server_messages() {
        let (mut server, mut client) = duplex(4096);

        write_frame(&mut server, &ServerToClient::Frame(vec![0x1b, b'[', b'H']))
            .await
            .expect("frame written");
        write_frame(&mut server, &ServerToClient::Exit(ExitReason::TakenOver))
            .await
            .expect("exit written");

        let frame: Option<ServerToClient> = read_frame(&mut client).await.expect("frame read");
        assert_eq!(frame, Some(ServerToClient::Frame(vec![0x1b, b'[', b'H'])));
        let exit: Option<ServerToClient> = read_frame(&mut client).await.expect("exit read");
        assert_eq!(exit, Some(ServerToClient::Exit(ExitReason::TakenOver)));
    }

    /// A frame split across writes must still decode: the reader has to keep
    /// pulling until the declared payload length is satisfied.
    #[tokio::test]
    async fn reassembles_a_frame_split_across_writes() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, mut reader) = duplex(8);
        let message = ServerToClient::Frame(vec![7u8; 64]);
        let bytes = encode(&message).expect("message encoded");

        let write_task = tokio::spawn(async move {
            for chunk in bytes.chunks(3) {
                writer.write_all(chunk).await.expect("chunk written");
                writer.flush().await.expect("chunk flushed");
            }
        });

        let received: Option<ServerToClient> = read_frame(&mut reader).await.expect("frame read");
        write_task.await.expect("writer finished");

        assert_eq!(received, Some(message));
    }

    #[tokio::test]
    async fn rejects_an_oversized_length_prefix() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, mut reader) = duplex(64);
        let length = u32::try_from(MAX_FRAME_BYTES + 1).expect("test length fits u32");
        writer
            .write_all(&length.to_le_bytes())
            .await
            .expect("length written");
        writer.flush().await.expect("length flushed");

        let result: anyhow::Result<Option<ServerToClient>> = read_frame(&mut reader).await;

        assert!(result.is_err(), "oversized frame length must be rejected");
    }
}
