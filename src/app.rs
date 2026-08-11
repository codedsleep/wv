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

use crate::agent::{self, AgentTracker};
use crate::anim::timeline::Timeline;
use crate::anim::tween::Easing;
use crate::backend::native::NativeBackend;
use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use crate::command::target::{Extreme, PaneRef, WindowRef};
use crate::command::{
    Command, LayoutPreset, ListScope, PaneSelector, ResizeChange, SpawnCommand, SplitSize, Target,
    WaitAction,
};
use crate::config::{Config, Options, ThemeConfig};
use crate::format::{expand as expand_format_impl, Variables};
use crate::input;
use crate::input::keymap::{format_key, Binding, Keymap, PREFIX_TABLE, ROOT_TABLE};
use crate::input::keys::parse_binding_key;
use crate::layout::geometry::{Direction, FRect, Rect, Split};
use crate::layout::tree::{self, Node};
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
/// Reshaping an existing layout — resize, zoom, swap, rotate, select-layout.
///
/// Shorter than opening a pane: nothing is appearing, so a long tween just
/// feels like lag when you are holding down a resize key.
const RESIZE_DURATION: Duration = Duration::from_millis(140);
/// How long a `display-message` stays on the status line.
const MESSAGE_DURATION: Duration = Duration::from_secs(3);
/// The share `main-vertical` and `main-horizontal` give their main pane.
const MAIN_PANE_RATIO: f32 = 0.5;
/// How long shutdown waits for socket writers to flush their last frame.
///
/// These are local unix sockets with at most a frame or two in flight, so this
/// is generous; it only has to outlast a single write syscall.
const SHUTDOWN_FLUSH_GRACE: Duration = Duration::from_millis(50);
/// Default `list-panes` output, matching tmux's shape closely enough that a
/// script written against tmux reads the same fields out.
const DEFAULT_PANE_FORMAT: &str =
    "#{window_index}.#{pane_index}: [#{pane_width}x#{pane_height}] #{pane_id}#{?pane_active, (active),}";
const DEFAULT_WINDOW_FORMAT: &str =
    "#{window_index}: #{window_name} [#{window_panes} panes]#{?window_active, (active),}";
const DEFAULT_SESSION_FORMAT: &str = "#{session_name}: #{session_windows} windows";
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
const EVENT_CHANNEL_CAPACITY: usize = 64;

type BoxedBackend = Box<dyn PaneBackend>;

/// Workspaces addressable with `Alt+1` .. `Alt+9`.
pub const WORKSPACE_COUNT: usize = 9;

/// What a window is called before anything names it.
const DEFAULT_WINDOW_NAME: &str = "shell";
/// Shown in place of a session name when weave is not running as a session.
const UNNAMED_SESSION: &str = "weave";

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
    /// The pane filling the window, if `resize-pane -Z` zoomed one.
    ///
    /// The layout tree is untouched while zoomed, so unzooming is just
    /// forgetting this — and both directions animate for free.
    zoomed: Option<PaneId>,
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

/// How often the panes' foreground jobs are re-read from `/proc`.
const AGENT_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[cfg(test)]
const fn test_theme() -> ThemeConfig {
    ThemeConfig {
        border_focused: crossterm::style::Color::Cyan,
        border_unfocused: crossterm::style::Color::DarkGrey,
        status_fg: crossterm::style::Color::White,
        status_bg: crossterm::style::Color::DarkBlue,
        accent: crossterm::style::Color::Red,
        agent_working: crossterm::style::Color::Green,
        agent_waiting: crossterm::style::Color::Yellow,
        agent_idle: crossterm::style::Color::DarkGrey,
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

/// The shell to run `run-shell` and `if-shell` commands with.
fn shell_program() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned())
}

/// Expand a format string, turning a bad one into a rejection rather than a
/// fatal error: a malformed `-F` is the caller's typo, not a session failure.
fn expand_format(template: &str, vars: &Variables) -> Result<String, Rejected> {
    expand_format_impl(template, vars).map_err(|error| Rejected::new(error.to_string()))
}

/// One terminal watching the session.
///
/// Each client keeps its own `front` surface and `DiffRenderer`: a diff frame
/// only means anything applied on top of what *that* terminal has already
/// seen, so the delta cannot be shared even though the composed frame is.
struct AttachedClient {
    id: u64,
    cols: u16,
    rows: u16,
    frames: mpsc::UnboundedSender<ServerToClient>,
    front: Surface,
    diff: DiffRenderer,
    buf: Vec<u8>,
    /// Set on attach and on resize: the next frame must redraw every cell,
    /// because what this terminal is showing is no longer known.
    needs_full_repaint: bool,
}

impl AttachedClient {
    fn new(
        id: u64,
        cols: u16,
        rows: u16,
        truecolor: bool,
        frames: mpsc::UnboundedSender<ServerToClient>,
    ) -> Self {
        let mut diff = DiffRenderer::new();
        diff.set_color_mode(if truecolor {
            ColorMode::Truecolor
        } else {
            ColorMode::Quantized
        });

        Self {
            id,
            cols,
            rows,
            frames,
            front: Surface::new(cols, rows),
            diff,
            buf: Vec::new(),
            needs_full_repaint: true,
        }
    }
}

/// An open `command-prompt`: a line being typed on the status bar.
///
/// While one is open it takes every key before the pane sees it, which is what
/// makes typing a window name possible at all — otherwise the letters would go
/// straight to the shell.
struct Prompt {
    /// What is shown before the input, e.g. `rename-window:`.
    label: String,
    /// The text so far, held as chars so the cursor can index it without
    /// worrying about UTF-8 boundaries.
    input: Vec<char>,
    cursor: usize,
    /// The command to run, with `%%` standing in for the input.
    template: String,
}

impl Prompt {
    fn text(&self) -> String {
        self.input.iter().collect()
    }

    /// The status-bar line, with a block where the cursor is.
    fn render(&self) -> String {
        let mut shown = self.input.clone();
        // A block *at* the cursor rather than after it, so it sits over the
        // character it would replace — and at the end there is nothing to sit
        // over, hence the trailing space.
        shown.insert(self.cursor.min(shown.len()), '\u{2588}');

        format!("{} {}", self.label, shown.iter().collect::<String>())
    }

