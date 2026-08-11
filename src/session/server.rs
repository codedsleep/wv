//! The session server: owns the socket, feeds the running `App`.
//!
//! One connection at a time renders a session. The accept loop performs the
//! `Attach` handshake, then pumps decoded client messages into the app's event
//! loop and rendered frames back out.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use super::paths;
use super::protocol::{
    check_protocol_version, read_frame, write_frame, write_hello, ClientToServer, CommandResult,
    ServerToClient,
};
use super::SessionEvent;
use crate::command::Command;

const APP_EVENT_CAPACITY: usize = 64;

/// A bound session socket, ready to accept clients.
pub struct SessionServer {
    listener: UnixListener,
    path: PathBuf,
    name: String,
}

impl SessionServer {
    /// Bind the socket for `name`, clearing a socket left behind by a dead server.
    pub fn bind(name: &str) -> anyhow::Result<Self> {
        let path = paths::socket_path(name)?;

        if path.exists() {
            if paths::is_socket_live(&path) {
                bail!("a weave session named `{name}` is already running");
            }
            tracing::info!("replacing stale session socket {}", path.display());
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind session socket {}", path.display()))?;

        Ok(Self {
            listener,
            path,
            name: name.to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start accepting clients, returning the app-side event stream.
    ///
    /// The returned guard unlinks the socket when dropped, so a shut-down
    /// server never leaves a name claimed.
    pub fn start(self) -> (mpsc::Receiver<SessionEvent>, SocketGuard) {
        let (app_tx, app_rx) = mpsc::channel(APP_EVENT_CAPACITY);
        let guard = SocketGuard { path: self.path };
        let listener = self.listener;

        tokio::spawn(async move {
            let mut next_client_id = 1u64;

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let id = next_client_id;
                        next_client_id = next_client_id.saturating_add(1);
                        let app_tx = app_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = serve_connection(id, stream, app_tx).await {
                                tracing::warn!("session client {id} ended: {error:#}");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::error!("session listener stopped: {error:#}");
                        break;
                    }
                }
            }
        });

        (app_rx, guard)
    }
}

/// Removes the socket file when the server goes away.
pub struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve_connection(
    id: u64,
    stream: UnixStream,
    app_tx: mpsc::Sender<SessionEvent>,
) -> anyhow::Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();

    // Every connection opens with a version handshake, so a client from a
    // different build fails loudly here instead of decoding frames into the
    // wrong variants further down.
    match read_frame::<_, ClientToServer>(&mut read_half).await? {
        Some(ClientToServer::Hello { protocol_version }) => {
            if let Err(error) = check_protocol_version(protocol_version) {
                let message = format!("{error:#}");
                tracing::warn!("rejecting client {id}: {message}");
                let _ = write_frame(&mut write_half, &ServerToClient::Error(message)).await;
                return Ok(());
            }
        }
        // A connect-and-close is how `ls`, `attach` and the server itself
        // check that a socket is live, not a misbehaving client.
        None => {
            tracing::debug!("client {id} was a liveness probe");
            return Ok(());
        }
        other => bail!("client {id} sent {other:?} instead of a protocol handshake"),
    }

    let handshake: Option<ClientToServer> = read_frame(&mut read_half).await?;
    let (cols, rows, truecolor) = match handshake {
        Some(ClientToServer::Attach {
            cols,
            rows,
            truecolor,
        }) => (cols, rows, truecolor),
        // `wv exec` is a one-shot: a single command, no rendering, no attach.
        // The reply goes back on this connection before it closes, so the
        // caller learns what the command produced and whether it worked.
        Some(ClientToServer::Request {
            id: request_id,
            command,
        }) => {
            let (reply_tx, reply_rx) = oneshot::channel();
            app_tx
                .send(SessionEvent::Request {
                    command,
                    reply: reply_tx,
                })
                .await
                .context("session app stopped accepting events")?;

            // A dropped sender means the app went away mid-command — a real
            // failure for the caller, not a silent success.
            let result = reply_rx.await.unwrap_or_else(|_| CommandResult::Error {
                message: "the session ended before the command completed".to_owned(),
            });
            write_frame(
                &mut write_half,
                &ServerToClient::Reply {
                    id: request_id,
                    result,
                },
            )
            .await?;
            return Ok(());
        }
        Some(message @ ClientToServer::Quit) => {
            app_tx
                .send(SessionEvent::Message(message))
                .await
                .context("session app stopped accepting events")?;
            return Ok(());
        }
        // The handshake frame arrived but nothing followed it: a probe that
        // speaks the protocol, still not a client.
        None => {
            tracing::debug!("client {id} said hello and left");
            return Ok(());
        }
        other => bail!("client {id} sent {other:?} instead of an attach handshake"),
    };

    let (frames_tx, frames_rx) = mpsc::unbounded_channel();
    app_tx
        .send(SessionEvent::ClientAttached {
            id,
            cols,
            rows,
            truecolor,
            frames: frames_tx,
        })
        .await
        .context("session app stopped accepting events")?;

    let writer = tokio::spawn(write_frames(write_half, frames_rx));

    loop {
        let message: Option<ClientToServer> = match read_frame(&mut read_half).await {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!("client {id} sent an undecodable frame: {error:#}");
                break;
            }
        };
        let Some(message) = message else {
            break;
        };

        // Detach and quit are the client's last words; forward them, then stop
        // reading so the writer can flush the server's reply and close.
        let final_message = matches!(message, ClientToServer::Detach | ClientToServer::Quit);
        if app_tx.send(SessionEvent::Message(message)).await.is_err() {
            break;
        }
        if final_message {
            break;
        }
    }

