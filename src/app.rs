//! `App`: event loop + state owner.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{bail, Context};
use bytes::Bytes;
use chrono::{Local, TimeZone};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::anim::timeline::Timeline;
use crate::anim::tween::Easing;
use crate::backend::native::NativeBackend;
use crate::backend::tmux::layout::LayoutAst;
use crate::backend::tmux::reconcile::{self, LayoutDelta, StructuralPath};
use crate::backend::tmux::windows::{AddOutcome, WindowMap};
use crate::backend::tmux::{TmuxBackend, TmuxInitialState};
use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use crate::command::Command;
use crate::config::{Config, ThemeConfig};
use crate::input;
use crate::input::keymap::Keymap;
use crate::layout::geometry::{Direction, FRect, Rect, Split};
use crate::layout::tree::Node;
use crate::render::diff::DiffRenderer;
use crate::render::{chrome, compositor};
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

pub use crate::backend::tmux::windows::WORKSPACE_COUNT;

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
    tmux_windows: WindowMap,
    external_in_flight: HashSet<usize>,
    external_animation_panes: HashMap<usize, HashSet<PaneId>>,
    pending_internal_commands: VecDeque<(usize, Command)>,
    resize_mode: ResizeMode,
    backend_kind: BackendKind,
    backend: BoxedBackend,
    output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: mpsc::Receiver<BackendEvent>,
    stdout: io::Stdout,
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

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BackendKind {
    #[default]
    Native,
    Tmux,
}

impl BackendKind {
    fn from_cli_value(value: &str) -> anyhow::Result<Self> {
        match value {
            "native" => Ok(Self::Native),
            "tmux" => Ok(Self::Tmux),
            _ => bail!("unknown backend `{value}`; expected `native` or `tmux`"),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Args {
    pub debug: bool,
    pub backend: BackendKind,
    pub session_name: Option<String>,
    pub bare: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttachArgs {
    pub session_name: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecArgs {
    pub session_name: Option<String>,
    pub tmux_args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchArgs {
    Run(Args),
    Bare(Args),
    Attach(AttachArgs),
    Exec(ExecArgs),
    ListSessions { windows: bool },
}

struct BackendParts {
    kind: BackendKind,
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
            } else if arg == "--bare" || arg == "--no-attach" {
                parsed.bare = true;
            } else if arg.starts_with("--bare=") {
                bail!("`--bare` does not accept a value");
            } else if arg.starts_with("--no-attach=") {
                bail!("`--no-attach` does not accept a value");
            } else if arg == "--backend" {
                let Some(value) = args.next() else {
                    bail!("missing value for `--backend`; expected `native` or `tmux`");
                };
                parsed.backend = BackendKind::from_cli_value(value.as_ref())?;
            } else if let Some(value) = arg.strip_prefix("--backend=") {
                parsed.backend = BackendKind::from_cli_value(value)?;
            } else if arg == "--session" {
                let Some(value) = args.next() else {
                    bail!("missing value for `--session`; expected a tmux session name");
                };
                let value = value.as_ref();
                validate_session_name(value)?;
                parsed.session_name = Some(value.to_owned());
            } else if let Some(value) = arg.strip_prefix("--session=") {
                validate_session_name(value)?;
                parsed.session_name = Some(value.to_owned());
            } else if arg == "attach" {
                bail!(
                    "`attach` reconnects to tmux sessions only; use `wv attach [name]`, not `--backend native attach`"
                );
            } else {
                bail!("unknown argument `{arg}`");
            }
        }

        if parsed.bare && parsed.backend == BackendKind::Native {
            bail!(
                "--bare requires --backend tmux; native backend has no persistent session to leave behind"
            );
        }

        if parsed.bare && parsed.session_name.is_none() {
            bail!(
                "--bare requires an explicit --session <name>; auto-generated names defeat the purpose of bare mode"
            );
        }

        Ok(parsed)
    }
}

pub(crate) fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("invalid tmux session name `{name}`: cannot be empty");
    }

    if name.len() > 64 {
        bail!("invalid tmux session name `{name}`: maximum length is 64 characters");
    }

    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!(
            "invalid tmux session name `{name}`: only ASCII letters, digits, hyphens, and underscores are allowed"
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
    fn parse<I, S>(session_name: Option<String>, args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<_>>();
        let (session_name, tmux_args) = parse_exec_args(session_name, &args)?;
        if tmux_args.is_empty() {
            bail!("`wv exec` requires tmux arguments, for example `wv exec split-window -h`");
        }

        Ok(Self {
            session_name,
            tmux_args,
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
                return Ok(Self::Exec(ExecArgs::parse(None, args.iter().skip(1))?));
            }
            Some("ls") => {
                let mut windows = false;
                for arg in args.iter().skip(1) {
                    if arg == "--windows" {
                        windows = true;
                    } else if arg.starts_with("--windows=") {
                        bail!("`wv ls --windows` does not accept a value");
                    } else {
                        bail!("`wv ls` does not accept `{arg}`");
                    }
                }
                return Ok(Self::ListSessions { windows });
            }
            _ => {}
        }

        if let Some(exec_args) = parse_session_prefixed_exec_args(&args)? {
            return Ok(Self::Exec(exec_args));
        }

        let args = Args::parse(args)?;
        if args.bare {
            Ok(Self::Bare(args))
        } else {
            Ok(Self::Run(args))
        }
    }
}

fn parse_session_prefixed_exec_args(args: &[String]) -> anyhow::Result<Option<ExecArgs>> {
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if arg == "exec" {
            let (session_name, prefix_args) = parse_exec_args(None, &args[..index])?;
            debug_assert!(prefix_args.is_empty());
            return Ok(Some(ExecArgs::parse(
                session_name,
                args.iter().skip(index + 1),
            )?));
        }

        if let Some(width) = session_option_width(arg) {
            index += width;
        } else {
            return Ok(None);
        }
    }

    Ok(None)
}

fn session_option_width(arg: &str) -> Option<usize> {
    if arg == "--session" || arg == "--session-named" {
        Some(2)
    } else if arg.starts_with("--session=") || arg.starts_with("--session-named=") {
        Some(1)
    } else {
        None
    }
}

fn parse_exec_args(
    mut session_name: Option<String>,
    args: &[String],
) -> anyhow::Result<(Option<String>, Vec<String>)> {
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if parse_session_option(arg, args.get(index + 1), &mut session_name)? {
            index += session_option_width(arg).expect("parsed session option has a width");
        } else {
            break;
        }
    }

    Ok((session_name, args[index..].to_vec()))
}

fn parse_session_option(
    arg: &str,
    next: Option<&String>,
    session_name: &mut Option<String>,
) -> anyhow::Result<bool> {
    if arg == "--session" || arg == "--session-named" {
        let Some(value) = next else {
            bail!("missing value for `{arg}`; expected a tmux session name");
        };
        set_exec_session_name(session_name, value)?;
        return Ok(true);
    }

    if let Some(value) = arg
        .strip_prefix("--session=")
        .or_else(|| arg.strip_prefix("--session-named="))
    {
        set_exec_session_name(session_name, value)?;
        return Ok(true);
    }

    Ok(false)
}

fn set_exec_session_name(session_name: &mut Option<String>, value: &str) -> anyhow::Result<()> {
    validate_session_name(value)?;
    if session_name.replace(value.to_owned()).is_some() {
        bail!("`wv exec` accepts at most one session name");
    }
    Ok(())
}

impl App {
    pub async fn new(width: u16, height: u16, args: Args) -> anyhow::Result<Self> {
        let backend_parts = build_backend(args.backend, args.session_name).await?;
        Ok(Self::from_backend(
            width,
            height,
            args.debug,
            backend_parts,
            Vec::new(),
        ))
    }

