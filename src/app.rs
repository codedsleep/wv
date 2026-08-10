//! `App`: event loop + state owner.

use std::collections::HashSet;
use std::io::{self, Write};

use anyhow::{bail, Context};
use bytes::Bytes;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::anim::timeline::Timeline;
use crate::anim::tween::Easing;
use crate::backend::native::NativeBackend;
use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use crate::command::Command;
use crate::config::{Config, ThemeConfig};
use crate::input;
use crate::input::keymap::Keymap;
use crate::layout::geometry::{Direction, FRect, Rect, Split};
use crate::layout::tree::Node;
use crate::render::diff::{ColorMode, DiffRenderer};
use crate::render::{chrome, compositor};
use crate::session::protocol::{ClientToServer, ExitReason, ServerToClient};
use crate::session::sink::OutputSink;
use crate::session::SessionEvent;
use crate::term::pane::Pane;
use crate::term::surface::Surface;

const FOCUS_BORDER_TWEEN_DURATION: Duration = Duration::from_millis(120);
const OPEN_NEW_PANE_DURATION: Duration = Duration::from_millis(220);
const OPEN_SIBLING_DURATION: Duration = Duration::from_millis(180);
// Close tweens use ease-out-cubic so panes decelerate into the collapsed line.
const CLOSE_PANE_DURATION: Duration = Duration::from_millis(180);
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
const EVENT_CHANNEL_CAPACITY: usize = 64;

type BoxedBackend = Box<dyn PaneBackend>;

/// Workspaces addressable with `Alt+1` .. `Alt+9`.
pub const WORKSPACE_COUNT: usize = 9;

#[derive(Default)]
struct Workspace {
    root: Option<Node>,
    focused: Option<PaneId>,
    closing: HashSet<PaneId>,
}

impl Workspace {
    fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    fn pane_count(&self) -> usize {
        self.root.as_ref().map_or(0, count_workspace_leaves)
    }

    fn leaf_panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_leaf_pane_ids(root, &mut panes);
        }
        panes
    }
}

