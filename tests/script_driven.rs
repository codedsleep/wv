use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{bail, Context};
use weave::app::{App, Args, AttachArgs, BackendKind};
use weave::term::surface::Surface;

const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;

const FINAL_FRAME_GOLDEN: &str = "\
┌──────────────────────────────────────────────────────────────────────────────┐
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
                                                                                
┌──────────────────────────────────────────────────────────────────────────────┐
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘";

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn script_driven_tmux_layout_matches_golden_snapshot() -> anyhow::Result<()> {
    if ProcessCommand::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not available, skipping script-driven test");
        return Ok(());
    }

    let _config_guard = IsolatedConfigGuard::new("script-driven")?;
    let session_name = unique_session_name();
    let _session_guard = TmuxSessionGuard::new(session_name.clone());

    App::create_bare(Args {
        backend: BackendKind::Tmux,
        session_name: Some(session_name.clone()),
        bare: true,
        ..Args::default()
    })
    .await
    .context("failed to create bare weave tmux session")?;

    run_scripted_tmux_sequence(&session_name).context("failed to drive tmux script")?;

    let mut app = App::attach(
        WIDTH,
        HEIGHT,
        AttachArgs {
            session_name: Some(session_name),
        },
    )
    .await
    .context("failed to attach to scripted tmux session")?;
    app.advance_animations_by(Duration::from_secs(1))
        .await
        .context("failed to advance animations")?;

    let frame = surface_chars(&app.compose_current_surface());
    assert_eq!(frame, FINAL_FRAME_GOLDEN);

    Ok(())
}

fn run_scripted_tmux_sequence(session_name: &str) -> anyhow::Result<()> {
    tmux_status([
        "resize-window",
        "-t",
        session_name,
        "-x",
        &WIDTH.to_string(),
        "-y",
        &HEIGHT.to_string(),
    ])?;

    let initial = list_tmux_panes(session_name)?
        .into_iter()
        .next()
        .context("expected bare session to expose an initial pane")?;

    let right = tmux_output([
        "split-window",
        "-h",
        "-t",
        &initial,
        "-P",
        "-F",
        "#{pane_id}",
    ])?;
    tmux_status(["split-window", "-v", "-t", &initial])?;
    tmux_status(["kill-pane", "-t", right.trim()])
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

fn surface_chars(surface: &Surface) -> String {
    let mut out = String::new();
    for y in 0..surface.height {
        if y > 0 {
            out.push('\n');
        }
        for x in 0..surface.width {
            let ch = surface.get(x, y).map_or(' ', |cell| cell.ch);
            out.push(ch);
        }
    }
    out
}

fn unique_session_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    format!("weave-e2-{}-{nanos}", std::process::id())
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

struct IsolatedConfigGuard {
    previous: Option<OsString>,
}

impl IsolatedConfigGuard {
    fn new(label: &str) -> anyhow::Result<Self> {
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        let path = isolated_config_home(label);
        std::fs::create_dir_all(&path)?;
        std::env::set_var("XDG_CONFIG_HOME", path);
        Ok(Self { previous })
    }
}

impl Drop for IsolatedConfigGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var("XDG_CONFIG_HOME", previous);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }
}

fn isolated_config_home(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "weave-{label}-config-{}",
        std::process::id()
    ))
}
