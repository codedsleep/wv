use std::collections::{HashMap, HashSet};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use weave::backend::native::NativeBackend;
use weave::backend::tmux::TmuxBackend;
use weave::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use weave::term::pane::Pane;
use weave::term::surface::Surface;

const PANE_COUNT: usize = 4;
const PANE_WIDTH: u16 = 20;
const PANE_HEIGHT: u16 = 5;
const OUTPUT_DRAIN: Duration = Duration::from_millis(500);

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn native_and_tmux_scripted_outputs_match() -> anyhow::Result<()> {
    if ProcessCommand::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not available, skipping parity test");
        return Ok(());
    }

    let native_output = run_native_scenario().await?;
    let tmux_output = run_tmux_scenario().await?;

    assert_surfaces_text_equal(&native_output, &tmux_output);
    Ok(())
}

async fn run_native_scenario() -> anyhow::Result<Surface> {
    let (backend, output_rx, event_rx) = NativeBackend::new();
    run_scenario(backend, output_rx, event_rx).await
}

async fn run_tmux_scenario() -> anyhow::Result<Surface> {
    let (output_tx, output_rx) = mpsc::channel::<(_, Bytes)>(256);
    let (event_tx, event_rx) = mpsc::channel(64);
    let backend = TmuxBackend::new(None, output_tx, event_tx).await?;

    run_scenario(backend, output_rx, event_rx).await
}

async fn run_scenario<B>(
    mut backend: B,
    mut output_rx: mpsc::Receiver<(PaneId, Bytes)>,
    mut event_rx: mpsc::Receiver<BackendEvent>,
) -> anyhow::Result<Surface>
where
    B: PaneBackend,
{
    let mut pane_ids = Vec::with_capacity(PANE_COUNT);
    let mut outputs = HashMap::new();

    for _ in 0..PANE_COUNT {
        let pane_id = backend.spawn(scripted_command()).await?;
        pane_ids.push(pane_id);
        outputs.insert(pane_id, Vec::new());
    }

    let tracked = pane_ids.iter().copied().collect::<HashSet<_>>();
    drain_until_all_initial_output(&mut output_rx, &mut event_rx, &tracked, &mut outputs).await?;

    backend
        .write(pane_ids[0], b"Z")
        .await
        .context("failed to write scripted input to pane 0")?;
    backend
        .kill(pane_ids[3])
        .await
        .context("failed to close pane 3")?;
    drain_for(
        OUTPUT_DRAIN,
        &mut output_rx,
        &mut event_rx,
        &tracked,
        &mut outputs,
    )
    .await?;

    let surface = compose_surface(&pane_ids, &outputs);

    for pane_id in pane_ids.into_iter().take(PANE_COUNT - 1) {
        let _ = backend.kill(pane_id).await;
    }
    drain_for(
        Duration::from_millis(100),
        &mut output_rx,
        &mut event_rx,
        &tracked,
        &mut outputs,
    )
    .await?;

    Ok(surface)
}

async fn drain_until_all_initial_output(
    output_rx: &mut mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: &mut mpsc::Receiver<BackendEvent>,
    tracked: &HashSet<PaneId>,
    outputs: &mut HashMap<PaneId, Vec<u8>>,
) -> anyhow::Result<()> {
    timeout(Duration::from_secs(3), async {
        loop {
            if tracked.iter().all(|pane_id| {
                outputs
                    .get(pane_id)
                    .is_some_and(|bytes| contains_bytes(bytes, b"abcd"))
            }) {
                return Ok(());
            }

            drain_next(output_rx, event_rx, tracked, outputs).await?;
        }
    })
    .await
    .context("timed out waiting for initial backend output")?
}

async fn drain_for(
    duration: Duration,
    output_rx: &mut mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: &mut mpsc::Receiver<BackendEvent>,
    tracked: &HashSet<PaneId>,
    outputs: &mut HashMap<PaneId, Vec<u8>>,
) -> anyhow::Result<()> {
    let timer = sleep(duration);
    tokio::pin!(timer);

    loop {
        tokio::select! {
            () = &mut timer => return Ok(()),
            result = drain_next(output_rx, event_rx, tracked, outputs) => result?,
        }
    }
}

async fn drain_next(
    output_rx: &mut mpsc::Receiver<(PaneId, Bytes)>,
    event_rx: &mut mpsc::Receiver<BackendEvent>,
    tracked: &HashSet<PaneId>,
    outputs: &mut HashMap<PaneId, Vec<u8>>,
) -> anyhow::Result<()> {
    tokio::select! {
        Some((pane_id, bytes)) = output_rx.recv() => {
            if tracked.contains(&pane_id) {
                outputs
                    .entry(pane_id)
                    .or_default()
                    .extend_from_slice(&bytes);
            }
            Ok(())
        }
        Some(event) = event_rx.recv() => {
            if let BackendEvent::SpawnFailed(pane_id, message) = event {
                if tracked.contains(&pane_id) {
                    bail!("scripted pane spawn failed for {pane_id:?}: {message}");
                }
            }
            Ok(())
        }
        else => Err(anyhow!("backend output and event channels closed")),
    }
}

fn compose_surface(pane_ids: &[PaneId], outputs: &HashMap<PaneId, Vec<u8>>) -> Surface {
    let pane_count = u16::try_from(pane_ids.len()).expect("test pane count fits u16");
    let mut surface = Surface::new(PANE_WIDTH * pane_count, PANE_HEIGHT);

    for (index, pane_id) in pane_ids.iter().copied().enumerate() {
        let mut pane = Pane::new(pane_id, PANE_WIDTH, PANE_HEIGHT);
        if let Some(bytes) = outputs.get(&pane_id) {
            pane.process(bytes);
        }

        let index = u16::try_from(index).expect("test pane index fits u16");
        pane.cells_into(&mut surface, index * PANE_WIDTH, 0);
    }

    surface
}

fn assert_surfaces_text_equal(expected: &Surface, actual: &Surface) {
    assert_eq!(expected.width, actual.width);
    assert_eq!(expected.height, actual.height);

    for y in 0..expected.height {
        for x in 0..expected.width {
            let expected_cell = expected.get(x, y).expect("expected cell exists");
            let actual_cell = actual.get(x, y).expect("actual cell exists");
            assert_eq!(
                expected_cell.ch, actual_cell.ch,
                "text mismatch at ({x},{y}): native {:?}, tmux {:?}",
                expected_cell.ch, actual_cell.ch
            );
        }
    }
}

fn scripted_command() -> PaneCommand {
    PaneCommand {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), "printf abcd; sleep 5".to_owned()],
        env: Vec::new(),
        cwd: None,
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