#[cfg(test)]
const fn test_theme() -> ThemeConfig {
    ThemeConfig {
        border_focused: crossterm::style::Color::Cyan,
        border_unfocused: crossterm::style::Color::DarkGrey,
        status_fg: crossterm::style::Color::White,
        status_bg: crossterm::style::Color::DarkBlue,
        accent: crossterm::style::Color::Red,
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum ResizeMode {
    Normal,
    HostResize,
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum DebugMode {
    Off,
    On,
}

impl DebugMode {
    const fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::On
        } else {
            Self::Off
        }
    }

    const fn is_enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ExitState {
    Running,
    Quit,
    Detached,
}

pub struct App {
    front: Surface,
    back: Surface,
    panes: Vec<Pane>,
    workspaces: Vec<Workspace>,
    current_workspace: usize,
    resize_mode: ResizeMode,
    backend: BoxedBackend,
    output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: mpsc::Receiver<BackendEvent>,
    sink: OutputSink,
    session_rx: Option<mpsc::Receiver<SessionEvent>>,
    client_id: Option<u64>,
    queue_buf: Vec<u8>,
    diff: DiffRenderer,
    timeline: Timeline,
    keymap: Keymap,
    theme: ThemeConfig,
    status_bar: bool,
    pane_titles: bool,
    tick_interval: Duration,
    debug: DebugMode,
    last_tick_dt: Duration,
    last_dirty_cells: usize,
    dirty: bool,
    exit: ExitState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Args {
    pub debug: bool,
    pub session_name: Option<String>,
    pub bare: bool,
    /// Run as the session daemon rather than spawning one and attaching.
    pub server: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttachArgs {
    pub session_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecArgs {
    pub session_name: Option<String>,
    pub command: Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchArgs {
    Run(Args),
    /// Run as the session daemon (`wv --server --session NAME`).
    Server(Args),
    Bare(Args),
    Attach(AttachArgs),
    Exec(ExecArgs),
    ListSessions,
}

struct BackendParts {
    backend: BoxedBackend,
    output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: mpsc::Receiver<BackendEvent>,
}

impl Args {
    pub fn parse_env() -> anyhow::Result<Self> {
        Self::parse(std::env::args().skip(1))
    }

    fn parse<I, S>(args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Self::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            let arg = arg.as_ref();

            if arg == "--debug" {
                parsed.debug = true;
            } else if arg == "--server" {
                parsed.server = true;
            } else if arg.starts_with("--server=") {
                bail!("`--server` does not accept a value");
            } else if arg == "--bare" || arg == "--no-attach" {
                parsed.bare = true;
            } else if arg.starts_with("--bare=") {
                bail!("`--bare` does not accept a value");
            } else if arg.starts_with("--no-attach=") {
                bail!("`--no-attach` does not accept a value");
            } else if arg == "--backend" || arg.starts_with("--backend=") {
                bail!(
                    "`--backend` was removed along with the tmux backend; weave now runs its own session server"
                );
            } else if arg == "--session" {
                let Some(value) = args.next() else {
                    bail!("missing value for `--session`; expected a weave session name");
                };
                let value = value.as_ref();
                validate_session_name(value)?;
                parsed.session_name = Some(value.to_owned());
            } else if let Some(value) = arg.strip_prefix("--session=") {
                validate_session_name(value)?;
                parsed.session_name = Some(value.to_owned());
            } else if arg == "attach" {
                bail!("`attach` must come first: use `wv attach [name]`");
            } else {
                bail!("unknown argument `{arg}`");
            }
        }

        if parsed.server && parsed.session_name.is_none() {
            bail!("`--server` requires an explicit `--session <name>`");
        }

        if parsed.bare && parsed.server {
            bail!("`--bare` spawns a session server; it cannot be combined with `--server`");
        }


        Ok(parsed)
    }
}

pub(crate) fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("invalid session name `{name}`: cannot be empty");
    }

    if name.len() > 64 {
        bail!("invalid session name `{name}`: maximum length is 64 characters");
    }

    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!(
            "invalid session name `{name}`: only ASCII letters, digits, hyphens, and underscores are allowed"
        );
    }

    Ok(())
}

impl AttachArgs {
    fn parse<I, S>(args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut session_name = None;

        for arg in args {
            let arg = arg.as_ref();
            if arg.starts_with('-') {
                bail!("`wv attach` does not accept `{arg}`; use `wv attach [name]`");
            }
            if session_name.replace(arg.to_owned()).is_some() {
                bail!("`wv attach` accepts at most one session name");
            }
        }

        Ok(Self { session_name })
    }
}

impl ExecArgs {
    /// Parse `wv exec [--session NAME] <command>`.
    fn parse<I, S>(args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut session_name = None;
        let mut command = None;
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            let arg = arg.as_ref();

            if arg == "--session" {
                let Some(value) = args.next() else {
                    bail!("missing value for `--session`; expected a weave session name");
                };
                let value = value.as_ref();
                validate_session_name(value)?;
                session_name = Some(value.to_owned());
            } else if let Some(value) = arg.strip_prefix("--session=") {
                validate_session_name(value)?;
                session_name = Some(value.to_owned());
            } else if arg.starts_with('-') {
                bail!("`wv exec` does not accept `{arg}`");
            } else if command.is_some() {
                bail!("`wv exec` takes a single command, for example `wv exec split-v`");
            } else {
                command = Some(Command::from_str(arg).with_context(|| {
                    format!("unknown command `{arg}`; try split-h, split-v, focus-left, focus-right, focus-up, focus-down, close, detach, quit, or workspace-1..9")
                })?);
            }
        }

        let command =
            command.context("`wv exec` requires a command, for example `wv exec split-v`")?;

        Ok(Self {
            session_name,
            command,
        })
    }
}

impl LaunchArgs {
    pub fn parse_env() -> anyhow::Result<Self> {
        Self::parse(std::env::args().skip(1))
    }

    fn parse<I, S>(args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<_>>();

        match args.first().map(String::as_str) {
            Some("attach") => return Ok(Self::Attach(AttachArgs::parse(args.iter().skip(1))?)),
            Some("exec") => {
                return Ok(Self::Exec(ExecArgs::parse(args.iter().skip(1))?));
            }
            Some("ls") => {
                if let Some(arg) = args.get(1) {
                    bail!("`wv ls` does not accept `{arg}`");
                }
                return Ok(Self::ListSessions);
            }
            _ => {}
        }

        let args = Args::parse(args)?;
        if args.server {
            Ok(Self::Server(args))
        } else if args.bare {
            Ok(Self::Bare(args))
        } else {
            Ok(Self::Run(args))
        }
    }
}

impl App {
    pub fn new(width: u16, height: u16, args: &Args) -> Self {
        Self::from_backend(
            width,
            height,
            args.debug,
            build_backend(),
            Vec::new(),
        )
    }

    /// Run this app as a session server: no client yet, socket events routed
    /// into the event loop.
    pub fn into_session(mut self, session_rx: mpsc::Receiver<SessionEvent>) -> Self {
        self.session_rx = Some(session_rx);
        self.sink = OutputSink::Null;
        self
    }

    /// Create a session without attaching, printing its name for scripts.
    pub async fn create_bare(args: Args) -> anyhow::Result<()> {
        let name = crate::session::launch::create_bare(&args).await?;
        println!("{name}");

        Ok(())
    }

    fn from_backend(
        width: u16,
        height: u16,
        debug: bool,
        backend_parts: BackendParts,
        initial_panes: Vec<PaneId>,
    ) -> Self {
        let config = Config::load();
        let tick_interval = frame_interval(config.ui.target_fps);
        let status_bar = config.ui.status_bar;
        let pane_titles = config.ui.pane_titles;
        let root_rect = Rect {
            x: 0,
            y: 0,
            w: width,
            h: if status_bar {
                height.saturating_sub(1)
            } else {
                height
            },
        };
        let root = flat_horizontal_root(&initial_panes, root_rect);
        let focused = initial_panes.first().copied();

        let mut workspaces: Vec<Workspace> =
            (0..WORKSPACE_COUNT).map(|_| Workspace::default()).collect();
        workspaces[0] = Workspace {
            root,
            focused,
            closing: HashSet::new(),
        };

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            panes: initial_panes
                .into_iter()
                .map(|pane| Pane::new(pane, width, height))
                .collect(),
            workspaces,
            current_workspace: 0,
            resize_mode: ResizeMode::Normal,
            backend: backend_parts.backend,
            output_rx: backend_parts.output_rx,
            event_rx: backend_parts.event_rx,
            sink: OutputSink::stdout(),
            session_rx: None,
            client_id: None,
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
            timeline: Timeline::new(),
            keymap: config.keymap,
            theme: config.theme,
            status_bar,
            pane_titles,
            tick_interval,
            debug: DebugMode::from_enabled(debug),
            last_tick_dt: Duration::ZERO,
            last_dirty_cells: 0,
            dirty: true,
            exit: ExitState::Running,
        }
    }

    fn current(&self) -> &Workspace {
        &self.workspaces[self.current_workspace]
    }

    fn current_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.current_workspace]
    }

    fn workspace_of_pane(&self, id: PaneId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|ws| ws.root.as_ref().is_some_and(|r| r.find_leaf(id).is_some()))
    }

    fn all_other_workspaces_empty(&self) -> bool {
        self.workspaces
            .iter()
            .enumerate()
            .all(|(i, ws)| i == self.current_workspace || ws.is_empty())
    }

    fn workspace_indicators(&self) -> Vec<chrome::WorkspaceIndicator> {
        self.workspaces
            .iter()
            .enumerate()
            .filter_map(|(idx, ws)| {
                let is_current = idx == self.current_workspace;
                if !is_current && ws.is_empty() {
                    return None;
                }
                Some(chrome::WorkspaceIndicator {
                    number: u8::try_from(idx + 1).unwrap_or(u8::MAX),
                    is_current,
                    pane_count: ws.pane_count(),
                })
            })
            .collect()
    }

    async fn switch_workspace(&mut self, number: u8) -> anyhow::Result<()> {
        if number < 1 || usize::from(number) > WORKSPACE_COUNT {
            return Ok(());
        }
        let target = usize::from(number) - 1;
        if target == self.current_workspace {
            return Ok(());
        }

        // Snap any inflight animations on the outgoing workspace so its layout
        // is at rest when we return to it later.
        self.snap_workspace_tweens(self.current_workspace).await?;

        self.current_workspace = target;
        if self.current().is_empty() {
            let pane_id = self.spawn_shell_pane(true).await?;
            let root_rect = self.root_rect();
            let ws = self.current_mut();
            ws.root = Some(Node::Leaf {
                pane: pane_id,
                rect_current: FRect::from(root_rect),
                rect_target: root_rect,
            });
            ws.focused = Some(pane_id);
        }
        self.recompute_layout();
        // Refresh PTY sizes for the incoming workspace so panes match the
        // current terminal geometry (it may have changed since they were last
        // visible).
        let pane_ids = self.current().leaf_panes();
        self.resize_mode = ResizeMode::HostResize;
        for pane in pane_ids {
            if let Some(rect) = self.leaf_rect_target(pane) {
                if let Some(p) = self.pane_mut(pane) {
                    p.resize(rect.w, rect.h);
                }
                let _ = self.resize_pane(pane, rect.w, rect.h).await;
            }
        }
        self.resize_mode = ResizeMode::Normal;
        self.dirty = true;
        Ok(())
    }

    async fn snap_workspace_tweens(&mut self, idx: usize) -> anyhow::Result<()> {
        // Finish any outstanding close animations synchronously: kill panes,
        // drop them from the tree, drop the tween entries.
        let closing: Vec<PaneId> = self.workspaces[idx].closing.iter().copied().collect();
        for pane in closing {
            self.workspaces[idx].closing.remove(&pane);
            self.timeline.clear_pane_tweens(pane);
            self.backend.kill(pane).await?;
            self.remove_pane(pane);
            let ws = &mut self.workspaces[idx];
            if let Some(root) = ws.root.as_mut() {
                if !root.close(pane) {
                    ws.root = None;
                    ws.focused = None;
                }
            }
        }

        // For all surviving leaves, snap rect_current to rect_target and drop
        // any remaining tweens so layout is at rest.
        let pane_ids: Vec<PaneId> = self.workspaces[idx].leaf_panes();
        for pane in pane_ids {
            self.timeline.clear_pane_tweens(pane);
        }
        self.timeline.clear_internal_ratio_tweens();
        if let Some(root) = self.workspaces[idx].root.as_mut() {
            snap_leaves_to_target(root);
        }
        Ok(())
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        if self.current().root.is_none() {
            let pane_id = self
                .spawn_shell_pane(true)
                .await
                .context("failed to spawn shell pane")?;
            let root_rect = self.root_rect();
            let ws = self.current_mut();
            ws.root = Some(Node::Leaf {
                pane: pane_id,
                rect_current: FRect::from(root_rect),
                rect_target: root_rect,
            });
            ws.focused = Some(pane_id);
            self.recompute_layout();
        }

        // A session server has no terminal of its own: input and size come
        // from the attached client over the socket, and the signals a terminal
        // would send it must not take the session down with the client.
        let session_mode = self.session_rx.is_some();
        let mut ticks = time::interval(self.tick_interval);
        let mut last_tick = time::Instant::now();
        // Constructing an `EventStream` requires a terminal on stdin, which a
        // session server does not have.
        let mut events = (!session_mode).then(EventStream::new);
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigwinch = signal(SignalKind::window_change())?;
        let mut sighup = signal(SignalKind::hangup())?;

        while self.exit == ExitState::Running {
            tokio::select! {
                _ = ticks.tick() => {
                    let now = time::Instant::now();
                    let dt = now.duration_since(last_tick);
                    last_tick = now;
                    self.tick(dt).await?;
                }
                Some((id, bytes)) = self.output_rx.recv() => {
                    if let Some(pane) = self.pane_mut(id) {
                        pane.process(&bytes);
                        self.dirty = true;
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    self.handle_backend_event(event);
                }
                Some(event) = next_session_event(&mut self.session_rx) => {
                    self.handle_session_event(event).await?;
                }
                event = next_terminal_event(&mut events), if !session_mode => {
                    self.handle_input(event).await?;
                }
                _ = sigint.recv(), if !session_mode => {
                    tracing::info!("SIGINT received");
                    break;
                }
                _ = sighup.recv() => {
                    // The client's terminal went away; the session outlives it.
                    tracing::info!("SIGHUP received");
                    if !session_mode {
                        break;
                    }
                }
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received");
                    break;
                }
                _ = sigwinch.recv(), if !session_mode => {
                    self.handle_resize().await;
                }
            }
        }

        if session_mode {
            // A no-op when the client already got its goodbye; otherwise it
            // learns the session ended rather than seeing a dropped socket.
            self.detach_client(ServerToClient::Exit(ExitReason::ServerShutdown));
            // Give the socket writer a chance to flush before the runtime goes.
            time::sleep(Duration::from_millis(50)).await;
        }

        if self.exit != ExitState::Detached {
            for pane_id in self.pane_ids() {
                let _ = self.backend.kill(pane_id).await;
            }
        }

        Ok(())
    }

    pub async fn execute(&mut self, cmd: Command) -> anyhow::Result<()> {
        self.execute_now(cmd).await
    }

    pub fn current_layout_root(&self) -> Option<&Node> {
        self.current().root.as_ref()
    }

    #[doc(hidden)]
    pub fn compose_current_surface(&self) -> Surface {
        let mut surface = Surface::new(self.front.width, self.front.height);
        let root = self.workspaces[self.current_workspace].root.as_ref();
        let focused = self.workspaces[self.current_workspace].focused;
        compositor::compose(
            root,
            &self.panes,
            focused,
            self.theme,
            &self.timeline,
            &mut surface,
            self.pane_titles,
        );
        surface
    }

    pub async fn advance_animations_by(&mut self, dt: Duration) -> anyhow::Result<()> {
        self.advance_animations(dt).await
    }

    async fn execute_now(&mut self, cmd: Command) -> anyhow::Result<()> {
        match cmd {
            Command::SplitH => self.split_focused(Split::Horizontal).await?,
            Command::SplitV => self.split_focused(Split::Vertical).await?,
            Command::FocusLeft => self.focus(Direction::Left),
            Command::FocusRight => self.focus(Direction::Right),
            Command::FocusUp => self.focus(Direction::Up),
            Command::FocusDown => self.focus(Direction::Down),
            Command::Close => self.close_focused().await?,
            Command::Detach => self.detach(),
            Command::Quit => self.exit = ExitState::Quit,
            Command::SwitchWorkspace(n) => self.switch_workspace(n).await?,
        }

        Ok(())
    }

    async fn spawn_shell_pane(&mut self, resize_immediately: bool) -> anyhow::Result<PaneId> {
        let mut cmd = default_pane_command();
        if let Some(focused) = self.current().focused {
            match self.backend.pane_cwd(focused).await {
                Ok(Some(cwd)) => cmd.cwd = Some(cwd),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(?error, ?focused, "failed to query pane cwd before spawn");
                }
            }
        }

        let pane_id = self.backend.spawn(cmd).await?;

        if resize_immediately {
            self.resize_pane(pane_id, self.back.width, self.back.height)
                .await
                .context("failed to resize shell pane")?;
        }

        self.panes
            .push(Pane::new(pane_id, self.back.width, self.back.height));

        Ok(pane_id)
    }

    fn detach(&mut self) {
        if self.session_rx.is_some() {
            // Detaching is purely a rendering concern: drop the client and
            // keep every pane running.
            self.detach_client(ServerToClient::Detached);
        } else {
            tracing::warn!("detach requires a weave session server; quitting");
            self.exit = ExitState::Quit;
        }
    }

    /// Take over rendering for a newly attached client.
    ///
    /// Any previous client is evicted first: one terminal at a time owns a
    /// session, so its size and color depth are unambiguous.
    pub async fn attach_client(
        &mut self,
        cols: u16,
        rows: u16,
        truecolor: bool,
        frames: mpsc::UnboundedSender<ServerToClient>,
    ) {
        self.detach_client(ServerToClient::Exit(ExitReason::TakenOver));

        self.diff.set_color_mode(if truecolor {
            ColorMode::Truecolor
        } else {
            ColorMode::Quantized
        });
        self.sink = OutputSink::client(frames);
        self.resize_to(cols, rows).await;

        // A reattaching client sees a settled layout, not the tail of whatever
        // animation was in flight when the previous client left.
        if let Err(error) = self.snap_workspace_tweens(self.current_workspace).await {
            tracing::warn!("failed to settle layout on attach: {error:#}");
        }
        self.recompute_layout();
        self.force_full_repaint();
    }

    /// Stop rendering, telling the outgoing client why if it is still there.
    fn detach_client(&mut self, farewell: ServerToClient) {
        let sink = std::mem::replace(&mut self.sink, OutputSink::Null);
        if let OutputSink::Client { frames, .. } = sink {
            let _ = frames.send(farewell);
        }
    }

    /// Force the next frame to redraw every cell.
    ///
    /// The client's screen is unknown after an attach, so the front buffer is
    /// reset to blank and the screen is cleared before the diff is computed.
    fn force_full_repaint(&mut self) {
        self.front = Surface::new(self.back.width, self.back.height);
        let _ = crossterm::queue!(
            self.sink,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        );
        self.dirty = true;
    }

    /// Handle one message from the session server's socket loop.
    async fn handle_session_event(&mut self, event: SessionEvent) -> anyhow::Result<()> {
        match event {
            SessionEvent::ClientAttached {
                id,
                cols,
                rows,
                truecolor,
                frames,
            } => {
                tracing::info!("client {id} attached at {cols}x{rows}");
                self.client_id = Some(id);
                self.attach_client(cols, rows, truecolor, frames).await;
            }
            SessionEvent::Message(ClientToServer::Attach { .. }) => {
                tracing::warn!("ignoring a second attach on an established connection");
            }
            SessionEvent::Message(ClientToServer::Input(event)) => {
                self.handle_input(Some(Ok(event))).await?;
            }
            SessionEvent::Message(ClientToServer::Resize { cols, rows }) => {
                self.resize_to(cols, rows).await;
            }
            SessionEvent::Message(ClientToServer::Exec(command)) => {
                self.execute(command).await?;
            }
            SessionEvent::Message(ClientToServer::Detach) => {
                self.detach_client(ServerToClient::Detached);
            }
            SessionEvent::Message(ClientToServer::Quit) => {
                self.detach_client(ServerToClient::Exit(ExitReason::Quit));
                self.exit = ExitState::Quit;
            }
            SessionEvent::ClientGone { id } => {
                if self.client_id == Some(id) {
                    tracing::info!("client {id} connection closed");
                    self.client_id = None;
                    self.sink = OutputSink::Null;
                }
            }
        }

        Ok(())
    }

    async fn split_focused(&mut self, split: Split) -> anyhow::Result<()> {
        let Some(focused) = self.current().focused else {
            return Ok(());
        };
        let Some(old_parent_rect) = self.leaf_rect_target(focused) else {
            return Ok(());
        };
        let new_pane = self.spawn_shell_pane(false).await?;

        let ws = self.current_mut();
        if let Some(root) = ws.root.as_mut() {
            root.split_focused(focused, split, new_pane);
            ws.focused = Some(new_pane);
            self.recompute_layout();
            self.start_open_tweens(focused, new_pane, split, old_parent_rect);
            self.dirty = true;
        }

        Ok(())
    }

    fn start_open_tweens(
        &mut self,
        sibling: PaneId,
        new_pane: PaneId,
        split: Split,
        old_parent_rect: Rect,
    ) {
        let Some(sibling_target) = self.leaf_rect_target(sibling) else {
            return;
        };
        let Some(new_target) = self.leaf_rect_target(new_pane) else {
            return;
        };

        let sibling_from = FRect::from(old_parent_rect);
        let sibling_to = FRect::from(sibling_target);
        let new_from = collapsed_open_rect(split, old_parent_rect, new_target);
        let new_to = FRect::from(new_target);

        self.set_leaf_rect_current(sibling, sibling_from);
        self.set_leaf_rect_current(new_pane, new_from);
        self.timeline.tween_leaf_rect(
            sibling,
            sibling_from,
            sibling_to,
            OPEN_SIBLING_DURATION,
            Easing::EaseOutCubic,
        );
        self.timeline.tween_leaf_rect(
            new_pane,
            new_from,
            new_to,
            OPEN_NEW_PANE_DURATION,
            Easing::EaseOutBack,
        );
    }

    fn focus(&mut self, dir: Direction) {
        let Some(focused) = self.current().focused else {
            return;
        };

        let next = self
            .current()
            .root
            .as_ref()
            .and_then(|root| root.focus_neighbor(focused, dir));

        if let Some(next) = next {
            self.start_focus_border_tweens(focused, next);
            self.current_mut().focused = Some(next);
            self.dirty = true;
        }
    }

    fn start_focus_border_tweens(&mut self, previous: PaneId, next: PaneId) {
        if previous == next {
            return;
        }

        let focused = self.current().focused;
        let focused_color = self.theme.border_focused;
        let unfocused_color = self.theme.border_unfocused;
        let previous_from =
            self.timeline
                .pane_border_color(previous, focused, focused_color, unfocused_color);
        let next_from =
            self.timeline
                .pane_border_color(next, focused, focused_color, unfocused_color);

        self.timeline.tween_pane_border_color(
            previous,
            previous_from,
            unfocused_color,
            FOCUS_BORDER_TWEEN_DURATION,
            Easing::EaseOutCubic,
        );
        self.timeline.tween_pane_border_color(
            next,
            next_from,
            focused_color,
            FOCUS_BORDER_TWEEN_DURATION,
            Easing::EaseOutCubic,
        );
    }

    async fn close_focused(&mut self) -> anyhow::Result<()> {
        let Some(focused) = self.current().focused else {
            return Ok(());
        };
        if self.current().closing.contains(&focused) {
            return Ok(());
        }

        let Some(root) = self.current().root.as_ref() else {
            self.current_mut().focused = None;
            if self.all_other_workspaces_empty() {
                self.exit = ExitState::Quit;
            }
            self.dirty = true;
            return Ok(());
        };

        let Some(close_plan) = close_plan(root, focused) else {
            self.backend.kill(focused).await?;
            self.remove_pane(focused);
            let last_pane_anywhere = self.all_other_workspaces_empty();
            let ws = self.current_mut();
            ws.root = None;
            ws.focused = None;
            if last_pane_anywhere {
                self.exit = ExitState::Quit;
            }
            self.dirty = true;
            return Ok(());
        };

        let Some(closing_current) = self.leaf_rect_current(focused) else {
            return Ok(());
        };
        let mut post_close_root = root.clone();
        if !post_close_root.close(focused) {
            return Ok(());
        }
        post_close_root.compute_layout(self.root_rect());

        let mut remaining_targets = Vec::new();
        collect_leaf_targets(&post_close_root, &mut remaining_targets);
        for (pane, target) in remaining_targets {
            if pane == focused {
                continue;
            }
            if let Some(from) = self.leaf_rect_current(pane) {
                self.timeline.tween_leaf_rect(
                    pane,
                    from,
                    FRect::from(target),
                    CLOSE_PANE_DURATION,
                    Easing::EaseOutCubic,
                );
            }
        }

        let collapsed =
            collapsed_close_rect(close_plan.split, close_plan.closing_is_a, closing_current);
        self.timeline.tween_leaf_rect(
            focused,
            closing_current,
            collapsed,
            CLOSE_PANE_DURATION,
            Easing::EaseOutCubic,
        );
        let new_focus = first_leaf_pane(&post_close_root);
        let ws = self.current_mut();
        ws.closing.insert(focused);
        ws.focused = new_focus;
        self.dirty = true;

        Ok(())
    }

    fn handle_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::PaneDied(id) => {
                tracing::info!("pane died: {id:?}");
                self.remove_pane(id);
                let owning_ws = self.workspace_of_pane(id);
                if let Some(ws_idx) = owning_ws {
                    let ws = &mut self.workspaces[ws_idx];
                    let was_focused = ws.focused == Some(id);
                    let closed = ws.root.as_mut().is_some_and(|root| root.close(id));
                    if closed {
                        let new_focus = ws.root.as_ref().and_then(first_leaf_pane);
                        ws.focused = new_focus;
                        if ws_idx == self.current_workspace {
                            self.recompute_layout();
                        }
                    } else if was_focused {
                        ws.root = None;
                        ws.focused = None;
                    }
                }
                if self.workspaces.iter().all(Workspace::is_empty) {
                    self.exit = ExitState::Quit;
                }
                self.dirty = true;
            }
            BackendEvent::SpawnFailed(id, message) => {
                tracing::error!("pane spawn failed: {id:?}: {message}");
                self.exit = ExitState::Quit;
            }
        }
    }

    async fn handle_input(&mut self, event: Option<io::Result<Event>>) -> anyhow::Result<()> {
        let Some(Ok(Event::Key(key))) = event else {
            return Ok(());
        };
        if matches!(key.kind, KeyEventKind::Release) {
            return Ok(());
        }

        if let Some(command) = self.keymap.command_for(&key) {
            self.dirty = true;
            self.execute(command).await?;
            return Ok(());
        }

        let Some(focused) = self.current().focused else {
            return Ok(());
        };

        if let Some(bytes) = input::encode(&key) {
            if let Err(error) = self.backend.write(focused, &bytes).await {
                tracing::warn!("failed to write input to pane: {error:#}");
            }
        }

        Ok(())
    }

    async fn handle_resize(&mut self) {
        let (cols, rows) = match crossterm::terminal::size() {
            Ok(size) => size,
            Err(error) => {
                tracing::warn!("failed to read terminal size after SIGWINCH: {error:#}");
                return;
            }
        };

        self.resize_to(cols, rows).await;
    }

    /// Resize the whole session to an explicit terminal size.
    ///
    /// Local runs get this from `SIGWINCH`; a session server gets it from the
    /// attached client, which is the only process that can see the terminal.
    pub async fn resize_to(&mut self, cols: u16, rows: u16) {
        if cols == self.back.width && rows == self.back.height {
            return;
        }

        self.front = Surface::new(cols, rows);
        self.back = Surface::new(cols, rows);
        for pane in &mut self.panes {
            pane.resize(cols, rows);
        }
        self.recompute_layout();

        self.resize_mode = ResizeMode::HostResize;
        for pane_id in self.pane_ids() {
            if let Err(error) = self.resize_pane(pane_id, cols, rows).await {
                tracing::warn!("failed to resize backend pane: {error:#}");
            }
        }
        self.resize_mode = ResizeMode::Normal;

        self.dirty = true;
    }

    async fn tick(&mut self, dt: Duration) -> anyhow::Result<()> {
        self.last_tick_dt = dt;
        self.advance_animations(dt).await?;

        // While detached there is nobody to render for: keep pane state and
        // tweens current, but skip compositing entirely.
        if !self.dirty || !self.sink.is_attached() {
            return Ok(());
        }

        self.queue_buf.clear();
        let root = self.workspaces[self.current_workspace].root.as_ref();
        let focused = self.workspaces[self.current_workspace].focused;
        compositor::compose(
            root,
            &self.panes,
            focused,
            self.theme,
            &self.timeline,
            &mut self.back,
            self.pane_titles,
        );
        if self.status_bar {
            let indicators = self.workspace_indicators();
            chrome::draw_status_bar(
                &mut self.back,
                "NORMAL",
                &indicators,
                chrono::Local::now(),
                self.theme,
            );
        }
        self.last_dirty_cells = self.estimated_dirty_cells();
        if self.debug.is_enabled() {
            let debug_overlay = self.debug_overlay();
            chrome::draw_debug_overlay(&mut self.back, debug_overlay);
        }
        self.diff.flush(&self.front, &self.back, &mut self.sink)?;
        self.sink.flush()?;
        std::mem::swap(&mut self.front, &mut self.back);
        self.back.clear();
        self.dirty = false;

        Ok(())
    }

    fn estimated_dirty_cells(&self) -> usize {
        usize::from(self.back.width) * usize::from(self.back.height)
    }

    fn debug_overlay(&self) -> chrome::DebugOverlay {
        let elapsed = self.last_tick_dt.as_secs_f64();
        let fps = if elapsed > 0.0 { 1.0 / elapsed } else { 0.0 };

        chrome::DebugOverlay {
            fps,
            frame_ms: elapsed * 1_000.0,
            tweens: self.timeline.active_count(),
            dirty_cells: self.last_dirty_cells,
        }
    }

    async fn advance_animations(&mut self, dt: Duration) -> anyhow::Result<()> {
        let advance = {
            let current_idx = self.current_workspace;
            let root = self.workspaces[current_idx].root.as_mut();
            self.timeline.advance(dt, root)
        };

        if !advance.changed_panes.is_empty() || !advance.completed_leaf_rects.is_empty() {
            self.dirty = true;
        }

        let completed_leaf_rects = advance.completed_leaf_rects;
        let mut completed_closings = completed_leaf_rects.clone();
        for pane in &self.current().closing {
            if !self.timeline.has_leaf_rect_tween(*pane) {
                completed_closings.push(*pane);
            }
        }
        self.resize_completed_leaf_tweens(&completed_leaf_rects)
            .await?;
        self.finish_completed_closings(&completed_closings).await?;

        Ok(())
    }

    async fn resize_completed_leaf_tweens(&mut self, completed: &[PaneId]) -> anyhow::Result<()> {
        for pane in completed {
            if self.current().closing.contains(pane) {
                continue;
            }
            let Some(rect) = self.leaf_rect_current(*pane) else {
                continue;
            };
            let rect = rect.to_rect();
            self.resize_pane(*pane, rect.w, rect.h).await?;
        }

        Ok(())
    }

    async fn finish_completed_closings(&mut self, completed: &[PaneId]) -> anyhow::Result<()> {
        for pane in completed {
            if !self.current_mut().closing.remove(pane) {
                continue;
            }

            self.backend.kill(*pane).await?;
            self.remove_pane(*pane);

            let ws = self.current_mut();
            if let Some(root) = ws.root.as_mut() {
                if root.close(*pane) {
                    let needs_refocus = ws.focused.is_none() || ws.focused == Some(*pane);
                    self.recompute_layout();
                    let ws = self.current_mut();
                    if needs_refocus {
                        ws.focused = ws.root.as_ref().and_then(first_leaf_pane);
                    }
                } else {
                    ws.root = None;
                    ws.focused = None;
                    if self.all_other_workspaces_empty() {
                        self.exit = ExitState::Quit;
                    }
                }
            }

            self.dirty = true;
        }

        Ok(())
    }

    fn root_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.back.width,
            h: if self.status_bar {
                self.back.height.saturating_sub(1)
            } else {
                self.back.height
            },
        }
    }

    fn recompute_layout(&mut self) {
        let root_rect = self.root_rect();
        if let Some(root) = self.current_mut().root.as_mut() {
            root.compute_layout(root_rect);
        }
    }

    fn leaf_rect_target(&self, pane: PaneId) -> Option<Rect> {
        for ws in &self.workspaces {
            if let Some(root) = ws.root.as_ref() {
                if let Some(Node::Leaf { rect_target, .. }) = root.find_leaf(pane) {
                    return Some(*rect_target);
                }
            }
        }
        None
    }

    fn leaf_rect_current(&self, pane: PaneId) -> Option<FRect> {
        for ws in &self.workspaces {
            if let Some(root) = ws.root.as_ref() {
                if let Some(Node::Leaf { rect_current, .. }) = root.find_leaf(pane) {
                    return Some(*rect_current);
                }
            }
        }
        None
    }

    fn set_leaf_rect_current(&mut self, pane: PaneId, rect: FRect) {
        for ws in &mut self.workspaces {
            let Some(root) = ws.root.as_mut() else {
                continue;
            };
            if let Some(Node::Leaf { rect_current, .. }) = root.find_leaf_mut(pane) {
                *rect_current = rect;
                return;
            }
        }
    }

    fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.iter().map(Pane::id).collect()
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id() == id)
    }

    fn remove_pane(&mut self, id: PaneId) {
        self.panes.retain(|pane| pane.id() != id);
    }

    async fn resize_pane(&mut self, pane: PaneId, cols: u16, rows: u16) -> anyhow::Result<()> {
        debug_assert!(self.is_safe_to_resize(pane));
        self.backend.resize(pane, cols, rows).await
    }

    fn is_safe_to_resize(&self, pane: PaneId) -> bool {
        self.resize_mode == ResizeMode::HostResize || !self.timeline.has_leaf_rect_tween(pane)
    }

    #[cfg(test)]
    fn with_backend_for_test(
        backend: BoxedBackend,
        width: u16,
        height: u16,
        pane_id: PaneId,
    ) -> Self {
        let (_output_tx, output_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let root_rect = Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };

        let mut workspaces: Vec<Workspace> =
            (0..WORKSPACE_COUNT).map(|_| Workspace::default()).collect();
        workspaces[0] = Workspace {
            root: Some(Node::Leaf {
                pane: pane_id,
                rect_current: FRect::from(root_rect),
                rect_target: root_rect,
            }),
            focused: Some(pane_id),
            closing: HashSet::new(),
        };

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            panes: vec![Pane::new(pane_id, width, height)],
            workspaces,
            current_workspace: 0,
            resize_mode: ResizeMode::Normal,
            backend,
            output_rx,
            event_rx,
            sink: OutputSink::stdout(),
            session_rx: None,
            client_id: None,
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
            timeline: Timeline::new(),
            keymap: Keymap::default(),
            theme: test_theme(),
            status_bar: true,
            pane_titles: true,
            tick_interval: frame_interval(crate::config::DEFAULT_TARGET_FPS),
            debug: DebugMode::Off,
            last_tick_dt: Duration::ZERO,
            last_dirty_cells: 0,
            dirty: true,
            exit: ExitState::Running,
        }
    }
}

