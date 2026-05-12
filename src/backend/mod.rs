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
    ActiveWindowChanged { window_id: u64 },
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

    async fn detach(&mut self) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn select_window(&mut self, _workspace_idx: usize) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn select_window_by_id(&mut self, _window_id: u64) -> Result<(), anyhow::Error> {
        Ok(())
    }

    /// Query the current working directory of a pane.
    ///
    /// Default: returns `Ok(None)` for backends that can't introspect their child shell.
    /// Tmux overrides this to return `#{pane_current_path}` for the given pane.
    async fn pane_cwd(&mut self, _pane: PaneId) -> Result<Option<PathBuf>, anyhow::Error> {
        Ok(None)
    }

    async fn ingest_external_pane(&mut self, _tmux_pane_id: u64) -> Result<PaneId, anyhow::Error> {
        anyhow::bail!("external tmux pane ingest is not supported by this backend")
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneBackend, PaneCommand, PaneId};

    struct DefaultCwdBackend;

    #[async_trait::async_trait]
    impl PaneBackend for DefaultCwdBackend {
        async fn spawn(&mut self, _cmd: PaneCommand) -> Result<PaneId, anyhow::Error> {
            Ok(PaneId(1))
        }

        async fn write(&mut self, _id: PaneId, _data: &[u8]) -> Result<(), anyhow::Error> {
            Ok(())
        }

        async fn resize(
            &mut self,
            _id: PaneId,
            _cols: u16,
            _rows: u16,
        ) -> Result<(), anyhow::Error> {
            Ok(())
        }

        async fn kill(&mut self, _id: PaneId) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_pane_cwd_returns_none() {
        let mut backend = DefaultCwdBackend;

        assert_eq!(backend.pane_cwd(PaneId(1)).await.unwrap(), None);
    }
}