    fn insert(&mut self, ch: char) {
        self.input.insert(self.cursor.min(self.input.len()), ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }
}

/// A message shown on the status line instead of returned to a caller.
struct StatusMessage {
    text: String,
    shown_for: Duration,
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
    /// Where this session is listening, so `rename-session` can move it.
    session_socket: Option<crate::session::server::SocketPath>,
    resize_mode: ResizeMode,
    /// What each pane is running and when it last spoke, for the agent
    /// indicators in the status bar.
    agents: AgentTracker,
    /// When the foreground jobs were last polled. Reading `/proc` for every
    /// pane is cheap but not free, and a job changes far more slowly than a
    /// frame is drawn.
    agents_polled: std::time::Instant,
    backend: BoxedBackend,
    output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: mpsc::Receiver<BackendEvent>,
    sink: OutputSink,
    session_rx: Option<mpsc::Receiver<SessionEvent>>,
    /// Every terminal watching this session.
    ///
    /// Empty when running locally, where `sink` writes straight to stdout.
    clients: Vec<AttachedClient>,
    queue_buf: Vec<u8>,
    diff: DiffRenderer,
    timeline: Timeline,
    keymap: Keymap,
    options: Options,
    /// A message to show on the status line, and how long it has been up.
    message: Option<StatusMessage>,
    /// An open `command-prompt`, if one is taking input.
    prompt: Option<Prompt>,
    /// Callers parked on a `wait-for` channel, by channel name.
    ///
    /// A waiting request does not reply until someone signals it, so the reply
    /// channel is held here instead of being answered.
    wait_channels: HashMap<String, Vec<tokio::sync::oneshot::Sender<CommandResult>>>,
    /// Which key table the next keypress is looked up in.
    ///
    /// `root` normally; the prefix key switches it to `prefix` for one key,
    /// or for as long as repeating bindings keep firing.
    key_table: String,
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
    /// `-d`: detach every other client as this one attaches.
    pub detach_others: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecArgs {
    pub session_name: Option<String>,
    pub command: Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchArgs {
    Run(Args),
    /// `wv has-session [name]`: exit 0 if it is live, 1 if not.
    ///
    /// Answered without connecting, because the whole point is to ask about a
    /// session that may not be there.
    HasSession { session_name: Option<String> },
    /// `wv kill-server`: end every live session.
    KillServer,
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
        let mut detach_others = false;

        for arg in args {
            let arg = arg.as_ref();
            if arg == "-d" {
                detach_others = true;
                continue;
            }
            if arg.starts_with('-') {
                bail!("`wv attach` does not accept `{arg}`; use `wv attach [-d] [name]`");
            }
            if session_name.replace(arg.to_owned()).is_some() {
                bail!("`wv attach` accepts at most one session name");
            }
        }

        Ok(Self {
            session_name,
            detach_others,
        })
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
            Some("ls" | "list-sessions") => {
                if let Some(arg) = args.get(1) {
                    bail!("`wv ls` does not accept `{arg}`");
                }
                return Ok(Self::ListSessions);
            }
            Some("has-session" | "has") => {
                let session_name = match args.get(1).map(String::as_str) {
                    // `-t name` is tmux's spelling; a bare name is ours.
                    Some("-t") => args.get(2).cloned().context(
                        "missing value for `-t`; expected a weave session name",
                    )?,
                    Some(name) if !name.starts_with('-') => name.to_owned(),
                    Some(other) => bail!("`wv has-session` does not accept `{other}`"),
                    None => {
                        return Ok(Self::HasSession { session_name: None });
                    }
                };
                validate_session_name(&session_name)?;
                return Ok(Self::HasSession {
                    session_name: Some(session_name),
                });
            }
            Some("kill-server") => {
                if let Some(arg) = args.get(1) {
                    bail!("`wv kill-server` does not accept `{arg}`");
                }
                return Ok(Self::KillServer);
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
            zoomed: None,
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
            session_socket: None,
            resize_mode: ResizeMode::Normal,
            agents: AgentTracker::default(),
            agents_polled: std::time::Instant::now(),
            backend: backend_parts.backend,
            output_rx: backend_parts.output_rx,
            event_rx: backend_parts.event_rx,
            sink: OutputSink::stdout(),
            session_rx: None,
            clients: Vec::new(),
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
            timeline: Timeline::new(),
            keymap: config.keymap,
            options: config.options,
            message: None,
            prompt: None,
            wait_channels: HashMap::new(),
            key_table: ROOT_TABLE.to_owned(),
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

    /// Tell the session where it is listening, so it can rename itself.
    pub fn with_session_socket(
        mut self,
        socket: crate::session::server::SocketPath,
    ) -> Self {
        self.session_socket = Some(socket);
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
            // Geometric neighbour of the focused pane, the same one directional
            // focus would move to. With nothing in that direction there is no
            // pane to name, so the command fails rather than picking one.
            Some(PaneRef::DirectionOf(direction)) => {
                let from = focused.unwrap_or(panes[0]);
                self.workspaces[workspace]
                    .root
                    .as_ref()
                    .and_then(|root| root.focus_neighbor(from, *direction))
                    .ok_or_else(|| Rejected::new("no pane in that direction".to_owned()))?
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

    /// Keep the agent indicators current.
    ///
    /// Two things move them: a pane's foreground job changing, which is polled,
    /// and an agent going quiet, which no event announces — so while any agent
    /// is inside its activity window the bar is kept redrawing, or the moment
    /// it turns grey would wait for unrelated output.
    async fn tick_agents(&mut self) {
        if !self.options.flag("agent-status") || !self.is_watched() {
            return;
        }

        let now = std::time::Instant::now();
        if now.duration_since(self.agents_polled) >= AGENT_POLL_INTERVAL {
            self.agents_polled = now;
            if self.poll_agent_commands().await {
                self.dirty = true;
            }
        }

        let window = self.agent_activity_window();
        if self
            .pane_ids()
            .iter()
            .any(|pane| self.agents.is_active(*pane, now, window))
        {
            self.dirty = true;
        }
    }

    /// How long after its last output an agent still counts as working.
    fn agent_activity_window(&self) -> Duration {
        Duration::from_millis(
            self.options
                .number("agent-activity-time")
                .unwrap_or(2_000),
        )
    }

    /// Re-read which pane is running what.
    ///
    /// Returns whether anything changed, so a pane that has just started or
    /// finished an agent redraws the bar even on an otherwise still frame.
    async fn poll_agent_commands(&mut self) -> bool {
        let mut changed = false;

        for pane in self.pane_ids() {
            let command = self
                .backend
                .pane_foreground_name(pane)
                .await
                .unwrap_or_default();
            if command.as_deref() != self.agents.foreground(pane) {
                self.agents.set_foreground(pane, command);
                changed = true;
            }
        }

        changed
    }

    /// Every agent running in this session, grouped by kind.
    ///
    /// The whole session rather than the current window: knowing an agent in
    /// window 3 has stopped is the point, and you cannot see window 3.
    ///
    /// Kinds are ordered as `agent-commands` names them and numbered from one
    /// within each kind, so two claudes read as `1:claude 2:claude` and sit
    /// together. The order is fixed rather than discovery-based so the bar's
    /// layout holds still as agents start and stop, leaving colour as the only
    /// thing that moves.
    fn agent_indicators(&self) -> Vec<chrome::AgentIndicator> {
        if !self.options.flag("agent-status") {
            return Vec::new();
        }

        let names = agent::parse_list(
            self.options
                .get("agent-commands")
                .unwrap_or_default(),
        );
        let patterns = agent::parse_list(
            self.options
                .get("agent-waiting-patterns")
                .unwrap_or_default(),
        );
        let window = self.agent_activity_window();
        let now = std::time::Instant::now();

        let mut found = Vec::new();
        for workspace in &self.workspaces {
            for pane in workspace.leaf_panes() {
                let Some(command) = self.agents.foreground(pane) else {
                    continue;
                };
                let Some(rank) = agent::agent_rank(command, &names) else {
                    continue;
                };

                let asking = self.pane(pane).is_some_and(|pane| {
                    agent::looks_like_a_question(&pane.capture_lines(), &patterns)
                });
                found.push((rank, self.agents.state(pane, now, window, asking)));
            }
        }

        // Stable, so panes of one kind keep the order they were found in.
        found.sort_by_key(|(rank, _)| *rank);

        let mut seen = 0_u8;
        let mut previous = None;
        found
            .into_iter()
            .map(|(rank, state)| {
                if previous != Some(rank) {
                    previous = Some(rank);
                    seen = 0;
                }
                seen = seen.saturating_add(1);
                chrome::AgentIndicator {
                    index: seen,
                    name: names[rank].clone(),
                    state,
                }
            })
            .collect()
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
            self.fit_pane_to_window(pane_id, root_rect).await?;
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
            if let Some(rect) = self.leaf_rect_target(pane).map(Rect::content) {
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
                    // Output is the signal the agent indicators run on.
                    self.agents.note_output(id, std::time::Instant::now());
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
            self.detach_clients(None, ServerToClient::Exit(ExitReason::ServerShutdown))
                .await;
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
                self.report_error(&message);
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

    /// Surface a non-fatal problem to everyone attached.
    fn report_error(&mut self, message: &str) {
        for client in &self.clients {
            let _ = client
                .frames
                .send(ServerToClient::Error(message.to_owned()));
        }
    }

    pub fn current_layout_root(&self) -> Option<&Node> {
        self.current().root.as_ref()
    }

    /// What sits at the far left of the status bar.
    ///
    /// A prompt outranks a message, which outranks the session name — all
    /// three share the slot, which is where tmux puts them. The name is what
    /// is there the rest of the time, so a renamed session shows its new name
    /// on the next frame.
    ///
    /// Returns an owned string: the caller needs it alive while it holds a
    /// mutable borrow of the back surface, and one short allocation per frame
    /// is nothing beside the per-pane surfaces the compositor already builds.
    fn status_left(&self) -> String {
        if let Some(prompt) = self.prompt.as_ref() {
            return prompt.render();
        }
        if let Some(message) = self.message.as_ref() {
            return message.text.clone();
        }

        self.session_name
            .clone()
            .unwrap_or_else(|| UNNAMED_SESSION.to_owned())
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
            compositor::ComposeOptions {
                pane_titles: self.pane_titles,
                zoomed: self.workspaces[self.current_workspace].zoomed,
            },
        );
        surface
    }

    pub async fn advance_animations_by(&mut self, dt: Duration) -> anyhow::Result<()> {
        self.advance_animations(dt).await
    }

    /// Run a command, returning whatever it printed.
    ///
    /// Most commands print nothing and return an empty string; only
    /// `display-message -p` and the listings have output.
    ///
    /// This is long because it is a dispatch table: one arm per command, each
    /// a few lines. Related groups already live in `execute_settings`,
    /// `execute_reshape` and `execute_outside`; splitting the rest further
    /// would scatter the mapping rather than clarify it.
    #[allow(clippy::too_many_lines)]
    async fn execute_now(&mut self, cmd: Command) -> Result<String, ExecuteError> {
        match cmd {
            Command::SplitWindow {
                split,
                target,
                command,
                detached,
                size,
            } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                self.switch_workspace(workspace).await?;
                self.split_pane(pane, split, command.as_ref(), detached, size)
                    .await?;
            }
            // Bindings and options are settings rather than actions, so they
            // share a method the way the reshaping commands do.
            settings @ (Command::BindKey { .. }
            | Command::UnbindKey { .. }
            | Command::ListKeys { .. }
            | Command::SetOption { .. }
            | Command::ShowOptions { .. }) => return self.execute_settings(settings),
            // Everything that reshapes an existing window shares a tail, so
            // it lives in one place rather than four arms of this match.
            reshape @ (Command::ResizePane { .. }
            | Command::SwapPane { .. }
            | Command::RotateWindow { .. }
            | Command::SelectLayout { .. }) => self.execute_reshape(reshape).await?,
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
            Command::KillPane {
                target,
                all_but_target,
            } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                self.switch_workspace(workspace).await?;
                if all_but_target {
                    // Close everything else, leaving the target alone.
                    for other in self.current().leaf_panes() {
                        if other != pane {
                            self.close_pane(other).await?;
                        }
                    }
                } else {
                    self.close_pane(pane).await?;
                }
            }
            // Moving panes between windows and running shell commands are
            // their own concerns; they live together rather than swelling this
            // match further.
            outside @ (Command::BreakPane { .. }
            | Command::JoinPane { .. }
            | Command::RunShell { .. }
            | Command::IfShell { .. }) => return self.execute_outside(outside).await,
            Command::WaitFor { .. } => {
                // Handled before dispatch: waiting parks the caller's reply
                // channel rather than answering, which `execute_now` cannot do.
                return Err(Rejected::new(
                    "`wait-for` needs a caller to answer; run it through `wv exec`".to_owned(),
                )
                .into());
            }
            Command::DetachClient { target, all } => {
                self.detach_for_command(target.as_deref(), all).await;
            }
            Command::RefreshClient => self.force_full_repaint(),
            Command::CommandPrompt {
                prompt,
                initial,
                template,
            } => {
                self.open_prompt(prompt, initial.as_deref(), template).await?;
            }
            Command::RenameSession { target, name } => {
                self.check_session(&target)?;
                self.rename_session(&name)?;
            }
            Command::KillSession { target } => {
                self.check_session(&target)?;
                self.exit = ExitState::Quit;
            }
            Command::DisplayMessage {
                message,
                target,
                print,
            } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                let vars = self.pane_variables(workspace, pane).await;
                let text = expand_format(&message, &vars)?;
                if print {
                    return Ok(text);
                }
                // Without `-p` the message belongs on screen, not in the reply.
                self.show_message(text);
            }
            Command::CapturePane { target, start, end } => {
                let (_, pane) = self.resolve_pane(&target)?;
                return self.capture_pane(pane, start, end);
            }
            Command::List { scope, format } => {
                return self.list(&scope, format.as_deref()).await;
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

        // The pane has no slot in the tree yet, so start it at the size a lone
        // pane would get. Whatever places it corrects this.
        let initial = self.root_rect().content();
        if resize_immediately {
            self.resize_pane(pane_id, initial.w, initial.h)
                .await
                .context("failed to resize shell pane")?;
        }

        self.panes.push(Pane::new(pane_id, initial.w, initial.h));

        Ok(pane_id)
    }

    /// Detach whichever clients `detach-client` named.
    ///
    /// With no target it detaches everyone, because a command arriving over a
    /// socket has no client of its own to mean. A keybinding routes through
    /// the same path, which is why `Alt+D` in a shared session detaches every
    /// terminal rather than guessing at one.
    async fn detach_for_command(&mut self, target: Option<&str>, all: bool) {
        // Running locally with nobody watching over a socket, there is nothing
        // to detach *from*, so detaching is quitting.
        if self.clients.is_empty() && self.session_rx.is_none() {
            tracing::warn!("detach requires a weave session server; quitting");
            self.exit = ExitState::Quit;
            return;
        }

        match target.and_then(|target| target.parse::<u64>().ok()) {
            Some(id) if !all => {
                if !self.drop_client(id, Some(ServerToClient::Detached)) {
                    tracing::warn!("no client {id} to detach");
                }
                self.renegotiate_size().await;
            }
            // `-a` keeps the named client and drops the rest.
            Some(id) => {
                self.detach_clients(Some(id), ServerToClient::Detached)
                    .await;
            }
            None => {
                self.detach_clients(None, ServerToClient::Detached).await;
            }
        }
    }

    /// Add a client, and resize the session to fit everyone watching.
    ///
    /// Nobody is evicted: several terminals can watch one session, each
    /// getting its own diff stream.
    pub async fn attach_client(
        &mut self,
        id: u64,
        cols: u16,
        rows: u16,
        truecolor: bool,
        frames: mpsc::UnboundedSender<ServerToClient>,
    ) {
        self.clients
            .push(AttachedClient::new(id, cols, rows, truecolor, frames));
        self.renegotiate_size().await;

        // A joining client sees a settled layout, not the tail of whatever
        // animation happened to be in flight.
        if let Err(error) = self.snap_workspace_tweens(self.current_workspace).await {
            tracing::warn!("failed to settle layout on attach: {error:#}");
        }
        self.recompute_layout();
        self.force_full_repaint();
    }

    /// Say goodbye to a client and drop it.
    fn drop_client(&mut self, id: u64, farewell: Option<ServerToClient>) -> bool {
        let Some(index) = self.clients.iter().position(|client| client.id == id) else {
            return false;
        };

        let client = self.clients.remove(index);
        if let Some(farewell) = farewell {
            let _ = client.frames.send(farewell);
        }

        true
    }

    /// Detach clients, optionally all but one.
    ///
    /// `except` keeps the client that asked, which is what `attach -d` and a
    /// keybinding both want: everyone else goes, you stay.
    async fn detach_clients(&mut self, except: Option<u64>, farewell: ServerToClient) {
        let ids: Vec<u64> = self
            .clients
            .iter()
            .map(|client| client.id)
            .filter(|id| Some(*id) != except)
            .collect();

        for id in ids {
            self.drop_client(id, Some(farewell.clone()));
        }
        self.renegotiate_size().await;
    }

    /// Resize the session to the smallest attached terminal.
    ///
    /// A session can only be as big as the smallest window watching it —
    /// anything larger would be cut off for somebody. Clients with more room
    /// see the session in their top-left corner, as in tmux.
    async fn renegotiate_size(&mut self) {
        let Some((cols, rows)) = self
            .clients
            .iter()
            .map(|client| (client.cols, client.rows))
            .reduce(|(w, h), (cols, rows)| (w.min(cols), h.min(rows)))
        else {
            // Nobody is watching; keep the last size so panes are undisturbed.
            return;
        };

        self.resize_to(cols, rows).await;
    }

    /// Force every client's next frame to redraw its whole screen.
    fn force_full_repaint(&mut self) {
        self.front = Surface::new(self.back.width, self.back.height);
        for client in &mut self.clients {
            client.needs_full_repaint = true;
        }
        // The local path clears its own terminal; a client is told to by the
        // escape sequence its repaint starts with.
        if !self.sink.is_attached() {
            self.dirty = true;
            return;
        }
        let _ = crossterm::queue!(
            self.sink,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        );
        self.dirty = true;
    }

    /// Whether anything is watching, locally or over a socket.
    fn is_watched(&self) -> bool {
        self.sink.is_attached() || !self.clients.is_empty()
    }

    /// Send this frame to every client, each diffed against its own screen.
    fn flush_clients(&mut self) -> anyhow::Result<()> {
        let back = &self.back;

        for client in &mut self.clients {
            client.buf.clear();

            if client.needs_full_repaint {
                client.front = Surface::new(back.width, back.height);
                let _ = crossterm::queue!(
                    client.buf,
                    crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                    crossterm::cursor::MoveTo(0, 0)
                );
                client.needs_full_repaint = false;
            }

            client.diff.flush(&client.front, back, &mut client.buf)?;
            if !client.buf.is_empty() {
                let frame = ServerToClient::Frame(std::mem::take(&mut client.buf));
                // A send failure means the client is gone; `ClientGone` will
                // remove it, so there is nothing to do here.
                let _ = client.frames.send(frame);
            }

            // Each client needs its own copy of what it has now seen. The
            // single-client path could swap buffers; with several, the price
            // of correct per-client deltas is a copy each.
            client.front.clone_from(back);
        }

        Ok(())
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
                self.attach_client(id, cols, rows, truecolor, frames).await;
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
                // Answering every client is wrong, but the socket layer does
                // not yet tag input with its connection, so the request cannot
                // be attributed. `wv exec` is the path that matters and it
                // gets its reply on its own connection.
                if let Some(client) = self.clients.first() {
                    let _ = client.frames.send(ServerToClient::Reply { id, result });
                }
            }
            SessionEvent::Request { command, reply } => {
                // `wait-for` is the one command that does not answer straight
                // away: waiting parks the reply channel until someone signals.
                if let Command::WaitFor { channel, action } = &command {
                    match action {
                        WaitAction::Wait => {
                            self.wait_channels
                                .entry(channel.clone())
                                .or_default()
                                .push(reply);
                            return Ok(());
                        }
                        WaitAction::Signal => {
                            self.signal_wait_channel(channel);
                            let _ = reply.send(CommandResult::empty());
                            return Ok(());
                        }
                    }
                }

                let result = self.execute_request(command).await?;
                // A dropped receiver means the caller hung up mid-command. The
                // command still ran; there is simply nobody left to tell.
                if reply.send(result).is_err() {
                    tracing::debug!("command completed after its caller disconnected");
                }
            }
            SessionEvent::Message(ClientToServer::Detach) => {
                // The socket layer knows which connection said this; it
                // arrives tagged so only that client is detached.
                self.detach_clients(None, ServerToClient::Detached).await;
            }
            SessionEvent::Message(ClientToServer::Quit) => {
                self.detach_clients(None, ServerToClient::Exit(ExitReason::Quit))
                    .await;
                self.exit = ExitState::Quit;
            }
            SessionEvent::ClientGone { id } => {
                if self.drop_client(id, None) {
                    tracing::info!("client {id} connection closed");
                    self.renegotiate_size().await;
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
        size: Option<SplitSize>,
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
            // `-p`/`-l` size the *new* pane, which is the second half, so the
            // ratio kept by the original is one minus what was asked for.
            if let Some(size) = size {
                let extent = match split {
                    Split::Vertical => old_parent_rect.w,
                    Split::Horizontal => old_parent_rect.h,
                };
                let share = match size {
                    SplitSize::Percent(percent) => f32::from(percent.min(100)) / 100.0,
                    SplitSize::Cells(cells) if extent > 0 => {
                        f32::from(cells) / f32::from(extent)
                    }
                    SplitSize::Cells(_) => 0.5,
                };
                if let Some(root) = self.current_mut().root.as_mut() {
                    root.set_parent_ratio(new_pane, 1.0 - share);
                }
            }
            self.recompute_layout();
            self.start_open_tweens(focused, new_pane, split, old_parent_rect);
            self.dirty = true;
        }

        Ok(())
    }

    /// Commands that change bindings or options rather than the session.
    fn execute_settings(&mut self, cmd: Command) -> Result<String, ExecuteError> {
        match cmd {
            Command::BindKey {
                table,
                key,
                repeat,
                command,
            } => {
                let key = parse_binding_key(&key)
                    .ok_or_else(|| Rejected::new(format!("`{key}` is not a key name")))?;
                let bound = Command::parse(&command).map_err(|error| {
                    Rejected::new(format!("cannot bind that command: {error}"))
                })?;
                let binding = if repeat {
                    Binding::repeating(bound)
                } else {
                    Binding::new(bound)
                };
                self.keymap.bind(&table, key, binding);
            }
            Command::UnbindKey { table, key, all } => {
                if all {
                    self.keymap.unbind_all(&table);
                } else if let Some(key) = key {
                    let parsed = parse_binding_key(&key)
                        .ok_or_else(|| Rejected::new(format!("`{key}` is not a key name")))?;
                    if !self.keymap.unbind(&table, parsed) {
                        return Err(Rejected::new(format!(
                            "`{key}` is not bound in table `{table}`"
                        ))
                        .into());
                    }
                }
            }
            Command::ListKeys { table } => {
                let lines = self
                    .keymap
                    .all_bindings()
                    .into_iter()
                    .filter(|(name, _, _)| table.as_ref().map_or(true, |wanted| wanted == name))
                    .map(|(name, key, binding)| {
                        format!(
                            "bind-key -T {name} {}{}",
                            format_key(&key),
                            if binding.repeat { " -r" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>();
                return Ok(lines.join("\n"));
            }
            Command::SetOption { name, value, unset } => {
                return self.set_option(&name, &value, unset).map(|()| String::new());
            }
            Command::ShowOptions { name } => {
                return Ok(match name {
                    Some(name) => self
                        .options
                        .get(&name)
                        .ok_or_else(|| Rejected::new(format!("unknown option `{name}`")))?
                        .to_owned(),
                    None => self.options.show().join("\n"),
                });
            }
            other => unreachable!("execute_settings got {other:?}"),
        }

        Ok(String::new())
    }

    /// Commands that move panes between windows, or reach outside the session
    /// to the shell.
    async fn execute_outside(&mut self, cmd: Command) -> Result<String, ExecuteError> {
        match cmd {
            Command::BreakPane {
                source,
                target,
                name,
                detached,
            } => {
                let (workspace, pane) = self.resolve_pane(&source)?;
                self.break_pane(workspace, pane, &target, name, detached)
                    .await?;
            }
            Command::JoinPane {
                source,
                target,
                split,
                detached,
            } => {
                let (source_window, pane) = self.resolve_pane(&source)?;
                let (target_window, onto) = self.resolve_pane(&target)?;
                self.join_pane(source_window, pane, target_window, onto, split, detached)
                    .await?;
            }
            Command::RunShell {
                command,
                background,
            } => return self.run_shell(&command, background).await,
            Command::IfShell {
                condition,
                then_command,
                else_command,
                background,
            } => {
                let succeeded = self.shell_status(&condition, background).await?;
                let branch = if succeeded {
                    Some(then_command)
                } else {
                    else_command
                };
                if let Some(branch) = branch {
                    let command = Command::parse(&branch).map_err(|error| {
                        Rejected::new(format!("`if-shell` branch does not parse: {error}"))
                    })?;
                    return Box::pin(self.execute_now(command)).await;
                }
            }
            other => unreachable!("execute_outside got {other:?}"),
        }

        Ok(String::new())
    }

    /// Commands that rearrange a window without adding or removing panes.
    async fn execute_reshape(&mut self, cmd: Command) -> Result<(), ExecuteError> {
        match cmd {
            Command::ResizePane { target, change } => {
                let (workspace, pane) = self.resolve_pane(&target)?;
                self.switch_workspace(workspace).await?;
                self.resize_pane_command(pane, change)?;
            }
            Command::SwapPane {
                source,
                target,
                keep_focus,
            } => {
                let (workspace, first) = self.resolve_pane(&source)?;
                let (other_workspace, second) = self.resolve_pane(&target)?;
                if workspace != other_workspace {
                    return Err(Rejected::new(
                        "swap-pane needs both panes in the same window".to_owned(),
                    )
                    .into());
                }
                self.switch_workspace(workspace).await?;
                self.swap_panes(first, second, keep_focus);
            }
            Command::RotateWindow { target, reverse } => {
                let workspace = self.resolve_window(&target)?;
                self.switch_workspace(workspace).await?;
                self.rotate_window(reverse);
            }
            Command::SelectLayout { target, layout } => {
                let workspace = self.resolve_window(&target)?;
                self.switch_workspace(workspace).await?;
                self.apply_layout(layout)?;
            }
            other => unreachable!("execute_reshape got {other:?}"),
        }

        Ok(())
    }

    /// Apply an option, honouring the live ones and warning about the rest.
    fn set_option(&mut self, name: &str, value: &str, unset: bool) -> Result<(), ExecuteError> {
        let value = if unset {
            crate::config::options::spec(name)
                .map(|spec| spec.default)
                .unwrap_or_default()
        } else {
            value
        };

        let spec = self
            .options
            .set(name, value)
            .map_err(|error| Rejected::new(error.to_string()))?;

        match spec.status {
            crate::config::options::OptionStatus::Live => self.apply_live_option(spec.name),
            // Accepted so a real `.tmux.conf` loads, but say so once rather
            // than letting the user wonder why nothing changed.
            crate::config::options::OptionStatus::Inert(reason) => {
                tracing::warn!("`{name}` is accepted but does nothing: {reason}");
            }
        }

        Ok(())
    }

    /// Push an option's new value into the state that reads it.
    fn apply_live_option(&mut self, name: &str) {
        match name {
            "prefix" | "prefix2" => {
                let keys = ["prefix", "prefix2"]
                    .into_iter()
                    .filter_map(|option| self.options.get(option))
                    .filter(|value| !value.is_empty())
                    .filter_map(parse_binding_key)
                    .collect::<Vec<_>>();
                self.keymap.set_prefix(&keys);
            }
            "status" => self.status_bar = self.options.flag("status"),
            "pane-border-status" => self.pane_titles = self.options.flag("pane-border-status"),
            "target-fps" => {
                if let Some(fps) = self.options.number("target-fps") {
                    let fps = u16::try_from(fps).unwrap_or(crate::config::DEFAULT_TARGET_FPS);
                    self.tick_interval = frame_interval(fps);
                }
            }
            // `repeat-time`, `default-shell` and `automatic-rename` are read
            // where they are used rather than cached here.
            _ => {}
        }
        self.dirty = true;
    }

    /// Move a pane into a window of its own.
    async fn break_pane(
        &mut self,
        workspace: usize,
        pane: PaneId,
        target: &Target,
        name: Option<String>,
        detached: bool,
    ) -> Result<(), ExecuteError> {
        if self.workspaces[workspace].leaf_panes().len() < 2 {
            return Err(Rejected::new(
                "that pane is the only one in its window; there is nothing to break out"
                    .to_owned(),
            )
            .into());
        }

        let destination = if target.window.is_some() {
            let requested = self.resolve_window(target)?;
            if !self.workspaces[requested].is_empty() {
                return Err(Rejected::new(format!(
                    "window {} already exists",
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

        self.detach_pane_from_window(workspace, pane);
        let rect = self.root_rect();
        let window = &mut self.workspaces[destination];
        window.name = name;
        window.root = Some(Node::Leaf {
            pane,
            rect_current: FRect::from(rect),
            rect_target: rect,
        });
        window.set_focus(Some(pane));

        if detached {
            self.fit_pane_to_window(pane, rect).await?;
            self.recompute_layout();
        } else {
            self.switch_workspace(destination).await?;
        }
        self.dirty = true;

        Ok(())
    }

    /// Move a pane out of its window and split it into another.
    async fn join_pane(
        &mut self,
        source_window: usize,
        pane: PaneId,
        target_window: usize,
        onto: PaneId,
        split: Split,
        detached: bool,
    ) -> Result<(), ExecuteError> {
        if pane == onto {
            return Err(Rejected::new("cannot join a pane to itself".to_owned()).into());
        }
        if self.workspaces[source_window].leaf_panes().len() < 2 && source_window == target_window
        {
            return Err(
                Rejected::new("that pane is already the whole window".to_owned()).into(),
            );
        }

        self.detach_pane_from_window(source_window, pane);
        self.switch_workspace(target_window).await?;

        // Split the destination and put the moved pane in the new half rather
        // than spawning a fresh one.
        let Some(old_parent_rect) = self.leaf_rect_target(onto) else {
            return Err(Rejected::new("the destination pane has no layout".to_owned()).into());
        };
        if let Some(root) = self.current_mut().root.as_mut() {
            root.split_focused(onto, split, pane);
        }
        if !detached {
            self.current_mut().set_focus(Some(pane));
        }
        self.recompute_layout();
        self.start_open_tweens(onto, pane, split, old_parent_rect);
        self.dirty = true;

        Ok(())
    }

    /// Take a pane out of a window's tree without killing it.
    ///
    /// The pane keeps running and keeps its `%N`; only its place in the layout
    /// goes away, which is what makes break and join moves rather than
    /// kill-and-respawn.
    fn detach_pane_from_window(&mut self, workspace: usize, pane: PaneId) {
        let window = &mut self.workspaces[workspace];
        let emptied = window
            .root
            .as_mut()
            .is_some_and(|root| !root.close(pane) || root.leaves().is_empty());
        if emptied {
            window.root = None;
        }
        window.closing.remove(&pane);
        if window.focused == Some(pane) {
            let next = window.root.as_ref().and_then(first_leaf_pane);
            window.focused = next;
        }
        if window.last_focused == Some(pane) {
            window.last_focused = None;
        }
        if window.zoomed == Some(pane) {
            window.zoomed = None;
        }

        if workspace == self.current_workspace {
            self.recompute_layout();
        }
    }

    /// Run a shell command outside any pane, returning its output.
    async fn run_shell(
        &mut self,
        command: &str,
        background: bool,
    ) -> Result<String, ExecuteError> {
        let mut child = tokio::process::Command::new(shell_program());
        child.arg("-c").arg(command);

        if background {
            // Detached: the caller gets an immediate reply and the command
            // outlives it. This is what makes `wait-for` useful.
            child
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            child
                .spawn()
                .map_err(|error| Rejected::new(format!("failed to run `{command}`: {error}")))?;
            return Ok(String::new());
        }

        let output = child
            .output()
            .await
            .map_err(|error| Rejected::new(format!("failed to run `{command}`: {error}")))?;

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            return Err(Rejected::new(text.trim_end().to_owned()).into());
        }

        Ok(text.trim_end().to_owned())
    }

    /// Whether a shell command succeeded, for `if-shell`.
    async fn shell_status(
        &mut self,
        command: &str,
        background: bool,
    ) -> Result<bool, ExecuteError> {
        if background {
            // tmux's `-b` makes the *test* asynchronous; weave runs it inline
            // and only skips waiting on the branch, which is the part that
            // could block the render tick.
            tracing::debug!("if-shell -b runs its condition inline");
        }

        let status = tokio::process::Command::new(shell_program())
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|error| Rejected::new(format!("failed to run `{command}`: {error}")))?;

        Ok(status.success())
    }

    /// Release everyone waiting on a channel.
    fn signal_wait_channel(&mut self, channel: &str) {
        let waiters = self.wait_channels.remove(channel).unwrap_or_default();
        let count = waiters.len();
        for waiter in waiters {
            let _ = waiter.send(CommandResult::empty());
        }
        tracing::debug!("signalled `{channel}`, releasing {count} waiter(s)");
    }

    /// Give this session a new name, moving its socket to match.
    ///
    /// Renaming moves the listening socket, which established connections do
    /// not care about: a Unix socket connection survives its path changing, so
    /// every attached client keeps rendering through the rename. Only new
    /// connections use the new name.
    fn rename_session(&mut self, name: &str) -> Result<(), ExecuteError> {
        crate::session::paths::validate_session_name(name)
            .map_err(|error| Rejected::new(error.to_string()))?;

        // Already called that: nothing to move, and nothing to complain about
        // either — a script that sets a name unconditionally should not fail
        // the second time it runs.
        if self.session_name.as_deref() == Some(name) {
            return Ok(());
        }

        let Some(socket) = self.session_socket.clone() else {
            return Err(Rejected::new(
                "this weave is not running as a session, so it has no name".to_owned(),
            )
            .into());
        };

        let destination = crate::session::paths::socket_path(name)
            .map_err(|error| Rejected::new(error.to_string()))?;
        if crate::session::paths::is_socket_live(&destination) {
            return Err(Rejected::new(format!(
                "a weave session named `{name}` is already running"
            ))
            .into());
        }

        let current = socket.get();
        std::fs::rename(&current, &destination).map_err(|error| {
            Rejected::new(format!(
                "failed to move the session socket to {}: {error}",
                destination.display()
            ))
        })?;

        // The guard unlinks whatever the socket is called now, so it has to
        // learn the new path or shutdown would leave the renamed one behind.
        socket.set(destination);
        let previous = self.session_name.replace(name.to_owned());
        tracing::info!(
            "session renamed from {} to {name}",
            previous.as_deref().unwrap_or("(unnamed)")
        );
        self.dirty = true;

        Ok(())
    }

    /// Open a prompt, prefilled with `initial` expanded as a format string.
    async fn open_prompt(
        &mut self,
        label: Option<String>,
        initial: Option<&str>,
        template: String,
    ) -> Result<(), ExecuteError> {
        let input: Vec<char> = match initial {
            Some(initial) => {
                let (workspace, pane) = self.resolve_pane(&Target::current())?;
                let vars = self.pane_variables(workspace, pane).await;
                expand_format(initial, &vars)?.chars().collect()
            }
            None => Vec::new(),
        };

        self.prompt = Some(Prompt {
            label: label.unwrap_or_else(|| ":".to_owned()),
            cursor: input.len(),
            input,
            template,
        });
        self.dirty = true;

        Ok(())
    }

    /// Feed a key to the open prompt.
    ///
    /// Returns whether the key was consumed — everything is, while a prompt is
    /// open, so a stray keystroke cannot reach the pane behind it.
    async fn handle_prompt_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> anyhow::Result<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};

        let Some(prompt) = self.prompt.as_mut() else {
            return Ok(false);
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Enter => {
                let prompt = self.prompt.take().expect("checked above");
                self.dirty = true;
                self.run_prompt_template(&prompt).await?;
            }
            // Escape, C-c and C-g all cancel, which are the three things
            // people reach for.
            KeyCode::Esc => {
                self.prompt = None;
                self.dirty = true;
            }
            KeyCode::Char('c' | 'g') if ctrl => {
                self.prompt = None;
                self.dirty = true;
            }
            KeyCode::Char('u') if ctrl => {
                prompt.input.clear();
                prompt.cursor = 0;
                self.dirty = true;
            }
            KeyCode::Backspace => {
                prompt.backspace();
                self.dirty = true;
            }
            KeyCode::Delete => {
                prompt.delete();
                self.dirty = true;
            }
            KeyCode::Left => {
                prompt.cursor = prompt.cursor.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Right => {
                prompt.cursor = (prompt.cursor + 1).min(prompt.input.len());
                self.dirty = true;
            }
            KeyCode::Home => {
                prompt.cursor = 0;
                self.dirty = true;
            }
            KeyCode::End => {
                prompt.cursor = prompt.input.len();
                self.dirty = true;
            }
            KeyCode::Char(ch) if !ctrl => {
                prompt.insert(ch);
                self.dirty = true;
            }
            // Anything else is swallowed rather than passed through: a prompt
            // is modal.
            _ => {}
        }

        Ok(true)
    }

    /// Substitute the typed text into the template and run it.
    async fn run_prompt_template(&mut self, prompt: &Prompt) -> anyhow::Result<()> {
        let typed = prompt.text();
        if typed.is_empty() {
            // Submitting nothing cancels, rather than running a command with
            // an empty argument.
            return Ok(());
        }

        // Substitute per word, so a `%%` standing alone becomes exactly one
        // argument however many spaces were typed into it.
        let argv: Vec<String> = prompt
            .template
            .split_whitespace()
            .map(|word| {
                if word == "%%" {
                    typed.clone()
                } else {
                    word.replace("%%", &typed)
                }
            })
            .collect();

        match Command::parse(&argv) {
            Ok(command) => self.execute(command).await,
            Err(error) => {
                self.show_message(format!("{error}"));
                Ok(())
            }
        }
    }

    /// Put a message on the status line for a while.
    fn show_message(&mut self, text: String) {
        tracing::info!("message: {text}");
        self.message = Some(StatusMessage {
            text,
            shown_for: Duration::ZERO,
        });
        self.dirty = true;
    }

    /// Age out a status message once it has had its time.
    fn advance_message(&mut self, dt: Duration) {
        let Some(message) = self.message.as_mut() else {
            return;
        };
        message.shown_for += dt;
        if message.shown_for >= MESSAGE_DURATION {
            self.message = None;
            self.dirty = true;
        }
    }

    /// Read a pane's visible screen.
    fn capture_pane(
        &self,
        pane: PaneId,
        start: Option<u16>,
        end: Option<u16>,
    ) -> Result<String, ExecuteError> {
        let Some(target) = self.pane(pane) else {
            return Err(Rejected::new("that pane is gone".to_owned()).into());
        };

        let lines = target.capture_lines();
        let first = usize::from(start.unwrap_or(0));
        let last = end.map_or(lines.len(), |end| usize::from(end) + 1);

        if first > lines.len() {
            // Asking past the end returns nothing rather than failing: a
            // script polling a quiet pane should see "" and carry on.
            return Ok(String::new());
        }

        Ok(lines[first..last.min(lines.len())].join("\n"))
    }

    /// Format one line per pane, window or session.
    async fn list(
        &mut self,
        scope: &ListScope,
        format: Option<&str>,
    ) -> Result<String, ExecuteError> {
        let (template, rows) = match scope {
            ListScope::Panes { target, all } => {
                let template = format.unwrap_or(DEFAULT_PANE_FORMAT);
                let windows = if *all {
                    (0..WORKSPACE_COUNT)
                        .filter(|&index| !self.workspaces[index].is_empty())
                        .collect()
                } else {
                    vec![self.resolve_window(target)?]
                };

                let mut rows = Vec::new();
                for window in windows {
                    for pane in self.workspaces[window].leaf_panes() {
                        rows.push(self.pane_variables(window, pane).await);
                    }
                }
                (template, rows)
            }
            ListScope::Windows { target } => {
                self.check_session(target)?;
                let template = format.unwrap_or(DEFAULT_WINDOW_FORMAT);
                let rows = (0..WORKSPACE_COUNT)
                    .filter(|&index| !self.workspaces[index].is_empty())
                    .map(|index| self.window_variables(index))
                    .collect();
                (template, rows)
            }
            ListScope::Sessions => {
                let template = format.unwrap_or(DEFAULT_SESSION_FORMAT);
                (template, vec![self.session_variables()])
            }
        };

        let mut lines = Vec::with_capacity(rows.len());
        for row in &rows {
            lines.push(expand_format(template, row)?);
        }

        Ok(lines.join("\n"))
    }

    /// The variables describing this session.
    fn session_variables(&self) -> Variables {
        let mut vars = Variables::new();
        vars.set(
            "session_name",
            self.session_name.clone().unwrap_or_default(),
        )
        .set("session_windows", self.occupied_workspaces().len().to_string())
        .set("session_clients", self.clients.len().to_string())
        .set_flag("session_attached", !self.clients.is_empty());

        vars
    }

    /// The variables describing one window, including its session's.
    fn window_variables(&self, window: usize) -> Variables {
        let mut vars = self.session_variables();
        let workspace = &self.workspaces[window];
        let is_current = window == self.current_workspace;

        vars.set("window_index", (window + 1).to_string())
            .set("window_name", self.window_name(window))
            .set("window_panes", workspace.pane_count().to_string())
            .set_flag("window_active", is_current)
            .set_flag("window_zoomed_flag", workspace.zoomed.is_some())
            // tmux's `#F`: `*` for the active window, `Z` when it is zoomed.
            .set(
                "window_flags",
                match (is_current, workspace.zoomed.is_some()) {
                    (true, true) => "*Z",
                    (true, false) => "*",
                    (false, true) => "Z",
                    (false, false) => "",
                },
            );

        vars
    }

    /// The variables describing one pane, including its window's.
    ///
    /// Takes `&mut self` because the pane's directory and process name are
    /// read from the backend rather than held in memory.
    async fn pane_variables(&mut self, window: usize, pane: PaneId) -> Variables {
        let cwd = self.backend.pane_cwd(pane).await.ok().flatten();
        let process = self.backend.pane_process_name(pane).await.ok().flatten();

        let mut vars = self.window_variables(window);
        let workspace = &self.workspaces[window];
        let index = workspace
            .leaf_panes()
            .iter()
            .position(|candidate| *candidate == pane)
            .unwrap_or(0);
        // `#{pane_width}` is what the process can draw on, not what the slot
        // takes up on screen.
        let rect = self
            .leaf_rect_target(pane)
            .map_or(
                Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                Rect::content,
            );

        vars.set(
            "pane_id",
            self.pane_number(pane)
                .map_or_else(String::new, |number| format!("%{number}")),
        )
        .set("pane_index", index.to_string())
        .set(
            "pane_title",
            self.pane(pane)
                .and_then(Pane::title)
                .unwrap_or_default()
                .to_owned(),
        )
        .set("pane_width", rect.w.to_string())
        .set("pane_height", rect.h.to_string())
        .set(
            "pane_current_path",
            cwd.map(|cwd| cwd.display().to_string()).unwrap_or_default(),
        )
        .set("pane_current_command", process.unwrap_or_default())
        .set_flag("pane_active", workspace.focused == Some(pane))
        .set_flag("pane_dead", self.pane(pane).is_none());

        vars
    }

    /// Apply a `resize-pane`, animating every pane the change moves.
    fn resize_pane_command(
        &mut self,
        pane: PaneId,
        change: ResizeChange,
    ) -> Result<(), ExecuteError> {
        if let ResizeChange::ToggleZoom = change {
            self.toggle_zoom(pane);
            return Ok(());
        }

        if self.current().zoomed.is_some() {
            return Err(Rejected::new(
                "cannot resize a zoomed pane; unzoom with `resize-pane -Z` first".to_owned(),
            )
            .into());
        }

        let applied = match change {
            ResizeChange::By { direction, cells } => self
                .current_mut()
                .root
                .as_mut()
                .is_some_and(|root| root.resize_leaf(pane, direction, cells)),
            ResizeChange::Width(cells) => self
                .current_mut()
                .root
                .as_mut()
                .is_some_and(|root| root.resize_leaf_to(pane, Split::Vertical, cells)),
            ResizeChange::Height(cells) => self
                .current_mut()
                .root
                .as_mut()
                .is_some_and(|root| root.resize_leaf_to(pane, Split::Horizontal, cells)),
            ResizeChange::ToggleZoom => unreachable!("zoom is handled above"),
        };

        if !applied {
            return Err(Rejected::new(
                "nothing to resize: the pane has no split on that axis".to_owned(),
            )
            .into());
        }

        self.animate_to_new_layout();

        Ok(())
    }

    /// Toggle a pane filling its window.
    ///
    /// The tree is left alone, so unzooming animates straight back to the
    /// layout that was there — nothing has to be remembered or rebuilt.
    fn toggle_zoom(&mut self, pane: PaneId) {
        let ws = self.current_mut();
        ws.zoomed = match ws.zoomed {
            Some(zoomed) if zoomed == pane => None,
            // Zooming a different pane moves the zoom rather than nesting it.
            _ => Some(pane),
        };
        if ws.zoomed.is_some() {
            ws.set_focus(Some(pane));
        }
        self.animate_to_new_layout();
    }

    fn swap_panes(&mut self, first: PaneId, second: PaneId, keep_focus: bool) {
        let swapped = self
            .current_mut()
            .root
            .as_mut()
            .is_some_and(|root| root.swap_leaves(first, second));
        if !swapped {
            return;
        }

        if !keep_focus {
            // Focus follows the pane, which is now where the other one was.
            self.current_mut().set_focus(Some(first));
        }
        self.animate_to_new_layout();
    }

    /// Move every pane one position around the window's layout.
    fn rotate_window(&mut self, reverse: bool) {
        let Some(mut rotated) = self
            .current()
            .root
            .as_ref()
            .map(tree::Node::leaf_placements)
        else {
            return;
        };
        if rotated.len() < 2 {
            return;
        }

        if reverse {
            rotated.rotate_right(1);
        } else {
            rotated.rotate_left(1);
        }

        // Write the rotated order back into the same layout positions.
        if let Some(root) = self.current_mut().root.as_mut() {
            let mut next = rotated.into_iter();
            root.set_leaves(&mut next);
        }
        self.animate_to_new_layout();
    }

    fn apply_layout(&mut self, layout: LayoutPreset) -> Result<(), ExecuteError> {
        let panes = self.current().leaf_panes();
        if panes.is_empty() {
            return Err(Rejected::new("window has no panes to lay out".to_owned()).into());
        }

        let rect = self.root_rect();
        let root = match layout {
            LayoutPreset::EvenHorizontal => tree::even_chain(&panes, Split::Vertical, rect),
            LayoutPreset::EvenVertical => tree::even_chain(&panes, Split::Horizontal, rect),
            LayoutPreset::MainVertical => {
                tree::main_and_stack(&panes, Split::Vertical, MAIN_PANE_RATIO, rect)
            }
            LayoutPreset::MainHorizontal => {
                tree::main_and_stack(&panes, Split::Horizontal, MAIN_PANE_RATIO, rect)
            }
            LayoutPreset::Tiled => tree::tiled(&panes, rect),
        };

        let Some(mut root) = root else {
            return Err(Rejected::new("could not build that layout".to_owned()).into());
        };

        // Start every pane where it is now so the rearrangement animates
        // rather than teleporting.
        for pane in &panes {
            if let Some(current) = self.leaf_rect_current(*pane) {
                if let Some(Node::Leaf { rect_current, .. }) = root.find_leaf_mut(*pane) {
                    *rect_current = current;
                }
            }
        }
        self.current_mut().root = Some(root);
        self.animate_to_new_layout();

        Ok(())
    }

    /// Recompute the layout and tween every pane from where it is to where it
    /// now belongs.
    ///
    /// This is the shared tail of every command that changes shape without
    /// adding or removing a pane.
    fn animate_to_new_layout(&mut self) {
        self.recompute_layout();

        let mut targets = Vec::new();
        if let Some(root) = self.current().root.as_ref() {
            collect_leaf_targets(root, &mut targets);
        }

        for (pane, target) in targets {
            let Some(from) = self.leaf_rect_current(pane) else {
                continue;
            };
            let to = FRect::from(target);
            if from == to {
                continue;
            }
            self.timeline.tween_leaf_rect(
                pane,
                from,
                to,
                RESIZE_DURATION,
                Easing::EaseOutCubic,
            );
        }

        self.dirty = true;
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

        // A prompt is modal: while one is open it takes every key, so typing a
        // window name cannot leak into the shell behind it.
        if self.handle_prompt_key(key).await? {
            return Ok(());
        }

        if self.handle_key_binding(key).await? {
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

    /// Route a keypress through the prefix state machine.
    ///
    /// Returns whether the key was consumed as a binding. Anything that falls
    /// through reaches the focused pane, which is what keeps typing working.
    async fn handle_key_binding(&mut self, key: crossterm::event::KeyEvent) -> anyhow::Result<bool> {
        // Waiting for the key after a prefix: look it up in the prefix table
        // and return to root afterwards, unless the binding repeats.
        if self.key_table != ROOT_TABLE {
            let table = std::mem::replace(&mut self.key_table, ROOT_TABLE.to_owned());

            if let Some(binding) = self.keymap.lookup(&table, &key).cloned() {
                if binding.repeat {
                    self.key_table = table;
                }
                self.dirty = true;
                self.execute(binding.command).await?;
                return Ok(true);
            }

            // An unbound key after the prefix is swallowed, as in tmux:
            // `C-b` then a typo must not leak the typo into the pane.
            tracing::debug!("no binding for {} in table `{table}`", format_key(&key));
            self.dirty = true;
            return Ok(true);
        }

        if self.keymap.is_prefix(&key) {
            PREFIX_TABLE.clone_into(&mut self.key_table);
            return Ok(true);
        }

        if let Some(binding) = self.keymap.lookup(ROOT_TABLE, &key).cloned() {
            self.dirty = true;
            self.execute(binding.command).await?;
            return Ok(true);
        }

        Ok(false)
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
        self.recompute_layout();

        // Every pane gets the size of its own slot in the new layout. Handing
        // them all the terminal size instead leaves each one drawing a screen
        // far wider than the slot it is blitted into.
        self.resize_mode = ResizeMode::HostResize;
        for pane_id in self.pane_ids() {
            let rect = self
                .leaf_rect_target(pane_id)
                .map_or_else(|| self.root_rect().content(), Rect::content);
            if let Some(pane) = self.pane_mut(pane_id) {
                pane.resize(rect.w, rect.h);
            }
            if let Err(error) = self.resize_pane(pane_id, rect.w, rect.h).await {
                tracing::warn!("failed to resize backend pane: {error:#}");
            }
        }
        self.resize_mode = ResizeMode::Normal;

        self.dirty = true;
    }

    async fn tick(&mut self, dt: Duration) -> anyhow::Result<()> {
        self.last_tick_dt = dt;
        self.advance_message(dt);
        self.advance_animations(dt).await?;
        self.tick_agents().await;

        // With nobody watching there is nothing to render for: keep pane state
        // and tweens current, but skip compositing entirely.
        if !self.dirty || !self.is_watched() {
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
            compositor::ComposeOptions {
                pane_titles: self.pane_titles,
                zoomed: self.workspaces[self.current_workspace].zoomed,
            },
        );
        if self.status_bar {
            let indicators = self.workspace_indicators();
            let agents = self.agent_indicators();
            let status_left = self.status_left();
            chrome::draw_status_bar(
                &mut self.back,
                &status_left,
                &indicators,
                &agents,
                chrono::Local::now(),
                self.theme,
            );
        }
        self.last_dirty_cells = self.estimated_dirty_cells();
        if self.debug.is_enabled() {
            let debug_overlay = self.debug_overlay();
            chrome::draw_debug_overlay(&mut self.back, debug_overlay);
        }
        if self.sink.is_attached() {
            // Running locally: one screen, and the buffers can be swapped
            // rather than copied.
            self.diff.flush(&self.front, &self.back, &mut self.sink)?;
            self.sink.flush()?;
            std::mem::swap(&mut self.front, &mut self.back);
        }
        self.flush_clients()?;

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
            let rect = rect.to_rect().content();
            self.resize_pane(*pane, rect.w, rect.h).await?;
            // The PTY alone is not enough: the emulator keeps its own grid, and
            // a stale grid reflows the pane's output to the width it used to
            // have no matter how much room the finished layout gave it.
            if let Some(p) = self.pane_mut(*pane) {
                p.resize(rect.w, rect.h);
            }
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
        let zoomed = self.current().zoomed;
        if let Some(root) = self.current_mut().root.as_mut() {
            root.compute_layout(root_rect);

            // A zoomed pane is laid out normally and then stretched over the
            // whole window. Leaving the rest of the tree at its real geometry
            // is what lets unzooming animate straight back.
            if let Some(zoomed) = zoomed {
                if let Some(Node::Leaf { rect_target, .. }) = root.find_leaf_mut(zoomed) {
                    *rect_target = root_rect;
                }
            }
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
        self.agents.forget(id);
        // Retire the `%N` with the pane. Numbers are never reused, so a stale
        // `%N` in a script fails loudly instead of hitting somebody else's pane.
        self.pane_numbers.remove(&id);
    }

    async fn resize_pane(&mut self, pane: PaneId, cols: u16, rows: u16) -> anyhow::Result<()> {
        debug_assert!(self.is_safe_to_resize(pane));
        self.backend.resize(pane, cols, rows).await
    }

    /// Resize a pane to fit a window it was just placed in.
    ///
    /// Goes through `HostResize` for the same reason switching windows does:
    /// this is a deliberate placement, not a mid-animation resize. The pane may
    /// still carry a tween from the layout it just left, so that tween is
    /// dropped first — it is animating towards a rectangle that no longer
    /// means anything.
    async fn fit_pane_to_window(&mut self, pane: PaneId, rect: Rect) -> anyhow::Result<()> {
        self.timeline.clear_pane_tweens(pane);

        let rect = rect.content();
        let previous = self.resize_mode;
        self.resize_mode = ResizeMode::HostResize;
        let resized = self.resize_pane(pane, rect.w, rect.h).await;
        self.resize_mode = previous;

        if let Some(pane) = self.pane_mut(pane) {
            pane.resize(rect.w, rect.h);
        }

        resized
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
            zoomed: None,
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
            session_socket: None,
            resize_mode: ResizeMode::Normal,
            agents: AgentTracker::default(),
            agents_polled: std::time::Instant::now(),
            backend,
            output_rx,
            event_rx,
            sink: OutputSink::stdout(),
            session_rx: None,
            clients: Vec::new(),
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
            timeline: Timeline::new(),
            keymap: Keymap::default(),
            options: Options::default(),
            message: None,
            prompt: None,
            wait_channels: HashMap::new(),
            key_table: ROOT_TABLE.to_owned(),
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
        LaunchArgs, Prompt, CLOSE_PANE_DURATION, FOCUS_BORDER_TWEEN_DURATION, MESSAGE_DURATION,
        OPEN_NEW_PANE_DURATION,
    };
    use crate::anim::tween::Easing;
    use crate::backend::{PaneBackend, PaneCommand, PaneId};
    use crate::command::Command;
    use crate::layout::geometry::{FRect, Split};
    use crate::command::Target;
    use crate::session::protocol::{CommandResult, ServerToClient};
    use tokio::sync::mpsc;
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

    impl App {
        /// Run a command and return what it printed, failing the test if it
        /// was rejected.
        async fn request_output(&mut self, line: &str) -> String {
            match self
                .execute_request(command(line))
                .await
                .expect("the request completes")
            {
                CommandResult::Ok { output } => output,
                CommandResult::Error { message } => {
                    panic!("`{line}` was rejected: {message}")
                }
            }
        }
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

    /// Ratios are what make `-p 30` mean 30%, so assert the geometry, not
    /// just that the command succeeded.
    #[tokio::test]
    async fn split_window_p_sizes_the_new_pane() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));

        app.execute(command("split-window -h -p 30"))
            .await
            .expect("split succeeds");

        let new_rect = app.leaf_rect_target(PaneId(2)).expect("new pane laid out");
        // 30% of a 100-cell window, within a cell of rounding.
        assert!(
            (29..=31).contains(&new_rect.w),
            "new pane should be ~30 cells wide, got {}",
            new_rect.w
        );
    }

    fn labels(app: &App) -> Vec<String> {
        app.agent_indicators()
            .iter()
            .map(|agent| format!("{}:{}", agent.index, agent.name))
            .collect()
    }

    /// The bar carries the whole session, not just the window you are looking
    /// at: an agent that has stopped in window 2 is exactly the one you cannot
    /// see. The name shown is the configured one, so an agent started by
    /// absolute path still reads as `opencode`.
    #[tokio::test]
    async fn agent_indicators_cover_every_window_and_ignore_plain_shells() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("new-window")).await.expect("new window");

        let panes = app.pane_ids();
        assert_eq!(panes.len(), 3, "two panes in window 1, one in window 2");
        app.agents.set_foreground(panes[0], Some("claude".to_owned()));
        app.agents.set_foreground(panes[1], Some("fish".to_owned()));
        app.agents
            .set_foreground(panes[2], Some("/usr/bin/opencode".to_owned()));

        assert_eq!(labels(&app), vec!["1:claude", "1:opencode"]);
    }

    /// Agents of one kind sit together and count up within that kind, so two
    /// claudes read `1:claude 2:claude` rather than repeating one label.
    #[tokio::test]
    async fn agents_are_grouped_by_kind_and_numbered_within_it() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        for _ in 0..3 {
            app.execute(command("split-window -h")).await.expect("split");
        }

        // Interleaved on screen: claude, codex, claude, opencode.
        let panes = app.pane_ids();
        for (pane, command) in panes.iter().zip(["claude", "codex", "claude", "opencode"]) {
            app.agents.set_foreground(*pane, Some(command.to_owned()));
        }

        assert_eq!(
            labels(&app),
            vec!["1:claude", "2:claude", "1:codex", "1:opencode"]
        );
    }

    /// Kind order follows `agent-commands`, not the order panes were made, so
    /// the bar's layout holds still as agents come and go.
    #[tokio::test]
    async fn kind_order_follows_the_configured_list() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("set-option agent-commands opencode,claude"))
            .await
            .expect("option set");

        let panes = app.pane_ids();
        app.agents.set_foreground(panes[0], Some("claude".to_owned()));
        app.agents.set_foreground(panes[1], Some("opencode".to_owned()));

        assert_eq!(labels(&app), vec!["1:opencode", "1:claude"]);
    }

    #[tokio::test]
    async fn an_agent_is_working_while_it_prints_and_idle_once_it_stops() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let pane = app.pane_ids()[0];
        app.agents.set_foreground(pane, Some("claude".to_owned()));

        app.agents.note_output(pane, std::time::Instant::now());
        assert_eq!(
            app.agent_indicators().first().map(|a| a.state),
            Some(crate::agent::AgentState::Working)
        );

        // Rewind the last-output stamp past the activity window.
        app.agents.note_output(
            pane,
            std::time::Instant::now()
                .checked_sub(Duration::from_secs(30))
                .expect("the clock has been up for 30s"),
        );
        assert_eq!(
            app.agent_indicators().first().map(|a| a.state),
            Some(crate::agent::AgentState::Idle)
        );
    }

    /// A quiet agent sitting on a question is the one worth a colour of its own.
    #[tokio::test]
    async fn a_quiet_agent_at_a_prompt_is_waiting() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let pane = app.pane_ids()[0];
        app.agents.set_foreground(pane, Some("claude".to_owned()));
        app.agents.note_output(
            pane,
            std::time::Instant::now()
                .checked_sub(Duration::from_secs(30))
                .expect("the clock has been up for 30s"),
        );

        if let Some(p) = app.pane_mut(pane) {
            p.process(b"Do you want to proceed?\r\n  1. Yes\r\n");
        }

        assert_eq!(
            app.agent_indicators().first().map(|a| a.state),
            Some(crate::agent::AgentState::Waiting)
        );
    }

    #[tokio::test]
    async fn agent_status_off_hides_the_indicators() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let pane = app.pane_ids()[0];
        app.agents.set_foreground(pane, Some("claude".to_owned()));

        app.execute(command("set-option agent-status off"))
            .await
            .expect("option set");

        assert!(app.agent_indicators().is_empty());
    }

    #[tokio::test]
    async fn resize_pane_moves_the_boundary_and_animates() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let before = app.leaf_rect_target(PaneId(1)).expect("laid out").w;
        app.execute(command("resize-pane -t %1 -R 10"))
            .await
            .expect("resize succeeds");
        let after = app.leaf_rect_target(PaneId(1)).expect("laid out").w;

        assert!(
            after > before,
            "moving %1's right border right should widen it: {before} -> {after}"
        );
        assert!(!app.timeline.is_idle(), "a resize should animate");
    }

    /// A pane's rect covers its border, so the process behind it must be sized
    /// to the inside. Handing it the whole rect makes it draw two columns and
    /// rows that are clipped away when the pane is blitted.
    #[tokio::test]
    async fn a_host_resize_sizes_each_pane_to_the_inside_of_its_slot() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.advance_animations(OPEN_NEW_PANE_DURATION)
            .await
            .expect("open animation completes");
        handle.clear_resized();

        app.resize_to(100, 30).await;

        // Status bar takes a row, so the slots are 50x29 each.
        let expected = [(PaneId(1), 48, 27), (PaneId(2), 48, 27)];
        for want in expected {
            assert!(
                handle.resized().contains(&want),
                "expected {want:?} in {:?}",
                handle.resized()
            );
        }
        for pane in &app.panes {
            assert_eq!(pane.size(), (48, 27), "emulator grid follows the PTY");
        }
    }

    /// `-L` moves a border, it does not always enlarge — which side the pane
    /// is on decides. This is tmux's behaviour and the easy thing to get wrong.
    #[tokio::test]
    async fn resize_left_shrinks_the_pane_on_the_left() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let before = app.leaf_rect_target(PaneId(1)).expect("laid out").w;
        app.execute(command("resize-pane -t %1 -L 10"))
            .await
            .expect("resize succeeds");
        let after = app.leaf_rect_target(PaneId(1)).expect("laid out").w;

        assert!(after < before, "{before} -> {after}");
    }

    #[tokio::test]
    async fn resizing_a_pane_with_no_split_on_that_axis_is_refused() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));