fn build_backend() -> BackendParts {
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

    BackendParts {
        backend: Box::new(NativeBackend::with_senders(output_tx, event_tx)),
        output_rx,
        event_rx,
    }
}

/// Await the next terminal event, or never resolve when there is no terminal.
async fn next_terminal_event(events: &mut Option<EventStream>) -> Option<io::Result<Event>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// Await the next session event, or never resolve when there is no session.
///
/// `tokio::select!` needs a future for every branch; a plain `pending` keeps
/// the branch inert for local runs that have no server socket.
async fn next_session_event(rx: &mut Option<mpsc::Receiver<SessionEvent>>) -> Option<SessionEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn default_pane_command() -> PaneCommand {
    PaneCommand {
        program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
        args: Vec::new(),
        env: Vec::new(),
        cwd: std::env::current_dir().ok(),
    }
}

fn flat_horizontal_root(panes: &[PaneId], rect: Rect) -> Option<Node> {
    let (&first, remaining_panes) = panes.split_first()?;

    if remaining_panes.is_empty() {
        return Some(Node::Leaf {
            pane: first,
            rect_current: FRect::from(rect),
            rect_target: rect,
        });
    }

    let pane_count = u16::try_from(panes.len()).unwrap_or(u16::MAX);
    let ratio = 1.0 / f32::from(pane_count);
    let (first_rect, rest_rect) = rect.split(Split::Horizontal, ratio);
    Some(Node::Internal {
        split: Split::Horizontal,
        ratio,
        ratio_target: ratio,
        a: Box::new(Node::Leaf {
            pane: first,
            rect_current: FRect::from(first_rect),
            rect_target: first_rect,
        }),
        b: Box::new(flat_horizontal_root(remaining_panes, rest_rect)?),
        rect,
    })
}

