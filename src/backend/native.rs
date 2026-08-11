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

    /// Read the pane process's name from `/proc/<pid>/comm`.
    async fn pane_process_name(&mut self, pane: PaneId) -> Result<Option<String>, Error> {
        let Some(pid) = self.panes.get(&pane).and_then(|pane| pane.pid) else {
            return Ok(None);
        };

        match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            Ok(name) => Ok(Some(name.trim_end().to_owned())),
            Err(error) => {
                tracing::debug!("could not read command of pane {pane:?} (pid {pid}): {error}");
                Ok(None)
            }
        }
    }

    /// Read the pane's foreground job from `/proc/<pid>/stat`.
    ///
    /// Field 8 of the shell's stat line is `tpgid`: the process group the
    /// kernel currently gives the terminal's input to. That is the job running
    /// in the pane, which is the one worth naming — `/proc/<pid>/comm` only
    /// ever says `fish`.
    async fn pane_foreground_names(&mut self, pane: PaneId) -> Result<Vec<String>, Error> {
        let Some(pid) = self.panes.get(&pane).and_then(|pane| pane.pid) else {
            return Ok(Vec::new());
        };

        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) => {
                tracing::debug!("could not read stat of pane {pane:?} (pid {pid}): {error}");
                return Ok(Vec::new());
            }
        };

        let Some(tpgid) = foreground_pgid(&stat) else {
            return Ok(Vec::new());
        };

        Ok(foreground_group_names(tpgid))
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

/// Pull `tpgid` out of a `/proc/<pid>/stat` line.
///
/// The second field is the executable name in parentheses and may itself
/// contain spaces and parentheses, so the fields are counted from the last
/// `)` rather than split from the start. After it come state, ppid, pgrp,
/// session, `tty_nr`, tpgid — `tpgid` sixth.
fn foreground_pgid(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let tpgid: i32 = rest.split_whitespace().nth(5)?.parse().ok()?;

    // -1 means the terminal has no foreground process group at all.
    (tpgid > 0).then_some(tpgid)
}

/// Pull `pgrp` out of a `/proc/<pid>/stat` line.
///
/// Counted from the last `)` for the same reason as `tpgid`, and third after
/// it: state, ppid, pgrp.
fn process_group(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];

    rest.split_whitespace().nth(2)?.parse().ok()
}

/// How deep below the group leader the foreground scan looks, and how many
/// commands it will name.
///
/// A wrapper shell puts the real job one level down, and a launcher one below
/// that. Deeper than this is the job's own work — the subprocesses a build or
/// an agent spawns — which the pane is not meaningfully "running".
const FOREGROUND_MAX_DEPTH: usize = 4;
const FOREGROUND_MAX_NAMES: usize = 16;

/// Every command in the foreground process group, leader first.
///
/// The leader alone is not enough: a shell running `fish -c claude` does no job
/// control, so it never hands the terminal to the agent. The group stays the
/// shell's, the leader stays `fish`, and the agent sits below it in that same
/// group — invisible to anything that reads only `/proc/<tpgid>/comm`. Walking
/// the leader's children finds it.
///
/// The walk stays inside the group, so a pane's background jobs are not named,
/// and a child that has left the group takes its descendants out of the walk
/// with it. Children come from `/proc/<pid>/task/<pid>/children`, which is the
/// main thread's — a job spawned from some other thread of the shell is missed,
/// which no shell does.
fn foreground_group_names(leader: i32) -> Vec<String> {
    let mut names = Vec::new();
    let mut frontier = vec![leader];

    for _ in 0..=FOREGROUND_MAX_DEPTH {
        if frontier.is_empty() || names.len() >= FOREGROUND_MAX_NAMES {
            break;
        }

        let mut next = Vec::new();
        for pid in frontier {
            // Read fresh rather than trusting the parent's view: the walk races
            // processes starting and exiting, and a stale answer names a job
            // that is no longer the pane's.
            if !in_group(pid, leader) {
                continue;
            }
            if let Some(name) = comm(pid) {
                names.push(name);
            }
            if names.len() >= FOREGROUND_MAX_NAMES {
                break;
            }
            next.extend(children(pid));
        }
        frontier = next;
    }

    names
}

/// A process's name, or `None` if it exited mid-walk.
fn comm(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim_end().to_owned())
}

/// Whether a process is still in `group`.
fn in_group(pid: i32, group: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| process_group(&stat))
        == Some(group)
}

/// A process's direct children.
fn children(pid: i32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .map(|list| {
            list.split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect()
        })
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::{foreground_pgid, process_group};

    /// A real line, trimmed to the fields that matter.
    #[test]
    fn reads_tpgid_from_a_stat_line() {
        let stat = "1234 (fish) S 1 1234 1234 34816 5678 4194304 0 0";

        assert_eq!(foreground_pgid(stat), Some(5678));
    }

    #[test]
    fn reads_pgrp_from_a_stat_line() {
        let stat = "1234 (fish) S 1 4321 1234 34816 5678 4194304 0 0";

        assert_eq!(process_group(stat), Some(4321));
    }

    /// The same escaping problem as `tpgid`, and the same fix.
    #[test]
    fn a_command_containing_parens_does_not_shift_the_pgrp_field() {
        let stat = "1234 (od) ) (weird) S 1 4321 1234 34816 5678 4194304 0 0";

        assert_eq!(process_group(stat), Some(4321));
    }

    #[test]
    fn a_truncated_stat_line_has_no_pgrp() {
        assert_eq!(process_group("1234 (fish) S 1"), None);
        assert_eq!(process_group("garbage"), None);
    }

    /// The comm field is not escaped, so a process named `a ) b` would break
    /// any parser that split from the left.
    #[test]
    fn a_command_containing_spaces_and_parens_does_not_shift_the_fields() {
        let stat = "1234 (od) ) (weird) S 1 1234 1234 34816 5678 4194304 0 0";

        assert_eq!(foreground_pgid(stat), Some(5678));
    }

    #[test]
    fn no_foreground_process_group_is_none() {
        let stat = "1234 (fish) S 1 1234 1234 34816 -1 4194304 0 0";

        assert_eq!(foreground_pgid(stat), None);
    }

    #[test]
    fn a_truncated_stat_line_is_none() {
        assert_eq!(foreground_pgid("1234 (fish) S 1"), None);
        assert_eq!(foreground_pgid("nonsense"), None);
    }
}
