//! The session server: owns the socket, feeds the running `App`.
//!
//! One connection at a time renders a session. The accept loop performs the
//! `Attach` handshake, then pumps decoded client messages into the app's event
//! loop and rendered frames back out.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::paths;
use super::protocol::{
    check_protocol_version, read_frame, write_frame, write_hello, ClientToServer, ServerToClient,
};
use super::SessionEvent;

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
        Some(message @ (ClientToServer::Exec(_) | ClientToServer::Quit)) => {
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

/// Send a one-shot command to a running session, as `wv exec` does.
pub async fn send_command(path: &Path, message: &ClientToServer) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to session socket {}", path.display()))?;
    write_hello(&mut stream).await?;
    write_frame(&mut stream, message).await?;

    // The server stays silent on this path and closes when it is done, so the
    // only thing that can come back is a rejection worth reporting. Waiting for
    // EOF also means a one-shot cannot exit before the server has read it.
    // Command *replies* land in PR 2; this is purely the version gate.
    match read_frame::<_, ServerToClient>(&mut stream).await {
        Ok(Some(ServerToClient::Error(message))) => bail!(message),
        Ok(Some(other)) => bail!("unexpected reply from session server: {other:?}"),
        Ok(None) => Ok(()),
        Err(error) => Err(error).context("failed to read the session server's reply"),
    }
}

/// Channel type used by the app to push frames at the attached client.
pub type FrameSender = UnboundedSender<ServerToClient>;
