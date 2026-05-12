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
