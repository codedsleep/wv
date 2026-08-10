//! `App`: event loop + state owner.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use anyhow::{anyhow, bail, Context};
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
use crate::command::target::{Extreme, PaneRef, WindowRef};
use crate::command::{Command, PaneSelector, SpawnCommand, Target};
use crate::config::{Config, ThemeConfig};
use crate::input;
use crate::input::keymap::Keymap;
use crate::layout::geometry::{Direction, FRect, Rect, Split};
use crate::layout::tree::Node;
use crate::render::diff::{ColorMode, DiffRenderer};
use crate::render::{chrome, compositor};
use crate::session::protocol::{ClientToServer, CommandResult, ExitReason, ServerToClient};
use crate::session::sink::OutputSink;
use crate::session::SessionEvent;
use crate::term::pane::Pane;
use crate::term::surface::Surface;

const FOCUS_BORDER_TWEEN_DURATION: Duration = Duration::from_millis(120);
const OPEN_NEW_PANE_DURATION: Duration = Duration::from_millis(220);
const OPEN_SIBLING_DURATION: Duration = Duration::from_millis(180);
// Close tweens use ease-out-cubic so panes decelerate into the collapsed line.
const CLOSE_PANE_DURATION: Duration = Duration::from_millis(180);
/// How long shutdown waits for socket writers to flush their last frame.
///
/// These are local unix sockets with at most a frame or two in flight, so this
/// is generous; it only has to outlast a single write syscall.
const SHUTDOWN_FLUSH_GRACE: Duration = Duration::from_millis(50);
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
const EVENT_CHANNEL_CAPACITY: usize = 64;

type BoxedBackend = Box<dyn PaneBackend>;

/// Workspaces addressable with `Alt+1` .. `Alt+9`.
pub const WORKSPACE_COUNT: usize = 9;

/// What a window is called before anything names it.
const DEFAULT_WINDOW_NAME: &str = "shell";

#[derive(Default)]
struct Workspace {
    /// The name `rename-window` gave it, if any.
    ///
    /// `None` means the name is automatic: it follows the focused pane's OSC
    /// title, which is tmux's `automatic-rename`. Naming a window pins it.
    name: Option<String>,
    root: Option<Node>,
    focused: Option<PaneId>,
    /// The pane focused before the current one, for `select-pane -l` / `{last}`.
    last_focused: Option<PaneId>,
    closing: HashSet<PaneId>,
}

