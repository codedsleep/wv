//! Session naming and socket discovery.
//!
//! Sockets live in `$XDG_RUNTIME_DIR/weave/`, falling back to a private
//! `/tmp/weave-<user>/` directory when the runtime dir is unset. A socket file
//! whose server is gone is stale: connecting to it fails, and we unlink it so a
//! session name can be reused.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};

const SOCKET_EXTENSION: &str = "sock";
const SESSION_PREFIX: &str = "weave-";

/// Directory holding one socket per live session, created if absent.
pub fn socket_dir() -> anyhow::Result<PathBuf> {
    ensure_socket_dir(resolve_socket_dir(std::env::var_os("XDG_RUNTIME_DIR")))
}

/// Pick the socket directory for a given `XDG_RUNTIME_DIR` value.
fn resolve_socket_dir(runtime_dir: Option<std::ffi::OsString>) -> PathBuf {
    match runtime_dir {
        Some(runtime_dir) if !runtime_dir.is_empty() => PathBuf::from(runtime_dir).join("weave"),
        _ => {
            let user = std::env::var("USER").unwrap_or_else(|_| "default".to_owned());
            std::env::temp_dir().join(format!("weave-{user}"))
        }
    }
}

fn ensure_socket_dir(dir: PathBuf) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create session directory {}", dir.display()))?;
    // Sockets grant full control of a session's panes: keep the directory private.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to restrict permissions on {}", dir.display()))?;

    Ok(dir)
}

/// Socket path for a named session.
pub fn socket_path(name: &str) -> anyhow::Result<PathBuf> {
    validate_session_name(name)?;

    Ok(socket_dir()?.join(format!("{name}.{SOCKET_EXTENSION}")))
}

/// Reject names that would escape the socket directory or confuse listings.
pub fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        bail!("session name `{name}` must contain only letters, digits, `-` and `_`");
    }

    Ok(())
}

/// Generate a `weave-<8 hex>` session name.
///
/// Uniqueness comes from the process id plus wall-clock nanoseconds; a
/// collision only matters if the resulting socket is already live, which the
/// server checks when it binds.
pub fn generate_session_name() -> String {
    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);

    format!("{SESSION_PREFIX}{:08x}", hasher.finish() as u32)
}

/// A session socket found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    pub path: PathBuf,
    /// Socket mtime as seconds since the epoch, used to order listings.
    pub created_secs: i64,
}

/// List live sessions, unlinking sockets whose server has gone away.
///
/// Ordered newest first so `attach` with no name picks the most recent session.
pub fn list_sessions() -> anyhow::Result<Vec<SessionEntry>> {
    let dir = socket_dir()?;
    let mut sessions = Vec::new();

    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read session directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some(SOCKET_EXTENSION) {
            continue;
        }

        let Some(name) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };

        if !is_socket_live(&path) {
            tracing::debug!("removing stale session socket {}", path.display());
            let _ = std::fs::remove_file(&path);
            continue;
        }

        sessions.push(SessionEntry {
            name: name.to_owned(),
            path: path.clone(),
            created_secs: socket_mtime_secs(&path),
        });
    }

    sessions.sort_by(|a, b| b.created_secs.cmp(&a.created_secs).then(a.name.cmp(&b.name)));

    Ok(sessions)
}

/// Whether a socket has a server listening on the other end.
pub fn is_socket_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Resolve the session to attach to: the requested name, or the newest live one.
pub fn resolve_session(requested: Option<&str>) -> anyhow::Result<SessionEntry> {
    let sessions = list_sessions()?;

    match requested {
        Some(name) => {
            validate_session_name(name)?;
            sessions
                .into_iter()
                .find(|session| session.name == name)
                .with_context(|| format!("no live weave session named `{name}`"))
        }
        None => sessions
            .into_iter()
            .next()
            .context("no live weave sessions; start one with `wv`"),
    }
}

fn socket_mtime_secs(path: &Path) -> i64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = metadata.modified() else {
        return 0;
    };

    modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{
        generate_session_name, resolve_socket_dir, socket_dir, socket_path, validate_session_name,
    };

    #[test]
    fn session_names_reject_path_separators_and_junk() {
        assert!(validate_session_name("weave-1a2b3c4d").is_ok());
        assert!(validate_session_name("main_2").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("../escape").is_err());
        assert!(validate_session_name("has space").is_err());
        assert!(validate_session_name("dot.sock").is_err());
    }

    #[test]
    fn generated_names_use_the_weave_prefix_and_hex_suffix() {
        let name = generate_session_name();

        assert!(name.starts_with("weave-"), "unexpected name {name}");
        let suffix = &name["weave-".len()..];
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(validate_session_name(&name).is_ok());
    }

    #[test]
    fn socket_dir_prefers_the_runtime_dir_and_falls_back_to_temp() {
        let runtime = resolve_socket_dir(Some(std::ffi::OsString::from("/run/user/1000")));
        assert_eq!(runtime, std::path::Path::new("/run/user/1000/weave"));

        let unset = resolve_socket_dir(None);
        assert!(unset.starts_with(std::env::temp_dir()), "got {unset:?}");

        let empty = resolve_socket_dir(Some(std::ffi::OsString::new()));
        assert_eq!(empty, unset);
    }

    #[test]
    fn socket_path_lands_inside_the_socket_directory() {
        let path = socket_path("main").expect("socket path resolves");

        assert_eq!(path.parent(), Some(socket_dir().expect("dir").as_path()));
        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("main.sock")
        );
    }
}
