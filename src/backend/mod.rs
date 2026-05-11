//! `PaneBackend` trait + `PaneId`/`PaneCommand`/`BackendEvent`.

#![allow(dead_code)]

use std::hash::Hash;
use std::path::PathBuf;

pub mod native;
pub mod tmux;

/// Backend-local pane identifier.
///
/// Each backend owns its own `PaneId` space: a `NativeBackend` and a
/// `TmuxBackend` may issue the same numeric ID for different panes. Callers
/// must only pass IDs back to the backend that created them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// Command used to spawn a pane process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

/// Asynchronous backend event for a backend-owned pane.
///
/// `PaneId` values in these events belong to the emitting backend's ID space.
/// Different backend instances can emit the same numeric ID for unrelated
/// panes, so consumers must keep backend ownership alongside the ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    PaneDied(PaneId),
    SpawnFailed(PaneId, String),
}

/// Pane process backend.
///
/// Implementations allocate and own their own `PaneId` space. IDs returned by
/// `spawn` are meaningful only to the backend instance that issued them and
/// must not be mixed with IDs from another backend.
#[async_trait::async_trait]
pub trait PaneBackend: Send {
    async fn spawn(&mut self, cmd: PaneCommand) -> Result<PaneId, anyhow::Error>;

    async fn write(&mut self, id: PaneId, data: &[u8]) -> Result<(), anyhow::Error>;

    async fn resize(&mut self, id: PaneId, cols: u16, rows: u16) -> Result<(), anyhow::Error>;

    async fn kill(&mut self, id: PaneId) -> Result<(), anyhow::Error>;
}