fn count_workspace_leaves(node: &Node) -> usize {
    match node {
        Node::Leaf { .. } => 1,
        Node::Internal { a, b, .. } => count_workspace_leaves(a) + count_workspace_leaves(b),
    }
}

fn collect_leaf_pane_ids(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf { pane, .. } => out.push(*pane),
        Node::Internal { a, b, .. } => {
            collect_leaf_pane_ids(a, out);
            collect_leaf_pane_ids(b, out);
        }
    }
}

fn snap_leaves_to_target(node: &mut Node) {
    match node {
        Node::Leaf {
            rect_current,
            rect_target,
            ..
        } => {
            *rect_current = FRect::from(*rect_target);
        }
        Node::Internal { a, b, .. } => {
            snap_leaves_to_target(a);
            snap_leaves_to_target(b);
        }
    }
}

fn first_leaf_pane(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf { pane, .. } => Some(*pane),
        Node::Internal { a, b, .. } => first_leaf_pane(a).or_else(|| first_leaf_pane(b)),
    }
}

#[derive(Copy, Clone)]
struct ClosePlan {
    split: Split,
    closing_is_a: bool,
}

fn close_plan(node: &Node, pane: PaneId) -> Option<ClosePlan> {
    match node {
        Node::Leaf { .. } => None,
        Node::Internal { split, a, b, .. } if node_is_leaf_for(a, pane) => Some(ClosePlan {
            split: *split,
            closing_is_a: true,
        }),
        Node::Internal { split, a, b, .. } if node_is_leaf_for(b, pane) => Some(ClosePlan {
            split: *split,
            closing_is_a: false,
        }),
        Node::Internal { a, b, .. } => close_plan(a, pane).or_else(|| close_plan(b, pane)),
    }
}