    let _ = app_tx.send(SessionEvent::ClientGone { id }).await;
    let _ = writer.await;

    Ok(())
}

async fn write_frames(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut frames: UnboundedReceiver<ServerToClient>,
) {
    while let Some(message) = frames.recv().await {
        let closing = matches!(
            message,
            ServerToClient::Detached | ServerToClient::Exit(_)
        );
        if let Err(error) = write_frame(&mut write_half, &message).await {
            tracing::debug!("dropping session client after write error: {error:#}");
            return;
        }
        if closing {
            return;
        }
    }
}

/// The id `wv exec` uses; it only ever has one request in flight.
const ONE_SHOT_REQUEST_ID: u64 = 0;

/// How long a one-shot waits for the session to answer.
///
/// A command runs on the session's event loop between frames, so a healthy
/// session replies in microseconds. This bound exists so a wedged session
/// fails a script instead of hanging it.
///
/// `wait-for` is exempt: waiting for a signal is the whole point of it, and
/// there is no sensible upper bound on how long that takes.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Run one command against a running session and return what it produced.
///
/// This is the whole of `wv exec`'s transport: connect, handshake, request,
/// reply, close.
pub async fn request(path: &Path, command: Command) -> anyhow::Result<CommandResult> {
    // Blocking until something signals is what `wait-for` is for, so it must
    // not be cut off by the reply timeout.
    let waits_indefinitely = matches!(
        command,
        Command::WaitFor {
            action: crate::command::WaitAction::Wait,
            ..
        }
    );
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to session socket {}", path.display()))?;
    write_hello(&mut stream).await?;
    write_frame(
        &mut stream,
        &ClientToServer::Request {
            id: ONE_SHOT_REQUEST_ID,
            command,
        },
    )
    .await?;

    let frame = if waits_indefinitely {
        read_frame::<_, ServerToClient>(&mut stream)
            .await
            .context("failed to read the session server's reply")?
    } else {
        timeout(REPLY_TIMEOUT, read_frame::<_, ServerToClient>(&mut stream))
            .await
            .with_context(|| {
                format!(
                    "the session did not answer within {} seconds",
                    REPLY_TIMEOUT.as_secs()
                )
            })?
            .context("failed to read the session server's reply")?
    };

    match frame {
        Some(ServerToClient::Reply { id, result }) => {
            // Ids exist for clients with several requests in flight; a
            // mismatch here means the stream is not what we think it is.
            if id == ONE_SHOT_REQUEST_ID {
                Ok(result)
            } else {
                bail!("session replied to request {id}, but we sent {ONE_SHOT_REQUEST_ID}")
            }
        }
        // The version gate answers with a bare error and closes.
        Some(ServerToClient::Error(message)) => bail!(message),
        Some(other) => bail!("unexpected reply from session server: {other:?}"),
        None => bail!(
            "the session closed the connection without replying; it may be running an older `wv`"
        ),
    }
}

/// Channel type used by the app to push frames at the attached client.
pub type FrameSender = UnboundedSender<ServerToClient>;
