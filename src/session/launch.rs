//! Entry points that tie the CLI to the session layer.
//!
//! `wv` starts a server if the session does not exist yet and then attaches to
//! it; `wv --server` is the daemon side of that spawn; `attach`, `ls` and
//! `exec` all work against sockets found in the session directory.

use std::path::Path;
use std::process::Stdio;

use anyhow::Context;

use super::client::{self, ClientOutcome};
use super::paths::{self, SessionEntry};
use super::protocol::CommandResult;
use super::server::{self, SessionServer};
use crate::app::{App, Args};
use crate::command::{Command, Target};
use crate::term::TerminalGuard;

/// Start a session (unless it is already running) and attach to it.
pub async fn start_and_attach(args: &Args) -> anyhow::Result<()> {
    let name = match &args.session_name {
        Some(name) => {
            paths::validate_session_name(name)?;
            name.clone()
        }
        None => paths::generate_session_name(),
    };
    let path = paths::socket_path(&name)?;

    if !paths::is_socket_live(&path) {
        spawn_server_process(&name, args.debug)?;
    }

    attach_to(&name, &path).await
}

/// Create a session without attaching, returning its name.
///
/// Waits for the server to bind so a script can immediately `wv exec` against it.
pub async fn create_bare(args: &Args) -> anyhow::Result<String> {
    let name = match &args.session_name {
        Some(name) => {
            paths::validate_session_name(name)?;
            name.clone()
        }
        None => paths::generate_session_name(),
    };
    let path = paths::socket_path(&name)?;

    if paths::is_socket_live(&path) {
        anyhow::bail!("a weave session named `{name}` is already running");
    }

    spawn_server_process(&name, args.debug)?;
    let stream = client::connect(&path).await?;
    drop(stream);

    Ok(name)
}

/// Attach to an existing session by name, or to the most recent one.
///
/// With `detach_others`, everyone already watching is detached first — tmux's
/// `attach -d`, for when you want the session to yourself.
pub async fn attach(session_name: Option<&str>, detach_others: bool) -> anyhow::Result<()> {
    let session = paths::resolve_session(session_name)?;

    if detach_others {
        let _ = server::request(
            &session.path,
            Command::DetachClient {
                target: None,
                all: false,
            },
        )
        .await?;
    }

    attach_to(&session.name, &session.path).await
}

/// Attach, and keep attaching for as long as the session hands us on.
///
/// The terminal guard is taken once and held across every hop, so switching
/// sessions never gives the terminal back to the shell — the next session's
/// first frame is a full repaint and simply overwrites the last one's.
async fn attach_to(name: &str, path: &Path) -> anyhow::Result<()> {
    let guard = TerminalGuard::new()?;
    let mut name = name.to_owned();
    let mut path = path.to_path_buf();

    let (outcome, name) = loop {
        let stream = client::connect(&path).await?;
        let outcome = client::run(stream, &name).await?;

        let ClientOutcome::Switch {
            name: wanted,
            window,
        } = &outcome
        else {
            break (outcome, name);
        };

        // A session can die between being listed in the picker and being
        // switched to. Report that rather than looping on a dead socket.
        match paths::resolve_session(Some(wanted)) {
            Ok(session) => {
                if let Some(window) = window {
                    // Told to the new server rather than carried in the
                    // handshake: it is an ordinary command, and one that must
                    // land before the first frame is composed.
                    // Both halves of the answer matter. A window listed in the
                    // picker can be gone by the time the jump lands, and the
                    // new server reports that as an ordinary refusal rather
                    // than a broken connection: taken as success it would
                    // attach to whichever window happens to be current and
                    // say nothing about having gone somewhere else.
                    let selected = server::request(
                        &session.path,
                        Command::SelectWindow {
                            target: Target::window(*window),
                            create: false,
                        },
                    )
                    .await;
                    match selected {
                        Ok(CommandResult::Ok { .. }) => {}
                        Ok(CommandResult::Error { message }) => {
                            tracing::warn!(
                                "{wanted} refused window {window}: {message}; \
                                 attaching to its current window instead"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                "could not select window {window} in {wanted}: {error:#}"
                            );
                        }
                    }
                }
                path = session.path;
                name = session.name;
            }
            Err(error) => {
                tracing::warn!("could not switch to {wanted}: {error:#}");
                break (outcome, name);
            }
        }
    };

    // Restore the terminal before the message: it has to land on a real screen,
    // not inside the alternate one.
    drop(guard);
    println!("{}", outcome.message(&name));
    if outcome == ClientOutcome::ConnectionLost {
        anyhow::bail!("session {name} ended unexpectedly; see the weave log for details");
    }

    Ok(())
}

