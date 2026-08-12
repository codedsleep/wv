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

/// Wire-format version, sent as the first frame of every connection.
///
/// Bincode encodes enum variants positionally, so a client built against a
/// different `Command` shape would not fail to decode — it would decode into
/// the wrong thing. The handshake turns that silent corruption into a clear
/// error.
///
/// **Bump this whenever `ClientToServer`, `ServerToClient`, or anything they
/// carry changes shape — and `Command` counts.** That is the part that got
/// missed: `Command` lives in another file, gained a dozen variants across
/// several changes, and none of them touched this number, so two builds with
/// incompatible command encodings both claimed to speak v2. See
/// [`command_shape_tripwire`] below, which makes the omission a compile error.
pub const PROTOCOL_VERSION: u32 = 5;

/// Fails to compile when [`Command`] changes shape, as a reminder to bump
/// [`PROTOCOL_VERSION`].
///
/// This match is deliberately exhaustive and deliberately lives *here* rather
/// than beside the enum: adding a command means editing this file, which puts
/// the version constant on screen at the moment it needs changing.
///
/// If you are reading this because the compiler sent you: add your variant
/// below, then bump `PROTOCOL_VERSION`.
#[allow(dead_code)]
fn command_shape_tripwire(command: &Command) {
    match command {
        Command::SplitWindow { .. }
        | Command::SelectPane { .. }
        | Command::SelectWindow { .. }
        | Command::KillPane { .. }
        | Command::DetachClient { .. }
        | Command::KillSession { .. }
        | Command::DisplayMessage { .. }
        | Command::SendKeys { .. }
        | Command::RespawnPane { .. }
        | Command::NewWindow { .. }
        | Command::KillWindow { .. }
        | Command::RenameWindow { .. }
        | Command::RenameSession { .. }
        | Command::ResizePane { .. }
        | Command::SwapPane { .. }
        | Command::RotateWindow { .. }
        | Command::SelectLayout { .. }
        | Command::CapturePane { .. }
        | Command::List { .. }
        | Command::BindKey { .. }
        | Command::UnbindKey { .. }
        | Command::ListKeys { .. }
        | Command::SetOption { .. }
        | Command::ShowOptions { .. }
        | Command::BreakPane { .. }
        | Command::JoinPane { .. }
        | Command::RunShell { .. }
        | Command::IfShell { .. }
        | Command::WaitFor { .. }
        | Command::RefreshClient
        | Command::CommandPrompt { .. } => {}
    }
}

/// Message sent from an attached client to the session server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientToServer {
    /// Version handshake. Must be the first frame on every connection that
    /// sends anything at all; a connect-and-close is still a liveness probe.
    Hello { protocol_version: u32 },
    /// Take over rendering at this size.
    Attach {
        cols: u16,
        rows: u16,
        truecolor: bool,
        /// Whether this terminal is on the far side of an SSH connection, and
        /// so probably inside another weave. The server uses it to decide
        /// where the leader modifier lives; see [`crate::input::nesting`].
        ///
        /// It has to come from the client: the server may have been started
        /// long ago, in a different login, from an environment that says
        /// nothing about the terminal now attached to it.
        nested: bool,
    },
    /// A terminal input event, forwarded verbatim from the client's `EventStream`.
    Input(Event),
    /// The client's terminal changed size.
    Resize { cols: u16, rows: u16 },
    /// Run a command and reply with what it produced.
    ///
    /// The id is echoed in the [`ServerToClient::Reply`], so a client with
    /// several requests in flight can match them up. `wv exec` sends one
    /// request per connection and uses id 0.
    Request { id: u64, command: Command },
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
    /// The outcome of a [`ClientToServer::Request`], tagged with its id.
    Reply { id: u64, result: CommandResult },
}

/// What running a command produced.
///
/// A command that fails is still a completed request: the session says why and
/// carries on. Only a broken connection is an error at the transport level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandResult {
    /// The command ran. `output` is what it printed, usually empty.
    Ok { output: String },
    /// The command was understood but could not be applied.
    Error { message: String },
}

