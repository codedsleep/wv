use std::collections::{HashMap, HashSet};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use weave::app::{App, AttachArgs};
use weave::backend::native::NativeBackend;
use weave::backend::tmux::layout::{parse_layout, LayoutAst};
use weave::backend::tmux::TmuxBackend;
use weave::backend::{BackendEvent, PaneBackend, PaneCommand, PaneId};
use weave::command::Command;
use weave::layout::geometry::Split;
use weave::layout::tree::Node;
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

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn tmux_internal_and_external_layout_commands_match() -> anyhow::Result<()> {
    if ProcessCommand::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not available, skipping parity test");
        return Ok(());
    }

    let internal_session = unique_session_name("internal");
    let external_session = unique_session_name("external");
    let _internal_guard = TmuxSessionGuard::new(internal_session.clone());
    let _external_guard = TmuxSessionGuard::new(external_session.clone());

    create_weave_tmux_session(&internal_session)
        .await
        .context("failed to create internal tmux session")?;
    create_weave_tmux_session(&external_session)
        .await
        .context("failed to create external tmux session")?;

    let mut internal_app = App::attach(
        PANE_WIDTH * 4,
        PANE_HEIGHT + 1,
        AttachArgs {
            session_name: Some(internal_session.clone()),
        },
    )
    .await
    .context("failed to attach internal app")?;
    run_internal_layout_commands(&mut internal_app)
        .await
        .context("failed to run internal layout commands")?;
    let internal_shape = root_shape(internal_app.current_layout_root())?;
    let internal_surface = captured_session_surface(&internal_session)?;

    let mut external_app = App::attach(
        PANE_WIDTH * 4,
        PANE_HEIGHT + 1,
        AttachArgs {
            session_name: Some(external_session.clone()),
        },
    )
    .await
    .context("failed to attach external app")?;
    run_external_layout_commands(&external_session, &mut external_app)
        .await
        .context("failed to run external layout commands")?;
    let external_shape = root_shape(external_app.current_layout_root())?;
    let external_surface = captured_session_surface(&external_session)?;

    assert_surfaces_text_equal(&internal_surface, &external_surface);
    assert_eq!(internal_shape, external_shape);

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

async fn create_weave_tmux_session(session_name: &str) -> anyhow::Result<()> {
    let (output_tx, _output_rx) = mpsc::channel::<(_, Bytes)>(256);
    let (event_tx, _event_rx) = mpsc::channel(64);
    let backend = TmuxBackend::new(Some(session_name.to_owned()), output_tx, event_tx).await?;
    drop(backend);
    Ok(())
}

async fn run_internal_layout_commands(app: &mut App) -> anyhow::Result<()> {
    app.execute(Command::SplitH).await?;
    app.advance_animations_by(Duration::from_secs(1)).await?;
    app.execute(Command::FocusUp).await?;
    app.execute(Command::SplitV).await?;
    app.advance_animations_by(Duration::from_secs(1)).await?;
    app.execute(Command::Close).await?;
    app.advance_animations_by(Duration::from_secs(1)).await
}

async fn run_external_layout_commands(session_name: &str, app: &mut App) -> anyhow::Result<()> {
    let initial = list_tmux_panes(session_name)?
        .into_iter()
        .next()
        .context("expected initial tmux pane")?;
    let _bottom = tmux_output([
        "split-window",
        "-v",
        "-t",
        &initial,
        "-P",
        "-F",
        "#{pane_id}",
    ])?;
    let right = tmux_output([
        "split-window",
        "-h",
        "-t",
        &initial,
        "-P",
        "-F",
        "#{pane_id}",
    ])?;
    tmux_status(["kill-pane", "-t", right.trim()])?;

    let (window_id, layout) = current_tmux_layout(session_name)?;
    app.apply_external_layout_change(window_id, layout).await
}

fn captured_session_surface(session_name: &str) -> anyhow::Result<Surface> {
    write_marker_to_all_panes(session_name)?;
    std::thread::sleep(OUTPUT_DRAIN);

    let panes = list_tmux_panes(session_name)?;
    let mut pane_ids = Vec::with_capacity(panes.len());
    let mut outputs = HashMap::with_capacity(panes.len());

    for (index, pane) in panes.iter().enumerate() {
        let pane_id = PaneId(u64::try_from(index + 1).expect("test pane index fits u64"));
        pane_ids.push(pane_id);
        outputs.insert(pane_id, capture_tmux_pane(pane)?.into_bytes());
    }

    Ok(compose_surface(&pane_ids, &outputs))
}

fn write_marker_to_all_panes(session_name: &str) -> anyhow::Result<()> {
    for pane in list_tmux_panes(session_name)? {
        tmux_status([
            "send-keys",
            "-t",
            &pane,
            "printf '\\033[2J\\033[Habcd'; sleep 5",
            "Enter",
        ])?;
    }
    Ok(())
}

fn capture_tmux_pane(pane: &str) -> anyhow::Result<String> {
    tmux_output(["capture-pane", "-p", "-t", pane]).map(|output| output.replace("\r\n", "\n"))
}

fn list_tmux_panes(session_name: &str) -> anyhow::Result<Vec<String>> {
    let output = tmux_output(["list-panes", "-t", session_name, "-F", "#{pane_id}"])?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn current_tmux_layout(session_name: &str) -> anyhow::Result<(u64, LayoutAst)> {
    let output = tmux_output([
        "display-message",
        "-p",
        "-t",
        session_name,
        "#{window_id}\t#{window_layout}",
    ])?;
    let (window_id, layout) = output
        .trim()
        .split_once('\t')
        .context("tmux layout output did not contain window id and layout")?;
    let window_id = window_id
        .strip_prefix('@')
        .context("tmux window id did not start with @")?
        .parse()
        .context("failed to parse tmux window id")?;
    let layout = parse_layout(layout).context("failed to parse tmux window layout")?;
    Ok((window_id, layout))
}

fn tmux_status<const N: usize>(args: [&str; N]) -> anyhow::Result<()> {
    let status = ProcessCommand::new("tmux")
        .args(args)
        .status()
        .context("failed to run tmux command")?;
    if !status.success() {
        bail!("tmux command exited with {status}");
    }
    Ok(())
}

fn tmux_output<const N: usize>(args: [&str; N]) -> anyhow::Result<String> {
    let output = ProcessCommand::new("tmux")
        .args(args)
        .output()
        .context("failed to run tmux command")?;
    if !output.status.success() {
        bail!(
            "tmux command exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("tmux output was not UTF-8")
}

#[derive(Debug, PartialEq, Eq)]
enum BspShape {
    Leaf,
    Internal {
        split: Split,
        a: Box<BspShape>,
        b: Box<BspShape>,
    },
}

fn root_shape(root: Option<&Node>) -> anyhow::Result<BspShape> {
    root.map(node_shape).context("expected current layout root")
}

fn node_shape(node: &Node) -> BspShape {
    match node {
        Node::Leaf { .. } => BspShape::Leaf,
        Node::Internal { split, a, b, .. } => BspShape::Internal {
            split: *split,
            a: Box::new(node_shape(a)),
            b: Box::new(node_shape(b)),
        },
    }
}

fn unique_session_name(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    format!("weave-e1-{label}-{}-{nanos}", std::process::id())
}

struct TmuxSessionGuard {
    session_name: String,
}

impl TmuxSessionGuard {
    fn new(session_name: String) -> Self {
        Self { session_name }
    }
}

impl Drop for TmuxSessionGuard {
    fn drop(&mut self) {
        let _ = ProcessCommand::new("tmux")
            .args(["kill-session", "-t", &self.session_name])
            .status();
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
