//! `App`: event loop + state owner.

use std::collections::HashSet;
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
use crate::backend::tmux::TmuxBackend;
use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use crate::command::Command;
use crate::config::Config;
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
    root: Option<Node>,
    focused: Option<PaneId>,
    closing: HashSet<PaneId>,
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
    focused_border_color: crossterm::style::Color,
    status_bar: bool,
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

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Args {
    pub debug: bool,
    pub backend: BackendKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttachArgs {
    pub session_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchArgs {
    Run(Args),
    Attach(AttachArgs),
    ListSessions,
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
            } else if arg == "--backend" {
                let Some(value) = args.next() else {
                    bail!("missing value for `--backend`; expected `native` or `tmux`");
                };
                parsed.backend = BackendKind::from_cli_value(value.as_ref())?;
            } else if let Some(value) = arg.strip_prefix("--backend=") {
                parsed.backend = BackendKind::from_cli_value(value)?;
            } else if arg == "attach" {
                bail!(
                    "`attach` reconnects to tmux sessions only; use `wv attach [name]`, not `--backend native attach`"
                );
            } else {
                bail!("unknown argument `{arg}`");
            }
        }

        Ok(parsed)
    }
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
            Some("ls") => {
                if let Some(arg) = args.get(1) {
                    bail!("`wv ls` does not accept `{arg}`");
                }
                return Ok(Self::ListSessions);
            }
            _ => {}
        }

        Ok(Self::Run(Args::parse(args)?))
    }
}

impl App {
    pub async fn new(width: u16, height: u16, args: Args) -> anyhow::Result<Self> {
        let backend_parts = build_backend(args.backend).await?;
        Ok(Self::from_backend(
            width,
            height,
            args.debug,
            backend_parts,
            Vec::new(),
        ))
    }

    pub async fn attach(width: u16, height: u16, args: AttachArgs) -> anyhow::Result<Self> {
        let (backend_parts, pane_ids) = build_attach_backend(args.session_name).await?;
        let mut app = Self::from_backend(width, height, false, backend_parts, pane_ids.clone());

        app.resize_attached_panes(&pane_ids).await?;
        Ok(app)
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

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            panes: initial_panes
                .into_iter()
                .map(|pane| Pane::new(pane, width, height))
                .collect(),
            root,
            focused,
            closing: HashSet::new(),
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
            focused_border_color: config.ui.border_color,
            status_bar,
            tick_interval,
            debug: DebugMode::from_enabled(debug),
            last_tick_dt: Duration::ZERO,
            last_dirty_cells: 0,
            dirty: true,
            exit: ExitState::Running,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        if self.root.is_none() {
            let pane_id = self
                .spawn_shell_pane(true)
                .await
                .context("failed to spawn shell pane")?;
            self.root = Some(Node::Leaf {
                pane: pane_id,
                rect_current: FRect::from(self.root_rect()),
                rect_target: self.root_rect(),
            });
            self.focused = Some(pane_id);
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
                    self.handle_backend_event(event);
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
        }

        Ok(())
    }

