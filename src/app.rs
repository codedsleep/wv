//! `App`: event loop + state owner.

use std::io::{self, Write};

use anyhow::Context;
use bytes::Bytes;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::backend::native::NativeBackend;
use crate::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use crate::command::Command;
use crate::input;
use crate::layout::geometry::{Direction, Rect, Split};
use crate::layout::tree::Node;
use crate::render::diff::DiffRenderer;
use crate::render::{chrome, compositor};
use crate::term::pane::Pane;
use crate::term::surface::Surface;

pub struct App<B = NativeBackend> {
    front: Surface,
    back: Surface,
    panes: Vec<Pane>,
    root: Option<Node>,
    focused: Option<PaneId>,
    backend: B,
    output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: mpsc::Receiver<BackendEvent>,
    stdout: io::Stdout,
    queue_buf: Vec<u8>,
    diff: DiffRenderer,
    dirty: bool,
    quit: bool,
}

impl App<NativeBackend> {
    pub fn new(width: u16, height: u16) -> Self {
        let (backend, output_rx, event_rx) = NativeBackend::new();

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            panes: Vec::new(),
            root: None,
            focused: None,
            backend,
            output_rx,
            event_rx,
            stdout: io::stdout(),
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
            dirty: true,
            quit: false,
        }
    }
}

impl<B> App<B>
where
    B: PaneBackend,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        let pane_id = self
            .spawn_shell_pane()
            .await
            .context("failed to spawn shell pane")?;
        self.root = Some(Node::Leaf {
            pane: pane_id,
            rect: self.root_rect(),
        });
        self.focused = Some(pane_id);
        self.recompute_layout();

        let mut ticks = time::interval(Duration::from_millis(16));
        let mut events = EventStream::new();
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigwinch = signal(SignalKind::window_change())?;

        while !self.quit {
            tokio::select! {
                _ = ticks.tick() => {
                    self.tick()?;
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
                    if should_quit(event.as_ref()) {
                        tracing::info!("ctrl-q received");
                        self.execute(Command::Quit).await?;
                    } else {
                        self.handle_input(event).await;
                    }
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

        for pane_id in self.pane_ids() {
            let _ = self.backend.kill(pane_id).await;
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
            Command::Quit => self.quit = true,
        }

        Ok(())
    }

    async fn spawn_shell_pane(&mut self) -> anyhow::Result<PaneId> {
        let pane_id = self.backend.spawn(default_pane_command()).await?;

        self.backend
            .resize(pane_id, self.back.width, self.back.height)
            .await
            .context("failed to resize shell pane")?;

        self.panes
            .push(Pane::new(pane_id, self.back.width, self.back.height));

        Ok(pane_id)
    }

    async fn split_focused(&mut self, split: Split) -> anyhow::Result<()> {
        let Some(focused) = self.focused else {
            return Ok(());
        };
        let new_pane = self.spawn_shell_pane().await?;

        if let Some(root) = self.root.as_mut() {
            root.split_focused(focused, split, new_pane);
            self.focused = Some(new_pane);
            self.recompute_layout();
            self.dirty = true;
        }

        Ok(())
    }

    fn focus(&mut self, dir: Direction) {
        let Some(focused) = self.focused else {
            return;
        };
        let Some(root) = self.root.as_ref() else {
            return;
        };

        if let Some(next) = root.focus_neighbor(focused, dir) {
            self.focused = Some(next);
            self.dirty = true;
        }
    }

    async fn close_focused(&mut self) -> anyhow::Result<()> {
        let Some(focused) = self.focused else {
            return Ok(());
        };

        self.backend.kill(focused).await?;
        self.remove_pane(focused);

        let Some(root) = self.root.as_mut() else {
            self.focused = None;
            self.quit = true;
            self.dirty = true;
            return Ok(());
        };

        if root.close(focused) {
            self.focused = self.root.as_ref().and_then(first_leaf_pane);
            if self.focused.is_some() {
                self.recompute_layout();
            } else {
                self.root = None;
                self.quit = true;
            }
        } else {
            self.root = None;
            self.focused = None;
            self.quit = true;
        }

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
                        self.quit = true;
                    }
                }
                self.dirty = true;
            }
            BackendEvent::SpawnFailed(id, message) => {
                tracing::error!("pane spawn failed: {id:?}: {message}");
                self.quit = true;
            }
        }
    }

    async fn handle_input(&mut self, event: Option<io::Result<Event>>) {
        let Some(Ok(Event::Key(key))) = event else {
            return;
        };
        let Some(focused) = self.focused else {
            return;
        };

        if let Some(bytes) = input::encode(&key) {
            if let Err(error) = self.backend.write(focused, &bytes).await {
                tracing::warn!("failed to write input to pane: {error:#}");
            }
        }
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

        for pane_id in self.pane_ids() {
            if let Err(error) = self.backend.resize(pane_id, cols, rows).await {
                tracing::warn!("failed to resize backend pane: {error:#}");
            }
        }

        self.dirty = true;
    }

    fn tick(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        self.queue_buf.clear();
        compositor::compose(
            self.root.as_ref(),
            &self.panes,
            self.focused,
            &mut self.back,
        );
        chrome::draw_status_bar(
            &mut self.back,
            "NORMAL",
            chrome::leaf_count(self.root.as_ref()),
            chrono::Local::now(),
        );
        self.diff.flush(&self.front, &self.back, &mut self.stdout)?;
        self.stdout.flush()?;
        std::mem::swap(&mut self.front, &mut self.back);
        self.back.clear();
        self.dirty = false;

        Ok(())
    }

    const fn root_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.back.width,
            h: self.back.height.saturating_sub(1),
        }
    }

    fn recompute_layout(&mut self) {
        let root_rect = self.root_rect();
        if let Some(root) = self.root.as_mut() {
            root.compute_layout(root_rect);
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

    #[cfg(test)]
    fn with_backend_for_test(backend: B, width: u16, height: u16, pane_id: PaneId) -> Self {
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
                rect: root_rect,
            }),
            focused: Some(pane_id),
            backend,
            output_rx,
            event_rx,
            stdout: io::stdout(),
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
            dirty: true,
            quit: false,
        }
    }
}

