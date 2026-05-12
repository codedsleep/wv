//! Process owner for a tmux control-mode backend.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Error};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::layout::{parse_layout, LayoutAst};
use super::parser::{CommandResponse, CommandResponseStatus, Parser, TmuxNotification};
use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};

type OutputSender = mpsc::Sender<(PaneId, Bytes)>;
type EventSender = mpsc::Sender<BackendEvent>;
type ResponseSender = mpsc::Sender<CommandResponse>;
type ResponseReceiver = mpsc::Receiver<CommandResponse>;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TmuxPaneId(pub u64);

#[derive(Default)]
struct PaneMaps {
    pane_to_tmux: HashMap<PaneId, TmuxPaneId>,
    tmux_to_pane: HashMap<TmuxPaneId, PaneId>,
    pending_output: HashMap<TmuxPaneId, Vec<Bytes>>,
    pending_died: HashSet<TmuxPaneId>,
}

impl PaneMaps {
    fn pane_for_tmux(&self, tmux_id: TmuxPaneId) -> Option<PaneId> {
        self.tmux_to_pane.get(&tmux_id).copied()
    }

    fn tmux_for_pane(&self, pane_id: PaneId) -> Option<TmuxPaneId> {
        self.pane_to_tmux.get(&pane_id).copied()
    }

    fn register(
        &mut self,
        pane_id: PaneId,
        tmux_id: TmuxPaneId,
    ) -> (Vec<Bytes>, Option<BackendEvent>) {
        self.pane_to_tmux.insert(pane_id, tmux_id);
        self.tmux_to_pane.insert(tmux_id, pane_id);

        let pending_output = self.pending_output.remove(&tmux_id).unwrap_or_default();
        let pending_died = self.pending_died.remove(&tmux_id);
        let event = pending_died.then_some(BackendEvent::PaneDied(pane_id));

        (pending_output, event)
    }

    fn remove_tmux(&mut self, tmux_id: TmuxPaneId) -> Option<PaneId> {
        let pane_id = self.tmux_to_pane.remove(&tmux_id)?;
        self.pane_to_tmux.remove(&pane_id);
        Some(pane_id)
    }

    fn remove_all(&mut self) -> Vec<PaneId> {
        let pane_ids = self.pane_to_tmux.keys().copied().collect();
        self.pane_to_tmux.clear();
        self.tmux_to_pane.clear();
        self.pending_output.clear();
        self.pending_died.clear();
        pane_ids
    }
}