/// Run as the session daemon: own the panes, serve one client at a time.
pub async fn run_server(args: &Args) -> anyhow::Result<()> {
    let name = args
        .session_name
        .clone()
        .context("`--server` requires an explicit `--session <name>`")?;

    let server = SessionServer::bind(&name)?;
    tracing::info!("session {name} listening on {}", server.path().display());
    let (session_rx, socket_guard) = server.start();

    // Size is provisional until a client attaches and reports its terminal.
    // The app is told its own name so a target like `-t other:1` is refused
    // rather than quietly applied here, and where it is listening so
    // `rename-session` can move the socket.
    let app = App::new(80, 24, args)
        .with_session_name(name.clone())
        .with_session_socket(socket_guard.path())
        .into_session(session_rx);
    app.run().await?;
    // The socket path is the only thing that tracks a rename, so read the name
    // back from it: `name` is what the session was called at startup, which is
    // wrong in the log if it was renamed since.
    let final_name = socket_guard
        .path()
        .get()
        .file_stem()
        .map_or_else(|| name.clone(), |stem| stem.to_string_lossy().into_owned());
    tracing::info!("session {final_name} shut down");
    drop(socket_guard);

    Ok(())
}

/// Run a command against a running session, as `wv exec` does.
///
/// Returns the command's result rather than printing it, so the CLI decides
/// how to report it and callers in tests can assert on it.
pub async fn exec(
    session_name: Option<&str>,
    command: Command,
) -> anyhow::Result<CommandResult> {
    let session = paths::resolve_session(session_name)?;

    server::request(&session.path, command).await
}

/// Whether a named session — or any session — is live right now.
///
/// Answered from the socket directory rather than by connecting, so asking
/// about a session that is not there is a plain `false`, not an error.
pub fn has_session(session_name: Option<&str>) -> anyhow::Result<bool> {
    let sessions = paths::list_sessions()?;

    Ok(match session_name {
        Some(name) => sessions.iter().any(|session| session.name == name),
        None => !sessions.is_empty(),
    })
}

/// End every live session.
pub async fn kill_server() -> anyhow::Result<usize> {
    let sessions = paths::list_sessions()?;
    let mut ended = 0;

    for session in &sessions {
        // One unreachable session must not stop the rest from being killed.
        match server::request(
            &session.path,
            Command::KillSession {
                target: crate::command::Target::current(),
            },
        )
        .await
        {
            Ok(_) => ended += 1,
            Err(error) => tracing::warn!("could not end session {}: {error:#}", session.name),
        }
    }

    Ok(ended)
}

/// List live sessions, newest first.
pub fn list() -> anyhow::Result<Vec<SessionEntry>> {
    paths::list_sessions()
}

/// Format `wv ls` output.
pub fn format_sessions(sessions: &[SessionEntry]) -> String {
    if sessions.is_empty() {
        return "no live weave sessions".to_owned();
    }

    let width = sessions
        .iter()
        .map(|session| session.name.len())
        .max()
        .unwrap_or(0);

    sessions
        .iter()
        .map(|session| format!("{:width$}  {}", session.name, session.path.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Re-exec this binary as a detached session server.
///
/// The child keeps running after this process exits: it holds no terminal
/// handles, ignores `SIGINT`/`SIGHUP`, and only stops on `SIGTERM` or when its
/// last pane exits.
fn spawn_server_process(name: &str, debug: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("failed to locate the weave binary")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--server")
        .arg("--session")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if debug {
        command.arg("--debug");
    }

    command
        .spawn()
        .with_context(|| format!("failed to start the session server for `{name}`"))?;
    tracing::info!("spawned session server for {name}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::format_sessions;
    use crate::session::paths::SessionEntry;

    #[test]
    fn formats_an_empty_listing() {
        assert_eq!(format_sessions(&[]), "no live weave sessions");
    }

    #[test]
    fn aligns_session_names() {
        let sessions = vec![
            SessionEntry {
                name: "main".to_owned(),
                path: PathBuf::from("/run/user/1000/weave/main.sock"),
                created_secs: 2,
            },
            SessionEntry {
                name: "weave-1a2b3c4d".to_owned(),
                path: PathBuf::from("/run/user/1000/weave/weave-1a2b3c4d.sock"),
                created_secs: 1,
            },
        ];

        let listing = format_sessions(&sessions);

        assert_eq!(
            listing,
            "main            /run/user/1000/weave/main.sock\n\
             weave-1a2b3c4d  /run/user/1000/weave/weave-1a2b3c4d.sock"
        );
    }
}
