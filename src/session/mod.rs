//! Client/server session layer: a daemon owns the panes, a thin client owns the
//! terminal.
//!
//! `wv` spawns a server process holding every PTY, the vt100 state, the layout
//! tree and the renderer. The client attaches over a unix socket, forwards
//! input and resizes, and writes back the rendered frames. Detaching drops the
//! connection while the server keeps running, which is what makes panes survive
//! without an external multiplexer.

pub mod client;
pub mod launch;
pub mod paths;
pub mod protocol;
pub mod server;
pub mod sink;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::command::Command;
use protocol::{ClientToServer, CommandResult, ServerToClient};

/// What the socket loop hands to the running `App`.
///
/// The server thread owns the listener and the framing; the app sees a single
/// ordered stream of events, so client I/O is just one more branch of its
/// existing event loop.
pub enum SessionEvent {
    /// A client completed its handshake and wants the session's frames.
    ClientAttached {
        id: u64,
        cols: u16,
        rows: u16,
        truecolor: bool,
        /// Whether this terminal is reached over SSH, and so wants the nested
        /// keybindings.
        nested: bool,
        frames: UnboundedSender<ServerToClient>,
    },
    /// A message from the attached client.
    Message(ClientToServer),
    /// Run a command and answer on `reply`.
    ///
    /// The reply channel travels with the request rather than going to the
    /// attached client's frame sink, because the caller is usually a one-shot
    /// `wv exec` connection with no interest in frames. If the sender is
    /// dropped without answering, the caller sees the connection close and
    /// reports that as a failure.
    Request {
        command: Command,
        reply: oneshot::Sender<CommandResult>,
    },
    /// A client connection ended. Carries the id so a late notice from an
    /// evicted client cannot tear down the client that replaced it.
    ClientGone { id: u64 },
}

