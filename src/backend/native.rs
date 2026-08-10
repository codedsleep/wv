//! portable-pty backed `NativeBackend`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

use anyhow::{Context, Error};
use bytes::Bytes;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

use super::{BackendEvent, PaneBackend, PaneCommand, PaneId};

type OutputSender = mpsc::Sender<(PaneId, Bytes)>;
type EventSender = mpsc::Sender<BackendEvent>;

struct NativePane {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The child's pid, kept so its working directory can be read back.
    pid: Option<u32>,
}

pub struct NativeBackend {
    panes: HashMap<PaneId, NativePane>,
    output_tx: OutputSender,
    event_tx: EventSender,
    next_id: u64,
}

impl NativeBackend {
    /// Create a backend and its output/event receivers.
    ///
    /// This constructor owns channel creation so callers cannot accidentally
    /// attach channels with mismatched capacities or lifetimes.
    pub fn new() -> (
        Self,
        mpsc::Receiver<(PaneId, Bytes)>,
        mpsc::Receiver<BackendEvent>,
    ) {
        let (output_tx, output_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(64);

        (
            Self {
                panes: HashMap::new(),
                output_tx,
                event_tx,
                next_id: 1,
            },
            output_rx,
            event_rx,
        )
    }

    pub fn with_senders(output_tx: OutputSender, event_tx: EventSender) -> Self {
        Self {
            panes: HashMap::new(),
            output_tx,
            event_tx,
            next_id: 1,
        }
    }

    fn allocate_id(&mut self) -> PaneId {
        let id = PaneId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn pty_size(cols: u16, rows: u16) -> PtySize {
        PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn command_builder(cmd: PaneCommand) -> CommandBuilder {
        let mut builder = CommandBuilder::new(cmd.program);
        builder.args(cmd.args);

        for (key, value) in cmd.env {
            builder.env(key, value);
        }

        if let Some(cwd) = cmd.cwd {
            builder.cwd(cwd);
        }

        builder
    }

    async fn send_spawn_failed(event_tx: EventSender, id: PaneId, message: String) {
        let _ = event_tx.send(BackendEvent::SpawnFailed(id, message)).await;
    }

    fn spawn_reader(
        id: PaneId,
        mut reader: Box<dyn Read + Send>,
        output_tx: OutputSender,
        event_tx: EventSender,
        dead: Arc<AtomicBool>,
    ) {
        thread::spawn(move || {
            let mut buf = vec![0; 8192];

            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        emit_pane_died(id, &event_tx, &dead);
                        break;
                    }
                    Ok(count) => {
                        if output_tx
                            .blocking_send((id, Bytes::copy_from_slice(&buf[..count])))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl PaneBackend for NativeBackend {
    async fn spawn(&mut self, cmd: PaneCommand) -> Result<PaneId, Error> {
        let id = self.allocate_id();
        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(Self::pty_size(80, 24)) {
            Ok(pair) => pair,
            Err(error) => {
                Self::send_spawn_failed(self.event_tx.clone(), id, error.to_string()).await;
                return Err(error);
            }
        };

        let mut child = match pair.slave.spawn_command(Self::command_builder(cmd)) {
            Ok(child) => child,
            Err(error) => {
                Self::send_spawn_failed(self.event_tx.clone(), id, error.to_string()).await;
                return Err(error);
            }
        };
        drop(pair.slave);

        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                Self::send_spawn_failed(self.event_tx.clone(), id, error.to_string()).await;
                return Err(error);
            }
        };

        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                Self::send_spawn_failed(self.event_tx.clone(), id, error.to_string()).await;
                return Err(error);
            }
        };

        let killer = child.clone_killer();
        // Read the pid before the child moves into the waiter thread.
        let pid = child.process_id();
        let dead = Arc::new(AtomicBool::new(false));
        Self::spawn_reader(
            id,
            reader,
            self.output_tx.clone(),
            self.event_tx.clone(),
            Arc::clone(&dead),
        );

        spawn_waiter(id, self.event_tx.clone(), Arc::clone(&dead), move || {
            let _ = child.wait();
        });

        self.panes.insert(
            id,
            NativePane {
                master: pair.master,
                writer,
                killer,
                pid,
            },
        );

        Ok(id)
    }

    async fn write(&mut self, id: PaneId, data: &[u8]) -> Result<(), Error> {
        let pane = self
            .panes
            .get_mut(&id)
            .with_context(|| format!("unknown pane id {id:?}"))?;
        pane.writer.write_all(data)?;
        pane.writer.flush()?;

        Ok(())
    }

    async fn resize(&mut self, id: PaneId, cols: u16, rows: u16) -> Result<(), Error> {
        let pane = self
            .panes
            .get(&id)
            .with_context(|| format!("unknown pane id {id:?}"))?;
        pane.master.resize(Self::pty_size(cols, rows))
    }

    async fn kill(&mut self, id: PaneId) -> Result<(), Error> {
        let mut pane = self
            .panes
            .remove(&id)
            .with_context(|| format!("unknown pane id {id:?}"))?;
        pane.killer.kill()?;

        Ok(())
    }

    /// Read a pane's working directory from `/proc/<pid>/cwd`.
    ///
    /// This is the shell's own cwd, so a new pane opens wherever the focused
    /// pane has `cd`-ed to, which is what tmux does and what people expect.
    /// Any failure — the process just exited, `/proc` not mounted — is a
    /// `None` rather than an error: it only costs the caller a fallback.
    async fn pane_cwd(&mut self, pane: PaneId) -> Result<Option<PathBuf>, Error> {
        let Some(pid) = self.panes.get(&pane).and_then(|pane| pane.pid) else {
            return Ok(None);
        };

        match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Ok(cwd) => Ok(Some(cwd)),
            Err(error) => {
                tracing::debug!("could not read cwd of pane {pane:?} (pid {pid}): {error}");
                Ok(None)
            }
        }
    }
}

fn emit_pane_died(id: PaneId, event_tx: &EventSender, dead: &AtomicBool) {
    if !dead.swap(true, Ordering::SeqCst) {
        let _ = event_tx.blocking_send(BackendEvent::PaneDied(id));
    }
}

fn spawn_waiter<F>(id: PaneId, event_tx: EventSender, dead: Arc<AtomicBool>, wait: F)
where
    F: FnOnce() + Send + 'static,
{
    thread::spawn(move || {
        wait();
        emit_pane_died(id, &event_tx, &dead);
    });
}