    pub async fn attach(width: u16, height: u16, args: AttachArgs) -> anyhow::Result<Self> {
        let (backend_parts, initial_state) = build_attach_backend(args.session_name).await?;
        let mut app = Self::from_backend(width, height, false, backend_parts, Vec::new());

        app.hydrate_attached_tmux_state(initial_state).await?;
        let pane_ids = app.pane_ids();
        app.resize_attached_panes(&pane_ids).await?;
        Ok(app)
    }

    pub async fn create_bare(args: Args) -> anyhow::Result<()> {
        if args.backend != BackendKind::Tmux {
            bail!("--bare requires --backend tmux; native backend has no persistent session to leave behind");
        }

        let session_name = args.session_name.clone().with_context(|| {
            "--bare requires an explicit --session <name>; auto-generated names defeat the purpose of bare mode"
        })?;
        let mut backend_parts = build_backend(args.backend, args.session_name).await?;
        backend_parts.backend.detach().await?;
        println!("{session_name}");

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
            tmux_windows: WindowMap::new(),
            external_in_flight: HashSet::new(),
            external_animation_panes: HashMap::new(),
            pending_internal_commands: VecDeque::new(),
            resize_mode: ResizeMode::Normal,
            backend_kind: backend_parts.kind,
            backend: backend_parts.backend,
            output_rx: backend_parts.output_rx,
            event_rx: backend_parts.event_rx,
            stdout: io::stdout(),
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
                    external_in_flight: self.has_external_changes_in_flight(idx),
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

        if self.backend_kind == BackendKind::Tmux {
            self.backend.select_window(target).await?;
        }

        // Snap any inflight animations on the outgoing workspace so its layout
        // is at rest when we return to it later.
        self.snap_workspace_tweens(self.current_workspace).await?;
        self.clear_external_tracking_for_workspace(self.current_workspace);

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

        let mut ticks = time::interval(self.tick_interval);
        let mut last_tick = time::Instant::now();
        let mut events = EventStream::new();
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigwinch = signal(SignalKind::window_change())?;

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
                    self.handle_backend_event(event).await?;
                }
                event = events.next() => {
                    self.handle_input(event).await?;
                }
                _ = sigint.recv() => {
                    tracing::info!("SIGINT received");
                    break;
                }
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received");
                    break;
                }
                _ = sigwinch.recv() => {
                    self.handle_resize().await;
                }
            }
        }

        if self.exit != ExitState::Detached {
            for pane_id in self.pane_ids() {
                let _ = self.backend.kill(pane_id).await;
            }
        }

        Ok(())
    }

    pub async fn execute(&mut self, cmd: Command) -> anyhow::Result<()> {
        if self.should_queue_internal_command(cmd) {
            self.pending_internal_commands
                .push_back((self.current_workspace, cmd));
            self.dirty = true;
            return Ok(());
        }

        self.execute_now(cmd).await
    }

    pub fn current_layout_root(&self) -> Option<&Node> {
        self.current().root.as_ref()
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
            Command::Detach => self.detach().await?,
            Command::Quit => self.exit = ExitState::Quit,
            Command::SwitchWorkspace(n) => self.switch_workspace(n).await?,
            Command::GotoWindow(window_id) => self.goto_window(window_id).await?,
        }

        Ok(())
    }

    async fn goto_window(&mut self, window_id: u64) -> anyhow::Result<()> {
        let known_mapped = self.tmux_windows.workspace_for_window(window_id).is_some();
        let known_overflow = self
            .tmux_windows
            .overflow_windows()
            .any(|overflow_window_id| overflow_window_id == window_id);

        if !known_mapped && !known_overflow {
            tracing::debug!(window_id, "ignoring goto-window for unknown tmux window");
            return Ok(());
        }

        self.backend.select_window_by_id(window_id).await
    }

    fn should_queue_internal_command(&self, cmd: Command) -> bool {
        is_layout_mutating_command(cmd)
            && self.has_external_changes_in_flight(self.current_workspace)
    }

    fn has_external_changes_in_flight(&self, workspace_idx: usize) -> bool {
        self.external_in_flight.contains(&workspace_idx)
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

    async fn detach(&mut self) -> anyhow::Result<()> {
        match self.backend_kind {
            BackendKind::Native => {
                tracing::warn!("detach only supported on --backend tmux; quitting");
                self.exit = ExitState::Quit;
            }
            BackendKind::Tmux => {
                self.backend.detach().await?;
                self.exit = ExitState::Detached;
            }
        }

        Ok(())
    }

    async fn resize_attached_panes(&mut self, pane_ids: &[PaneId]) -> anyhow::Result<()> {
        self.resize_mode = ResizeMode::HostResize;
        for pane_id in pane_ids {
            let Some(rect) = self.leaf_rect_target(*pane_id) else {
                continue;
            };
            if let Some(pane) = self.pane_mut(*pane_id) {
                pane.resize(rect.w, rect.h);
            }
            self.resize_pane(*pane_id, rect.w, rect.h).await?;
        }
        self.resize_mode = ResizeMode::Normal;
        Ok(())
    }

    async fn hydrate_attached_tmux_state(
        &mut self,
        initial_state: TmuxInitialState,
    ) -> anyhow::Result<()> {
        for pane in &initial_state.panes {
            let pane_id = self.backend.ingest_external_pane(pane.tmux_id.0).await?;
            self.ensure_pane_registered(pane_id);
        }

        self.tmux_windows = WindowMap::new();
        for workspace in &mut self.workspaces {
            workspace.root = None;
            workspace.focused = None;
            workspace.closing.clear();
        }

        for window in initial_state.windows {
            let workspace_idx = match self
                .tmux_windows
                .on_window_add(window.window_id, usize::from(window.window_index))
            {
                AddOutcome::Assigned(workspace_idx) => workspace_idx,
                AddOutcome::Overflow => {
                    tracing::debug!(
                        window_id = window.window_id,
                        window_index = window.window_index,
                        "skipping tmux window outside workspace range"
                    );
                    continue;
                }
            };

            let layout = self.translate_external_layout_panes(window.layout).await?;
            let normalized = reconcile::normalize(layout);
            let root = node_from_layout_ast(&normalized, None, &mut Vec::new());

            let ws = &mut self.workspaces[workspace_idx];
            ws.focused = first_leaf_pane(&root);
            ws.root = Some(root);
        }

        if self.workspaces[self.current_workspace].root.is_none() {
            self.current_workspace = self
                .workspaces
                .iter()
                .position(|workspace| workspace.root.is_some())
                .unwrap_or(0);
        }

        self.timeline.clear_internal_ratio_tweens();
        for pane in self.pane_ids() {
            self.timeline.clear_pane_tweens(pane);
        }
        self.external_in_flight.clear();
        self.external_animation_panes.clear();
        self.pending_internal_commands.clear();
        self.dirty = true;

        Ok(())
    }

    pub async fn apply_external_layout_change(
        &mut self,
        window_id: u64,
        layout: LayoutAst,
    ) -> anyhow::Result<()> {
        let workspace_idx = self.workspace_for_tmux_window(window_id);
        self.snap_workspace_tweens(workspace_idx).await?;
        self.clear_external_tracking_for_workspace(workspace_idx);

        let layout = self.translate_external_layout_panes(layout).await?;
        let normalized = reconcile::normalize(layout);
        let Some(old_root) = self.workspaces[workspace_idx].root.clone() else {
            self.install_external_layout_without_diff(workspace_idx, &normalized);
            return Ok(());
        };
        let deltas = reconcile::diff(&old_root, &normalized);
        if deltas.is_empty() {
            return Ok(());
        }

        let new_root = node_from_layout_ast(&normalized, Some(&old_root), &mut Vec::new());
        self.workspaces[workspace_idx].root = Some(new_root);

        self.ensure_external_panes(&deltas);
        let animated_panes = self.queue_external_layout_tweens(workspace_idx, &old_root, &deltas);
        if !animated_panes.is_empty() || self.timeline.has_internal_ratio_tweens() {
            self.external_in_flight.insert(workspace_idx);
            self.external_animation_panes
                .insert(workspace_idx, animated_panes);
        }
        self.remove_external_panes(&deltas);

        let ws = &mut self.workspaces[workspace_idx];
        if ws.focused.map_or(true, |pane| {
            ws.root
                .as_ref()
                .map_or(true, |root| root.find_leaf(pane).is_none())
        }) {
            ws.focused = ws.root.as_ref().and_then(first_leaf_pane);
        }

        if workspace_idx == self.current_workspace {
            self.dirty = true;
        }

        Ok(())
    }

    fn workspace_for_tmux_window(&self, window_id: u64) -> usize {
        self.tmux_windows
            .workspace_for_window(window_id)
            .unwrap_or(0)
    }

    fn install_external_layout_without_diff(&mut self, workspace_idx: usize, layout: &LayoutAst) {
        let root = node_from_layout_ast(layout, None, &mut Vec::new());
        let mut panes = Vec::new();
        collect_layout_panes(layout, &mut panes);
        for pane in panes {
            self.ensure_pane_registered(pane);
        }

        let ws = &mut self.workspaces[workspace_idx];
        ws.focused = first_leaf_pane(&root);
        ws.root = Some(root);
        if workspace_idx == self.current_workspace {
            self.dirty = true;
        }
    }

    fn ensure_external_panes(&mut self, deltas: &[LayoutDelta]) {
        for delta in deltas {
            if let LayoutDelta::AddPane { pane, .. } = delta {
                self.ensure_pane_registered(*pane);
            }
        }
    }

    fn ensure_pane_registered(&mut self, pane: PaneId) {
        if self.panes.iter().any(|existing| existing.id() == pane) {
            return;
        }

        self.panes
            .push(Pane::new(pane, self.back.width, self.back.height));
    }

    async fn translate_external_layout_panes(
        &mut self,
        layout: LayoutAst,
    ) -> anyhow::Result<LayoutAst> {
        let mut tmux_ids = Vec::new();
        collect_layout_tmux_panes(&layout, &mut tmux_ids);
        tmux_ids.sort_unstable();
        tmux_ids.dedup();

        let mut pane_ids = HashMap::with_capacity(tmux_ids.len());
        for tmux_id in tmux_ids {
            let pane_id = self.backend.ingest_external_pane(tmux_id).await?;
            pane_ids.insert(tmux_id, pane_id.0);
        }

        Ok(rewrite_layout_pane_ids(layout, &pane_ids))
    }

    fn queue_external_layout_tweens(
        &mut self,
        workspace_idx: usize,
        old_root: &Node,
        deltas: &[LayoutDelta],
    ) -> HashSet<PaneId> {
        let Some((new_targets, ratio_tweens)) =
            self.workspaces[workspace_idx]
                .root
                .as_ref()
                .map(|new_root| {
                    let mut new_targets = Vec::new();
                    collect_leaf_targets(new_root, &mut new_targets);
                    let ratio_tweens = deltas
                        .iter()
                        .filter_map(|delta| match delta {
                            LayoutDelta::ResizeRatio { path, from, to } => {
                                internal_preorder_index_at_path(new_root, path)
                                    .map(|index| (index, *from, *to))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    (new_targets, ratio_tweens)
                })
        else {
            return HashSet::new();
        };
        let mut animated_panes = HashSet::new();
        let added = deltas
            .iter()
            .filter_map(|delta| match delta {
                LayoutDelta::AddPane { pane, .. } => Some(*pane),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let removed = deltas
            .iter()
            .filter_map(|delta| match delta {
                LayoutDelta::RemovePane { pane, .. } => Some(*pane),
                _ => None,
            })
            .collect::<HashSet<_>>();

        for (pane, target) in new_targets {
            if added.contains(&pane) || removed.contains(&pane) {
                continue;
            }
            let Some(from) = leaf_current_in(old_root, pane) else {
                continue;
            };
            self.timeline.tween_leaf_rect(
                pane,
                from,
                FRect::from(target),
                OPEN_SIBLING_DURATION,
                Easing::EaseOutCubic,
            );
            animated_panes.insert(pane);
        }

        for delta in deltas {
            match delta {
                LayoutDelta::AddPane { path, pane, rect } => {
                    self.queue_external_add_tween(workspace_idx, old_root, path, *pane, *rect);
                    animated_panes.insert(*pane);
                }
                LayoutDelta::RemovePane { pane, .. } => {
                    if let Some(from) = leaf_current_in(old_root, *pane) {
                        let to = close_plan(old_root, *pane).map_or(from, |plan| {
                            collapsed_close_rect(plan.split, plan.closing_is_a, from)
                        });
                        self.timeline.tween_leaf_rect(
                            *pane,
                            from,
                            to,
                            CLOSE_PANE_DURATION,
                            Easing::EaseOutCubic,
                        );
                        animated_panes.insert(*pane);
                    }
                }
                LayoutDelta::ResizeRatio { .. }
                | LayoutDelta::SplitInternal { .. }
                | LayoutDelta::MergeInternal { .. }
                | LayoutDelta::SwapLeaves { .. } => {}
            }
        }

        for (index, from, to) in ratio_tweens {
            self.timeline.tween_internal_ratio(
                index,
                from,
                to,
                OPEN_SIBLING_DURATION,
                Easing::EaseOutCubic,
            );
        }

        animated_panes
    }

    fn queue_external_add_tween(
        &mut self,
        workspace_idx: usize,
        old_root: &Node,
        path: &[usize],
        pane: PaneId,
        target: Rect,
    ) {
        let parent_path = path
            .split_last()
            .map_or(&[][..], |(_leaf_index, parent_path)| parent_path);
        let Some(new_root) = self.workspaces[workspace_idx].root.as_ref() else {
            return;
        };
        let Some(split) = internal_split_at_path(new_root, parent_path) else {
            return;
        };
        let old_parent_rect = node_rect_at_path(old_root, parent_path).unwrap_or(target);
        let from = collapsed_open_rect(split, old_parent_rect, target);
        let to = FRect::from(target);

        self.set_leaf_rect_current(pane, from);
        self.timeline
            .tween_leaf_rect(pane, from, to, OPEN_NEW_PANE_DURATION, Easing::EaseOutBack);
    }

    fn remove_external_panes(&mut self, deltas: &[LayoutDelta]) {
        for delta in deltas {
            if let LayoutDelta::RemovePane { pane, .. } = delta {
                self.remove_pane(*pane);
            }
        }
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

    async fn handle_backend_event(&mut self, event: BackendEvent) -> anyhow::Result<()> {
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
            BackendEvent::ActiveWindowChanged { window_id } => {
                self.sync_active_tmux_window(window_id).await?;
            }
        }

        Ok(())
    }

    async fn sync_active_tmux_window(&mut self, window_id: u64) -> anyhow::Result<()> {
        let Some(workspace_idx) = self.tmux_windows.workspace_for_window(window_id) else {
            tracing::debug!(
                window_id,
                "ignoring active-window switch to unmapped window"
            );
            return Ok(());
        };

        if workspace_idx == self.current_workspace {
            return Ok(());
        }

        self.snap_workspace_tweens(self.current_workspace).await?;
        self.clear_external_tracking_for_workspace(self.current_workspace);

        self.current_workspace = workspace_idx;
        self.recompute_layout();

        let pane_ids = self.current().leaf_panes();
        self.resize_attached_panes(&pane_ids).await?;
        self.dirty = true;

        Ok(())
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

        if !self.dirty {
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
        self.diff.flush(&self.front, &self.back, &mut self.stdout)?;
        self.stdout.flush()?;
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
        self.refresh_external_in_flight().await?;

        Ok(())
    }

    async fn refresh_external_in_flight(&mut self) -> anyhow::Result<()> {
        let completed = self
            .external_in_flight
            .iter()
            .copied()
            .filter(|workspace_idx| self.external_workspace_is_idle(*workspace_idx))
            .collect::<Vec<_>>();

        if completed.is_empty() {
            return Ok(());
        }

        for workspace_idx in completed {
            self.clear_external_tracking_for_workspace(workspace_idx);
        }
        self.dirty = true;
        self.drain_pending_internal_commands().await
    }

    fn external_workspace_is_idle(&self, workspace_idx: usize) -> bool {
        let panes_idle = self
            .external_animation_panes
            .get(&workspace_idx)
            .map_or(true, |panes| {
                panes
                    .iter()
                    .all(|pane| !self.timeline.has_leaf_rect_tween(*pane))
            });

        panes_idle && !self.timeline.has_internal_ratio_tweens()
    }

    async fn drain_pending_internal_commands(&mut self) -> anyhow::Result<()> {
        let mut deferred = VecDeque::new();
        while let Some((workspace_idx, command)) = self.pending_internal_commands.pop_front() {
            if self.has_external_changes_in_flight(workspace_idx) {
                deferred.push_back((workspace_idx, command));
                continue;
            }

            let previous_workspace = self.current_workspace;
            self.current_workspace = workspace_idx;
            let result = self.execute_now(command).await;
            self.current_workspace = previous_workspace;
            result?;
        }

        self.pending_internal_commands = deferred;
        Ok(())
    }

    fn clear_external_tracking_for_workspace(&mut self, workspace_idx: usize) {
        self.external_in_flight.remove(&workspace_idx);
        self.external_animation_panes.remove(&workspace_idx);
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
            tmux_windows: WindowMap::new(),
            external_in_flight: HashSet::new(),
            external_animation_panes: HashMap::new(),
            pending_internal_commands: VecDeque::new(),
            resize_mode: ResizeMode::Normal,
            backend_kind: BackendKind::Native,
            backend,
            output_rx,
            event_rx,
            stdout: io::stdout(),
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

async fn build_backend(
    backend: BackendKind,
    session_name: Option<String>,
) -> anyhow::Result<BackendParts> {
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

    let boxed_backend: BoxedBackend = match backend {
        BackendKind::Native => Box::new(NativeBackend::with_senders(output_tx, event_tx)),
        BackendKind::Tmux => {
            ensure_tmux_available()?;
            Box::new(TmuxBackend::new(session_name, output_tx, event_tx).await?)
        }
    };

    Ok(BackendParts {
        kind: backend,
        backend: boxed_backend,
        output_rx,
        event_rx,
    })
}

async fn build_attach_backend(
    requested_session: Option<String>,
) -> anyhow::Result<(BackendParts, TmuxInitialState)> {
    ensure_tmux_available()?;

    let session_name = resolve_attach_session(requested_session.as_deref())?;
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let mut backend = TmuxBackend::attach(session_name, output_tx, event_tx).await?;
    let initial_state = backend.initial_state().await?;

    Ok((
        BackendParts {
            kind: BackendKind::Tmux,
            backend: Box::new(backend),
            output_rx,
            event_rx,
        },
        initial_state,
    ))
}

fn ensure_tmux_available() -> anyhow::Result<()> {
    match ProcessCommand::new("tmux")
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => bail!(
            "tmux backend selected, but `tmux -V` exited with {status}; install a working tmux or run `wv --backend native`"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "tmux backend selected, but `tmux` was not found on PATH; install tmux or run `wv --backend native`"
        ),
        Err(error) => Err(error).context("failed to check tmux availability for tmux backend"),
    }
}

pub fn print_weave_sessions(windows: bool) -> anyhow::Result<()> {
    let sessions = list_weave_tmux_sessions_for_display()?;
    if sessions.is_empty() {
        eprintln!("no weave sessions");
        return Ok(());
    }

    println!("{:<17}  {:<19}  state", "name", "created");
    for session in &sessions {
        println!(
            "{:<17}  {:<19}  {}",
            session.name,
            format_epoch_seconds(session.created),
            session.state
        );
    }

    if windows {
        for session in &sessions {
            let windows = list_tmux_windows_for_display(&session.name)?;
            print!("{}", format_window_table(&session.name, &windows));
        }
    }

    Ok(())
}

pub fn run_tmux_exec(args: &ExecArgs) -> anyhow::Result<()> {
    ensure_tmux_available()?;
    let session_name = resolve_exec_session(args.session_name.as_deref())?;
    let tmux_args = tmux_exec_args(&args.tmux_args, &session_name);
    let status = match ProcessCommand::new("tmux").args(&tmux_args).status() {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("tmux was not found on PATH; install tmux to use `wv exec`");
        }
        Err(error) => return Err(error).context("failed to run tmux for `wv exec`"),
    };

    if !status.success() {
        bail!("`tmux {}` exited with {status}", tmux_args.join(" "));
    }

    Ok(())
}

fn tmux_exec_args(args: &[String], session_name: &str) -> Vec<String> {
    if args.is_empty() || tmux_args_have_target(args) {
        return args.to_vec();
    }

    let mut tmux_args = Vec::with_capacity(args.len() + 2);
    tmux_args.push(args[0].clone());
    tmux_args.push("-t".to_owned());
    tmux_args.push(session_name.to_owned());
    tmux_args.extend(args.iter().skip(1).cloned());
    tmux_args
}

fn tmux_args_have_target(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "-t"
            || arg
                .strip_prefix("-t")
                .is_some_and(|target| !target.is_empty())
            || arg == "--target"
            || arg.starts_with("--target=")
    })
}

fn resolve_attach_session(requested: Option<&str>) -> anyhow::Result<String> {
    if let Some(session_name) = resolve_requested_session(requested)? {
        return Ok(session_name);
    }

    let Some(session) = list_weave_tmux_sessions()?
        .into_iter()
        .max_by_key(|session| session.activity)
    else {
        bail!("no weave tmux sessions found; start one with `wv --backend tmux`");
    };

    Ok(session.name)
}

fn resolve_exec_session(requested: Option<&str>) -> anyhow::Result<String> {
    if let Some(session_name) = resolve_requested_session(requested)? {
        return Ok(session_name);
    }

    let mut sessions = list_weave_tmux_sessions()?;
    match sessions.len() {
        0 => bail!("no weave tmux sessions found; start one with `wv --backend tmux`"),
        1 => Ok(sessions.remove(0).name),
        _ => {
            sessions.sort_by(|left, right| {
                right
                    .activity
                    .cmp(&left.activity)
                    .then_with(|| left.name.cmp(&right.name))
            });
            let candidates = sessions
                .iter()
                .map(|session| format!("  {}", session.name))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "ambiguous weave session for `wv exec`; pass `--session <name>`. candidates:\n{candidates}"
            );
        }
    }
}

fn resolve_requested_session(requested: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(session_name) = requested else {
        return Ok(None);
    };

    if tmux_session_exists(session_name)? {
        return Ok(Some(session_name.to_owned()));
    }

    bail!(
        "tmux session `{session_name}` does not exist; start one with `wv --backend tmux` or choose a name from `tmux list-sessions`"
    );
}

#[derive(Debug)]
struct DisplayTmuxSession {
    name: String,
    created: i64,
    state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowRow {
    window_index: usize,
    window_id: u64,
    name: String,
}

fn list_tmux_windows_for_display(session_name: &str) -> anyhow::Result<Vec<WindowRow>> {
    let output = match ProcessCommand::new("tmux")
        .args([
            "list-windows",
            "-t",
            session_name,
            "-F",
            "#{window_index}|#{window_id}|#{window_name}",
        ])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("tmux was not found on PATH; install tmux to list weave sessions")
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to list tmux windows for session `{session_name}`")
            });
        }
    };

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(crate::backend::tmux::format::parse_rows(&stdout, 3)
        .iter()
        .filter_map(|row| parse_tmux_window_row(row))
        .collect())
}