impl CommandResult {
    /// An `Ok` with nothing to print, which is what most commands produce.
    pub fn empty() -> Self {
        Self::Ok {
            output: String::new(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }
}

/// Why the server ended a client connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// The session shut down at the user's request.
    Quit,
    /// Another client attached and took over this session.
    ///
    /// weave no longer evicts — several clients share a session — so nothing
    /// sends this. It stays so a client can still explain the message if an
    /// older server sends it.
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

/// Send the version handshake that opens every connection.
pub async fn write_hello<W>(writer: &mut W) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_frame(
        writer,
        &ClientToServer::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .context("failed to send the protocol handshake")
}

/// Check a peer's handshake against ours.
pub fn check_protocol_version(peer: u32) -> anyhow::Result<()> {
    if peer == PROTOCOL_VERSION {
        return Ok(());
    }

    bail!(
        "weave protocol mismatch: this session speaks v{PROTOCOL_VERSION} but the client speaks \
         v{peer}. The session server is running an older or newer `wv` than the one you just \
         ran — end the session (`wv kill-session`) or reinstall so both sides match."
    )
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
        check_protocol_version, encode, read_frame, write_frame, write_hello, ClientToServer,
        ExitReason, ServerToClient, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    };
    use crate::command::{Command, Target, WindowRef};

    #[tokio::test]
    async fn round_trips_client_messages() {
        let (mut client, mut server) = duplex(4096);
        let sent = vec![
            ClientToServer::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            ClientToServer::Attach {
                cols: 120,
                rows: 40,
                truecolor: true,
                nested: false,
            },
            ClientToServer::Input(Event::Key(KeyEvent::new(
                KeyCode::Char('v'),
                KeyModifiers::ALT,
            ))),
            ClientToServer::Resize { cols: 80, rows: 24 },
            ClientToServer::Request {
                id: 7,
                command: Command::SelectWindow {
                    target: Target {
                        window: Some(WindowRef::Index(3)),
                        ..Target::default()
                    },
                    create: false,
                },
            },
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
    async fn hello_carries_our_version_and_only_ours_is_accepted() {
        let (mut client, mut server) = duplex(4096);
        write_hello(&mut client).await.expect("hello written");

        let received: Option<ClientToServer> = read_frame(&mut server).await.expect("hello read");
        assert_eq!(
            received,
            Some(ClientToServer::Hello {
                protocol_version: PROTOCOL_VERSION,
            })
        );

        assert!(check_protocol_version(PROTOCOL_VERSION).is_ok());
        let error = check_protocol_version(PROTOCOL_VERSION + 1)
            .expect_err("a mismatched version is fatal")
            .to_string();
        assert!(error.contains("protocol mismatch"), "{error}");
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

    /// `read_frame` is built on `read_exact`, which is not cancel-safe: a read
    /// dropped mid-frame takes the bytes it already consumed with it. Callers
    /// must therefore never poll it from a `select!` arm — the client reads
    /// frames on a dedicated task and selects on the channel instead.
    #[tokio::test]
    async fn a_read_cancelled_mid_frame_loses_the_bytes_it_consumed() {
        use std::time::Duration;

        use tokio::io::AsyncWriteExt;

        let (mut writer, mut reader) = duplex(4096);
        let message = ServerToClient::Frame(vec![7u8; 32]);
        let bytes = encode(&message).expect("message encoded");
        let (head, tail) = bytes.split_at(8);

        writer.write_all(head).await.expect("head written");
        writer.flush().await.expect("head flushed");

        // Drop the read while it is still waiting on the payload, exactly as a
        // `select!` arm losing the race would.
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            read_frame::<_, ServerToClient>(&mut reader),
        )
        .await;
        assert!(cancelled.is_err(), "the read should still be pending");

        writer.write_all(tail).await.expect("tail written");
        writer.flush().await.expect("tail flushed");

        // The stream is now desynced: the consumed prefix is unrecoverable.
        if let Ok(Some(resumed)) = read_frame::<_, ServerToClient>(&mut reader).await {
            assert_ne!(
                resumed, message,
                "a cancelled read must not appear to be recoverable"
            );
        }
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
