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

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn applies_tilerm_session_options() -> anyhow::Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        return Ok(());
    }

    let (output_tx, _output_rx) = mpsc::channel::<(_, Bytes)>(256);
    let (event_tx, _event_rx) = mpsc::channel(64);
    let backend = TmuxBackend::new(None, output_tx, event_tx).await?;
    let session = backend.session_name();

    assert_eq!(show_global_option(session, "prefix")?, "prefix None");
    assert_eq!(show_global_option(session, "prefix2")?, "prefix2 None");
    assert_eq!(
        show_global_option(session, "allow-passthrough")?,
        "allow-passthrough on"
    );
    assert_eq!(
        show_global_option(session, "aggressive-resize")?,
        "aggressive-resize on"
    );

    drop(backend);
    Ok(())
}

fn show_global_option(session: &str, option: &str) -> anyhow::Result<String> {
    let output = Command::new("tmux")
        .args(["show-options", "-g", "-t", session, option])
        .output()?;
    if !output.status.success() {
        bail!(
            "tmux show-options failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn auto_named_startup_cleans_unmarked_weave_orphan() -> anyhow::Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        return Ok(());
    }

    let orphan = "weave-zzz-orphan-test";
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", orphan])
        .status();

    let created = Command::new("tmux")
        .args(["new-session", "-d", "-s", orphan, "sleep 60"])
        .status()?;
    if !created.success() {
        return Ok(());
    }

    let (output_tx, _output_rx) = mpsc::channel::<(_, Bytes)>(256);
    let (event_tx, _event_rx) = mpsc::channel(64);
    let backend = match TmuxBackend::new(None, output_tx, event_tx).await {
        Ok(backend) => backend,
        Err(error) => {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", orphan])
                .status();
            return Err(error);
        }
    };

    timeout(Duration::from_secs(3), async {
        loop {
            if !tmux_has_session(orphan)? {
                return Ok::<(), anyhow::Error>(());
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;

    drop(backend);
    Ok(())
}

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn pane_cwd_returns_focused_shell_path() -> anyhow::Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        return Ok(());
    }

    let (output_tx, _output_rx) = mpsc::channel::<(_, Bytes)>(256);
    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut backend = TmuxBackend::new(None, output_tx, event_tx).await?;
    let pane = backend
        .pane_ids()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("tmux backend did not expose a default pane"))?;
    let session = backend.session_name().to_owned();

    let sent = Command::new("tmux")
        .args([
            "send-keys",
            "-t",
            &format!("{session}:0.0"),
            "cd /tmp",
            "Enter",
        ])
        .status()?;
    if !sent.success() {
        drop(backend);
        return Ok(());
    }

    let expected = std::fs::canonicalize("/tmp")?;
    timeout(Duration::from_secs(3), async {
        loop {
            if let Some(cwd) = backend.pane_cwd(pane).await? {
                if std::fs::canonicalize(cwd)? == expected {
                    return Ok::<(), anyhow::Error>(());
                }
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;

    drop(backend);
    Ok(())
}

#[tokio::test]
#[ignore = "requires tmux on PATH"]
async fn drop_kills_auto_named_session_but_preserves_user_named() -> anyhow::Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        return Ok(());
    }

    {
        let (output_tx, _output_rx) = mpsc::channel::<(_, Bytes)>(256);
        let (event_tx, _event_rx) = mpsc::channel(64);
        let backend = TmuxBackend::new(None, output_tx, event_tx).await?;
        let session = backend.session_name().to_owned();
        drop(backend);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let status = Command::new("tmux")
            .args(["has-session", "-t", &session])
            .status()?;
        assert!(
            !status.success(),
            "auto-named session {session} should be killed by Drop"
        );
    }

    let user_named = format!("weave-test-a10-{}", std::process::id());
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &user_named])
        .status();
    {
        let (output_tx, _output_rx) = mpsc::channel::<(_, Bytes)>(256);
        let (event_tx, _event_rx) = mpsc::channel(64);
        let backend = TmuxBackend::new(Some(user_named.clone()), output_tx, event_tx).await?;
        drop(backend);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let alive = Command::new("tmux")
        .args(["has-session", "-t", &user_named])
        .status()?
        .success();
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &user_named])
        .status();
    assert!(alive, "user-named session {user_named} must survive Drop");

    Ok(())
}

fn tmux_has_session(session: &str) -> anyhow::Result<bool> {
    let status = Command::new("tmux")
        .args(["has-session", "-t", session])
        .status()?;

    Ok(status.success())
}
