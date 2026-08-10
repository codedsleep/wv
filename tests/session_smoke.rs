//! End-to-end coverage for the native session layer: attach, input, detach,
//! reattach, takeover, quit.
//!
//! The whole flow runs in one test because it drives a single session through
//! its lifecycle, and because the socket directory is selected by an
//! environment variable that must not be raced by a parallel test.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio::net::UnixStream;
use tokio::time::timeout;
use weave::app::{App, Args};
use weave::command::Command;
use weave::session::protocol::{
    read_frame, write_frame, write_hello, ClientToServer, ExitReason, ServerToClient,
};
use weave::session::server::SessionServer;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn session_survives_detach_and_serves_one_client_at_a_time() -> anyhow::Result<()> {
    let runtime_dir = test_runtime_dir();
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    // `cat` echoes through the PTY without a prompt, which keeps assertions
    // about pane contents independent of whatever shell the host uses.
    std::env::set_var("SHELL", "/bin/cat");

    let name = "smoke";
    let server = SessionServer::bind(name).context("session binds")?;
    let socket = server.path().to_path_buf();
    let (session_rx, socket_guard) = server.start();

    let args = Args {
        session_name: Some(name.to_owned()),
        ..Args::default()
    };
    let app = App::new(80, 24, &args).into_session(session_rx);
    let session = tokio::spawn(async move { app.run().await });

    // Attach, and confirm the server paints the screen for its new client.
    let mut client = attach(&socket, 80, 24).await?;
    let first_frame = next_frame(&mut client).await?;
    assert!(
        !first_frame.is_empty(),
        "attaching should trigger a full repaint"
    );

    // Input reaches the pane: `cat` echoes it back and it lands in a frame.
    write_frame(
        &mut client,
        &ClientToServer::Input(key_event('z')),
    )
    .await?;
    assert!(
        frame_containing(&mut client, "z").await?,
        "typed input should be echoed back inside a rendered frame"
    );

    // Detach: the server acknowledges, closes the connection, and keeps going.
    write_frame(&mut client, &ClientToServer::Detach).await?;
    assert_eq!(
        expect_message(&mut client).await?,
        ServerToClient::Detached,
        "detach should be acknowledged"
    );
    assert!(
        timeout(STEP_TIMEOUT, read_frame::<_, ServerToClient>(&mut client))
            .await??
            .is_none(),
        "the server should close the connection after a detach"
    );
    assert!(!session.is_finished(), "the session must outlive its client");

    // Reattach: pane state survived, so the echoed character is still there.
    let mut client = attach(&socket, 80, 24).await?;
    assert!(
        frame_containing(&mut client, "z").await?,
        "pane contents should survive a detach"
    );

    // A command sent over the socket animates the session like a keybinding.
    write_frame(
        &mut client,
        &ClientToServer::Exec(Command::parse_str("split-v").expect("command parses")),
    )
    .await?;
    next_frame(&mut client).await?;

    // A second client evicts the first.
    let mut taking_over = attach(&socket, 100, 30).await?;
    assert_eq!(
        expect_message(&mut client).await?,
        ServerToClient::Exit(ExitReason::TakenOver),
        "the previous client should be told why it was dropped"
    );
    next_frame(&mut taking_over).await?;

    // Quit tears the session down.
    write_frame(&mut taking_over, &ClientToServer::Quit).await?;
    timeout(STEP_TIMEOUT, session)
        .await
        .context("session shuts down after quit")???;

    drop(socket_guard);
    let _ = std::fs::remove_dir_all(&runtime_dir);

    Ok(())
}

async fn attach(socket: &std::path::Path, cols: u16, rows: u16) -> anyhow::Result<UnixStream> {
    let mut stream = timeout(STEP_TIMEOUT, weave::session::client::connect(socket)).await??;
    write_hello(&mut stream).await?;
    write_frame(
        &mut stream,
        &ClientToServer::Attach {
            cols,
            rows,
            truecolor: true,
        },
    )
    .await?;

    Ok(stream)
}

/// Read the next message, failing the test on timeout or a closed socket.
async fn expect_message(stream: &mut UnixStream) -> anyhow::Result<ServerToClient> {
    timeout(STEP_TIMEOUT, read_frame::<_, ServerToClient>(stream))
        .await
        .context("timed out waiting for a server message")??
        .context("server closed the connection unexpectedly")
}

/// Read until a rendered frame arrives, returning its bytes.
async fn next_frame(stream: &mut UnixStream) -> anyhow::Result<Vec<u8>> {
    match expect_message(stream).await? {
        ServerToClient::Frame(bytes) => Ok(bytes),
        other => anyhow::bail!("expected a frame, got {other:?}"),
    }
}

/// Whether `needle` shows up in any frame within the step timeout.
///
/// Frames are diffs, so the text may arrive several frames after the input
/// that produced it.
async fn frame_containing(stream: &mut UnixStream, needle: &str) -> anyhow::Result<bool> {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;

    while tokio::time::Instant::now() < deadline {
        let frame = next_frame(stream).await?;
        if String::from_utf8_lossy(&frame).contains(needle) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn key_event(ch: char) -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(ch),
        crossterm::event::KeyModifiers::NONE,
    ))
}

fn test_runtime_dir() -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "weave-session-smoke-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test runtime dir created");

    dir
}