pub struct TmuxBackend {
    session_name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    maps: Arc<Mutex<PaneMaps>>,
    output_tx: OutputSender,
    event_tx: EventSender,
    response_rx: ResponseReceiver,
    next_id: u64,
    detached: bool,
    /// True when weave owns this session (auto-named, fresh `new-session`).
    /// False for user-named sessions and attach backends. Drop only kills the
    /// session when this is true.
    owns_session: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxInitialState {
    pub windows: Vec<TmuxInitialWindow>,
    pub panes: Vec<TmuxInitialPane>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxInitialWindow {
    pub window_id: u64,
    pub window_index: u8,
    pub layout: LayoutAst,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxInitialPane {
    pub tmux_id: TmuxPaneId,
    pub pid: u32,
}

impl TmuxBackend {
    pub async fn new(
        session_name: Option<String>,
        output_tx: OutputSender,
        event_tx: EventSender,
    ) -> anyhow::Result<Self> {
        let explicit_session_name = session_name.is_some();
        if !explicit_session_name {
            let killed = cleanup_orphaned_weave_sessions();
            if killed > 0 {
                tracing::info!(killed, "cleaned up orphaned weave-* tmux sessions");
            }
        }

        let session_name = session_name.unwrap_or_else(new_session_name);
        if explicit_session_name && tmux_session_exists(&session_name)? {
            bail!(
                "tmux session `{session_name}` already exists; attach with `wv attach {session_name}`"
            );
        }

        let child = Command::new("tmux")
            .args(["-C", "new-session", "-s", &session_name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| "failed to spawn tmux -C")?;

        let owns_session = !explicit_session_name;
        let mut backend = Self::from_child(session_name, child, output_tx, event_tx, owns_session)?;
        backend.configure_session().await?;

        let tmux_id = backend
            .list_session_panes()
            .await?
            .into_iter()
            .next()
            .with_context(|| format!("tmux session `{}` has no panes", backend.session_name))?;
        let pane_id = backend.allocate_id();
        backend.register_mapping(pane_id, tmux_id).await;

        Ok(backend)
    }

    pub async fn attach(
        session_name: String,
        output_tx: OutputSender,
        event_tx: EventSender,
    ) -> anyhow::Result<Self> {
        let child = Command::new("tmux")
            .args(["-CC", "attach", "-t", &session_name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn tmux -CC attach -t {session_name}"))?;

        let mut backend = Self::from_child(session_name, child, output_tx, event_tx, false)?;
        backend.configure_session().await?;

        Ok(backend)
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut pane_ids = {
            let maps = lock_maps(&self.maps);
            maps.pane_to_tmux.keys().copied().collect::<Vec<_>>()
        };
        pane_ids.sort_by_key(|pane| pane.0);
        pane_ids
    }

    pub async fn initial_state(&mut self) -> Result<TmuxInitialState, Error> {
        let windows = self.list_session_windows().await?;
        let panes = self.list_session_pane_details().await?;

        if panes.is_empty() {
            bail!(
                "tmux session `{}` has no panes to attach",
                self.session_name
            );
        }

        Ok(TmuxInitialState { windows, panes })
    }

    fn from_child(
        session_name: String,
        child: Child,
        output_tx: OutputSender,
        event_tx: EventSender,
        owns_session: bool,
    ) -> anyhow::Result<Self> {
        let mut child = child;
        let stdin = child
            .stdin
            .take()
            .with_context(|| "tmux child did not expose stdin")?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| "tmux child did not expose stdout")?;
        if let Some(stderr) = child.stderr.take() {
            drain_stderr(stderr);
        }

        let maps = Arc::new(Mutex::new(PaneMaps::default()));
        let (response_tx, response_rx) = mpsc::channel(64);
        spawn_reader(
            stdout,
            Arc::clone(&maps),
            output_tx.clone(),
            event_tx.clone(),
            response_tx,
        );

        Ok(Self {
            session_name,
            child,
            stdin: Some(stdin),
            maps,
            output_tx,
            event_tx,
            response_rx,
            next_id: 1,
            detached: false,
            owns_session,
        })
    }

    async fn configure_session(&mut self) -> Result<(), Error> {
        let response = self.send_command("set -g @weave-instance 1").await?;
        ensure_success(&response)?;
        let response = self.send_command("set -g prefix None").await?;
        ensure_success(&response)?;
        let response = self.send_command("set -g prefix2 None").await?;
        ensure_success(&response)?;
        let response = self.send_command("set -g allow-passthrough on").await?;
        ensure_success(&response)?;
        let response = self.send_command("set -g aggressive-resize on").await?;
        ensure_success(&response)?;
        let response = self.send_command("set -g status off").await?;
        ensure_success(&response)?;
        let response = self.send_command("set -g pane-border-status off").await?;
        ensure_success(&response)?;
        let response = self
            .send_command(&format!(
                "set-hook -g pane-exited {}",
                quote_tmux_arg("display-message \"weave-pane-exited #{hook_pane}\"")
            ))
            .await?;
        ensure_success(&response)
    }

    async fn list_session_panes(&mut self) -> Result<Vec<TmuxPaneId>, Error> {
        self.write_command(&format!(
            "list-panes -t {} -F {}",
            quote_tmux_arg(&format!("{}:", self.session_name)),
            quote_tmux_arg("#{pane_id}")
        ))?;

        loop {
            let response = self.next_response().await?;
            let pane_ids = response_pane_ids(&response);
            if !pane_ids.is_empty() {
                ensure_success(&response)?;
                return Ok(pane_ids);
            }
            ensure_success(&response)?;
        }
    }

    async fn list_session_windows(&mut self) -> Result<Vec<TmuxInitialWindow>, Error> {
        let response = self
            .send_command(&format!(
                "list-windows -t {} -F {}",
                quote_tmux_arg(&self.session_name),
                quote_tmux_arg("#{window_id}\t#{window_index}\t#{window_layout}")
            ))
            .await?;
        ensure_success(&response)?;
        parse_initial_windows(&response_message(&response))
    }

    async fn list_session_pane_details(&mut self) -> Result<Vec<TmuxInitialPane>, Error> {
        let response = self
            .send_command(&format!(
                "list-panes -s -t {} -F {}",
                quote_tmux_arg(&self.session_name),
                quote_tmux_arg("#{pane_id}\t#{pane_pid}")
            ))
            .await?;
        ensure_success(&response)?;
        parse_initial_panes(&response_message(&response))
    }

    fn allocate_id(&mut self) -> PaneId {
        allocate_pane_id(&mut self.next_id)
    }

    fn write_command(&mut self, command: &str) -> Result<(), Error> {
        write_command_to(&mut self.stdin, command)
    }

    async fn send_command(&mut self, command: &str) -> Result<CommandResponse, Error> {
        self.write_command(command)?;
        self.next_response().await
    }

    async fn next_response(&mut self) -> Result<CommandResponse, Error> {
        next_response_from(&mut self.response_rx).await
    }

    async fn register_mapping(&self, pane_id: PaneId, tmux_id: TmuxPaneId) {
        let (pending_output, pending_event) = {
            let mut maps = lock_maps(&self.maps);
            maps.register(pane_id, tmux_id)
        };

        for output in pending_output {
            let _ = self.output_tx.send((pane_id, output)).await;
        }

        if let Some(event) = pending_event {
            let _ = self.event_tx.send(event).await;
        }
    }

    fn tmux_pane_id(&self, pane_id: PaneId) -> Result<TmuxPaneId, Error> {
        let maps = lock_maps(&self.maps);
        maps.tmux_for_pane(pane_id)
            .with_context(|| format!("unknown pane id {pane_id:?}"))
    }

    pub async fn select_window(&mut self, workspace_idx: usize) -> Result<(), Error> {
        let command = format!(
            "select-window -t {}",
            quote_tmux_arg(&format!("{}:{}", self.session_name, workspace_idx + 1))
        );
        let response = self.send_command(&command).await?;
        ensure_success(&response)
    }

    pub async fn select_window_by_id(&mut self, window_id: u64) -> Result<(), Error> {
        let command = format!(
            "select-window -t {}",
            quote_tmux_arg(&format!("@{window_id}"))
        );
        let response = self.send_command(&command).await?;
        ensure_success(&response)
    }
}

#[async_trait::async_trait]
impl PaneBackend for TmuxBackend {
    async fn spawn(&mut self, cmd: PaneCommand) -> Result<PaneId, Error> {
        let pane_id = self.allocate_id();
        let command = split_window_command(&cmd);
        let response = self.send_command(&command).await?;

        if let Err(error) = ensure_success(&response) {
            let message = response_message(&response);
            let _ = self
                .event_tx
                .send(BackendEvent::SpawnFailed(pane_id, message))
                .await;
            return Err(error);
        }

        let tmux_id = response_pane_id(&response).with_context(|| {
            format!("tmux split-window response did not include pane id: {response:?}")
        })?;
        self.register_mapping(pane_id, tmux_id).await;

        Ok(pane_id)
    }

    async fn pane_cwd(&mut self, id: PaneId) -> Result<Option<std::path::PathBuf>, Error> {
        let tmux_id = self.tmux_pane_id(id)?;
        let command = format!(
            "display-message -p -t %{} {}",
            tmux_id.0,
            quote_tmux_arg("#{pane_current_path}")
        );
        let response = self.send_command(&command).await?;
        ensure_success(&response)?;

        let path_line = response.lines.first().map_or("", |line| line.trim());
        if path_line.is_empty() {
            Ok(None)
        } else {
            Ok(Some(std::path::PathBuf::from(path_line)))
        }
    }

    async fn write(&mut self, id: PaneId, data: &[u8]) -> Result<(), Error> {
        let tmux_id = self.tmux_pane_id(id)?;
        let command = format!("send-keys -t %{} -l {}", tmux_id.0, quote_tmux_bytes(data));
        let response = self.send_command(&command).await?;
        ensure_success(&response)
    }

    async fn resize(&mut self, id: PaneId, cols: u16, rows: u16) -> Result<(), Error> {
        let tmux_id = self.tmux_pane_id(id)?;
        let command = format!("resize-pane -t %{} -x {cols} -y {rows}", tmux_id.0);
        let response = self.send_command(&command).await?;
        ensure_success(&response)
    }

    async fn kill(&mut self, id: PaneId) -> Result<(), Error> {
        let tmux_id = self.tmux_pane_id(id)?;
        let command = format!("kill-pane -t %{}", tmux_id.0);
        let response = self.send_command(&command).await?;
        ensure_success(&response)
    }

    async fn detach(&mut self) -> Result<(), Error> {
        self.write_command("detach-client")?;
        self.detached = true;
        Ok(())
    }

    async fn select_window(&mut self, workspace_idx: usize) -> Result<(), Error> {
        TmuxBackend::select_window(self, workspace_idx).await
    }

    async fn select_window_by_id(&mut self, window_id: u64) -> Result<(), Error> {
        TmuxBackend::select_window_by_id(self, window_id).await
    }

    async fn ingest_external_pane(&mut self, tmux_pane_id: u64) -> Result<PaneId, Error> {
        let mut verifier = TmuxCommandPaneVerifier {
            stdin: &mut self.stdin,
            response_rx: &mut self.response_rx,
        };
        ingest_external_pane_with_verifier(
            &self.maps,
            &self.output_tx,
            &self.event_tx,
            &mut self.next_id,
            &mut verifier,
            TmuxPaneId(tmux_pane_id),
        )
        .await
    }
}

fn allocate_pane_id(next_id: &mut u64) -> PaneId {
    let id = PaneId(*next_id);
    *next_id = (*next_id).saturating_add(1);
    id
}

fn write_command_to(stdin: &mut Option<ChildStdin>, command: &str) -> Result<(), Error> {
    let stdin = stdin.as_mut().with_context(|| "tmux stdin is closed")?;
    stdin.write_all(command.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

async fn next_response_from(response_rx: &mut ResponseReceiver) -> Result<CommandResponse, Error> {
    timeout(COMMAND_TIMEOUT, response_rx.recv())
        .await
        .with_context(|| "timed out waiting for tmux command response")?
        .with_context(|| "tmux command response channel closed")
}

#[async_trait::async_trait]
trait ExternalPaneVerifier {
    async fn verify_external_pane(&mut self, tmux_id: TmuxPaneId) -> Result<(), Error>;
}

struct TmuxCommandPaneVerifier<'a> {
    stdin: &'a mut Option<ChildStdin>,
    response_rx: &'a mut ResponseReceiver,
}

#[async_trait::async_trait]
impl ExternalPaneVerifier for TmuxCommandPaneVerifier<'_> {
    async fn verify_external_pane(&mut self, tmux_id: TmuxPaneId) -> Result<(), Error> {
        let command = format!(
            "display-message -p -t %{} {}",
            tmux_id.0,
            quote_tmux_arg("#{pane_pid}")
        );
        write_command_to(self.stdin, &command)?;
        let response = next_response_from(self.response_rx).await?;
        ensure_success(&response)?;
        ensure_pane_pid_response(&response, tmux_id)
    }
}

async fn ingest_external_pane_with_verifier(
    maps: &Arc<Mutex<PaneMaps>>,
    output_tx: &OutputSender,
    event_tx: &EventSender,
    next_id: &mut u64,
    verifier: &mut impl ExternalPaneVerifier,
    tmux_id: TmuxPaneId,
) -> Result<PaneId, Error> {
    if let Some(pane_id) = {
        let maps = lock_maps(maps);
        maps.pane_for_tmux(tmux_id)
    } {
        return Ok(pane_id);
    }

    verifier.verify_external_pane(tmux_id).await?;

    if let Some(pane_id) = {
        let maps = lock_maps(maps);
        maps.pane_for_tmux(tmux_id)
    } {
        return Ok(pane_id);
    }

    let pane_id = allocate_pane_id(next_id);
    let (pending_output, pending_event) = {
        let mut maps = lock_maps(maps);
        maps.register(pane_id, tmux_id)
    };

    for output in pending_output {
        let _ = output_tx.send((pane_id, output)).await;
    }

    if let Some(event) = pending_event {
        let _ = event_tx.send(event).await;
    }

    Ok(pane_id)
}

impl Drop for TmuxBackend {
    fn drop(&mut self) {
        if self.owns_session && !self.detached {
            // Auto-named transient session: send kill via control-mode stdin
            // first, then use a direct kill-session as a best-effort fallback.
            if let Some(stdin) = self.stdin.as_mut() {
                let _ = writeln!(
                    stdin,
                    "kill-session -t {}",
                    quote_tmux_arg(&self.session_name)
                );
                let _ = stdin.flush();
            }

            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.session_name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // User-named or attached sessions are intentionally left alive; only
        // the control-mode child needs to be released here.

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_reader(
    mut stdout: impl Read + Send + 'static,
    maps: Arc<Mutex<PaneMaps>>,
    output_tx: OutputSender,
    event_tx: EventSender,
    response_tx: ResponseSender,
) {
    thread::spawn(move || {
        let mut parser = Parser::new();
        let mut buffer = [0; 8192];

        loop {
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    emit_all_panes_dead(&maps, &event_tx);
                    break;
                }
                Ok(count) => {
                    for notification in parser.feed(&buffer[..count]) {
                        handle_notification(
                            notification,
                            &maps,
                            &output_tx,
                            &event_tx,
                            &response_tx,
                        );
                    }
                }
            }
        }
    });
}

fn handle_notification(
    notification: TmuxNotification,
    maps: &Arc<Mutex<PaneMaps>>,
    output_tx: &OutputSender,
    event_tx: &EventSender,
    response_tx: &ResponseSender,
) {
    match notification {
        TmuxNotification::Output { pane_id, data } => {
            handle_output(TmuxPaneId(pane_id), Bytes::from(data), maps, output_tx);
        }
        TmuxNotification::PaneDied { pane_id } => {
            handle_pane_died(TmuxPaneId(pane_id), maps, event_tx);
        }
        TmuxNotification::ActiveWindowChanged { window_id } => {
            let _ = event_tx.blocking_send(BackendEvent::ActiveWindowChanged { window_id });
        }
        TmuxNotification::Exit => emit_all_panes_dead(maps, event_tx),
        TmuxNotification::CommandResponse(response) => {
            let _ = response_tx.blocking_send(response);
        }
        TmuxNotification::WindowAdd { .. }
        | TmuxNotification::WindowClose { .. }
        | TmuxNotification::SessionChanged { .. }
        | TmuxNotification::LayoutChange { .. } => {}
        TmuxNotification::NotificationParseError { kind, raw, error } => {
            tracing::debug!(?kind, raw, error, "failed to parse tmux notification");
        }
    }
}

fn handle_output(
    tmux_id: TmuxPaneId,
    data: Bytes,
    maps: &Arc<Mutex<PaneMaps>>,
    output_tx: &OutputSender,
) {
    let pane_id = {
        let mut maps = lock_maps(maps);
        if let Some(pane_id) = maps.pane_for_tmux(tmux_id) {
            Some(pane_id)
        } else {
            maps.pending_output
                .entry(tmux_id)
                .or_default()
                .push(data.clone());
            None
        }
    };

    if let Some(pane_id) = pane_id {
        let _ = output_tx.blocking_send((pane_id, data));
    }
}

fn handle_pane_died(tmux_id: TmuxPaneId, maps: &Arc<Mutex<PaneMaps>>, event_tx: &EventSender) {
    let pane_id = {
        let mut maps = lock_maps(maps);
        if let Some(pane_id) = maps.remove_tmux(tmux_id) {
            Some(pane_id)
        } else {
            maps.pending_died.insert(tmux_id);
            None
        }
    };

    if let Some(pane_id) = pane_id {
        let _ = event_tx.blocking_send(BackendEvent::PaneDied(pane_id));
    }
}

fn emit_all_panes_dead(maps: &Arc<Mutex<PaneMaps>>, event_tx: &EventSender) {
    let pane_ids = {
        let mut maps = lock_maps(maps);
        maps.remove_all()
    };

    for pane_id in pane_ids {
        let _ = event_tx.blocking_send(BackendEvent::PaneDied(pane_id));
    }
}

fn drain_stderr(mut stderr: impl Read + Send + 'static) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        while matches!(stderr.read(&mut buffer), Ok(count) if count > 0) {}
    });
}

fn lock_maps(maps: &Arc<Mutex<PaneMaps>>) -> std::sync::MutexGuard<'_, PaneMaps> {
    maps.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ensure_success(response: &CommandResponse) -> Result<(), Error> {
    match response.status {
        CommandResponseStatus::End(_) => Ok(()),
        CommandResponseStatus::Error(_) => {
            bail!("tmux command failed: {}", response_message(response))
        }
    }
}

fn ensure_pane_pid_response(response: &CommandResponse, tmux_id: TmuxPaneId) -> Result<(), Error> {
    let pid = response.lines.first().map_or("", |line| line.trim());
    if pid.parse::<u32>().is_ok_and(|value| value > 0) {
        return Ok(());
    }

    bail!(
        "tmux pane %{} did not report a valid pane pid: {}",
        tmux_id.0,
        response_message(response)
    )
}

fn response_message(response: &CommandResponse) -> String {
    if response.lines.is_empty() {
        return "tmux command failed".to_owned();
    }

    response.lines.join("\n")
}

fn response_pane_id(response: &CommandResponse) -> Option<TmuxPaneId> {
    response_pane_ids(response).into_iter().next()
}

fn response_pane_ids(response: &CommandResponse) -> Vec<TmuxPaneId> {
    response
        .lines
        .iter()
        .filter_map(|line| parse_tmux_pane_id(line))
        .collect()
}

fn parse_tmux_pane_id(line: &str) -> Option<TmuxPaneId> {
    let raw = line.trim();
    let id = raw.strip_prefix('%')?.parse().ok()?;
    Some(TmuxPaneId(id))
}

fn parse_initial_windows(output: &str) -> Result<Vec<TmuxInitialWindow>, Error> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_initial_window_row)
        .collect()
}

fn parse_initial_window_row(line: &str) -> Result<TmuxInitialWindow, Error> {
    let fields: Vec<_> = line.splitn(3, '\t').collect();
    let [window_id, window_index, raw_layout] = fields.as_slice() else {
        bail!("malformed tmux list-windows row: {line:?}");
    };
    let window_id = window_id
        .strip_prefix('@')
        .with_context(|| format!("malformed tmux window id in row: {line:?}"))?
        .parse::<u64>()
        .with_context(|| format!("invalid tmux window id in row: {line:?}"))?;
    let window_index = window_index
        .parse::<u8>()
        .with_context(|| format!("invalid tmux window index in row: {line:?}"))?;
    let layout = parse_layout(raw_layout)
        .with_context(|| format!("invalid tmux layout in row: {line:?}"))?;

    Ok(TmuxInitialWindow {
        window_id,
        window_index,
        layout,
    })
}

fn parse_initial_panes(output: &str) -> Result<Vec<TmuxInitialPane>, Error> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_initial_pane_row)
        .collect()
}