        let result = app
            .execute_request(command("resize-pane -R 5"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok(), "a lone pane has no boundary to move");
    }

    #[tokio::test]
    async fn resize_pane_x_sets_an_absolute_width() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        app.execute(command("resize-pane -t %1 -x 40"))
            .await
            .expect("resize succeeds");

        let width = app.leaf_rect_target(PaneId(1)).expect("laid out").w;
        assert!((39..=41).contains(&width), "expected ~40, got {width}");
    }

    /// Zoom leaves the tree alone, so unzooming restores the exact layout
    /// without anything having to be remembered.
    #[tokio::test]
    async fn zoom_fills_the_window_and_unzoom_restores_the_layout() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        let unzoomed = app.leaf_rect_target(PaneId(2)).expect("laid out");

        app.execute(command("resize-pane -Z"))
            .await
            .expect("zoom succeeds");
        assert_eq!(app.current().zoomed, Some(PaneId(2)));
        let zoomed = app.leaf_rect_target(PaneId(2)).expect("laid out");
        assert_eq!(zoomed, app.root_rect(), "a zoomed pane fills the window");

        app.execute(command("resize-pane -Z"))
            .await
            .expect("unzoom succeeds");
        assert_eq!(app.current().zoomed, None);
        assert_eq!(app.leaf_rect_target(PaneId(2)), Some(unzoomed));
    }

    #[tokio::test]
    async fn resizing_while_zoomed_is_refused_rather_than_silently_ignored() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("resize-pane -Z")).await.expect("zoom");

        let result = app
            .execute_request(command("resize-pane -R 5"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
    }

    #[tokio::test]
    async fn swap_pane_exchanges_positions() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let left = app.current().leaf_panes()[0];
        let right = app.current().leaf_panes()[1];
        app.execute(command("swap-pane -U"))
            .await
            .expect("swap succeeds");

        assert_eq!(app.current().leaf_panes(), vec![right, left]);
    }

    #[tokio::test]
    async fn swap_pane_direction_of_moves_the_focused_pane_and_its_focus() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let left = app.current().leaf_panes()[0];
        let right = app.current().leaf_panes()[1];
        assert_eq!(app.current().focused, Some(right));

        app.execute(command("swap-pane -t {left-of}"))
            .await
            .expect("swap succeeds");

        assert_eq!(app.current().leaf_panes(), vec![right, left]);
        assert_eq!(
            app.current().focused,
            Some(right),
            "focus travels with the pane that moved"
        );
    }

    #[tokio::test]
    async fn swap_pane_direction_of_rejects_an_empty_direction() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let before = app.current().leaf_panes();
        let result = app
            .execute_request(command("swap-pane -t {right-of}"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
        assert_eq!(app.current().leaf_panes(), before);
    }

    #[tokio::test]
    async fn rotate_window_moves_every_pane_along() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("split-window -v")).await.expect("split");
        let before = app.current().leaf_panes();

        app.execute(command("rotate-window"))
            .await
            .expect("rotate succeeds");

        let after = app.current().leaf_panes();
        assert_ne!(before, after);
        let mut sorted_before = before.clone();
        let mut sorted_after = after.clone();
        sorted_before.sort_by_key(|pane| pane.0);
        sorted_after.sort_by_key(|pane| pane.0);
        assert_eq!(sorted_before, sorted_after, "rotate must not lose a pane");
    }

    #[tokio::test]
    async fn select_layout_even_horizontal_gives_every_pane_the_same_width() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 120, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("split-window -v")).await.expect("split");

        app.execute(command("select-layout even-horizontal"))
            .await
            .expect("layout succeeds");

        let widths: Vec<u16> = app
            .current()
            .leaf_panes()
            .into_iter()
            .map(|pane| app.leaf_rect_target(pane).expect("laid out").w)
            .collect();
        assert_eq!(widths.len(), 3);
        let min = widths.iter().min().copied().unwrap_or(0);
        let max = widths.iter().max().copied().unwrap_or(0);
        assert!(max - min <= 1, "widths should be even, got {widths:?}");
    }

    #[tokio::test]
    async fn select_layout_keeps_every_pane() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 120, 40, PaneId(1));
        for _ in 0..3 {
            app.execute(command("split-window -h")).await.expect("split");
        }

        for layout in [
            "even-vertical",
            "main-vertical",
            "main-horizontal",
            "tiled",
        ] {
            app.execute(command(&format!("select-layout {layout}")))
                .await
                .unwrap_or_else(|error| panic!("{layout} failed: {error}"));
            assert_eq!(
                app.current().leaf_panes().len(),
                4,
                "{layout} lost a pane"
            );
        }
    }

    #[tokio::test]
    async fn list_panes_formats_one_line_per_pane() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let result = app
            .execute_request(command("list-panes -F #{pane_id}"))
            .await
            .expect("the request completes");

        match result {
            CommandResult::Ok { output } => assert_eq!(output, "%1\n%2"),
            other @ CommandResult::Error { .. } => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn list_panes_marks_the_active_one() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        let output = app
            .request_output("list-panes -F #{pane_id}#{?pane_active,*,}")
            .await;

        // The split focused the new pane, so it is the marked one.
        assert_eq!(output, "%1\n%2*");
    }

    #[tokio::test]
    async fn list_panes_a_covers_every_window() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("new-window -d")).await.expect("window");

        let all = app.request_output("list-panes -a -F #{window_index}.#{pane_index}").await;
        let one = app.request_output("list-panes -F #{window_index}.#{pane_index}").await;

        assert_eq!(all, "1.0\n2.0");
        assert_eq!(one, "1.0", "without -a only the current window is listed");
    }

    #[tokio::test]
    async fn list_windows_reports_names_and_flags() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("new-window -d -n build"))
            .await
            .expect("window");

        let output = app.request_output("list-windows -F #{window_index}:#{window_name}#F").await;

        assert_eq!(output, "1:shell*\n2:build");
    }

    #[tokio::test]
    async fn window_flags_mark_a_zoomed_window() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("resize-pane -Z")).await.expect("zoom");

        assert_eq!(app.request_output("list-windows -F #F").await, "*Z");
    }

    #[tokio::test]
    async fn a_bad_format_is_rejected_rather_than_expanded_to_nothing() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));

        let result = app
            .execute_request(command("list-panes -F #{pane_id"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok(), "an unterminated format must not silently pass");
    }

    #[tokio::test]
    async fn display_message_expands_formats() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));

        assert_eq!(app.request_output("display-message -p #{pane_id}").await, "%1");
        assert_eq!(
            app.request_output("display-message -p #{window_index}:#{pane_index}").await,
            "1:0"
        );
    }

    #[tokio::test]
    async fn capture_pane_returns_the_visible_screen() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 40, 10, PaneId(1));

        // Feed the pane some output, as its PTY would.
        if let Some(pane) = app.pane_mut(PaneId(1)) {
            pane.process(b"first\r\nsecond\r\n");
        }

        assert_eq!(
            app.request_output("capture-pane -p").await,
            "first\nsecond"
        );
    }

    #[tokio::test]
    async fn capture_pane_honours_a_line_range() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 40, 10, PaneId(1));
        if let Some(pane) = app.pane_mut(PaneId(1)) {
            pane.process(b"a\r\nb\r\nc\r\n");
        }

        assert_eq!(app.request_output("capture-pane -p -S 1").await, "b\nc");
        assert_eq!(app.request_output("capture-pane -p -S 0 -E 1").await, "a\nb");
        // Past the end is empty rather than an error: a script polling a quiet
        // pane should see nothing and carry on.
        assert_eq!(app.request_output("capture-pane -p -S 99").await, "");
    }

    /// A key in no table reaches the pane; a bound one does not.
    #[tokio::test]
    async fn the_prefix_swallows_its_key_and_the_next_one() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        // A plain letter goes to the pane.
        app.handle_input(Some(Ok(crossterm::event::Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )))))
        .await
        .expect("input handled");
        assert_eq!(handle.written_to(PaneId(1)), b"a".to_vec());

        // The prefix does not, and neither does the key after it.
        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('%'), KeyModifiers::NONE),
        ] {
            app.handle_input(Some(Ok(crossterm::event::Event::Key(key))))
                .await
                .expect("input handled");
        }

        assert_eq!(
            handle.written_to(PaneId(1)),
            b"a".to_vec(),
            "neither the prefix nor its key should reach the pane"
        );
        assert_eq!(app.current().leaf_panes().len(), 2, "C-b % should split");
        assert_eq!(app.key_table, "root", "the table resets after one key");
    }

    /// An unbound key after the prefix is swallowed rather than leaking into
    /// the pane, so a mistyped chord cannot corrupt what you are editing.
    #[tokio::test]
    async fn an_unbound_key_after_the_prefix_is_swallowed() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        for key in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('~'), KeyModifiers::NONE),
        ] {
            app.handle_input(Some(Ok(crossterm::event::Event::Key(key))))
                .await
                .expect("input handled");
        }

        assert!(handle.written_to(PaneId(1)).is_empty());
        assert_eq!(app.key_table, "root");
    }

    #[tokio::test]
    async fn bind_key_and_list_keys_round_trip() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("bind-key -n M-s split-window -h"))
            .await
            .expect("bind succeeds");

        let listed = app.request_output("list-keys -T root").await;
        assert!(listed.contains("M-s"), "{listed}");

        app.execute(command("unbind-key -n M-s"))
            .await
            .expect("unbind succeeds");
        let listed = app.request_output("list-keys -T root").await;
        assert!(!listed.contains("M-s"), "{listed}");
    }

    #[tokio::test]
    async fn unbinding_a_key_that_is_not_bound_is_reported() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let result = app
            .execute_request(command("unbind-key -n M-nope"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
    }

    #[tokio::test]
    async fn setting_the_prefix_at_runtime_takes_effect() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("set-option -g prefix C-a"))
            .await
            .expect("set succeeds");

        assert!(app
            .keymap
            .is_prefix(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)));
        assert!(!app
            .keymap
            .is_prefix(&KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)));
    }

    #[tokio::test]
    async fn setting_status_off_hides_the_status_bar() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        assert!(app.status_bar);

        app.execute(command("set-option -g status off"))
            .await
            .expect("set succeeds");

        assert!(!app.status_bar);
    }

    /// An inert option is stored so `show-options` round-trips, but an unknown
    /// one is a typo and must fail.
    #[tokio::test]
    async fn inert_options_are_stored_and_unknown_ones_rejected() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(command("set-option -g history-limit 5000"))
            .await
            .expect("inert options are accepted");
        assert_eq!(app.request_output("show-options history-limit").await, "5000");

        let result = app
            .execute_request(command("set-option -g nonsense on"))
            .await
            .expect("the request completes");
        assert!(!result.is_ok());
    }

    #[tokio::test]
    async fn break_pane_moves_a_pane_into_its_own_window() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");

        app.execute(command("break-pane -s %2 -n solo"))
            .await
            .expect("break succeeds");

        // The pane moved rather than being replaced: same id, same process.
        assert!(handle.killed().is_empty(), "break must not kill the pane");
        assert_eq!(app.current_workspace, 1);
        assert_eq!(app.window_name(1), "solo");
        assert_eq!(app.current().leaf_panes(), vec![PaneId(2)]);
        assert_eq!(app.workspaces[0].leaf_panes(), vec![PaneId(1)]);
        assert_eq!(app.pane_number(PaneId(2)), Some(2), "%2 still means %2");
    }

    #[tokio::test]
    async fn breaking_the_only_pane_is_refused() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));

        let result = app
            .execute_request(command("break-pane"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
    }

    #[tokio::test]
    async fn join_pane_moves_a_pane_between_windows() {
        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("new-window -d")).await.expect("window");

        // %2 lives in window 2; bring it back into window 1 beside %1.
        app.execute(command("join-pane -s %2 -t %1 -h"))
            .await
            .expect("join succeeds");

        assert!(handle.killed().is_empty(), "join must not kill the pane");
        assert_eq!(app.current_workspace, 0);
        assert_eq!(app.current().leaf_panes(), vec![PaneId(1), PaneId(2)]);
        assert!(app.workspaces[1].is_empty(), "the source window emptied");
    }

    #[tokio::test]
    async fn kill_pane_a_leaves_only_the_target() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 24, PaneId(1));
        app.execute(command("split-window -h")).await.expect("split");
        app.execute(command("split-window -v")).await.expect("split");

        app.execute(command("kill-pane -a -t %1"))
            .await
            .expect("kill succeeds");

        // The others are closing; %1 is not.
        assert!(!app.current().closing.contains(&PaneId(1)));
        assert!(app.current().closing.contains(&PaneId(2)));
        assert!(app.current().closing.contains(&PaneId(3)));
    }

    #[tokio::test]
    async fn run_shell_returns_command_output() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        // Quoting is the caller's job: the shell command is one argument.
        let result = app
            .execute_request(Command::RunShell {
                command: "echo hi".to_owned(),
                background: false,
            })
            .await
            .expect("the request completes");
        match result {
            CommandResult::Ok { output } => assert_eq!(output, "hi"),
            other @ CommandResult::Error { .. } => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_run_shell_is_an_error_result() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let result = app
            .execute_request(Command::RunShell {
                command: "exit 3".to_owned(),
                background: false,
            })
            .await
            .expect("the request completes");

        assert!(!result.is_ok());
    }

    #[tokio::test]
    async fn if_shell_picks_a_branch_by_exit_status() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.execute(Command::IfShell {
            condition: "true".to_owned(),
            then_command: vec!["split-window".to_owned(), "-h".to_owned()],
            else_command: None,
            background: false,
        })
        .await
        .expect("if-shell succeeds");
        assert_eq!(app.current().leaf_panes().len(), 2, "the then branch ran");

        app.execute(Command::IfShell {
            condition: "false".to_owned(),
            then_command: vec!["split-window".to_owned(), "-h".to_owned()],
            else_command: None,
            background: false,
        })
        .await
        .expect("if-shell succeeds");
        assert_eq!(app.current().leaf_panes().len(), 2, "no branch ran");
    }

    #[tokio::test]
    async fn display_message_without_p_goes_to_the_status_line() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let output = app.request_output("display-message hello").await;

        assert_eq!(output, "", "it shows rather than returns");
        assert_eq!(
            app.message.as_ref().map(|message| message.text.as_str()),
            Some("hello")
        );

        // And it ages out.
        app.advance_message(MESSAGE_DURATION);
        assert!(app.message.is_none());
    }

    fn client_channel() -> (
        mpsc::UnboundedSender<ServerToClient>,
        mpsc::UnboundedReceiver<ServerToClient>,
    ) {
        mpsc::unbounded_channel()
    }

    /// A session is only as big as the smallest terminal watching it —
    /// anything larger would be cut off for somebody.
    #[tokio::test]
    async fn the_session_shrinks_to_the_smallest_client() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 100, 40, PaneId(1));

        let (big, _big_rx) = client_channel();
        app.attach_client(1, 100, 40, true, big).await;
        assert_eq!((app.back.width, app.back.height), (100, 40));

        let (small, _small_rx) = client_channel();
        app.attach_client(2, 60, 20, true, small).await;
        assert_eq!(
            (app.back.width, app.back.height),
            (60, 20),
            "the session fits the smaller terminal"
        );

        // When the small one leaves, the session grows back.
        app.drop_client(2, None);
        app.renegotiate_size().await;
        assert_eq!((app.back.width, app.back.height), (100, 40));
    }

    /// Attaching used to evict; now both terminals watch the same session.
    #[tokio::test]
    async fn a_second_client_joins_rather_than_evicting() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let (first, mut first_rx) = client_channel();
        let (second, _second_rx) = client_channel();
        app.attach_client(1, 80, 24, true, first).await;
        app.attach_client(2, 80, 24, true, second).await;

        assert_eq!(app.clients.len(), 2);
        // The first client was not sent a goodbye.
        while let Ok(message) = first_rx.try_recv() {
            assert!(
                !matches!(message, ServerToClient::Exit(_)),
                "the first client must not be evicted"
            );
        }
    }

    /// Each client gets its own delta, computed against its own screen.
    #[tokio::test]
    async fn every_client_gets_its_own_frame() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let (first, mut first_rx) = client_channel();
        let (second, mut second_rx) = client_channel();
        app.attach_client(1, 80, 24, true, first).await;
        app.attach_client(2, 80, 24, true, second).await;

        app.tick(Duration::from_millis(16))
            .await
            .expect("a frame renders");

        assert!(
            matches!(first_rx.try_recv(), Ok(ServerToClient::Frame(_))),
            "the first client should get a frame"
        );
        assert!(
            matches!(second_rx.try_recv(), Ok(ServerToClient::Frame(_))),
            "the second client should get one too"
        );
    }

    #[tokio::test]
    async fn detach_client_by_id_leaves_the_others_alone() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let (first, _first_rx) = client_channel();
        let (second, mut second_rx) = client_channel();
        app.attach_client(1, 80, 24, true, first).await;
        app.attach_client(2, 80, 24, true, second).await;

        app.execute(command("detach-client -t 2"))
            .await
            .expect("detach succeeds");

        assert_eq!(app.clients.len(), 1);
        assert_eq!(app.clients[0].id, 1);
        // The detached one was told why.
        let mut told = false;
        while let Ok(message) = second_rx.try_recv() {
            told |= matches!(message, ServerToClient::Detached);
        }
        assert!(told, "the detached client should be told");
    }

    #[tokio::test]
    async fn detach_client_a_keeps_only_the_target() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        for id in 1..=3 {
            let (frames, _rx) = client_channel();
            app.attach_client(id, 80, 24, true, frames).await;
        }

        app.execute(command("detach-client -a -t 2"))
            .await
            .expect("detach succeeds");

        assert_eq!(app.clients.len(), 1);
        assert_eq!(app.clients[0].id, 2);
    }

    #[tokio::test]
    async fn refresh_client_repaints_everyone() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let (frames, _rx) = client_channel();
        app.attach_client(1, 80, 24, true, frames).await;
        app.tick(Duration::from_millis(16)).await.expect("a frame");
        assert!(!app.clients[0].needs_full_repaint);

        app.execute(command("refresh-client"))
            .await
            .expect("refresh succeeds");

        assert!(app.clients[0].needs_full_repaint);
    }

    #[tokio::test]
    async fn rename_session_needs_a_session_to_rename() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        let result = app
            .execute_request(command("rename-session newname"))
            .await
            .expect("the request completes");

        assert!(!result.is_ok(), "a local weave has no session name to change");
    }

    #[tokio::test]
    async fn rename_session_rejects_a_name_the_socket_layer_would_refuse() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.session_name = Some("dev".to_owned());

        for bad in ["with space", "slash/name", ""] {
            let result = app
                .execute_request(Command::RenameSession {
                    target: Target::current(),
                    name: bad.to_owned(),
                })
                .await
                .expect("the request completes");
            assert!(!result.is_ok(), "`{bad}` should be refused");
        }
    }

    /// Renaming to the name it already has is a no-op, not an error: a script
    /// that sets a name unconditionally should not fail on the second run.
    #[tokio::test]
    async fn renaming_to_the_current_name_is_a_no_op() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.session_name = Some("dev".to_owned());

        let result = app
            .execute_request(command("rename-session dev"))
            .await
            .expect("the request completes");

        assert!(result.is_ok());
        assert_eq!(app.session_name.as_deref(), Some("dev"));
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn alt_key(ch: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(ch),
            crossterm::event::KeyModifiers::ALT,
        ))
    }

    /// The whole point of the prompt: Alt+R, type a name, Enter, renamed.
    #[tokio::test]
    async fn alt_r_opens_a_prompt_that_renames_the_window() {
        use crossterm::event::KeyCode;

        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.handle_input(Some(Ok(alt_key('r'))))
            .await
            .expect("input handled");
        assert!(app.prompt.is_some(), "Alt+R should open a prompt");
        // Prefilled with the current name, so it is an edit not a retype.
        assert_eq!(app.prompt.as_ref().expect("open").text(), "shell");

        // Clear it and type a new name.
        for code in [
            KeyCode::Char('u'),
        ] {
            app.handle_input(Some(Ok(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::CONTROL),
            ))))
            .await
            .expect("input handled");
        }
        for ch in "api".chars() {
            app.handle_input(Some(Ok(key(KeyCode::Char(ch)))))
                .await
                .expect("input handled");
        }
        app.handle_input(Some(Ok(key(KeyCode::Enter))))
            .await
            .expect("input handled");

        assert!(app.prompt.is_none(), "Enter should close the prompt");
        assert_eq!(app.window_name(0), "api");
        // And none of the typing reached the pane.
        assert!(handle.written_to(PaneId(1)).is_empty());
    }

    /// A prompt is modal — letters must not leak into the shell behind it.
    #[tokio::test]
    async fn typing_into_a_prompt_never_reaches_the_pane() {
        use crossterm::event::KeyCode;

        let (backend, handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.handle_input(Some(Ok(alt_key('r'))))
            .await
            .expect("input handled");
        for ch in "rm -rf /".chars() {
            app.handle_input(Some(Ok(key(KeyCode::Char(ch)))))
                .await
                .expect("input handled");
        }

        assert!(
            handle.written_to(PaneId(1)).is_empty(),
            "a modal prompt must swallow every key"
        );
    }

    #[tokio::test]
    async fn escape_cancels_a_prompt_without_running_anything() {
        use crossterm::event::KeyCode;

        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let before = app.window_name(0);

        app.handle_input(Some(Ok(alt_key('r'))))
            .await
            .expect("input handled");
        for ch in "nope".chars() {
            app.handle_input(Some(Ok(key(KeyCode::Char(ch)))))
                .await
                .expect("input handled");
        }
        app.handle_input(Some(Ok(key(KeyCode::Esc))))
            .await
            .expect("input handled");

        assert!(app.prompt.is_none());
        assert_eq!(app.window_name(0), before);
    }

    #[tokio::test]
    async fn submitting_an_empty_prompt_does_nothing() {
        use crossterm::event::KeyCode;

        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        let before = app.window_name(0);

        app.handle_input(Some(Ok(alt_key('r'))))
            .await
            .expect("input handled");
        app.handle_input(Some(Ok(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                KeyCode::Char('u'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ))))
        .await
        .expect("input handled");
        app.handle_input(Some(Ok(key(KeyCode::Enter))))
            .await
            .expect("input handled");

        assert!(app.prompt.is_none());
        assert_eq!(app.window_name(0), before, "empty input must not rename");
    }

    /// `%%` standing alone becomes one argument however many spaces are typed
    /// into it, so a two-word name does not become two arguments.
    #[tokio::test]
    async fn a_typed_name_with_spaces_stays_one_argument() {
        use crossterm::event::KeyCode;

        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        app.handle_input(Some(Ok(alt_key('r'))))
            .await
            .expect("input handled");
        app.handle_input(Some(Ok(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                KeyCode::Char('u'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ))))
        .await
        .expect("input handled");
        for ch in "my window".chars() {
            app.handle_input(Some(Ok(key(KeyCode::Char(ch)))))
                .await
                .expect("input handled");
        }
        app.handle_input(Some(Ok(key(KeyCode::Enter))))
            .await
            .expect("input handled");

        assert_eq!(app.window_name(0), "my window");
    }

    #[tokio::test]
    async fn alt_shift_r_prompts_for_a_session_name() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.session_name = Some("dev".to_owned());

        app.handle_input(Some(Ok(alt_key('R'))))
            .await
            .expect("input handled");

        let prompt = app.prompt.as_ref().expect("a prompt opened");
        assert_eq!(prompt.label, "rename-session:");
        assert_eq!(prompt.text(), "dev", "prefilled with the current name");
    }

    #[tokio::test]
    async fn prompt_editing_keys_work() {
        use crossterm::event::KeyCode;

        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.prompt = Some(Prompt {
            label: ":".to_owned(),
            input: "abc".chars().collect(),
            cursor: 3,
            template: "rename-window %%".to_owned(),
        });

        app.handle_input(Some(Ok(key(KeyCode::Backspace))))
            .await
            .expect("input handled");
        assert_eq!(app.prompt.as_ref().expect("open").text(), "ab");

        app.handle_input(Some(Ok(key(KeyCode::Home))))
            .await
            .expect("input handled");
        app.handle_input(Some(Ok(key(KeyCode::Char('X')))))
            .await
            .expect("input handled");
        assert_eq!(app.prompt.as_ref().expect("open").text(), "Xab");

        app.handle_input(Some(Ok(key(KeyCode::Delete))))
            .await
            .expect("input handled");
        assert_eq!(app.prompt.as_ref().expect("open").text(), "Xb");
    }

    /// The rendered bottom row, as text.
    fn status_row(app: &App) -> String {
        let surface = app.compose_current_surface();
        let mut back = surface.clone();
        let indicators = app.workspace_indicators();
        let agents = app.agent_indicators();
        crate::render::chrome::draw_status_bar(
            &mut back,
            &app.status_left(),
            &indicators,
            &agents,
            chrono::Local::now(),
            app.theme,
        );
        (0..back.width)
            .map(|x| back.get(x, back.height - 1).expect("cell exists").ch)
            .collect()
    }

    /// The session name has to be *visible*, not just stored — renaming a
    /// session you cannot see the name of is not much use.
    #[tokio::test]
    async fn the_status_bar_shows_the_session_name() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.session_name = Some("dev".to_owned());

        assert!(status_row(&app).starts_with("[dev] "), "{}", status_row(&app));
    }

    #[tokio::test]
    async fn renaming_a_session_updates_the_status_bar() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.session_name = Some("before".to_owned());
        assert!(status_row(&app).starts_with("[before] "));

        // Rename through the same path a command takes.
        app.session_name = Some("after".to_owned());

        assert!(status_row(&app).starts_with("[after] "), "{}", status_row(&app));
    }

    #[tokio::test]
    async fn a_prompt_and_a_message_take_over_the_name_slot() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.session_name = Some("dev".to_owned());

        app.show_message("saved".to_owned());
        assert!(status_row(&app).starts_with("[saved] "), "{}", status_row(&app));

        app.message = None;
        app.prompt = Some(Prompt {
            label: "rename-window:".to_owned(),
            input: "api".chars().collect(),
            cursor: 3,
            template: "rename-window %%".to_owned(),
        });
        assert!(
            status_row(&app).starts_with("[rename-window: api"),
            "{}",
            status_row(&app)
        );

        // And the name comes back when both are gone.
        app.prompt = None;
        assert!(status_row(&app).starts_with("[dev] "));
    }

    #[tokio::test]
    async fn a_local_weave_shows_a_placeholder_rather_than_nothing() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let app = App::with_backend_for_test(backend, 80, 24, PaneId(1));

        assert!(status_row(&app).starts_with("[weave] "), "{}", status_row(&app));
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
        // Each pane gets the inside of its slot, not the slot itself.
        assert!(resized.contains(&(PaneId(1), 38, 21)));
        assert!(resized.contains(&(PaneId(2), 38, 21)));
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

        assert_eq!(handle.resized(), vec![(PaneId(1), 78, 21)]);
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
                detach_others: false,
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
        let error = LaunchArgs::parse(["exec", "new-window", "-S"])
            .expect_err("not implemented yet")
            .to_string();

        assert!(error.contains("PR 9"), "{error}");
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