fn should_quit(event: Option<&io::Result<Event>>) -> bool {
    matches!(
        event,
        Some(Ok(Event::Key(key)))
            if key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('q')
                && key.modifiers.contains(KeyModifiers::CONTROL)
    )
}

fn default_pane_command() -> PaneCommand {
    PaneCommand {
        program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
        args: Vec::new(),
        env: Vec::new(),
        cwd: std::env::current_dir().ok(),
    }
}

fn first_leaf_pane(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf { pane, .. } => Some(*pane),
        Node::Internal { a, b, .. } => first_leaf_pane(a).or_else(|| first_leaf_pane(b)),
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Error;

    use super::App;
    use crate::backend::{PaneBackend, PaneCommand, PaneId};
    use crate::command::Command;
    use crate::layout::geometry::Split;
    use crate::layout::tree::Node;

    struct MockBackend {
        next_id: PaneId,
        resized: Vec<PaneId>,
    }

    #[async_trait::async_trait]
    impl PaneBackend for MockBackend {
        async fn spawn(&mut self, _cmd: PaneCommand) -> Result<PaneId, Error> {
            Ok(self.next_id)
        }

        async fn write(&mut self, _id: PaneId, _data: &[u8]) -> Result<(), Error> {
            Ok(())
        }

        async fn resize(&mut self, id: PaneId, _cols: u16, _rows: u16) -> Result<(), Error> {
            self.resized.push(id);
            Ok(())
        }

        async fn kill(&mut self, _id: PaneId) -> Result<(), Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn execute_split_h_splits_focused_leaf_with_spawned_pane() {
        let mut app = App::with_backend_for_test(
            MockBackend {
                next_id: PaneId(2),
                resized: Vec::new(),
            },
            80,
            24,
            PaneId(1),
        );

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
        assert_eq!(app.backend.resized, vec![PaneId(2)]);
    }
}
