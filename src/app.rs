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
use crate::input;
use crate::render::compositor;
use crate::render::diff::DiffRenderer;
use crate::term::pane::Pane;
use crate::term::surface::Surface;

pub struct App {
    front: Surface,
    back: Surface,
    pane: Pane,
    backend: NativeBackend,
    output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: mpsc::Receiver<BackendEvent>,
    stdout: io::Stdout,
    queue_buf: Vec<u8>,
    diff: DiffRenderer,
}

impl App {
    pub fn new(width: u16, height: u16) -> Self {
        let (backend, output_rx, event_rx) = NativeBackend::new();

        Self {
            front: Surface::new(width, height),
            back: Surface::new(width, height),
            pane: Pane::new(PaneId(0), width, height),
            backend,
            output_rx,
            event_rx,
            stdout: io::stdout(),
            queue_buf: Vec::new(),
            diff: DiffRenderer::new(),
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        let pane_id = self
            .backend
            .spawn(PaneCommand {
                program: shell,
                args: Vec::new(),
                env: Vec::new(),
                cwd: std::env::current_dir().ok(),
            })
            .await
            .context("failed to spawn shell pane")?;

        self.backend
            .resize(pane_id, self.back.width, self.back.height)
            .await
            .context("failed to resize shell pane")?;
        self.pane = Pane::new(pane_id, self.back.width, self.back.height);

        let mut ticks = time::interval(Duration::from_millis(16));
        let mut events = EventStream::new();
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;

        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    self.tick()?;
                }
                Some((id, bytes)) = self.output_rx.recv() => {
                    if id == self.pane.id() {
                        self.pane.process(&bytes);
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    match event {
                        BackendEvent::PaneDied(id) if id == self.pane.id() => {
                            tracing::info!("pane died: {id:?}");
                            break;
                        }
                        BackendEvent::SpawnFailed(id, message) => {
                            tracing::error!("pane spawn failed: {id:?}: {message}");
                            break;
                        }
                        BackendEvent::PaneDied(_) => {}
                    }
                }
                event = events.next() => {
                    if should_quit(event.as_ref()) {
                        tracing::info!("ctrl-q received");
                        break;
                    }
                    self.handle_input(event).await;
                }
                _ = sigint.recv() => {
                    tracing::info!("SIGINT received");
                    break;
                }
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received");
                    break;
                }
            }
        }

        let _ = self.backend.kill(self.pane.id()).await;
        Ok(())
    }

    async fn handle_input(&mut self, event: Option<io::Result<Event>>) {
        let Some(Ok(Event::Key(key))) = event else {
            return;
        };

        if let Some(bytes) = input::encode(&key) {
            if let Err(error) = self.backend.write(self.pane.id(), &bytes).await {
                tracing::warn!("failed to write input to pane: {error:#}");
            }
        }
    }

    fn tick(&mut self) -> io::Result<()> {
        self.queue_buf.clear();
        compositor::compose(std::slice::from_ref(&self.pane), &mut self.back);
        self.diff.flush(&self.front, &self.back, &mut self.stdout)?;
        self.stdout.flush()?;
        std::mem::swap(&mut self.front, &mut self.back);
        self.back.clear();

        Ok(())
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