impl Workspace {
    fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Move focus, remembering where it came from.
    ///
    /// Every focus change goes through here so `{last}` stays truthful no
    /// matter which path caused it — a keybinding, a script, or a pane dying.
    fn set_focus(&mut self, pane: Option<PaneId>) {
        if self.focused == pane {
            return;
        }
        if let Some(previous) = self.focused {
            self.last_focused = Some(previous);
        }
        self.focused = pane;
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

/// A command the session declined to run, with a message meant for the user.
///
/// Deliberately not an `anyhow::Error`: this is the "you asked for a pane that
/// isn't there" case, which must be reported and forgotten, not propagated.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Rejected(String);

impl Rejected {
    fn new(message: String) -> Self {
        Self(message)
    }
}

/// Why a command did not complete.
enum ExecuteError {
    /// The command was understood but could not be applied. Non-fatal.
    Rejected(String),
    /// Something broke that the session cannot continue past.
    Fatal(anyhow::Error),
}

impl From<Rejected> for ExecuteError {
    fn from(rejected: Rejected) -> Self {
        Self::Rejected(rejected.0)
    }
}

impl From<anyhow::Error> for ExecuteError {
    fn from(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

pub struct App {
    front: Surface,
    back: Surface,
    panes: Vec<Pane>,
    workspaces: Vec<Workspace>,
    current_workspace: usize,
    /// The workspace shown before the current one, for `select-window -l`.
    last_workspace: Option<usize>,
    /// Backend pane id → the `%N` a user or script sees.
    ///
    /// `PaneId` is documented as backend-local and not portable, so targets
    /// address panes through this map instead. Numbers are handed out in
    /// order and never reused, so a `%N` in a script keeps meaning the same
    /// pane for as long as that pane exists.
    pane_numbers: HashMap<PaneId, u64>,
    next_pane_number: u64,
    /// This session's name, so a target naming another session is rejected
    /// rather than silently applied here.
    session_name: Option<String>,
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
    /// Parse `wv exec [--session NAME] <command> [args...]`.
    ///
    /// `--session` is weave's own flag and must come before the command name.
    /// Everything from the command name onward belongs to the command, so its
    /// flags never collide with `wv`'s: `wv exec split-window -h -t %2` passes
    /// `-h -t %2` through untouched.
    fn parse<I, S>(args: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut session_name = None;
        let mut args = args.into_iter().peekable();

        while let Some(arg) = args.peek() {
            let arg = arg.as_ref().to_owned();

            if arg == "--session" {
                args.next();
                let Some(value) = args.next() else {
                    bail!("missing value for `--session`; expected a weave session name");
                };
                let value = value.as_ref();
                validate_session_name(value)?;
                session_name = Some(value.to_owned());
            } else if let Some(value) = arg.strip_prefix("--session=") {
                validate_session_name(value)?;
                session_name = Some(value.to_owned());
                args.next();
            } else if arg == "--" {
                args.next();
                break;
            } else if arg.starts_with('-') {
                bail!("`wv exec` does not accept `{arg}` before the command name");
            } else {
                break;
            }
        }

        let command_line = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        if command_line.is_empty() {
            bail!("`wv exec` requires a command, for example `wv exec split-window -h`");
        }

        // The parse error leads and the hint follows, so `to_string()` — what
        // the CLI prints — carries the actual problem first.
        let command = Command::parse(&command_line).map_err(|error| {
            anyhow!(
                "{error}\n\ncommands: {}\naliases: {}",
                crate::command::COMMAND_NAMES.join(", "),
                crate::command::ALIAS_NAMES.join(", ")
            )
        })?;

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

        // Number the panes we start with in layout order, so `%1` is the first
        // pane on screen for a script that attaches immediately.
        let pane_numbers: HashMap<PaneId, u64> = initial_panes
            .iter()
            .enumerate()
            .map(|(index, &pane)| (pane, index as u64 + 1))
            .collect();
        let next_pane_number = pane_numbers.len() as u64 + 1;

        let mut workspaces: Vec<Workspace> =
            (0..WORKSPACE_COUNT).map(|_| Workspace::default()).collect();
        workspaces[0] = Workspace {
            name: None,
            root,
            focused,
            last_focused: None,
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
            last_workspace: None,
            pane_numbers,
            next_pane_number,
            session_name: None,
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

    /// Name this session, so targets naming a session can be checked.
    pub fn with_session_name(mut self, name: impl Into<String>) -> Self {
        self.session_name = Some(name.into());
        self
    }

    /// The `%N` a script uses to address `pane`.
    pub fn pane_number(&self, pane: PaneId) -> Option<u64> {
        self.pane_numbers.get(&pane).copied()
    }

    fn pane_by_number(&self, number: u64) -> Option<PaneId> {
        self.pane_numbers
            .iter()
            .find(|(_, &n)| n == number)
            .map(|(&pane, _)| pane)
    }

    /// Hand out the next `%N`. Numbers are monotonic and never reused.
    fn register_pane_number(&mut self, pane: PaneId) -> u64 {
        let number = self.next_pane_number;
        self.next_pane_number = self.next_pane_number.saturating_add(1);
        self.pane_numbers.insert(pane, number);
        number
    }

    /// Workspaces with panes in them, in order, always including the current
    /// one — the set `+`, `-`, `{start}` and `{end}` cycle over.
    fn occupied_workspaces(&self) -> Vec<usize> {
        (0..WORKSPACE_COUNT)
            .filter(|&idx| idx == self.current_workspace || !self.workspaces[idx].is_empty())
            .collect()
    }

    /// Reject a target that names a different session than this one.
    fn check_session(&self, target: &Target) -> Result<(), Rejected> {
        let Some(requested) = target.session.as_deref() else {
            return Ok(());
        };

        match self.session_name.as_deref() {
            Some(name) if name == requested => Ok(()),
            Some(name) => Err(Rejected::new(format!(
                "target names session `{requested}` but this is session `{name}`; \
                 a command can only act on the session it is sent to"
            ))),
            None => Err(Rejected::new(format!(
                "target names session `{requested}` but this weave is not running as a session"
            ))),
        }
    }

    /// Resolve the window half of a target to a workspace index.
    fn resolve_window(&self, target: &Target) -> Result<usize, Rejected> {
        self.check_session(target)?;

        let Some(window) = target.window.as_ref() else {
            return Ok(self.current_workspace);
        };

        let occupied = self.occupied_workspaces();
        let position = occupied
            .iter()
            .position(|&idx| idx == self.current_workspace)
            .unwrap_or(0);

        match window {
            WindowRef::Index(number) => usize::try_from(*number)
                .ok()
                .and_then(|number| number.checked_sub(1))
                .filter(|index| *index < WORKSPACE_COUNT)
                .ok_or_else(|| {
                    Rejected::new(format!(
                        "no window {number}: weave has workspaces 1..={WORKSPACE_COUNT}"
                    ))
                }),
            WindowRef::Next => Ok(occupied[(position + 1) % occupied.len()]),
            WindowRef::Previous => {
                Ok(occupied[(position + occupied.len() - 1) % occupied.len()])
            }
            WindowRef::Last => self.last_workspace.ok_or_else(|| {
                Rejected::new("no previous window to switch back to".to_owned())
            }),
            WindowRef::Start => Ok(occupied.first().copied().unwrap_or(self.current_workspace)),
            WindowRef::End => Ok(occupied.last().copied().unwrap_or(self.current_workspace)),
            WindowRef::Name(name) => self.window_by_name(name).ok_or_else(|| {
                Rejected::new(format!("no window named `{name}`"))
            }),
            // Windows are fixed slots, so an index already identifies one for
            // life; there is no separate id space to address.
            WindowRef::Id(id) => Err(Rejected::new(format!(
                "weave has no window ids (`@{id}`); a window's index is stable, so use `:{id}`"
            ))),
        }
    }

    /// Resolve a target to the workspace and pane it names.
    ///
    /// Pane indices are zero-based within their window, matching tmux's
    /// default `pane-base-index`; window indices are one-based.
    fn resolve_pane(&self, target: &Target) -> Result<(usize, PaneId), Rejected> {
        self.check_session(target)?;

        // `%N` carries its own scope: it finds the pane wherever it lives.
        if let Some(PaneRef::Id(number)) = target.pane.as_ref() {
            let pane = self
                .pane_by_number(*number)
                .ok_or_else(|| Rejected::new(format!("no pane `%{number}`")))?;
            let workspace = self.workspace_of_pane(pane).ok_or_else(|| {
                Rejected::new(format!("pane `%{number}` is not in any window"))
            })?;

            if target.window.is_some() {
                let requested = self.resolve_window(target)?;
                if requested != workspace {
                    return Err(Rejected::new(format!(
                        "pane `%{number}` is in window {}, not window {}",
                        workspace + 1,
                        requested + 1
                    )));
                }
            }

            return Ok((workspace, pane));
        }

        let workspace = self.resolve_window(target)?;
        let panes = self.workspaces[workspace].leaf_panes();
        if panes.is_empty() {
            return Err(Rejected::new(format!(
                "window {} has no panes",
                workspace + 1
            )));
        }

        let focused = self.workspaces[workspace].focused;
        let pane = match target.pane.as_ref() {
            None => focused.unwrap_or(panes[0]),
            Some(PaneRef::Index(index)) => usize::try_from(*index)
                .ok()
                .and_then(|index| panes.get(index).copied())
                .ok_or_else(|| {
                    Rejected::new(format!(
                        "no pane {index} in window {}: it has {} pane(s), numbered 0..{}",
                        workspace + 1,
                        panes.len(),
                        panes.len() - 1
                    ))
                })?,
            Some(PaneRef::Next) => step_pane(&panes, focused, Step::Forward),
            Some(PaneRef::Previous) => step_pane(&panes, focused, Step::Backward),
            Some(PaneRef::Last) => self.workspaces[workspace]
                .last_focused
                .filter(|pane| panes.contains(pane))
                .ok_or_else(|| {
                    Rejected::new("no previously focused pane to go back to".to_owned())
                })?,
            Some(PaneRef::Extreme(extreme)) => {
                extreme_pane(self.workspaces[workspace].root.as_ref(), *extreme)
                    .unwrap_or(panes[0])
            }
            // Handled above, before the window is resolved.
            Some(PaneRef::Id(_)) => unreachable!("pane ids are resolved before the window is"),
        };

        Ok((workspace, pane))
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
                    name: self.window_name(idx),
                    is_current,
                    pane_count: ws.pane_count(),
                })
            })
            .collect()
    }

    /// What window `index` is called.
    ///
    /// A renamed window keeps its name. Otherwise the name follows the focused
    /// pane's OSC title, so a window running `vim` labels itself `vim` without
    /// anyone having to say so — tmux's `automatic-rename`.
    fn window_name(&self, index: usize) -> String {
        let Some(workspace) = self.workspaces.get(index) else {
            return DEFAULT_WINDOW_NAME.to_owned();
        };

        if let Some(name) = workspace.name.as_deref() {
            return name.to_owned();
        }

        workspace
            .focused
            .and_then(|pane| self.pane(pane))
            .and_then(Pane::title)
            .map_or_else(|| DEFAULT_WINDOW_NAME.to_owned(), str::to_owned)
    }

    /// The window a name refers to, searching current-first so an ambiguous
    /// name resolves to the one you are looking at.
    fn window_by_name(&self, name: &str) -> Option<usize> {
        let matches = |index: usize| self.window_name(index) == name;

        if matches(self.current_workspace) {
            return Some(self.current_workspace);
        }

        (0..WORKSPACE_COUNT).find(|&index| {
            // An empty window has no panes and no identity worth matching.
            !self.workspaces[index].is_empty() && matches(index)
        })
    }

    /// The lowest-numbered window with nothing in it, for `new-window`.
    fn first_free_window(&self) -> Option<usize> {
        (0..WORKSPACE_COUNT).find(|&index| self.workspaces[index].is_empty())
    }

    /// Show workspace `target`, given as a zero-based index.
    ///
    /// Targets are resolved to an index before they get here, so an
    /// out-of-range value is a bug rather than a user error.
    /// Create a window and, unless `detached`, switch to it.
    ///
    /// With no `-t`, the window goes in the lowest-numbered free slot, which
    /// is how tmux picks an index too.
    async fn new_window(
        &mut self,
        target: &Target,
        name: Option<String>,
        command: Option<&SpawnCommand>,
        detached: bool,
    ) -> Result<(), ExecuteError> {
        let workspace = if target.window.is_some() {
            let requested = self.resolve_window(target)?;
            if !self.workspaces[requested].is_empty() {
                return Err(Rejected::new(format!(
                    "window {} already exists; `kill-window` frees it first",
                    requested + 1
                ))
                .into());
            }
            requested
        } else {
            self.first_free_window().ok_or_else(|| {
                Rejected::new(format!("all {WORKSPACE_COUNT} windows are in use"))
            })?
        };

        // Spawning happens against the outgoing window, so the new pane
        // inherits the directory you were working in.
        let pane_id = self.spawn_pane(false, command).await?;
        let previous = self.current_workspace;
        self.current_workspace = workspace;
        let root_rect = self.root_rect();
        let ws = &mut self.workspaces[workspace];
        ws.name = name;
        ws.root = Some(Node::Leaf {
            pane: pane_id,
            rect_current: FRect::from(root_rect),
            rect_target: root_rect,
        });
        ws.set_focus(Some(pane_id));

        // `switch_workspace` does the animation snapshotting and PTY resizing,
        // so route through it rather than repeating that here.
        self.current_workspace = previous;
        if detached {
            self.resize_pane(pane_id, root_rect.w, root_rect.h).await?;
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.resize(root_rect.w, root_rect.h);
            }
        } else {
            self.switch_workspace(workspace).await?;
        }
        self.dirty = true;

        Ok(())
    }

    /// Close a window and every pane in it.
    ///
    /// Unlike `kill-pane` this does not animate: the whole window is going
    /// away, so there is nothing left on screen to animate into.
    async fn kill_window(&mut self, workspace: usize) -> Result<(), ExecuteError> {
        if self.workspaces[workspace].is_empty() {
            return Err(Rejected::new(format!("window {} is empty", workspace + 1)).into());
        }

        for pane in self.workspaces[workspace].leaf_panes() {
            let _ = self.backend.kill(pane).await;
            self.remove_pane(pane);
        }

        let ws = &mut self.workspaces[workspace];
        ws.root = None;
        ws.set_focus(None);
        ws.last_focused = None;
        ws.closing.clear();
        ws.name = None;

        if self.workspaces.iter().all(Workspace::is_empty) {
            // The last window is gone, so there is no session left to show.
            self.exit = ExitState::Quit;
        } else if workspace == self.current_workspace {
            let next = self
                .workspaces
                .iter()
                .position(|ws| !ws.is_empty())
                .unwrap_or(0);
            self.switch_workspace(next).await?;
        }
        self.dirty = true;

        Ok(())
    }

    async fn switch_workspace(&mut self, target: usize) -> anyhow::Result<()> {
        if target >= WORKSPACE_COUNT || target == self.current_workspace {
            return Ok(());
        }

        // Snap any inflight animations on the outgoing workspace so its layout
        // is at rest when we return to it later.
        self.snap_workspace_tweens(self.current_workspace).await?;

        self.last_workspace = Some(self.current_workspace);
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
            ws.set_focus(Some(pane_id));
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
                    ws.set_focus(None);
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
            ws.set_focus(Some(pane_id));
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
                    if let Some(replies) = self.pane_mut(id).map(|pane| pane.process(&bytes)) {
                        self.dirty = true;
                        // A pane that asked its terminal a question is waiting
                        // on the answer before it writes anything else.
                        if !replies.is_empty() {
                            if let Err(error) = self.backend.write(id, &replies).await {
                                tracing::warn!(
                                    "failed to answer a terminal query from pane {id:?}: {error:#}"
                                );
                            }
                        }
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
            // Let the connection tasks flush before the runtime drops them.
            // Two things depend on this: the goodbye above, and the reply to a
            // request that ended the session — `wv exec kill-session` must not
            // race the shutdown and report a lost connection instead of Ok.
            time::sleep(SHUTDOWN_FLUSH_GRACE).await;
        }

        if self.exit != ExitState::Detached {
            for pane_id in self.pane_ids() {
                let _ = self.backend.kill(pane_id).await;
            }
        }

        Ok(())
    }

    /// Run a command from a keybinding, where nobody is waiting for an answer.
    ///
    /// A command the session cannot carry out — a target naming a pane that
    /// does not exist, say — is reported to the client and swallowed. Only a
    /// genuine failure, like a PTY that would not spawn, propagates and ends
    /// the session. A typo in a script must never kill a user's panes.
    pub async fn execute(&mut self, cmd: Command) -> anyhow::Result<()> {
        match self.execute_now(cmd).await {
            Ok(output) => {
                // A keybinding has nowhere to print to yet; PR 7's status line
                // message area is where this text belongs.
                if !output.is_empty() {
                    tracing::info!("command output: {output}");
                }
                Ok(())
            }
            Err(ExecuteError::Fatal(error)) => Err(error),
            Err(ExecuteError::Rejected(message)) => {
                tracing::warn!("command rejected: {message}");
                self.report_error(message);
                Ok(())
            }
        }
    }

    /// Run a command on behalf of a caller waiting for the result.
    ///
    /// A rejected command comes back as `CommandResult::Error` rather than an
    /// `Err`: the request itself succeeded, and the caller — usually
    /// `wv exec` — turns it into a message and a non-zero exit.
    pub async fn execute_request(&mut self, cmd: Command) -> anyhow::Result<CommandResult> {
        match self.execute_now(cmd).await {
            Ok(output) => Ok(CommandResult::Ok { output }),
            Err(ExecuteError::Rejected(message)) => {
                tracing::warn!("command rejected: {message}");
                Ok(CommandResult::Error { message })
            }
            Err(ExecuteError::Fatal(error)) => Err(error),
        }
    }

    /// Surface a non-fatal problem to whoever is attached.
    fn report_error(&mut self, message: String) {
        if let OutputSink::Client { frames, .. } = &self.sink {
            let _ = frames.send(ServerToClient::Error(message));
        }
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

    /// Run a command, returning whatever it printed.
    ///
    /// Most commands print nothing and return an empty string; only
    /// `display-message -p` has output today.
    async fn execute_now(&mut self, cmd: Command) -> Result<String, ExecuteError> {
        match cmd {
            Command::SplitWindow {
                split,
                target,
                command,
                detached,
            } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                self.switch_workspace(workspace).await?;
                self.split_pane(pane, split, command.as_ref(), detached)
                    .await?;
            }
            Command::SendKeys {
                target,
                keys,
                literal,
            } => {
                let (_, pane) = self.resolve_pane(&target)?;
                self.send_keys(pane, &keys, literal).await?;
            }
            Command::RespawnPane {
                target,
                kill,
                command,
            } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                self.switch_workspace(workspace).await?;
                self.respawn_pane(pane, kill, command.as_ref()).await?;
            }
            Command::SelectPane { selector } => self.select_pane(selector).await?,
            Command::SelectWindow { target, create } => {
                let workspace = self.resolve_window(&target)?;
                if !create && self.workspaces[workspace].is_empty() {
                    return Err(Rejected::new(format!(
                        "window {} does not exist; `new-window` makes one",
                        workspace + 1
                    ))
                    .into());
                }
                self.switch_workspace(workspace).await?;
            }
            Command::NewWindow {
                target,
                name,
                command,
                detached,
            } => {
                self.new_window(&target, name, command.as_ref(), detached)
                    .await?;
            }
            Command::KillWindow { target } => {
                let workspace = self.resolve_window(&target)?;
                self.kill_window(workspace).await?;
            }
            Command::RenameWindow { target, name } => {
                let workspace = self.resolve_window(&target)?;
                self.workspaces[workspace].name = Some(name);
                self.dirty = true;
            }
            Command::KillPane { target } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                self.switch_workspace(workspace).await?;
                self.close_pane(pane).await?;
            }
            Command::DetachClient => self.detach(),
            Command::KillSession { target } => {
                self.check_session(&target)?;
                self.exit = ExitState::Quit;
            }
            Command::DisplayMessage { message, target } => {
                // The target is resolved even though the message is literal, so
                // `-t` is validated now and means something once PR 6 adds the
                // `#{...}` variables that read from it.
                self.resolve_pane(&target)?;
                return Ok(message);
            }
        }