fn parse_initial_pane_row(line: &str) -> Result<TmuxInitialPane, Error> {
    let fields: Vec<_> = line.splitn(2, '\t').collect();
    let [pane_id, pid] = fields.as_slice() else {
        bail!("malformed tmux list-panes row: {line:?}");
    };
    let tmux_id = parse_tmux_pane_id(pane_id)
        .with_context(|| format!("invalid tmux pane id in row: {line:?}"))?;
    let pid = pid
        .parse::<u32>()
        .with_context(|| format!("invalid tmux pane pid in row: {line:?}"))?;

    Ok(TmuxInitialPane { tmux_id, pid })
}

fn split_window_command(cmd: &PaneCommand) -> String {
    let mut command = format!("split-window -h -P -F {}", quote_tmux_arg("#{pane_id}"));

    if let Some(cwd) = &cmd.cwd {
        command.push_str(" -c ");
        command.push_str(&quote_tmux_arg(&cwd.to_string_lossy()));
    }

    for (key, value) in &cmd.env {
        command.push_str(" -e ");
        command.push_str(&quote_tmux_arg(&format!("{key}={value}")));
    }

    command.push(' ');
    command.push_str(&quote_tmux_arg(&shell_command(cmd)));
    command
}

fn shell_command(cmd: &PaneCommand) -> String {
    let mut parts = Vec::with_capacity(cmd.args.len() + 1);
    parts.push(shell_quote(&cmd.program));
    parts.extend(cmd.args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }

    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn quote_tmux_arg(value: &str) -> String {
    quote_tmux_bytes(value.as_bytes())
}

fn quote_tmux_bytes(bytes: &[u8]) -> String {
    let mut quoted = String::from("\"");

    for &byte in bytes {
        match byte {
            b'\\' => quoted.push_str("\\\\"),
            b'"' => quoted.push_str("\\\""),
            b'\n' => quoted.push_str("\\n"),
            b'\r' => quoted.push_str("\\r"),
            b'\t' => quoted.push_str("\\t"),
            b'\x20'..=b'\x7e' => quoted.push(char::from(byte)),
            _ => {
                let _ = write!(quoted, "\\{byte:03o}");
            }
        }
    }

    quoted.push('"');
    quoted
}

fn new_session_name() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let uid = duration.as_secs()
        ^ u64::from(duration.subsec_nanos())
        ^ u64::from(std::process::id())
        ^ counter;
    format!("weave-{:08x}", uid & 0xffff_ffff)
}