fn node_is_leaf_for(node: &Node, pane: PaneId) -> bool {
    matches!(node, Node::Leaf { pane: leaf_pane, .. } if *leaf_pane == pane)
}

fn collect_leaf_targets(node: &Node, targets: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf {
            pane, rect_target, ..
        } => targets.push((*pane, *rect_target)),
        Node::Internal { a, b, .. } => {
            collect_leaf_targets(a, targets);
            collect_leaf_targets(b, targets);
        }
    }
}

fn frame_interval(target_fps: u16) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(target_fps))
}

fn collapsed_open_rect(split: Split, old_parent_rect: Rect, new_target: Rect) -> FRect {
    match split {
        Split::Vertical => FRect {
            x: f32::from(new_target.x),
            y: f32::from(old_parent_rect.y),
            w: 0.0,
            h: f32::from(old_parent_rect.h),
        },
        Split::Horizontal => FRect {
            x: f32::from(old_parent_rect.x),
            y: f32::from(new_target.y),
            w: f32::from(old_parent_rect.w),
            h: 0.0,
        },
    }
}

fn collapsed_close_rect(split: Split, closing_is_a: bool, current: FRect) -> FRect {
    match (split, closing_is_a) {
        (Split::Vertical, true) => FRect { w: 0.0, ..current },
        (Split::Vertical, false) => FRect {
            x: current.x + current.w,
            w: 0.0,
            ..current
        },
        (Split::Horizontal, true) => FRect { h: 0.0, ..current },
        (Split::Horizontal, false) => FRect {
            y: current.y + current.h,
            h: 0.0,
            ..current
        },
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Error;
    use crossterm::style::Color;
    use std::sync::{Arc, Mutex};

    use super::{
        frame_interval, validate_session_name, App, Args, AttachArgs, ExecArgs, ExitState,
        LaunchArgs, CLOSE_PANE_DURATION, FOCUS_BORDER_TWEEN_DURATION, OPEN_NEW_PANE_DURATION,
    };
    use crate::anim::tween::Easing;
    use crate::backend::{PaneBackend, PaneCommand, PaneId};
    use crate::command::Command;
    use crate::layout::geometry::{FRect, Split};
    use crate::layout::tree::Node;
    use tokio::time::Duration;

    struct MockBackend {
        next_id: PaneId,
        resized: Arc<Mutex<Vec<(PaneId, u16, u16)>>>,
    }

    #[derive(Clone, Default)]
    struct MockBackendHandle {
        resized: Arc<Mutex<Vec<(PaneId, u16, u16)>>>,
    }

    impl MockBackendHandle {
        fn resized(&self) -> Vec<(PaneId, u16, u16)> {
            lock_resized(&self.resized).clone()
        }

        fn clear_resized(&self) {
            lock_resized(&self.resized).clear();
        }
    }

    fn mock_backend(next_id: PaneId) -> (Box<dyn PaneBackend>, MockBackendHandle) {
        let handle = MockBackendHandle::default();
        let backend = MockBackend {
            next_id,
            resized: Arc::clone(&handle.resized),
        };

        (Box::new(backend), handle)
    }

    fn lock_resized(
        resized: &Mutex<Vec<(PaneId, u16, u16)>>,
    ) -> std::sync::MutexGuard<'_, Vec<(PaneId, u16, u16)>> {
        resized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[async_trait::async_trait]
    impl PaneBackend for MockBackend {
        async fn spawn(&mut self, _cmd: PaneCommand) -> Result<PaneId, Error> {
            let pane_id = self.next_id;
            self.next_id = PaneId(self.next_id.0.saturating_add(1));
            Ok(pane_id)
        }

        async fn write(&mut self, _id: PaneId, _data: &[u8]) -> Result<(), Error> {
            Ok(())
        }

        async fn resize(&mut self, id: PaneId, cols: u16, rows: u16) -> Result<(), Error> {
            lock_resized(&self.resized).push((id, cols, rows));
            Ok(())
        }

        async fn kill(&mut self, _id: PaneId) -> Result<(), Error> {
            Ok(())
        }

    }

    #[tokio::test]
    async fn execute_split_h_splits_focused_leaf_with_spawned_pane() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(Command::SplitH).await.expect("split succeeds");

        let ws = &app.workspaces[app.current_workspace];
        match ws.root.clone().expect("root exists") {
            Node::Internal {
                split, a, b, rect, ..
            } => {
                assert_eq!(split, Split::Horizontal);
                assert_eq!(rect.w, 80);
                assert_eq!(rect.h, 23);
                assert!(matches!(
                    *a,
                    Node::Leaf {
                        pane: PaneId(1),
                        ..
                    }
                ));
                assert!(matches!(
                    *b,
                    Node::Leaf {
                        pane: PaneId(2),
                        ..
                    }
                ));
            }
            Node::Leaf { .. } => panic!("expected split root"),
        }
        assert_eq!(ws.focused, Some(PaneId(2)));
        assert!(app.panes.iter().any(|pane| pane.id() == PaneId(2)));
        assert!(handle.resized().is_empty());
    }

    #[tokio::test]
    async fn advance_animations_updates_leaf_current_rect_and_marks_dirty() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.dirty = false;
        app.timeline.tween_leaf_rect(
            PaneId(1),
            FRect {
                x: 0.0,
                y: 0.0,
                w: 80.0,
                h: 24.0,
            },
            FRect {
                x: 10.0,
                y: 0.0,
                w: 70.0,
                h: 24.0,
            },
            Duration::from_millis(100),
            Easing::Linear,
        );

        app.advance_animations(Duration::from_millis(50))
            .await
            .expect("animations advance");

        assert!(app.dirty);
        match app.workspaces[app.current_workspace]
            .root
            .clone()
            .expect("root exists")
        {
            Node::Leaf { rect_current, .. } => {
                assert!((rect_current.x - 5.0).abs() < f32::EPSILON);
                assert!((rect_current.w - 75.0).abs() < f32::EPSILON);
            }
            Node::Internal { .. } => panic!("expected leaf root"),
        }
    }

    #[tokio::test]
    async fn focus_change_starts_border_tweens_and_reaches_targets() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(Command::SplitH).await.expect("split succeeds");
        assert_eq!(
            app.workspaces[app.current_workspace].focused,
            Some(PaneId(2))
        );

        app.execute(Command::FocusUp).await.expect("focus succeeds");

        let focused = app.workspaces[app.current_workspace].focused;
        assert_eq!(focused, Some(PaneId(1)));
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(2),
                focused,
                app.theme.border_focused,
                app.theme.border_unfocused,
            ),
            Color::Cyan
        );
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(1),
                focused,
                app.theme.border_focused,
                app.theme.border_unfocused,
            ),
            Color::DarkGrey
        );

        app.advance_animations(FOCUS_BORDER_TWEEN_DURATION)
            .await
            .expect("animations advance");

        let focused = app.workspaces[app.current_workspace].focused;
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(1),
                focused,
                app.theme.border_focused,
                app.theme.border_unfocused,
            ),
            Color::Cyan
        );
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(2),
                focused,
                app.theme.border_focused,
                app.theme.border_unfocused,
            ),
            Color::DarkGrey
        );
    }

    #[tokio::test]
    async fn split_v_starts_new_pane_collapsed_and_opens_to_target() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(Command::SplitV).await.expect("split succeeds");
        assert!(handle.resized().is_empty());

        let new_target = app
            .leaf_rect_target(PaneId(2))
            .expect("new pane has target");
        match app.workspaces[app.current_workspace]
            .root
            .as_ref()
            .expect("root exists")
            .find_leaf(PaneId(2))
            .expect("new pane exists")
        {
            Node::Leaf { rect_current, .. } => {
                assert!((rect_current.x - f32::from(new_target.x)).abs() < f32::EPSILON);
                assert!(rect_current.y.abs() < f32::EPSILON);
                assert!(rect_current.w.abs() < f32::EPSILON);
                assert!((rect_current.h - 24.0).abs() < f32::EPSILON);
            }
            Node::Internal { .. } => panic!("expected new leaf"),
        }

        app.advance_animations(OPEN_NEW_PANE_DURATION)
            .await
            .expect("animations advance");

        let resized = handle.resized();
        assert_eq!(resized.len(), 2);
        assert!(resized.contains(&(PaneId(1), 40, 23)));
        assert!(resized.contains(&(PaneId(2), 40, 23)));
        match app.workspaces[app.current_workspace]
            .root
            .as_ref()
            .expect("root exists")
            .find_leaf(PaneId(2))
            .expect("new pane exists")
        {
            Node::Leaf {
                rect_current,
                rect_target,
                ..
            } => assert_eq!(*rect_current, FRect::from(*rect_target)),
            Node::Internal { .. } => panic!("expected new leaf"),
        }
    }

    #[tokio::test]
    async fn close_keeps_pane_mid_tween_and_removes_after_completion() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(Command::SplitH).await.expect("split succeeds");
        app.advance_animations(OPEN_NEW_PANE_DURATION)
            .await
            .expect("open animation completes");
        handle.clear_resized();

        app.execute(Command::Close).await.expect("close starts");

        assert!(app.workspaces[app.current_workspace]
            .root
            .as_ref()
            .expect("root exists")
            .find_leaf(PaneId(2))
            .is_some());
        assert!(app.panes.iter().any(|pane| pane.id() == PaneId(2)));

        app.advance_animations(CLOSE_PANE_DURATION)
            .await
            .expect("close animation completes");

        assert_eq!(handle.resized(), vec![(PaneId(1), 80, 23)]);
        assert!(app.workspaces[app.current_workspace]
            .root
            .as_ref()
            .expect("root exists")
            .find_leaf(PaneId(2))
            .is_none());
        assert!(!app.panes.iter().any(|pane| pane.id() == PaneId(2)));
    }

    #[tokio::test]
    async fn switch_workspace_spawns_lazy_shell_and_changes_current() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(Command::SwitchWorkspace(2))
            .await
            .expect("switch succeeds");

        assert_eq!(app.current_workspace, 1);
        let ws = &app.workspaces[1];
        assert_eq!(ws.focused, Some(PaneId(2)));
        assert!(ws.root.is_some());
        // Workspace 1 (the original) keeps its pane.
        assert!(app.workspaces[0].root.is_some());
        assert!(app.workspaces[0].focused.is_some());
    }

    #[tokio::test]
    async fn switch_workspace_round_trip_keeps_existing_panes() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(Command::SwitchWorkspace(2))
            .await
            .expect("switch to 2");
        app.execute(Command::SwitchWorkspace(1))
            .await
            .expect("switch back to 1");

        assert_eq!(app.current_workspace, 0);
        assert_eq!(app.workspaces[0].focused, Some(PaneId(1)));
        assert_eq!(app.workspaces[1].focused, Some(PaneId(2)));
    }

    #[test]
    fn workspace_indicators_show_current_and_occupied() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let indicators = app.workspace_indicators();

        assert_eq!(indicators.len(), 1);
        assert_eq!(indicators[0].number, 1);
        assert!(indicators[0].is_current);
        assert_eq!(indicators[0].pane_count, 1);
    }

    #[tokio::test]
    async fn detach_on_native_quits_without_marking_detached() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(Command::Detach).await.expect("detach succeeds");

        assert_eq!(app.exit, ExitState::Quit);
    }

    #[test]
    fn frame_interval_uses_configured_fps() {
        assert_eq!(frame_interval(160), Duration::from_micros(6_250));
    }

    #[test]
    fn args_default_to_native_backend() {
        assert_eq!(
            Args::parse(std::iter::empty::<&str>()).expect("args parse"),
            Args {
                debug: false,
                session_name: None,
                bare: false,
                server: false,
            }
        );
    }

    #[test]
    fn args_parse_session_flag() {
        assert_eq!(
            Args::parse(["--session", "foo"])
                .expect("args parse")
                .session_name,
            Some("foo".to_owned())
        );
    }

    #[test]
    fn args_parse_session_equals_flag() {
        assert_eq!(
            Args::parse(["--session=foo"])
                .expect("args parse")
                .session_name,
            Some("foo".to_owned())
        );
    }

    #[test]
    fn args_reject_invalid_session_name_characters() {
        assert!(Args::parse(["--session", "foo;rm -rf /"]).is_err());
    }

    #[test]
    fn args_reject_too_long_session_name() {
        let too_long = "a".repeat(65);
        assert!(Args::parse(["--session", &too_long]).is_err());
    }

    #[test]
    fn args_reject_empty_session_name() {
        assert!(Args::parse(["--session", ""]).is_err());
    }

    #[test]
    fn args_parse_bare_mode() {
        let args = Args::parse(["--bare", "--session", "foo"]).expect("args parse");

        assert!(args.bare);
        assert_eq!(args.session_name, Some("foo".to_owned()));
    }

    #[test]
    fn args_parse_no_attach_alias() {
        let args = Args::parse(["--no-attach", "--session", "foo"]).expect("args parse");

        assert!(args.bare);
        assert_eq!(args.session_name, Some("foo".to_owned()));
    }

    #[test]
    fn args_reject_the_removed_backend_flag() {
        let error = Args::parse(["--backend", "tmux"])
            .expect_err("--backend should be gone")
            .to_string();

        assert!(error.contains("was removed"), "{error}");
    }

    #[test]
    fn args_parse_server_flag() {
        let args = Args::parse(["--server", "--session", "foo"]).expect("args parse");

        assert!(args.server);
        assert_eq!(args.session_name, Some("foo".to_owned()));
    }

    #[test]
    fn args_reject_server_without_session() {
        let error = Args::parse(["--server"])
            .expect_err("server without session should fail")
            .to_string();

        assert!(error.contains("requires an explicit `--session"), "{error}");
    }

    #[test]
    fn args_accept_bare_without_a_session_name() {
        // An auto-named session is fine here: `wv --bare` prints the name it
        // generated so a script can use it.
        let args = Args::parse(["--bare"]).expect("args parse");

        assert!(args.bare);
        assert_eq!(args.session_name, None);
    }

    #[test]
    fn args_reject_bare_value() {
        assert!(Args::parse(["--bare=true", "--session", "foo"]).is_err());
    }

    #[test]
    fn launch_args_parse_attach_subcommand() {
        assert_eq!(
            LaunchArgs::parse(["attach", "weave-test"]).expect("launch args parse"),
            LaunchArgs::Attach(AttachArgs {
                session_name: Some("weave-test".to_owned()),
            })
        );
    }

    #[test]
    fn launch_args_parse_exec_subcommand() {
        assert_eq!(
            LaunchArgs::parse(["exec", "split-v"]).expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: None,
                command: Command::SplitV,
            })
        );
        assert_eq!(
            LaunchArgs::parse(["exec", "--session", "main", "workspace-3"])
                .expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: Some("main".to_owned()),
                command: Command::SwitchWorkspace(3),
            })
        );
    }

    #[test]
    fn launch_args_reject_unknown_exec_commands() {
        let error = LaunchArgs::parse(["exec", "split-window"])
            .expect_err("tmux verbs are no longer accepted")
            .to_string();

        assert!(error.contains("unknown command"), "{error}");
    }

    #[test]
    fn launch_args_parse_bare_variant() {
        match LaunchArgs::parse(["--bare", "--session", "foo"]).expect("launch args parse") {
            LaunchArgs::Bare(args) => {
                assert!(args.bare);
                assert_eq!(args.session_name, Some("foo".to_owned()));
            }
            other => panic!("expected bare launch args, got {other:?}"),
        }
    }

    #[test]
    fn launch_args_parse_server_variant() {
        match LaunchArgs::parse(["--server", "--session", "foo"]).expect("launch args parse") {
            LaunchArgs::Server(args) => {
                assert!(args.server);
                assert_eq!(args.session_name, Some("foo".to_owned()));
            }
            other => panic!("expected server launch args, got {other:?}"),
        }
    }

    #[test]
    fn launch_args_reject_attach_session_flag() {
        assert!(LaunchArgs::parse(["attach", "weave-test", "--session", "x"]).is_err());
    }

    #[test]
    fn validate_session_name_accepts_safe_names() {
        assert!(validate_session_name("foo").is_ok());
        assert!(validate_session_name("foo_bar-123").is_ok());
    }

    #[test]
    fn validate_session_name_rejects_empty_too_long_and_bad_chars() {
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name(&"a".repeat(65)).is_err());
        assert!(validate_session_name("foo;rm").is_err());
        assert!(validate_session_name("foo bar").is_err());
        assert!(validate_session_name("foo/bar").is_err());
    }

}