        Ok(String::new())
    }

    async fn select_pane(&mut self, selector: PaneSelector) -> Result<(), ExecuteError> {
        let target = match selector {
            // Directional focus stays geometric: it walks the layout tree
            // rather than the flat pane order a target would give.
            PaneSelector::Direction(direction) => {
                self.focus_direction(direction);
                return Ok(());
            }
            PaneSelector::Last => Target {
                pane: Some(PaneRef::Last),
                ..Target::default()
            },
            PaneSelector::Target(target) => target,
        };

        let (workspace, pane) = self.resolve_pane(&target)?;
        self.switch_workspace(workspace).await?;
        self.focus_pane(pane);

        Ok(())
    }

    /// Focus an addressed pane in the current workspace.
    fn focus_pane(&mut self, pane: PaneId) {
        let Some(previous) = self.current().focused else {
            self.current_mut().set_focus(Some(pane));
            self.dirty = true;
            return;
        };
        if previous == pane {
            return;
        }

        self.start_focus_border_tweens(previous, pane);
        self.current_mut().set_focus(Some(pane));
        self.dirty = true;
    }

    async fn spawn_shell_pane(&mut self, resize_immediately: bool) -> anyhow::Result<PaneId> {
        self.spawn_pane(resize_immediately, None).await
    }

    /// Spawn a pane process, defaulting to the user's shell.
    ///
    /// `spec` is what `split-window`'s `-c` and trailing command line said. A
    /// pane with no explicit cwd inherits the focused pane's, so a new split
    /// opens where you were working.
    async fn spawn_pane(
        &mut self,
        resize_immediately: bool,
        spec: Option<&SpawnCommand>,
    ) -> anyhow::Result<PaneId> {
        let mut cmd = pane_command_for(spec);

        if cmd.cwd.is_none() {
            if let Some(focused) = self.current().focused {
                match self.backend.pane_cwd(focused).await {
                    Ok(Some(cwd)) => cmd.cwd = Some(cwd),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(?error, ?focused, "failed to query pane cwd before spawn");
                    }
                }
            }
        }
        // Last resort: the directory the session server itself started in.
        if cmd.cwd.is_none() {
            cmd.cwd = std::env::current_dir().ok();
        }

        let pane_id = self.backend.spawn(cmd).await?;
        let number = self.register_pane_number(pane_id);
        tracing::debug!("spawned pane %{number}");

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
            SessionEvent::Message(ClientToServer::Hello { .. }) => {
                // The socket layer checks the version before forwarding
                // anything, so a handshake reaching the app is redundant.
                tracing::warn!("ignoring a repeated protocol handshake");
            }
            SessionEvent::Message(ClientToServer::Input(event)) => {
                self.handle_input(Some(Ok(event))).await?;
            }
            SessionEvent::Message(ClientToServer::Resize { cols, rows }) => {
                self.resize_to(cols, rows).await;
            }
            // A request from the attached client: it is watching frames, not
            // this socket, so the reply goes back through its frame sink.
            SessionEvent::Message(ClientToServer::Request { id, command }) => {
                let result = self.execute_request(command).await?;
                if let OutputSink::Client { frames, .. } = &self.sink {
                    let _ = frames.send(ServerToClient::Reply { id, result });
                }
            }
            SessionEvent::Request { command, reply } => {
                let result = self.execute_request(command).await?;
                // A dropped receiver means the caller hung up mid-command. The
                // command still ran; there is simply nobody left to tell.
                if reply.send(result).is_err() {
                    tracing::debug!("command completed after its caller disconnected");
                }
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

    /// Split `pane` in two, spawning a process in the new half.
    ///
    /// The pane is passed in rather than read from the workspace so `-t` can
    /// split something other than the focused pane. `detached` is tmux's `-d`:
    /// make the pane but leave focus alone.
    async fn split_pane(
        &mut self,
        pane: PaneId,
        split: Split,
        command: Option<&SpawnCommand>,
        detached: bool,
    ) -> anyhow::Result<()> {
        let focused = pane;
        let Some(old_parent_rect) = self.leaf_rect_target(focused) else {
            return Ok(());
        };
        let new_pane = self.spawn_pane(false, command).await?;

        let ws = self.current_mut();
        if let Some(root) = ws.root.as_mut() {
            root.split_focused(focused, split, new_pane);
            if !detached {
                ws.set_focus(Some(new_pane));
            }
            self.recompute_layout();
            self.start_open_tweens(focused, new_pane, split, old_parent_rect);
            self.dirty = true;
        }

        Ok(())
    }

    /// Type `keys` into `pane`, producing exactly the bytes the keyboard would.
    ///
    /// Each argument is a key name when it parses as one and literal text
    /// otherwise, so `send-keys 'npm run dev' Enter` reads the way it looks.
    /// `-l` forces every argument to be literal.
    async fn send_keys(
        &mut self,
        pane: PaneId,
        keys: &[String],
        literal: bool,
    ) -> Result<(), ExecuteError> {
        let mut bytes = Vec::new();

        for key in keys {
            match (literal, crate::input::keys::parse_key_name(key)) {
                (false, Some(event)) => match crate::input::encode(&event) {
                    Some(encoded) => bytes.extend(encoded),
                    None => {
                        return Err(Rejected::new(format!(
                            "key `{key}` has no terminal encoding"
                        ))
                        .into())
                    }
                },
                _ => bytes.extend(key.as_bytes()),
            }
        }

        self.backend
            .write(pane, &bytes)
            .await
            .with_context(|| format!("failed to send keys to pane {pane:?}"))?;

        Ok(())
    }

    /// Restart a pane's process, keeping the pane where it is in the layout.
    ///
    /// The pane keeps its `%N` and its rectangle; only the process behind it
    /// is replaced, so a respawn does not animate.
    async fn respawn_pane(
        &mut self,
        pane: PaneId,
        kill: bool,
        command: Option<&SpawnCommand>,
    ) -> Result<(), ExecuteError> {
        if !kill {
            // tmux refuses to respawn a pane whose process is still alive
            // unless `-k` says to kill it first.
            return Err(Rejected::new(
                "pane is still running; pass `-k` to kill it first".to_owned(),
            )
            .into());
        }

        let number = self.pane_number(pane);
        let cwd = self.backend.pane_cwd(pane).await.ok().flatten();
        self.backend
            .kill(pane)
            .await
            .with_context(|| format!("failed to kill pane {pane:?} before respawning"))?;

        // Inherit the dead pane's directory unless the caller named one.
        let mut spec = command.cloned().unwrap_or_default();
        if spec.cwd.is_none() {
            spec.cwd = cwd;
        }

        let replacement = self.spawn_pane(false, Some(&spec)).await?;
        self.replace_pane(pane, replacement);
        if let Some(number) = number {
            // The pane the user addressed is still the pane they see, so it
            // keeps its number rather than appearing to have been replaced.
            self.pane_numbers.remove(&replacement);
            self.pane_numbers.insert(replacement, number);
        }
        self.dirty = true;

        Ok(())
    }

    /// Swap `replacement` in wherever `pane` sat: layout, focus and panes list.
    fn replace_pane(&mut self, pane: PaneId, replacement: PaneId) {
        let rect = self.leaf_rect_target(pane);

        for workspace in &mut self.workspaces {
            if let Some(root) = workspace.root.as_mut() {
                if let Some(Node::Leaf { pane: leaf, .. }) = root.find_leaf_mut(pane) {
                    *leaf = replacement;
                }
            }
            if workspace.focused == Some(pane) {
                workspace.focused = Some(replacement);
            }
            if workspace.last_focused == Some(pane) {
                workspace.last_focused = Some(replacement);
            }
        }

        self.panes.retain(|existing| existing.id() != pane);
        self.pane_numbers.remove(&pane);
        let (cols, rows) = rect.map_or((self.back.width, self.back.height), |rect| {
            (rect.w, rect.h)
        });
        self.panes.push(Pane::new(replacement, cols, rows));
        self.recompute_layout();
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

    /// Move focus to the nearest pane in `dir`, geometrically.
    fn focus_direction(&mut self, dir: Direction) {
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
            self.current_mut().set_focus(Some(next));
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

    /// Close `pane`, animating the panes that grow into its space.
    ///
    /// The pane is passed in rather than read from the workspace so `-t` can
    /// close something other than the focused pane.
    async fn close_pane(&mut self, pane: PaneId) -> anyhow::Result<()> {
        let focused = pane;
        if self.current().closing.contains(&focused) {
            return Ok(());
        }

        let Some(root) = self.current().root.as_ref() else {
            self.current_mut().set_focus(None);
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
            ws.set_focus(None);
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
        ws.set_focus(new_focus);
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
                        ws.set_focus(new_focus);
                        if ws_idx == self.current_workspace {
                            self.recompute_layout();
                        }
                    } else if was_focused {
                        ws.root = None;
                        ws.set_focus(None);
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
                        let next = ws.root.as_ref().and_then(first_leaf_pane);
                        ws.set_focus(next);
                    }
                } else {
                    ws.root = None;
                    ws.set_focus(None);
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

    fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id() == id)
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id() == id)
    }

    fn remove_pane(&mut self, id: PaneId) {
        self.panes.retain(|pane| pane.id() != id);
        // Retire the `%N` with the pane. Numbers are never reused, so a stale
        // `%N` in a script fails loudly instead of hitting somebody else's pane.
        self.pane_numbers.remove(&id);
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
            name: None,
            root: Some(Node::Leaf {
                pane: pane_id,
                rect_current: FRect::from(root_rect),
                rect_target: root_rect,
            }),
            focused: Some(pane_id),
            last_focused: None,
            closing: HashSet::new(),
        };

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            panes: vec![Pane::new(pane_id, width, height)],
            workspaces,
            current_workspace: 0,
            last_workspace: None,
            pane_numbers: std::iter::once((pane_id, 1)).collect(),
            next_pane_number: 2,
            session_name: None,
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

/// Build the process to run in a new pane.
///
/// With no command the pane runs the user's login shell. With one, the first
/// word is the program and the rest are its arguments — the caller's shell has
/// already done the quoting, so weave does no word splitting of its own.
fn pane_command_for(spec: Option<&SpawnCommand>) -> PaneCommand {
    let default_shell = || std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());

    let (program, args) = match spec.map(|spec| spec.argv.as_slice()) {
        Some([program, args @ ..]) => (program.clone(), args.to_vec()),
        _ => (default_shell(), Vec::new()),
    };

    PaneCommand {
        program,
        args,
        env: Vec::new(),
        // Left as given: `None` means "wherever the focused pane is", which
        // only `spawn_pane` can work out.
        cwd: spec.and_then(|spec| spec.cwd.clone()),
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

/// Which way `+` and `-` walk the pane list.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Step {
    Forward,
    Backward,
}

/// Step one place through `panes` from `focused`, wrapping at both ends.
///
/// `panes` is in layout order, so this is the order `+` and `-` walk.
fn step_pane(panes: &[PaneId], focused: Option<PaneId>, step: Step) -> PaneId {
    debug_assert!(!panes.is_empty(), "callers check for an empty window first");

    let len = panes.len();
    let current = focused
        .and_then(|focused| panes.iter().position(|&pane| pane == focused))
        .unwrap_or(0);
    let next = match step {
        Step::Forward => (current + 1) % len,
        // `+ len` keeps the arithmetic on usize when current is 0.
        Step::Backward => (current + len - 1) % len,
    };

    panes[next]
}

/// The pane furthest in one direction, for `{top}`, `{bottom}`, `{left}`, `{right}`.
fn extreme_pane(root: Option<&Node>, extreme: Extreme) -> Option<PaneId> {
    let mut targets = Vec::new();
    collect_leaf_targets(root?, &mut targets);

    targets
        .into_iter()
        .min_by_key(|(_, rect)| match extreme {
            Extreme::Top => i32::from(rect.y),
            Extreme::Bottom => -i32::from(rect.y),
            Extreme::Left => i32::from(rect.x),
            Extreme::Right => -i32::from(rect.x),
        })
        .map(|(pane, _)| pane)
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
    use crate::session::protocol::CommandResult;
    use crate::layout::tree::Node;
    use tokio::time::Duration;

    /// Every write the app made, in order, tagged with its pane.
    type WriteLog = Arc<Mutex<Vec<(PaneId, Vec<u8>)>>>;

    struct MockBackend {
        next_id: PaneId,
        resized: Arc<Mutex<Vec<(PaneId, u16, u16)>>>,
        spawned: Arc<Mutex<Vec<PaneCommand>>>,
        written: WriteLog,
        killed: Arc<Mutex<Vec<PaneId>>>,
    }

    #[derive(Clone, Default)]
    struct MockBackendHandle {
        resized: Arc<Mutex<Vec<(PaneId, u16, u16)>>>,
        spawned: Arc<Mutex<Vec<PaneCommand>>>,
        written: WriteLog,
        killed: Arc<Mutex<Vec<PaneId>>>,
    }

    impl MockBackendHandle {
        fn resized(&self) -> Vec<(PaneId, u16, u16)> {
            lock_resized(&self.resized).clone()
        }

        fn clear_resized(&self) {
            lock_resized(&self.resized).clear();
        }

        fn spawned(&self) -> Vec<PaneCommand> {
            lock_vec(&self.spawned).clone()
        }

        /// Every byte written to `pane`, concatenated in order.
        fn written_to(&self, pane: PaneId) -> Vec<u8> {
            lock_vec(&self.written)
                .iter()
                .filter(|(id, _)| *id == pane)
                .flat_map(|(_, bytes)| bytes.clone())
                .collect()
        }

        fn killed(&self) -> Vec<PaneId> {
            lock_vec(&self.killed).clone()
        }
    }

    /// Drive the app the way a script does: through the parser.
    ///
    /// The behaviour tests go through `Command::parse_str` rather than
    /// building variants directly, so they double as proof that the weave
    /// aliases still mean what they always meant.
    fn command(line: &str) -> Command {
        Command::parse_str(line).expect("command parses")
    }

    fn mock_backend(next_id: PaneId) -> (Box<dyn PaneBackend>, MockBackendHandle) {
        let handle = MockBackendHandle::default();
        let backend = MockBackend {
            next_id,
            resized: Arc::clone(&handle.resized),
            spawned: Arc::clone(&handle.spawned),
            written: Arc::clone(&handle.written),
            killed: Arc::clone(&handle.killed),
        };

        (Box::new(backend), handle)
    }

    fn lock_vec<T>(values: &Mutex<Vec<T>>) -> std::sync::MutexGuard<'_, Vec<T>> {
        values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        async fn spawn(&mut self, cmd: PaneCommand) -> Result<PaneId, Error> {
            lock_vec(&self.spawned).push(cmd);
            let pane_id = self.next_id;
            self.next_id = PaneId(self.next_id.0.saturating_add(1));
            Ok(pane_id)
        }

        async fn write(&mut self, id: PaneId, data: &[u8]) -> Result<(), Error> {
            lock_vec(&self.written).push((id, data.to_vec()));
            Ok(())
        }

        async fn resize(&mut self, id: PaneId, cols: u16, rows: u16) -> Result<(), Error> {
            lock_resized(&self.resized).push((id, cols, rows));
            Ok(())
        }

        async fn kill(&mut self, id: PaneId) -> Result<(), Error> {
            lock_vec(&self.killed).push(id);
            Ok(())
        }

    }

    /// A two-pane session with focus on the pane spawned second (`%2`).
    async fn two_pane_app() -> App {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(command("split-v")).await.expect("split succeeds");
        assert_eq!(app.current().focused, Some(PaneId(2)));

        app
    }

    #[tokio::test]
    async fn split_window_runs_the_command_it_was_given() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("split-window -h npm run dev"))
            .await
            .expect("split succeeds");

        let spawned = handle.spawned();
        let last = spawned.last().expect("a pane was spawned");
        assert_eq!(last.program, "npm");
        assert_eq!(last.args, vec!["run", "dev"]);
    }

    #[tokio::test]
    async fn split_window_without_a_command_runs_the_shell() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("split-h")).await.expect("split succeeds");

        let expected = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        assert_eq!(handle.spawned().last().expect("spawned").program, expected);
    }

    #[tokio::test]
    async fn split_window_c_sets_the_new_panes_directory() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("split-window -c /srv"))
            .await
            .expect("split succeeds");

        assert_eq!(
            handle.spawned().last().expect("spawned").cwd,
            Some(std::path::PathBuf::from("/srv"))
        );
    }

    /// `-d` is what lets a script build a layout without the focus jumping
    /// around underneath it.
    #[tokio::test]
    async fn detached_split_leaves_focus_alone() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("split-window -d"))
            .await
            .expect("split succeeds");

        assert_eq!(app.current().focused, Some(PaneId(1)));
        assert_eq!(app.current().leaf_panes().len(), 2);
    }

    /// send-keys must be indistinguishable from typing, so it is asserted
    /// against the very encoder the keyboard path uses.
    #[tokio::test]
    async fn send_keys_writes_the_same_bytes_as_typing() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("send-keys -t %1 echo Enter"))
            .await
            .expect("send-keys succeeds");

        let typed_enter = crate::input::encode(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
        .expect("Enter encodes");
        let mut expected = b"echo".to_vec();
        expected.extend(typed_enter);

        assert_eq!(handle.written_to(PaneId(1)), expected);
    }

    #[tokio::test]
    async fn send_keys_l_sends_a_key_name_as_text() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("send-keys -l Enter"))
            .await
            .expect("send-keys succeeds");

        assert_eq!(handle.written_to(PaneId(1)), b"Enter".to_vec());
    }

    #[tokio::test]
    async fn send_keys_encodes_control_keys() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("send-keys C-c"))
            .await
            .expect("send-keys succeeds");

        assert_eq!(handle.written_to(PaneId(1)), vec![0x03]);
    }

    #[tokio::test]
    async fn respawn_pane_replaces_the_process_but_keeps_the_pane_number() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("respawn-pane -k -t %1 htop"))
            .await
            .expect("respawn succeeds");

        assert_eq!(handle.killed(), vec![PaneId(1)]);
        assert_eq!(handle.spawned().last().expect("spawned").program, "htop");
        // The pane the script addressed is still the pane it sees.
        assert_eq!(app.pane_number(PaneId(2)), Some(1));
        assert_eq!(app.current().focused, Some(PaneId(2)));
        assert_eq!(app.current().leaf_panes(), vec![PaneId(2)]);
    }

    /// Without `-k`, tmux refuses rather than killing something silently.
    #[tokio::test]
    async fn respawn_pane_without_k_is_refused() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let result = app
            .execute_request(command("respawn-pane -t %1"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
        assert!(handle.killed().is_empty());
    }

    #[tokio::test]
    async fn new_window_takes_the_lowest_free_slot_and_switches_to_it() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("new-window -n build"))
            .await
            .expect("new-window succeeds");

        assert_eq!(app.current_workspace, 1, "window 1 was taken");
        assert_eq!(app.window_name(1), "build");
        assert_eq!(app.current().leaf_panes(), vec![PaneId(2)]);
    }

    #[tokio::test]
    async fn new_window_d_stays_where_it_is() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("new-window -d -n build"))
            .await
            .expect("new-window succeeds");

        assert_eq!(app.current_workspace, 0);
        assert_eq!(app.window_name(1), "build");
        assert!(!app.workspaces[1].is_empty());
    }

    #[tokio::test]
    async fn new_window_runs_a_command_and_refuses_an_occupied_slot() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("new-window -d -t :2 -- htop"))
            .await
            .expect("new-window succeeds");
        assert_eq!(handle.spawned().last().expect("spawned").program, "htop");

        let result = app
            .execute_request(command("new-window -t :2"))
            .await
            .expect("the request completes");
        assert!(!result.is_ok(), "an occupied window must not be reused");
    }

    /// This is what the whole PR is for: addressing a window by name.
    #[tokio::test]
    async fn a_window_can_be_addressed_by_name() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("new-window -d -n build"))
            .await
            .expect("new-window succeeds");
        app.execute(command("select-window -t :build"))
            .await
            .expect("select by name succeeds");

        assert_eq!(app.current_workspace, 1);
    }

    #[tokio::test]
    async fn an_unknown_window_name_is_rejected() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let result = app
            .execute_request(command("select-window -t :nope"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
        assert_eq!(app.current_workspace, 0);
    }

    #[tokio::test]
    async fn rename_window_pins_the_name_against_pane_titles() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        // A window with no name follows its focused pane's title.
        assert_eq!(app.window_name(0), "shell");
        app.execute(command("rename-window editor"))
            .await
            .expect("rename succeeds");

        assert_eq!(app.window_name(0), "editor");
        assert_eq!(app.workspaces[0].name.as_deref(), Some("editor"));
    }

    /// `select-window` fails on a window that is not there, as tmux does,
    /// while the `workspace-N` alias behind `Alt+N` still creates one.
    #[tokio::test]
    async fn select_window_does_not_create_but_the_alias_does() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let result = app
            .execute_request(command("select-window -t :4"))
            .await
            .expect("the request completes");
        assert!(!result.is_ok());
        assert_eq!(app.current_workspace, 0);

        app.execute(command("workspace-4"))
            .await
            .expect("the alias creates");
        assert_eq!(app.current_workspace, 3);
    }

    #[tokio::test]
    async fn kill_window_closes_every_pane_in_it() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("new-window -d -n build"))
            .await
            .expect("new-window succeeds");
        app.execute(command("kill-window -t :build"))
            .await
            .expect("kill-window succeeds");

        assert!(app.workspaces[1].is_empty());
        assert!(handle.killed().contains(&PaneId(2)));
        assert_eq!(app.current_workspace, 0, "we were never in it");
        assert_eq!(app.exit, ExitState::Running);
    }

    #[tokio::test]
    async fn killing_the_last_window_ends_the_session() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("kill-window")).await.expect("kill succeeds");

        assert_eq!(app.exit, ExitState::Quit);
    }

    /// A waiting caller gets the command's output, not just "it ran".
    #[tokio::test]
    async fn a_request_returns_command_output() {
        let mut app = two_pane_app().await;

        let result = app
            .execute_request(command("display-message -p hello"))
            .await
            .expect("the request completes");

        assert_eq!(
            result,
            CommandResult::Ok {
                output: "hello".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn most_commands_return_no_output() {
        let mut app = two_pane_app().await;

        let result = app
            .execute_request(command("select-pane -t %1"))
            .await
            .expect("the request completes");

        assert_eq!(result, CommandResult::empty());
    }

    /// A rejected command is a completed request with an error in it, not a
    /// transport failure: the caller needs the message, and the session lives.
    #[tokio::test]
    async fn a_rejected_command_comes_back_as_an_error_result() {
        let mut app = two_pane_app().await;

        let result = app
            .execute_request(command("kill-pane -t %99"))
            .await
            .expect("a bad target is not a transport failure");

        match result {
            CommandResult::Error { message } => assert!(message.contains("%99"), "{message}"),
            other @ CommandResult::Ok { .. } => panic!("expected an error result, got {other:?}"),
        }
        assert_eq!(app.exit, ExitState::Running);
        assert_eq!(app.panes.len(), 2);
    }

    #[tokio::test]
    async fn a_target_that_names_a_missing_pane_still_reports_through_display_message() {
        let mut app = two_pane_app().await;

        let result = app
            .execute_request(command("display-message -p -t %99 hi"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok(), "an unresolvable -t must not print");
    }

    #[tokio::test]
    async fn a_pane_id_target_acts_on_a_pane_that_is_not_focused() {
        let mut app = two_pane_app().await;

        app.execute(command("kill-pane -t %1"))
            .await
            .expect("kill succeeds");

        assert!(
            app.current().closing.contains(&PaneId(1)),
            "`-t %1` should close the unfocused pane, not the focused one"
        );
    }

    #[tokio::test]
    async fn split_window_targets_a_pane_that_is_not_focused() {
        let mut app = two_pane_app().await;

        app.execute(command("split-window -v -t %1"))
            .await
            .expect("split succeeds");

        // The new pane is %1's sibling, so %1's half of the screen is the one
        // that got divided.
        let root = app.current().root.as_ref().expect("a layout exists");
        let panes = app.current().leaf_panes();
        assert_eq!(panes.len(), 3);
        assert!(root.find_leaf(PaneId(3)).is_some());
    }

    /// A target naming something that is not there must be reported, not
    /// fatal: a typo in a script cannot be allowed to take panes down with it.
    #[tokio::test]
    async fn an_unresolvable_target_leaves_the_session_untouched() {
        let mut app = two_pane_app().await;

        app.execute(command("kill-pane -t %99"))
            .await
            .expect("an unknown target is not a fatal error");

        assert_eq!(app.panes.len(), 2);
        assert!(app.current().closing.is_empty());
        assert_eq!(app.exit, ExitState::Running);
    }

    #[tokio::test]
    async fn a_target_naming_another_session_is_refused() {
        let mut app = two_pane_app().await;
        app.session_name = Some("dev".to_owned());

        app.execute(command("kill-pane -t other:1.0"))
            .await
            .expect("a foreign session is rejected, not fatal");

        assert!(app.current().closing.is_empty());
    }

    #[tokio::test]
    async fn pane_index_targets_follow_layout_order() {
        let mut app = two_pane_app().await;
        let panes = app.current().leaf_panes();

        app.execute(command("select-pane -t .0"))
            .await
            .expect("select succeeds");

        assert_eq!(app.current().focused, Some(panes[0]));
    }

    #[tokio::test]
    async fn last_target_returns_to_the_previously_focused_pane() {
        let mut app = two_pane_app().await;

        app.execute(command("select-pane -t %1"))
            .await
            .expect("select succeeds");
        assert_eq!(app.current().focused, Some(PaneId(1)));

        app.execute(command("select-pane -l"))
            .await
            .expect("select succeeds");

        assert_eq!(app.current().focused, Some(PaneId(2)));
    }

    /// `%N` is absolute, so addressing a pane in another workspace brings that
    /// workspace forward rather than failing.
    #[tokio::test]
    async fn a_pane_id_target_switches_to_the_window_holding_it() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("workspace-2"))
            .await
            .expect("switch succeeds");
        assert_eq!(app.current_workspace, 1);

        app.execute(command("select-pane -t %1"))
            .await
            .expect("select succeeds");

        assert_eq!(app.current_workspace, 0);
        assert_eq!(app.current().focused, Some(PaneId(1)));
    }

    #[tokio::test]
    async fn select_window_last_returns_to_the_previous_workspace() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("workspace-3"))
            .await
            .expect("switch succeeds");
        app.execute(command("select-window -l"))
            .await
            .expect("switch succeeds");

        assert_eq!(app.current_workspace, 0);
    }

    /// Numbers are handed out in order and retired with their pane, so a `%N`
    /// held by a script never silently starts pointing at a different pane.
    #[tokio::test]
    async fn pane_numbers_are_monotonic_and_never_reused() {
        let mut app = two_pane_app().await;
        assert_eq!(app.pane_number(PaneId(1)), Some(1));
        assert_eq!(app.pane_number(PaneId(2)), Some(2));

        app.remove_pane(PaneId(2));
        assert_eq!(app.pane_number(PaneId(2)), None);

        app.execute(command("split-v")).await.expect("split succeeds");

        assert_eq!(
            app.pane_number(PaneId(3)),
            Some(3),
            "a new pane takes the next number rather than the freed one"
        );
    }

    #[tokio::test]
    async fn execute_split_h_splits_focused_leaf_with_spawned_pane() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("split-h")).await.expect("split succeeds");

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
        app.execute(command("split-h")).await.expect("split succeeds");
        assert_eq!(
            app.workspaces[app.current_workspace].focused,
            Some(PaneId(2))
        );

        app.execute(command("focus-up")).await.expect("focus succeeds");

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

        app.execute(command("split-v")).await.expect("split succeeds");
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
        app.execute(command("split-h")).await.expect("split succeeds");
        app.advance_animations(OPEN_NEW_PANE_DURATION)
            .await
            .expect("open animation completes");
        handle.clear_resized();

        app.execute(command("close")).await.expect("close starts");

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

        app.execute(command("workspace-2"))
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

        app.execute(command("workspace-2"))
            .await
            .expect("switch to 2");
        app.execute(command("workspace-1"))
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

        app.execute(command("detach")).await.expect("detach succeeds");

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
                command: command("split-v"),
            })
        );
        assert_eq!(
            LaunchArgs::parse(["exec", "--session", "main", "workspace-3"])
                .expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: Some("main".to_owned()),
                command: command("workspace-3"),
            })
        );
    }

    #[test]
    fn launch_args_accept_tmux_verbs_with_flags() {
        assert_eq!(
            LaunchArgs::parse(["exec", "split-window", "-h", "-t", "%2"])
                .expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: None,
                command: command("split-window -h -t %2"),
            })
        );
    }

    /// `wv exec`'s own flags stop at the command name, so a command flag that
    /// looks like one of weave's is passed through rather than swallowed.
    #[test]
    fn exec_flags_stop_at_the_command_name() {
        assert_eq!(
            LaunchArgs::parse(["exec", "--session", "main", "select-pane", "-l"])
                .expect("launch args parse"),
            LaunchArgs::Exec(ExecArgs {
                session_name: Some("main".to_owned()),
                command: command("select-pane -l"),
            })
        );

        let error = LaunchArgs::parse(["exec", "-t", "%1", "select-pane"])
            .expect_err("weave flags before the command are checked")
            .to_string();
        assert!(error.contains("before the command name"), "{error}");
    }

    #[test]
    fn launch_args_reject_unknown_exec_commands() {
        let error = LaunchArgs::parse(["exec", "split-pane"])
            .expect_err("unknown commands are rejected")
            .to_string();

        assert!(error.contains("unknown command"), "{error}");
    }

    /// An unsupported flag names the PR that brings it, so a user porting a
    /// tmux script learns what is missing rather than that something failed.
    #[test]
    fn exec_reports_unsupported_flags_with_their_plan() {
        let error = LaunchArgs::parse(["exec", "split-window", "-p", "30"])
            .expect_err("sizing is not implemented yet")
            .to_string();

        assert!(error.contains("PR 5"), "{error}");
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