fn parse_tmux_window_row(row: &[&str]) -> Option<WindowRow> {
    let [window_index, window_id, name] = row else {
        return None;
    };

    Some(WindowRow {
        window_index: window_index.parse().ok()?,
        window_id: window_id.strip_prefix('@')?.parse().ok()?,
        name: (*name).to_owned(),
    })
}

fn format_window_table(session_name: &str, windows: &[WindowRow]) -> String {
    let mut output = format!("\nwindows for {session_name}\n");
    let _ = writeln!(
        output,
        "{:<7}  {:<10}  {:<10}  name",
        "index", "window-id", "tag"
    );

    for window in windows {
        let _ = writeln!(
            output,
            "{:<7}  @{:<9}  {:<10}  {}",
            window.window_index,
            window.window_id,
            window_workspace_tag(window.window_index),
            window.name
        );
    }

    output
}

fn window_workspace_tag(window_index: usize) -> String {
    match window_index.checked_sub(1) {
        Some(workspace) if workspace < WORKSPACE_COUNT => format!("[ws {workspace}]"),
        _ => "[overflow]".to_owned(),
    }
}

fn list_weave_tmux_sessions_for_display() -> anyhow::Result<Vec<DisplayTmuxSession>> {
    let output = match ProcessCommand::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{@weave-instance}|#{session_name}|#{session_created}|#{?session_attached,attached,detached}",
        ])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("tmux was not found on PATH; install tmux to list weave sessions")
        }
        Err(error) => return Err(error).context("failed to list tmux sessions"),
    };

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(crate::backend::tmux::format::parse_rows(&stdout, 4)
        .iter()
        .filter_map(|row| parse_weave_tmux_session_for_display(row))
        .collect())
}