fn tmux_session_exists(session_name: &str) -> anyhow::Result<bool> {
    let status = Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to check tmux session")?;

    Ok(status.success())
}

/// List `weave-*` sessions and kill those lacking the `@weave-instance` marker.
/// Best-effort: any failure is logged and ignored.
/// Returns the count of sessions actually killed.
fn cleanup_orphaned_weave_sessions() -> usize {
    let output = match Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}|#{@weave-instance}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(?error, "failed to list tmux sessions for orphan cleanup");
            return 0;
        }
    };

    if !output.status.success() {
        tracing::debug!("tmux list-sessions failed during orphan cleanup");
        return 0;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut killed = 0;
    for row in super::format::parse_rows(&stdout, 2) {
        let name = row[0];
        let marker = row[1];

        if !name.starts_with("weave-") || marker == "1" {
            continue;
        }

        let killed_ok = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);

        if killed_ok {
            killed += 1;
        } else {
            tracing::debug!(session = name, "failed to kill orphaned tmux session");
        }
    }

    killed
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_pane_pid_response, ingest_external_pane_with_verifier, CommandResponse,
        CommandResponseStatus, ExternalPaneVerifier, PaneMaps, TmuxPaneId,
    };
    use crate::backend::tmux::parser::BlockMarker;
    use crate::backend::PaneId;
    use anyhow::Error;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct MockVerifier {
        verified: Vec<TmuxPaneId>,
    }

    #[async_trait::async_trait]
    impl ExternalPaneVerifier for MockVerifier {
        async fn verify_external_pane(&mut self, tmux_id: TmuxPaneId) -> Result<(), Error> {
            self.verified.push(tmux_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn ingest_external_pane_registers_unknown_tmux_id_and_flushes_pending_output() {
        let maps = Arc::new(Mutex::new(PaneMaps::default()));
        let (output_tx, mut output_rx) = mpsc::channel(4);
        let (event_tx, _event_rx) = mpsc::channel(4);
        let tmux_id = TmuxPaneId(42);
        let mut next_id = 7;
        let mut verifier = MockVerifier::default();

        maps.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_output
            .insert(tmux_id, vec![Bytes::from_static(b"pending")]);
        assert!(output_rx.try_recv().is_err());

        let pane_id = ingest_external_pane_with_verifier(
            &maps,
            &output_tx,
            &event_tx,
            &mut next_id,
            &mut verifier,
            tmux_id,
        )
        .await
        .expect("ingest succeeds");

        assert_eq!(pane_id, PaneId(7));
        assert_eq!(next_id, 8);
        assert_eq!(verifier.verified, vec![tmux_id]);
        {
            let maps = maps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(maps.pane_for_tmux(tmux_id), Some(PaneId(7)));
            assert_eq!(maps.tmux_for_pane(PaneId(7)), Some(tmux_id));
        }
        assert_eq!(
            output_rx.recv().await,
            Some((PaneId(7), Bytes::from_static(b"pending")))
        );
    }

    #[tokio::test]
    async fn ingest_external_pane_reuses_existing_mapping_without_verifying() {
        let maps = Arc::new(Mutex::new(PaneMaps::default()));
        let (output_tx, _output_rx) = mpsc::channel(4);
        let (event_tx, _event_rx) = mpsc::channel(4);
        let tmux_id = TmuxPaneId(42);
        let mut next_id = 7;
        let mut verifier = MockVerifier::default();

        maps.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register(PaneId(3), tmux_id);

        let pane_id = ingest_external_pane_with_verifier(
            &maps,
            &output_tx,
            &event_tx,
            &mut next_id,
            &mut verifier,
            tmux_id,
        )
        .await
        .expect("ingest succeeds");

        assert_eq!(pane_id, PaneId(3));
        assert_eq!(next_id, 7);
        assert!(verifier.verified.is_empty());
    }

    #[test]
    fn pane_pid_response_requires_positive_integer_pid() {
        let marker = BlockMarker {
            raw: "%end 1".to_owned(),
            timestamp: Some(1),
            command_number: None,
            flags: None,
        };
        let ok = CommandResponse {
            begin: marker.clone(),
            status: CommandResponseStatus::End(marker.clone()),
            lines: vec!["12345".to_owned()],
        };
        let bad = CommandResponse {
            begin: marker.clone(),
            status: CommandResponseStatus::End(marker),
            lines: vec!["not-a-pid".to_owned()],
        };

        assert!(ensure_pane_pid_response(&ok, TmuxPaneId(9)).is_ok());
        assert!(ensure_pane_pid_response(&bad, TmuxPaneId(9)).is_err());
    }
}
