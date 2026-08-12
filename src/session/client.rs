//! The thin client: owns the terminal, owns nothing else.
//!
//! It forwards input events and resizes to the session server and writes the
//! frames it gets back straight to stdout. Every piece of session state —
//! panes, layout, animation — lives in the server, which is what lets the
//! client come and go.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::protocol::{
    read_frame, write_frame, write_hello, ClientToServer, ExitReason, ServerToClient,
};

/// How long `wv` waits for a freshly spawned server to bind its socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Why a client stopped rendering a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientOutcome {
    /// The user detached; the session is still running.
    Detached,
    /// The goto picker or `switch-client` sent this client somewhere else.
    ///
    /// Unlike every other outcome this one is not the end: the caller connects
    /// to the named session and keeps going on the same terminal.
    Switch { name: String, window: Option<u32> },
    /// The server ended the connection for the given reason.
    Exited(ExitReason),
    /// The connection dropped without a goodbye.
    ConnectionLost,
}

impl ClientOutcome {
    /// Message to print after the terminal has been restored.
    pub fn message(&self, session: &str) -> String {
        match self {
            Self::Detached => format!("[detached from {session}]"),
            // Printed only if the hop fails; a switch that lands says nothing,
            // because nothing ended.
            Self::Switch { name, .. } => format!("[could not switch to {name}]"),
            Self::Exited(reason) => format!("[{}]", reason.message()),
            Self::ConnectionLost => format!("[lost connection to {session}]"),
        }
    }
}

/// Connect to a socket, retrying until the server has bound it.
pub async fn connect(path: &Path) -> anyhow::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;

    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error).with_context(|| {
                        format!("failed to connect to session socket {}", path.display())
                    });
                }
                tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
            }
        }
    }
}

/// Attach to a session and render it until the user detaches or it ends.
///
/// The caller owns the [`TerminalGuard`], and so owns raw mode and the
/// alternate screen across a whole run of attachments. That is what makes
/// switching sessions seamless: this returns, the caller connects to the next
/// socket and calls it again, and the terminal is never handed back in between.
pub async fn run(stream: UnixStream, name: &str) -> anyhow::Result<ClientOutcome> {
    let (cols, rows) = crossterm::terminal::size().context("failed to read terminal size")?;
    let truecolor = matches!(
        std::env::var("COLORTERM").as_deref(),
        Ok("truecolor" | "24bit")
    );
    // Read here rather than in the server: this process is the one holding the
    // terminal, so its environment is the one that describes it.
    let nested = crate::input::nesting::over_ssh();

    let (mut read_half, mut write_half) = stream.into_split();
    write_hello(&mut write_half).await?;
    write_frame(
        &mut write_half,
        &ClientToServer::Attach {
            cols,
            rows,
            truecolor,
            nested,
        },
    )
    .await
    .context("failed to send attach handshake")?;

    let mut events = EventStream::new();
    let mut stdout = std::io::stdout();
    tracing::info!("attached to session {name} at {cols}x{rows}");

    // Frames are read on their own task rather than inside the `select!` below.
    // `read_frame` is not cancel-safe — half a frame consumed by a future that
    // `select!` then drops is gone for good, and the stream desyncs on the very
    // first keystroke. `Receiver::recv` is cancel-safe, so the select branch
    // reads from this channel instead. Closing it means end of stream.
    let (frames_tx, mut frames_rx) = mpsc::unbounded_channel::<anyhow::Result<ServerToClient>>();
    let reader = tokio::spawn(async move {
        loop {
            match read_frame::<_, ServerToClient>(&mut read_half).await {
                Ok(Some(message)) => {
                    if frames_tx.send(Ok(message)).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    let _ = frames_tx.send(Err(error));
                    return;
                }
            }
        }
    });

    let result: anyhow::Result<ClientOutcome> = async {
        loop {
            tokio::select! {
                frame = frames_rx.recv() => {
                    match frame {
                        Some(Ok(ServerToClient::Frame(bytes))) => {
                            stdout.write_all(&bytes)?;
                            stdout.flush()?;
                        }
                        Some(Ok(ServerToClient::Detached)) => return Ok(ClientOutcome::Detached),
                        Some(Ok(ServerToClient::SwitchSession { name, window })) => {
                            return Ok(ClientOutcome::Switch { name, window });
                        }
                        Some(Ok(ServerToClient::Exit(reason))) => {
                            return Ok(ClientOutcome::Exited(reason));
                        }
                        Some(Ok(ServerToClient::Error(message))) => {
                            tracing::warn!("session error: {message}");
                        }
                        // An attached client has no requests in flight yet;
                        // PR 7's command prompt is the first thing to send one.
                        Some(Ok(ServerToClient::Reply { id, result })) => {
                            tracing::debug!("unexpected reply to request {id}: {result:?}");
                        }
                        Some(Err(error)) => return Err(error),
                        None => return Ok(ClientOutcome::ConnectionLost),
                    }
                }
                event = events.next() => {
                    match event {
                        Some(Ok(Event::Resize(cols, rows))) => {
                            write_frame(&mut write_half, &ClientToServer::Resize { cols, rows }).await?;
                        }
                        Some(Ok(event)) => {
                            write_frame(&mut write_half, &ClientToServer::Input(event)).await?;
                        }
                        Some(Err(error)) => {
                            tracing::warn!("terminal input error: {error:#}");
                        }
                        // stdin closed: nothing more can be sent, so leave the
                        // session running rather than killing its panes.
                        None => return Ok(ClientOutcome::Detached),
                    }
                }
            }
        }
    }
    .await;

    reader.abort();

    result
}

#[cfg(test)]
mod tests {
    use super::{ClientOutcome, ExitReason};

    #[test]
    fn outcome_messages_name_the_session() {
        assert_eq!(
            ClientOutcome::Detached.message("weave-1a2b3c4d"),
            "[detached from weave-1a2b3c4d]"
        );
        assert_eq!(
            ClientOutcome::Exited(ExitReason::TakenOver).message("weave-1a2b3c4d"),
            "[detached: another client attached to this session]"
        );
        assert_eq!(
            ClientOutcome::ConnectionLost.message("main"),
            "[lost connection to main]"
        );
    }
}