    async fn spawn_shell_pane(&mut self, resize_immediately: bool) -> anyhow::Result<PaneId> {
        let pane_id = self.backend.spawn(default_pane_command()).await?;

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

    async fn split_focused(&mut self, split: Split) -> anyhow::Result<()> {
        let Some(focused) = self.focused else {
            return Ok(());
        };
        let Some(old_parent_rect) = self.leaf_rect_target(focused) else {
            return Ok(());
        };
        let new_pane = self.spawn_shell_pane(false).await?;

        if let Some(root) = self.root.as_mut() {
            root.split_focused(focused, split, new_pane);
            self.focused = Some(new_pane);
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
        let Some(focused) = self.focused else {
            return;
        };

        let next = self
            .root
            .as_ref()
            .and_then(|root| root.focus_neighbor(focused, dir));

        if let Some(next) = next {
            self.start_focus_border_tweens(focused, next);
            self.focused = Some(next);
            self.dirty = true;
        }
    }

    fn start_focus_border_tweens(&mut self, previous: PaneId, next: PaneId) {
        if previous == next {
            return;
        }

        let focused_color = self.focused_border_color;
        let unfocused_color = chrome::UNFOCUSED_BORDER;
        let previous_from =
            self.timeline
                .pane_border_color(previous, self.focused, focused_color, unfocused_color);
        let next_from =
            self.timeline
                .pane_border_color(next, self.focused, focused_color, unfocused_color);

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
        let Some(focused) = self.focused else {
            return Ok(());
        };
        if self.closing.contains(&focused) {
            return Ok(());
        }

        let Some(root) = self.root.as_ref() else {
            self.focused = None;
            self.exit = ExitState::Quit;
            self.dirty = true;
            return Ok(());
        };

        let Some(close_plan) = close_plan(root, focused) else {
            self.backend.kill(focused).await?;
            self.remove_pane(focused);
            self.root = None;
            self.focused = None;
            self.exit = ExitState::Quit;
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
        self.closing.insert(focused);
        self.focused = first_leaf_pane(&post_close_root);
        self.dirty = true;

        Ok(())
    }

    fn handle_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::PaneDied(id) => {
                tracing::info!("pane died: {id:?}");
                self.remove_pane(id);
                if let Some(root) = self.root.as_mut() {
                    if root.close(id) {
                        self.focused = self.root.as_ref().and_then(first_leaf_pane);
                        self.recompute_layout();
                    } else if self.focused == Some(id) {
                        self.root = None;
                        self.focused = None;
                        self.exit = ExitState::Quit;
                    }
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

        let Some(focused) = self.focused else {
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
        compositor::compose(
            self.root.as_ref(),
            &self.panes,
            self.focused,
            self.focused_border_color,
            &self.timeline,
            &mut self.back,
        );
        if self.status_bar {
            chrome::draw_status_bar(
                &mut self.back,
                "NORMAL",
                chrome::leaf_count(self.root.as_ref()),
                chrono::Local::now(),
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
        let advance = self.timeline.advance(dt, self.root.as_mut());

        if !advance.changed_panes.is_empty() || !advance.completed_leaf_rects.is_empty() {
            self.dirty = true;
        }

        let completed_leaf_rects = advance.completed_leaf_rects;
        let mut completed_closings = completed_leaf_rects.clone();
        for pane in &self.closing {
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
            if self.closing.contains(pane) {
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
            if !self.closing.remove(pane) {
                continue;
            }

            self.backend.kill(*pane).await?;
            self.remove_pane(*pane);

            if let Some(root) = self.root.as_mut() {
                if root.close(*pane) {
                    self.recompute_layout();
                    if self.focused.is_none() || self.focused == Some(*pane) {
                        self.focused = self.root.as_ref().and_then(first_leaf_pane);
                    }
                } else {
                    self.root = None;
                    self.focused = None;
                    self.exit = ExitState::Quit;
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
        if let Some(root) = self.root.as_mut() {
            root.compute_layout(root_rect);
        }
    }

    fn leaf_rect_target(&self, pane: PaneId) -> Option<Rect> {
        match self.root.as_ref()?.find_leaf(pane)? {
            Node::Leaf { rect_target, .. } => Some(*rect_target),
            Node::Internal { .. } => None,
        }
    }

    fn leaf_rect_current(&self, pane: PaneId) -> Option<FRect> {
        match self.root.as_ref()?.find_leaf(pane)? {
            Node::Leaf { rect_current, .. } => Some(*rect_current),
            Node::Internal { .. } => None,
        }
    }

    fn set_leaf_rect_current(&mut self, pane: PaneId, rect: FRect) {
        let Some(root) = self.root.as_mut() else {
            return;
        };
        let Some(Node::Leaf { rect_current, .. }) = root.find_leaf_mut(pane) else {
            return;
        };
        *rect_current = rect;
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

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            panes: vec![Pane::new(pane_id, width, height)],
            root: Some(Node::Leaf {
                pane: pane_id,
                rect_current: FRect::from(root_rect),
                rect_target: root_rect,
            }),
            focused: Some(pane_id),
            closing: HashSet::new(),
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
            focused_border_color: crossterm::style::Color::Cyan,
            status_bar: true,
            tick_interval: frame_interval(crate::config::DEFAULT_TARGET_FPS),
            debug: DebugMode::Off,
            last_tick_dt: Duration::ZERO,
            last_dirty_cells: 0,
            dirty: true,
            exit: ExitState::Running,
        }
    }
}

async fn build_backend(backend: BackendKind) -> anyhow::Result<BackendParts> {
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

    let boxed_backend: BoxedBackend = match backend {
        BackendKind::Native => Box::new(NativeBackend::with_senders(output_tx, event_tx)),
        BackendKind::Tmux => {
            ensure_tmux_available()?;
            Box::new(TmuxBackend::new(output_tx, event_tx).await?)
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
) -> anyhow::Result<(BackendParts, Vec<PaneId>)> {
    ensure_tmux_available()?;

    let session_name = resolve_attach_session(requested_session.as_deref())?;
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let backend = TmuxBackend::attach(session_name, output_tx, event_tx).await?;
    let pane_ids = backend.pane_ids();

    Ok((
        BackendParts {
            kind: BackendKind::Tmux,
            backend: Box::new(backend),
            output_rx,
            event_rx,
        },
        pane_ids,
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

pub fn print_weave_sessions() -> anyhow::Result<()> {
    let sessions = list_weave_tmux_sessions_for_display()?;
    if sessions.is_empty() {
        eprintln!("no weave sessions");
        return Ok(());
    }

    println!("{:<17}  {:<19}  state", "name", "created");
    for session in sessions {
        println!(
            "{:<17}  {:<19}  {}",
            session.name,
            format_epoch_seconds(session.created),
            session.state
        );
    }

    Ok(())
}

fn resolve_attach_session(requested: Option<&str>) -> anyhow::Result<String> {
    if let Some(session_name) = requested {
        if tmux_session_exists(session_name)? {
            return Ok(session_name.to_owned());
        }

        bail!(
            "tmux session `{session_name}` does not exist; start one with `wv --backend tmux` or choose a name from `tmux list-sessions`"
        );
    }

    let Some(session) = list_weave_tmux_sessions()?
        .into_iter()
        .max_by_key(|session| session.activity)
    else {
        bail!("no weave tmux sessions found; start one with `wv --backend tmux`");
    };

    Ok(session.name)
}

#[derive(Debug)]
struct DisplayTmuxSession {
    name: String,
    created: i64,
    state: String,
}

fn list_weave_tmux_sessions_for_display() -> anyhow::Result<Vec<DisplayTmuxSession>> {
    let output = match ProcessCommand::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_created}\t#{?session_attached,attached,detached}",
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

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_weave_tmux_session_for_display)
        .collect())
}

fn parse_weave_tmux_session_for_display(line: &str) -> Option<DisplayTmuxSession> {
    let mut parts = line.split('\t');
    let name = parts.next()?;
    let created = parts.next()?;
    let state = parts.next()?;
    if parts.next().is_some() || !name.starts_with("weave-") {
        return None;
    }

    Some(DisplayTmuxSession {
        name: name.to_owned(),
        created: created.parse().unwrap_or_default(),
        state: state.to_owned(),
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
            "#{session_name}\t#{session_activity}",
        ])
        .stdin(Stdio::null())
        .output()
        .context("failed to list tmux sessions")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_weave_tmux_session)
        .collect())
}

fn parse_weave_tmux_session(line: &str) -> Option<TmuxSession> {
    let (name, activity) = line.split_once('\t')?;
    if !name.starts_with("weave-") {
        return None;
    }

    Some(TmuxSession {
        name: name.to_owned(),
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
        frame_interval, App, Args, AttachArgs, BackendKind, ExitState, LaunchArgs,
        CLOSE_PANE_DURATION, FOCUS_BORDER_TWEEN_DURATION, OPEN_NEW_PANE_DURATION,
    };
    use crate::anim::tween::Easing;
    use crate::backend::{PaneBackend, PaneCommand, PaneId};
    use crate::command::Command;
    use crate::layout::geometry::{FRect, Split};
    use crate::layout::tree::Node;
    use crate::render::chrome;
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
        (
            Box::new(MockBackend {
                next_id,
                resized: Arc::clone(&handle.resized),
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

    #[async_trait::async_trait]
    impl PaneBackend for MockBackend {
        async fn spawn(&mut self, _cmd: PaneCommand) -> Result<PaneId, Error> {
            Ok(self.next_id)
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

        match app.root.expect("root exists") {
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
        assert_eq!(app.focused, Some(PaneId(2)));
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
        match app.root.expect("root exists") {
            Node::Leaf { rect_current, .. } => {
                assert_eq!(rect_current.x, 5.0);
                assert_eq!(rect_current.w, 75.0);
            }
            Node::Internal { .. } => panic!("expected leaf root"),
        }
    }

    #[tokio::test]
    async fn focus_change_starts_border_tweens_and_reaches_targets() {
        let (backend, _handle) = mock_backend(PaneId(2));
        let mut app = App::with_backend_for_test(backend, 80, 24, PaneId(1));
        app.execute(Command::SplitH).await.expect("split succeeds");
        assert_eq!(app.focused, Some(PaneId(2)));

        app.execute(Command::FocusUp).await.expect("focus succeeds");

        assert_eq!(app.focused, Some(PaneId(1)));
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(2),
                app.focused,
                app.focused_border_color,
                chrome::UNFOCUSED_BORDER,
            ),
            Color::Cyan
        );
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(1),
                app.focused,
                app.focused_border_color,
                chrome::UNFOCUSED_BORDER,
            ),
            chrome::UNFOCUSED_BORDER
        );

        app.advance_animations(FOCUS_BORDER_TWEEN_DURATION)
            .await
            .expect("animations advance");

        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(1),
                app.focused,
                app.focused_border_color,
                chrome::UNFOCUSED_BORDER,
            ),
            Color::Cyan
        );
        assert_eq!(
            app.timeline.pane_border_color(
                PaneId(2),
                app.focused,
                app.focused_border_color,
                chrome::UNFOCUSED_BORDER,
            ),
            chrome::UNFOCUSED_BORDER
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
        match app
            .root
            .as_ref()
            .expect("root exists")
            .find_leaf(PaneId(2))
            .expect("new pane exists")
        {
            Node::Leaf { rect_current, .. } => {
                assert_eq!(rect_current.x, f32::from(new_target.x));
                assert_eq!(rect_current.y, 0.0);
                assert_eq!(rect_current.w, 0.0);
                assert_eq!(rect_current.h, 24.0);
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
        match app
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

        assert!(app
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
        assert!(app
            .root
            .as_ref()
            .expect("root exists")
            .find_leaf(PaneId(2))
            .is_none());
        assert!(!app.panes.iter().any(|pane| pane.id() == PaneId(2)));
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
    fn launch_args_parse_attach_subcommand() {
        assert_eq!(
            LaunchArgs::parse(["attach", "weave-test"]).expect("launch args parse"),
            LaunchArgs::Attach(AttachArgs {
                session_name: Some("weave-test".to_owned()),
            })
        );
    }

    #[test]
    fn launch_args_reject_native_attach() {
        let error = LaunchArgs::parse(["--backend", "native", "attach"])
            .expect_err("native attach should fail")
            .to_string();

        assert!(error.contains("wv attach [name]"));
    }
}
