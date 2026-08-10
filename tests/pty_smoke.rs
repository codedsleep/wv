#[path = "../src/backend/mod.rs"]
mod backend;
#[path = "../src/layout/mod.rs"]
mod layout;

use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::time::timeout;

use backend::native::NativeBackend;
use backend::{BackendEvent, PaneBackend, PaneCommand};

#[tokio::test]
async fn spawn_echo_reads_output_and_reports_death() -> anyhow::Result<()> {
    let (mut backend, mut output_rx, mut event_rx) = NativeBackend::new();
    let id = backend
        .spawn(PaneCommand {
            program: "echo".to_owned(),
            args: vec!["hello".to_owned()],
            env: Vec::new(),
            cwd: None,
        })
        .await?;

    timeout(Duration::from_secs(2), async {
        let mut output = Vec::new();

        while let Some((pane_id, bytes)) = output_rx.recv().await {
            if pane_id != id {
                continue;
            }

            output.extend_from_slice(&bytes);
            if output
                .windows(b"hello".len())
                .any(|window| window == b"hello")
            {
                return Ok(());
            }
        }

        Err(anyhow!("output channel closed before echo output arrived"))
    })
    .await??;

    timeout(Duration::from_secs(2), async {
        while let Some(event) = event_rx.recv().await {
            match event {
                BackendEvent::PaneDied(pane_id) if pane_id == id => return Ok(()),
                BackendEvent::SpawnFailed(pane_id, message) if pane_id == id => {
                    bail!("pane spawn failed after successful spawn: {message}");
                }
                _ => {}
            }
        }

        Err(anyhow!("event channel closed before PaneDied arrived"))
    })
    .await??;

    Ok(())
}

/// `pane_cwd` is what makes a new split open where you were working, so it has
/// to read the shell's *current* directory, not the one it started in.
#[tokio::test]
async fn pane_cwd_follows_the_shell_into_a_new_directory() -> anyhow::Result<()> {
    let (mut backend, _output_rx, _event_rx) = NativeBackend::new();
    let id = backend
        .spawn(PaneCommand {
            program: "/bin/sh".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: Some(std::path::PathBuf::from("/")),
        })
        .await?;

    assert_eq!(
        backend.pane_cwd(id).await?,
        Some(std::path::PathBuf::from("/")),
        "a fresh pane reports the directory it was spawned in"
    );

    // Drive the shell into another directory. Waiting on the shell's *output*
    // is a trap: the PTY echoes the command line back, so any marker in the
    // command appears before the command has run. Poll the directory instead.
    backend.write(id, b"cd /tmp\n").await?;
    let moved = timeout(Duration::from_secs(5), async {
        loop {
            if backend.pane_cwd(id).await.ok().flatten()
                == Some(std::path::PathBuf::from("/tmp"))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false);

    assert!(moved, "pane_cwd should track the shell, not the spawn directory");

    backend.kill(id).await?;

    Ok(())
}

/// A pane that has already exited must not report a stale directory, and must
/// not error either: the caller just falls back.
#[tokio::test]
async fn pane_cwd_of_an_unknown_pane_is_none() -> anyhow::Result<()> {
    let (mut backend, _output_rx, _event_rx) = NativeBackend::new();

    assert_eq!(backend.pane_cwd(backend::PaneId(999)).await?, None);

    Ok(())
}
