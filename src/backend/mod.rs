//! `PaneBackend` trait + `PaneId`/`PaneCommand`/`BackendEvent`.

#![allow(dead_code)]

use std::hash::Hash;
use std::path::PathBuf;

pub mod native;

/// Backend-local pane identifier.
///
/// Each backend owns its own `PaneId` space: two backend instances may issue
/// the same numeric ID for different panes. Callers must only pass IDs back to
/// the backend that created them.
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

    /// Query the current working directory of a pane.
    ///
    /// Default: returns `Ok(None)` for backends that can't introspect their child shell.
    async fn pane_cwd(&mut self, _pane: PaneId) -> Result<Option<PathBuf>, anyhow::Error> {
        Ok(None)
    }

    /// The name of the process a pane is running.
    ///
    /// This is the pane's own process — the shell, or whatever was spawned in
    /// its place — not the foreground job inside it. A pane running a shell
    /// that is running vim reports the shell. Default: `Ok(None)`.
    async fn pane_process_name(
        &mut self,
        _pane: PaneId,
    ) -> Result<Option<String>, anyhow::Error> {
        Ok(None)
    }

    /// The commands running in a pane's foreground process group, leader first.
    ///
    /// This is what the pane is *running* — the agent, the build, the editor —
    /// as opposed to the shell that launched it, which is what
    /// `pane_process_name` reports. A pane sitting at a prompt reports the
    /// shell, because the shell is then the foreground job.
    ///
    /// Several names rather than one because the group leader is not reliably
    /// the command worth naming: a shell that does no job control keeps the
    /// terminal for itself and runs the real job as a child in the same group.
    /// The caller knows which names it cares about, so the whole group is
    /// offered and it picks. Default: `Ok(Vec::new())`.
    async fn pane_foreground_names(
        &mut self,
        _pane: PaneId,
    ) -> Result<Vec<String>, anyhow::Error> {
        Ok(Vec::new())
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