fn parse_weave_tmux_session_for_display(row: &[&str]) -> Option<DisplayTmuxSession> {
    let [marker, name, created, state] = row else {
        return None;
    };
    if *marker != "1" {
        return None;
    }

    Some(DisplayTmuxSession {
        name: (*name).to_owned(),
        created: created.parse().unwrap_or_default(),
        state: (*state).to_owned(),
    })
}

fn format_epoch_seconds(seconds: i64) -> String {
    Local.timestamp_opt(seconds, 0).single().map_or_else(
        || "unknown".to_owned(),
        |time| time.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

fn tmux_session_exists(session_name: &str) -> anyhow::Result<bool> {
    let status = ProcessCommand::new("tmux")
        .args(["has-session", "-t", session_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to check tmux session")?;

    Ok(status.success())
}

#[derive(Debug)]
struct TmuxSession {
    name: String,
    activity: u64,
}

fn list_weave_tmux_sessions() -> anyhow::Result<Vec<TmuxSession>> {
    let output = ProcessCommand::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{@weave-instance}|#{session_name}|#{session_activity}",
        ])
        .stdin(Stdio::null())
        .output()
        .context("failed to list tmux sessions")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(crate::backend::tmux::format::parse_rows(&stdout, 3)
        .iter()
        .filter_map(|row| parse_weave_tmux_session(row))
        .collect())
}

fn parse_weave_tmux_session(row: &[&str]) -> Option<TmuxSession> {
    let [marker, name, activity] = row else {
        return None;
    };
    if *marker != "1" {
        return None;
    }

    Some(TmuxSession {
        name: (*name).to_owned(),
        activity: activity.parse().unwrap_or_default(),
    })
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

fn node_from_layout_ast(
    ast: &LayoutAst,
    old_root: Option<&Node>,
    path: &mut StructuralPath,
) -> Node {
    match ast {
        LayoutAst::Leaf { pane_id, rect } => {
            let pane = PaneId(*pane_id);
            Node::Leaf {
                pane,
                rect_current: old_root
                    .and_then(|root| leaf_current_in(root, pane))
                    .unwrap_or_else(|| FRect::from(*rect)),
                rect_target: *rect,
            }
        }
        LayoutAst::Horizontal { rect, children } | LayoutAst::Vertical { rect, children } => {
            let [first, second] = children.as_slice() else {
                panic!("tmux layout reconciliation requires normalized binary groups");
            };
            let (split, ratio_target) = layout_ast_split_ratio(ast);
            let ratio = old_root
                .and_then(|root| internal_ratio_at_path(root, path, split))
                .unwrap_or(ratio_target);

            path.push(0);
            let a = node_from_layout_ast(first, old_root, path);
            path.pop();

            path.push(1);
            let b = node_from_layout_ast(second, old_root, path);
            path.pop();

            Node::Internal {
                split,
                ratio,
                ratio_target,
                a: Box::new(a),
                b: Box::new(b),
                rect: *rect,
            }
        }
    }
}

fn layout_ast_split_ratio(ast: &LayoutAst) -> (Split, f32) {
    match ast {
        LayoutAst::Leaf { .. } => panic!("leaves do not have split ratios"),
        LayoutAst::Horizontal { rect, children } => {
            let [first, _second] = children.as_slice() else {
                panic!("tmux layout reconciliation requires normalized binary groups");
            };
            (Split::Vertical, split_ratio(first.rect().w, rect.w))
        }
        LayoutAst::Vertical { rect, children } => {
            let [first, _second] = children.as_slice() else {
                panic!("tmux layout reconciliation requires normalized binary groups");
            };
            (Split::Horizontal, split_ratio(first.rect().h, rect.h))
        }
    }
}

fn split_ratio(first: u16, total: u16) -> f32 {
    if total == 0 {
        return 0.5;
    }

    f32::from(first) / f32::from(total)
}

fn collect_layout_panes(ast: &LayoutAst, panes: &mut Vec<PaneId>) {
    match ast {
        LayoutAst::Leaf { pane_id, .. } => panes.push(PaneId(*pane_id)),
        LayoutAst::Horizontal { children, .. } | LayoutAst::Vertical { children, .. } => {
            for child in children {
                collect_layout_panes(child, panes);
            }
        }
    }
}

fn collect_layout_tmux_panes(ast: &LayoutAst, panes: &mut Vec<u64>) {
    match ast {
        LayoutAst::Leaf { pane_id, .. } => panes.push(*pane_id),
        LayoutAst::Horizontal { children, .. } | LayoutAst::Vertical { children, .. } => {
            for child in children {
                collect_layout_tmux_panes(child, panes);
            }
        }
    }
}

fn rewrite_layout_pane_ids(ast: LayoutAst, pane_ids: &HashMap<u64, u64>) -> LayoutAst {
    match ast {
        LayoutAst::Leaf { pane_id, rect } => LayoutAst::Leaf {
            pane_id: *pane_ids
                .get(&pane_id)
                .expect("all tmux layout pane ids should be ingested before rewrite"),
            rect,
        },
        LayoutAst::Horizontal { rect, children } => LayoutAst::Horizontal {
            rect,
            children: children
                .into_iter()
                .map(|child| rewrite_layout_pane_ids(child, pane_ids))
                .collect(),
        },
        LayoutAst::Vertical { rect, children } => LayoutAst::Vertical {
            rect,
            children: children
                .into_iter()
                .map(|child| rewrite_layout_pane_ids(child, pane_ids))
                .collect(),
        },
    }
}

fn is_layout_mutating_command(command: Command) -> bool {
    matches!(command, Command::SplitH | Command::SplitV | Command::Close)
}

fn leaf_current_in(node: &Node, pane: PaneId) -> Option<FRect> {
    match node.find_leaf(pane)? {
        Node::Leaf { rect_current, .. } => Some(*rect_current),
        Node::Internal { .. } => None,
    }
}

fn internal_ratio_at_path(node: &Node, path: &[usize], expected_split: Split) -> Option<f32> {
    let Node::Internal {
        split, ratio, a, b, ..
    } = node
    else {
        return None;
    };

    if path.is_empty() {
        return (*split == expected_split).then_some(*ratio);
    }

    match path[0] {
        0 => internal_ratio_at_path(a, &path[1..], expected_split),
        1 => internal_ratio_at_path(b, &path[1..], expected_split),
        _ => None,
    }
}

fn internal_split_at_path(node: &Node, path: &[usize]) -> Option<Split> {
    let Node::Internal { split, a, b, .. } = node else {
        return None;
    };

    if path.is_empty() {
        return Some(*split);
    }

    match path[0] {
        0 => internal_split_at_path(a, &path[1..]),
        1 => internal_split_at_path(b, &path[1..]),
        _ => None,
    }
}

fn internal_preorder_index_at_path(root: &Node, target_path: &[usize]) -> Option<usize> {
    let mut index = 0;
    internal_preorder_index_at_path_inner(root, target_path, &mut Vec::new(), &mut index)
}

fn internal_preorder_index_at_path_inner(
    node: &Node,
    target_path: &[usize],
    current_path: &mut StructuralPath,
    next_index: &mut usize,
) -> Option<usize> {
    match node {
        Node::Leaf { .. } => None,
        Node::Internal { a, b, .. } => {
            let current_index = *next_index;
            *next_index += 1;
            if current_path == target_path {
                return Some(current_index);
            }

            current_path.push(0);
            let found =
                internal_preorder_index_at_path_inner(a, target_path, current_path, next_index);
            current_path.pop();
            if found.is_some() {
                return found;
            }

            current_path.push(1);
            let found =
                internal_preorder_index_at_path_inner(b, target_path, current_path, next_index);
            current_path.pop();
            found
        }
    }
}

fn node_rect_at_path(node: &Node, path: &[usize]) -> Option<Rect> {
    match node {
        Node::Leaf { rect_target, .. } if path.is_empty() => Some(*rect_target),
        Node::Leaf { .. } => None,
        Node::Internal { rect, a, b, .. } if path.is_empty() => Some(*rect),
        Node::Internal { a, b, .. } => match path[0] {
            0 => node_rect_at_path(a, &path[1..]),
            1 => node_rect_at_path(b, &path[1..]),
            _ => None,
        },
    }
}

trait LayoutAstRect {
    fn rect(&self) -> Rect;
}

impl LayoutAstRect for LayoutAst {
    fn rect(&self) -> Rect {
        match self {
            Self::Leaf { rect, .. }
            | Self::Horizontal { rect, .. }
            | Self::Vertical { rect, .. } => *rect,
        }
    }
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
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::{
        format_window_table, frame_interval, parse_weave_tmux_session,
        parse_weave_tmux_session_for_display, tmux_exec_args, validate_session_name, App, Args,
        AttachArgs, BackendKind, ExecArgs, ExitState, LaunchArgs, WindowRow, CLOSE_PANE_DURATION,
        FOCUS_BORDER_TWEEN_DURATION, OPEN_NEW_PANE_DURATION, OPEN_SIBLING_DURATION,
    };
    use crate::anim::tween::Easing;
    use crate::backend::tmux::layout::LayoutAst;
    use crate::backend::tmux::process::TmuxPaneId;
    use crate::backend::tmux::{TmuxInitialPane, TmuxInitialState, TmuxInitialWindow};
    use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
    use crate::command::Command;
    use crate::layout::geometry::{FRect, Rect, Split};
    use crate::layout::tree::Node;
    use tokio::time::Duration;

    struct MockBackend {
        next_id: PaneId,
        external_next_id: u64,
        external_panes: HashMap<u64, PaneId>,
        resized: Arc<Mutex<Vec<(PaneId, u16, u16)>>>,
        selected_windows: Arc<Mutex<Vec<usize>>>,
        selected_window_ids: Arc<Mutex<Vec<u64>>>,
    }

    #[derive(Clone, Default)]
    struct MockBackendHandle {
        resized: Arc<Mutex<Vec<(PaneId, u16, u16)>>>,
        selected_windows: Arc<Mutex<Vec<usize>>>,
        selected_window_ids: Arc<Mutex<Vec<u64>>>,
    }

    impl MockBackendHandle {
        fn resized(&self) -> Vec<(PaneId, u16, u16)> {
            lock_resized(&self.resized).clone()
        }

        fn selected_windows(&self) -> Vec<usize> {
            lock_selected_windows(&self.selected_windows).clone()
        }

        fn selected_window_ids(&self) -> Vec<u64> {
            lock_selected_window_ids(&self.selected_window_ids).clone()
        }

        fn clear_resized(&self) {
            lock_resized(&self.resized).clear();
        }
    }

    fn mock_backend(next_id: PaneId) -> (Box<dyn PaneBackend>, MockBackendHandle) {
        mock_backend_with_external(next_id, next_id.0, HashMap::from([(1, PaneId(1))]))
    }

    fn mock_backend_with_external(
        next_id: PaneId,
        external_next_id: u64,
        external_panes: HashMap<u64, PaneId>,
    ) -> (Box<dyn PaneBackend>, MockBackendHandle) {
        let handle = MockBackendHandle::default();
        (
            Box::new(MockBackend {
                next_id,
                external_next_id,
                external_panes,
                resized: Arc::clone(&handle.resized),
                selected_windows: Arc::clone(&handle.selected_windows),
                selected_window_ids: Arc::clone(&handle.selected_window_ids),
            }),
            handle,
        )
    }

    fn lock_resized(
        resized: &Mutex<Vec<(PaneId, u16, u16)>>,
    ) -> std::sync::MutexGuard<'_, Vec<(PaneId, u16, u16)>> {
        resized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_selected_window_ids(
        selected_window_ids: &Mutex<Vec<u64>>,
    ) -> std::sync::MutexGuard<'_, Vec<u64>> {
        selected_window_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_selected_windows(
        selected_windows: &Mutex<Vec<usize>>,
    ) -> std::sync::MutexGuard<'_, Vec<usize>> {
        selected_windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    fn layout_leaf(pane_id: u64, rect: Rect) -> LayoutAst {
        LayoutAst::Leaf { pane_id, rect }
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

        async fn select_window(&mut self, workspace_idx: usize) -> Result<(), Error> {
            lock_selected_windows(&self.selected_windows).push(workspace_idx);
            Ok(())
        }

        async fn select_window_by_id(&mut self, window_id: u64) -> Result<(), Error> {
            lock_selected_window_ids(&self.selected_window_ids).push(window_id);
            Ok(())
        }

        async fn ingest_external_pane(&mut self, tmux_pane_id: u64) -> Result<PaneId, Error> {
            if let Some(pane_id) = self.external_panes.get(&tmux_pane_id).copied() {
                return Ok(pane_id);
            }

            let pane_id = PaneId(self.external_next_id);
            self.external_next_id = self.external_next_id.saturating_add(1);
            self.external_panes.insert(tmux_pane_id, pane_id);
            Ok(pane_id)
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
    async fn external_layout_split_mutates_tree_and_queues_animations() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let layout = LayoutAst::Horizontal {
            rect: rect(0, 0, 80, 24),
            children: vec![
                layout_leaf(1, rect(0, 0, 40, 24)),
                layout_leaf(2, rect(40, 0, 40, 24)),
            ],
        };

        app.apply_external_layout_change(1, layout)
            .await
            .expect("external layout applies");

        let ws = &app.workspaces[0];
        match ws.root.as_ref().expect("root exists") {
            Node::Internal { split, a, b, .. } => {
                assert_eq!(*split, Split::Vertical);
                assert!(matches!(
                    **a,
                    Node::Leaf {
                        pane: PaneId(1),
                        ..
                    }
                ));
                assert!(matches!(
                    **b,
                    Node::Leaf {
                        pane: PaneId(2),
                        ..
                    }
                ));
            }
            Node::Leaf { .. } => panic!("expected external split root"),
        }
        assert!(app.panes.iter().any(|pane| pane.id() == PaneId(2)));
        assert!(app.timeline.has_leaf_rect_tween(PaneId(1)));
        assert!(app.timeline.has_leaf_rect_tween(PaneId(2)));
    }

    #[tokio::test]
    async fn external_layout_merge_mutates_tree_and_queues_animations() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let split_layout = LayoutAst::Horizontal {
            rect: rect(0, 0, 80, 24),
            children: vec![
                layout_leaf(1, rect(0, 0, 40, 24)),
                layout_leaf(2, rect(40, 0, 40, 24)),
            ],
        };
        app.apply_external_layout_change(1, split_layout)
            .await
            .expect("external split applies");

        let merge_layout = layout_leaf(1, rect(0, 0, 80, 24));
        app.apply_external_layout_change(1, merge_layout)
            .await
            .expect("external merge applies");

        let ws = &app.workspaces[0];
        assert!(matches!(
            ws.root.as_ref().expect("root exists"),
            Node::Leaf {
                pane: PaneId(1),
                ..
            }
        ));
        assert!(app.panes.iter().any(|pane| pane.id() == PaneId(1)));
        assert!(!app.panes.iter().any(|pane| pane.id() == PaneId(2)));
        assert!(app.timeline.has_leaf_rect_tween(PaneId(1)));
    }

    #[tokio::test]
    async fn internal_split_during_external_resize_waits_until_external_animation_completes() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(Command::SplitH).await.expect("initial split");
        app.advance_animations(OPEN_NEW_PANE_DURATION)
            .await
            .expect("initial split animation completes");

        let resized_layout = LayoutAst::Vertical {
            rect: rect(0, 0, 80, 23),
            children: vec![
                layout_leaf(1, rect(0, 0, 80, 8)),
                layout_leaf(2, rect(0, 8, 80, 15)),
            ],
        };
        app.apply_external_layout_change(1, resized_layout)
            .await
            .expect("external resize applies");
        assert!(app.has_external_changes_in_flight(0));

        app.execute(Command::SplitH)
            .await
            .expect("internal split is queued");

        assert_eq!(app.pending_internal_commands.len(), 1);
        assert_eq!(app.pane_ids(), vec![PaneId(1), PaneId(2)]);

        app.advance_animations(OPEN_SIBLING_DURATION)
            .await
            .expect("external animation completes and queued split drains");

        assert!(!app.has_external_changes_in_flight(0));
        assert!(app.pending_internal_commands.is_empty());
        assert_eq!(app.pane_ids(), vec![PaneId(1), PaneId(2), PaneId(3)]);
        match app.workspaces[0].root.as_ref().expect("root exists") {
            Node::Internal { split, a, b, .. } => {
                assert_eq!(*split, Split::Horizontal);
                assert!(matches!(
                    **a,
                    Node::Leaf {
                        pane: PaneId(1),
                        ..
                    }
                ));
                match &**b {
                    Node::Internal {
                        split: nested_split,
                        a: nested_a,
                        b: nested_b,
                        ..
                    } => {
                        assert_eq!(*nested_split, Split::Horizontal);
                        assert!(matches!(
                            **nested_a,
                            Node::Leaf {
                                pane: PaneId(2),
                                ..
                            }
                        ));
                        assert!(matches!(
                            **nested_b,
                            Node::Leaf {
                                pane: PaneId(3),
                                ..
                            }
                        ));
                    }
                    Node::Leaf { .. } => panic!("expected queued split to split focused pane"),
                }
            }
            Node::Leaf { .. } => panic!("expected split root"),
        }
    }

    #[tokio::test]
    async fn hydrate_attached_tmux_state_builds_workspace_forest_without_animations() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let initial_state = TmuxInitialState {
            windows: vec![
                TmuxInitialWindow {
                    window_id: 101,
                    window_index: 1,
                    layout: LayoutAst::Horizontal {
                        rect: rect(0, 0, 80, 24),
                        children: vec![
                            layout_leaf(1, rect(0, 0, 40, 24)),
                            layout_leaf(2, rect(40, 0, 40, 24)),
                        ],
                    },
                },
                TmuxInitialWindow {
                    window_id: 102,
                    window_index: 2,
                    layout: LayoutAst::Vertical {
                        rect: rect(0, 0, 80, 24),
                        children: vec![
                            layout_leaf(3, rect(0, 0, 80, 12)),
                            layout_leaf(4, rect(0, 12, 80, 12)),
                        ],
                    },
                },
                TmuxInitialWindow {
                    window_id: 103,
                    window_index: 3,
                    layout: layout_leaf(5, rect(0, 0, 80, 24)),
                },
            ],
            panes: (1..=5)
                .map(|tmux_id| TmuxInitialPane {
                    tmux_id: TmuxPaneId(tmux_id),
                    pid: 10_000 + u32::try_from(tmux_id).expect("test id fits"),
                })
                .collect(),
        };

        app.hydrate_attached_tmux_state(initial_state)
            .await
            .expect("hydrate succeeds");

        assert_eq!(
            app.pane_ids(),
            vec![PaneId(1), PaneId(2), PaneId(3), PaneId(4), PaneId(5)]
        );
        assert_eq!(app.tmux_windows.workspace_for_window(101), Some(0));
        assert_eq!(app.tmux_windows.workspace_for_window(102), Some(1));
        assert_eq!(app.tmux_windows.workspace_for_window(103), Some(2));
        assert!(app.timeline.is_idle());
        assert!(handle.resized().is_empty());

        assert!(matches!(
            app.workspaces[0].root.as_ref().expect("workspace 1 root"),
            Node::Internal {
                split: Split::Vertical,
                ..
            }
        ));
        assert!(matches!(
            app.workspaces[1].root.as_ref().expect("workspace 2 root"),
            Node::Internal {
                split: Split::Horizontal,
                ..
            }
        ));
        assert!(matches!(
            app.workspaces[2].root.as_ref().expect("workspace 3 root"),
            Node::Leaf {
                pane: PaneId(5),
                ..
            }
        ));

        app.apply_external_layout_change(102, layout_leaf(3, rect(0, 0, 80, 24)))
            .await
            .expect("external update applies to mapped workspace");

        assert!(matches!(
            app.workspaces[1].root.as_ref().expect("workspace 2 root"),
            Node::Leaf {
                pane: PaneId(3),
                ..
            }
        ));
        assert!(matches!(
            app.workspaces[0].root.as_ref().expect("workspace 1 root"),
            Node::Internal { .. }
        ));
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
    async fn switch_workspace_on_tmux_backend_selects_tmux_window() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.backend_kind = BackendKind::Tmux;

        app.execute(Command::SwitchWorkspace(3))
            .await
            .expect("switch succeeds");

        assert_eq!(handle.selected_windows(), vec![2]);
        assert_eq!(app.current_workspace, 2);
    }

    #[tokio::test]
    async fn active_window_changed_updates_current_workspace_from_window_map() {
        let (backend, handle) = mock_backend(PaneId(3));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let target_rect = rect(0, 0, 80, 23);
        let _ = app.tmux_windows.on_window_add(101, 1);
        let _ = app.tmux_windows.on_window_add(102, 2);
        app.workspaces[1].root = Some(Node::Leaf {
            pane: PaneId(2),
            rect_current: FRect::from(target_rect),
            rect_target: target_rect,
        });
        app.workspaces[1].focused = Some(PaneId(2));
        app.panes
            .push(crate::term::pane::Pane::new(PaneId(2), 80, 24));

        app.handle_backend_event(BackendEvent::ActiveWindowChanged { window_id: 102 })
            .await
            .expect("active window sync succeeds");

        assert_eq!(app.current_workspace, 1);
        assert_eq!(handle.resized(), vec![(PaneId(2), 80, 23)]);
    }

    #[tokio::test]
    async fn active_window_changed_to_unmapped_window_leaves_workspace_unchanged() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let _ = app.tmux_windows.on_window_add(101, 1);

        app.handle_backend_event(BackendEvent::ActiveWindowChanged { window_id: 999 })
            .await
            .expect("unmapped active window is ignored");

        assert_eq!(app.current_workspace, 0);
        assert!(handle.resized().is_empty());
    }

    #[tokio::test]
    async fn goto_window_with_mapped_window_id_selects_backend_window_by_id() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let _ = app.tmux_windows.on_window_add(101, 1);

        app.execute(Command::GotoWindow(101))
            .await
            .expect("goto-window succeeds");

        assert_eq!(handle.selected_window_ids(), vec![101]);
    }

    #[tokio::test]
    async fn goto_window_with_unknown_window_id_is_noop() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let _ = app.tmux_windows.on_window_add(101, 1);

        app.execute(Command::GotoWindow(999))
            .await
            .expect("unknown goto-window is ignored");

        assert!(handle.selected_window_ids().is_empty());
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
        assert_eq!(frame_interval(160), Duration::from_nanos(6_250_000));
    }

    #[test]
    fn args_default_to_native_backend() {
        assert_eq!(
            Args::parse(std::iter::empty::<&str>()).expect("args parse"),
            Args {
                debug: false,
                backend: BackendKind::Native,
                session_name: None,
                bare: false,
            }
        );
    }

    #[test]
    fn args_parse_backend_and_debug_flags() {
        assert_eq!(
            Args::parse(["--backend", "tmux", "--debug"]).expect("args parse"),
            Args {
                debug: true,
                backend: BackendKind::Tmux,
                session_name: None,
                bare: false,
            }
        );
        assert_eq!(
            Args::parse(["--backend=native"])
                .expect("args parse")
                .backend,
            BackendKind::Native
        );
    }

    #[test]
    fn args_reject_invalid_backend() {
        assert!(Args::parse(["--backend", "bogus"]).is_err());
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
        let args =
            Args::parse(["--bare", "--session", "foo", "--backend", "tmux"]).expect("args parse");

        assert!(args.bare);
        assert_eq!(args.session_name, Some("foo".to_owned()));
        assert_eq!(args.backend, BackendKind::Tmux);
    }

    #[test]
    fn args_parse_no_attach_alias() {
        let args = Args::parse(["--no-attach", "--session", "foo", "--backend", "tmux"])
            .expect("args parse");

        assert!(args.bare);
        assert_eq!(args.session_name, Some("foo".to_owned()));
        assert_eq!(args.backend, BackendKind::Tmux);
    }

    #[test]
    fn args_reject_bare_with_native_backend() {
        let error = Args::parse(["--bare", "--session", "foo"])
            .expect_err("bare native should fail")
            .to_string();

        assert!(error.contains("requires --backend tmux"));
    }

    #[test]
    fn args_reject_bare_without_session() {
        let error = Args::parse(["--bare", "--backend", "tmux"])
            .expect_err("bare without session should fail")
            .to_string();

        assert!(error.contains("requires an explicit --session"));
    }

    #[test]
    fn args_reject_bare_value() {
        assert!(Args::parse(["--bare=true", "--session", "foo", "--backend", "tmux"]).is_err());
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
            LaunchArgs::parse(["exec", "split-window", "-h"]).expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: None,
                tmux_args: vec!["split-window".to_owned(), "-h".to_owned()],
            })
        );
    }

    #[test]
    fn launch_args_parse_exec_with_session_prefix() {
        assert_eq!(
            LaunchArgs::parse(["--session", "main", "exec", "split-window", "-h"])
                .expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: Some("main".to_owned()),
                tmux_args: vec!["split-window".to_owned(), "-h".to_owned()],
            })
        );
    }

    #[test]
    fn launch_args_parse_exec_with_session_flag_after_exec() {
        assert_eq!(
            LaunchArgs::parse(["exec", "--session=main", "send-keys", "echo hi", "Enter"])
                .expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: Some("main".to_owned()),
                tmux_args: vec![
                    "send-keys".to_owned(),
                    "echo hi".to_owned(),
                    "Enter".to_owned()
                ],
            })
        );
    }

    #[test]
    fn launch_args_reject_exec_without_tmux_args() {
        assert!(LaunchArgs::parse(["exec"]).is_err());
        assert!(LaunchArgs::parse(["--session", "main", "exec"]).is_err());
    }

    #[test]
    fn tmux_exec_args_injects_target_after_verb() {
        assert_eq!(
            tmux_exec_args(&["split-window".to_owned(), "-h".to_owned()], "main"),
            vec![
                "split-window".to_owned(),
                "-t".to_owned(),
                "main".to_owned(),
                "-h".to_owned()
            ]
        );
    }

    #[test]
    fn tmux_exec_args_preserves_user_target() {
        assert_eq!(
            tmux_exec_args(
                &[
                    "split-window".to_owned(),
                    "-t".to_owned(),
                    "main:1.0".to_owned(),
                    "-h".to_owned()
                ],
                "other"
            ),
            vec![
                "split-window".to_owned(),
                "-t".to_owned(),
                "main:1.0".to_owned(),
                "-h".to_owned()
            ]
        );
    }

    #[test]
    fn launch_args_parse_ls_windows_flag() {
        assert_eq!(
            LaunchArgs::parse(["ls"]).expect("launch args parse"),
            LaunchArgs::ListSessions { windows: false }
        );
        assert_eq!(
            LaunchArgs::parse(["ls", "--windows"]).expect("launch args parse"),
            LaunchArgs::ListSessions { windows: true }
        );
    }

    #[test]
    fn launch_args_reject_bad_ls_windows_usage() {
        assert!(LaunchArgs::parse(["ls", "--windows=foo"]).is_err());
        assert!(LaunchArgs::parse(["ls", "foo", "--windows"]).is_err());
    }

    #[test]
    fn launch_args_parse_bare_variant() {
        match LaunchArgs::parse(["--bare", "--session", "foo", "--backend", "tmux"])
            .expect("launch args parse")
        {
            LaunchArgs::Bare(args) => {
                assert!(args.bare);
                assert_eq!(args.session_name, Some("foo".to_owned()));
                assert_eq!(args.backend, BackendKind::Tmux);
            }
            other => panic!("expected bare launch args, got {other:?}"),
        }
    }

    #[test]
    fn launch_args_reject_attach_session_flag() {
        assert!(LaunchArgs::parse(["attach", "weave-test", "--session", "x"]).is_err());
    }

    #[test]
    fn launch_args_reject_native_attach() {
        let error = LaunchArgs::parse(["--backend", "native", "attach"])
            .expect_err("native attach should fail")
            .to_string();

        assert!(error.contains("wv attach [name]"));
    }

    #[test]
    fn format_window_table_tags_workspace_and_overflow_windows() {
        let table = format_window_table(
            "weave-test",
            &[
                WindowRow {
                    window_index: 1,
                    window_id: 101,
                    name: "editor".to_owned(),
                },
                WindowRow {
                    window_index: 10,
                    window_id: 110,
                    name: "logs".to_owned(),
                },
            ],
        );

        assert!(table.contains("windows for weave-test"));
        let editor_line = table
            .lines()
            .find(|line| line.contains("editor"))
            .expect("editor window row");
        let logs_line = table
            .lines()
            .find(|line| line.contains("logs"))
            .expect("logs window row");

        assert!(editor_line.contains("1"));
        assert!(editor_line.contains("@101"));
        assert!(editor_line.contains("[ws 0]"));
        assert!(logs_line.contains("10"));
        assert!(logs_line.contains("@110"));
        assert!(logs_line.contains("[overflow]"));
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

    #[test]
    fn parse_weave_tmux_session_requires_marker() {
        let session = parse_weave_tmux_session(&["1", "custom", "123"]).expect("marked session");
        assert_eq!(session.name, "custom");
        assert_eq!(session.activity, 123);

        assert!(parse_weave_tmux_session(&["", "weave-old", "123"]).is_none());
        assert!(parse_weave_tmux_session(&["0", "weave-old", "123"]).is_none());
    }

    #[test]
    fn parse_weave_tmux_session_for_display_requires_marker() {
        let session = parse_weave_tmux_session_for_display(&["1", "custom", "123", "detached"])
            .expect("marked session");
        assert_eq!(session.name, "custom");
        assert_eq!(session.created, 123);
        assert_eq!(session.state, "detached");

        assert!(
            parse_weave_tmux_session_for_display(&["0", "weave-old", "123", "detached"]).is_none()
        );
    }
}
