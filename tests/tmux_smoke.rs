#[path = "../src/backend/mod.rs"]
mod backend;
#[path = "../src/layout/mod.rs"]
mod layout;

use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::timeout;

use backend::tmux::TmuxBackend;
use backend::{BackendEvent, PaneBackend, PaneCommand};

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn tmux_spawn_reads_output_and_reports_death() -> anyhow::Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        return Ok(());
    }

    let (output_tx, mut output_rx) = mpsc::channel::<(_, Bytes)>(256);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let mut backend = TmuxBackend::new(None, output_tx, event_tx).await?;

    let id = backend
        .spawn(PaneCommand {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "printf hello; sleep 0.1".to_owned()],
            env: Vec::new(),
            cwd: None,
        })
        .await?;

    timeout(Duration::from_secs(3), async {
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

        Err(anyhow!(
            "output channel closed before tmux pane output arrived"
        ))
    })
    .await??;

    timeout(Duration::from_secs(3), async {
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

    drop(backend);
    Ok(())
}
