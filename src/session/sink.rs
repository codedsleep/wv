//! Where rendered frames go.
//!
//! Running locally the renderer writes straight to stdout. Inside a session
//! server it writes into a buffer that is shipped to the attached client as one
//! `Frame` per flush, and while detached it writes nowhere at all.

use std::io::{self, Write};

use tokio::sync::mpsc::UnboundedSender;

use super::protocol::ServerToClient;

/// Frames queued for the attached client are never dropped: a diff frame only
/// makes sense applied in order on top of every earlier one, so the channel is
/// unbounded and a slow client shows up as depth, not as corruption.
pub enum OutputSink {
    Stdout(io::Stdout),
    Client {
        buf: Vec<u8>,
        frames: UnboundedSender<ServerToClient>,
    },
    Null,
}

impl OutputSink {
    pub fn stdout() -> Self {
        Self::Stdout(io::stdout())
    }

    pub const fn client(frames: UnboundedSender<ServerToClient>) -> Self {
        Self::Client {
            buf: Vec::new(),
            frames,
        }
    }

    /// Whether anything is listening; `false` means rendering can be skipped.
    pub const fn is_attached(&self) -> bool {
        !matches!(self, Self::Null)
    }
}

impl Write for OutputSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(stdout) => stdout.write(data),
            Self::Client { buf, .. } => {
                buf.extend_from_slice(data);
                Ok(data.len())
            }
            Self::Null => Ok(data.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => stdout.flush(),
            Self::Client { buf, frames } => {
                if buf.is_empty() {
                    return Ok(());
                }

                let frame = ServerToClient::Frame(std::mem::take(buf));
                frames.send(frame).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "session client disconnected")
                })
            }
            Self::Null => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tokio::sync::mpsc::unbounded_channel;

    use super::OutputSink;
    use crate::session::protocol::ServerToClient;

    #[test]
    fn client_sink_emits_one_frame_per_flush() {
        let (tx, mut rx) = unbounded_channel();
        let mut sink = OutputSink::client(tx);

        sink.write_all(b"hello ").expect("write buffered");
        sink.write_all(b"world").expect("write buffered");
        assert!(rx.try_recv().is_err(), "nothing sent before flush");

        sink.flush().expect("flush sends a frame");

        assert_eq!(
            rx.try_recv().expect("frame sent"),
            ServerToClient::Frame(b"hello world".to_vec())
        );
        sink.flush().expect("empty flush is a no-op");
        assert!(rx.try_recv().is_err(), "no empty frames");
    }

    #[test]
    fn null_sink_swallows_output_and_reports_detached() {
        let mut sink = OutputSink::Null;

        assert!(!sink.is_attached());
        sink.write_all(b"ignored").expect("write accepted");
        sink.flush().expect("flush accepted");
    }

    #[test]
    fn client_sink_reports_a_broken_pipe_once_the_client_is_gone() {
        let (tx, rx) = unbounded_channel();
        let mut sink = OutputSink::client(tx);
        drop(rx);

        sink.write_all(b"frame").expect("write buffered");

        let error = sink.flush().expect_err("flush fails without a receiver");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
