//! Deciding whether this weave is the inner one.
//!
//! Two weaves in the same key stream cannot both own `Alt`. The outer one sees
//! every keystroke first, so `Alt+v` splits the outer session and the inner one
//! never learns the key was pressed — the same trap a nested tmux falls into
//! with its prefix. The inner instance therefore moves its leader to
//! `Ctrl+Alt`, which the outer passes through to the pane because its own root
//! table holds bare `Alt`.
//!
//! weave sets no marker in a pane's environment, so there is nothing to read
//! that says "you are inside another weave" directly. What it can read is the
//! shape nesting almost always takes: the inner instance is on the far side of
//! an SSH connection, and OpenSSH does mark that. A local weave inside a local
//! weave is not detected, and does not need to be — the user who did that on
//! purpose can `set -g nested-keys on`.

use std::ffi::OsString;

/// The variables OpenSSH exports into a login shell.
///
/// All three are set for an interactive session; `SSH_TTY` only when a
/// terminal was allocated, which is the case that matters here. Any one of
/// them is enough, because a hardened sshd or a stripped environment may pass
/// on fewer than all three.
const SSH_MARKERS: [&str; 3] = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// Whether the terminal this process is talking to is reached over SSH.
pub fn over_ssh() -> bool {
    detect(|name| std::env::var_os(name))
}

/// The check itself, over an environment lookup, so it can be tested without
/// mutating the process environment.
fn detect(lookup: impl Fn(&str) -> Option<OsString>) -> bool {
    SSH_MARKERS
        .iter()
        // An empty value is what a `SSH_TTY=` in a wrapper script leaves
        // behind; it says the variable was cleared, not that a session exists.
        .any(|name| lookup(name).is_some_and(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::detect;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(*value)))
            .collect();

        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn a_local_terminal_is_not_nested() {
        assert!(!detect(env(&[("TERM", "xterm-256color")])));
    }

    #[test]
    fn any_single_ssh_marker_is_enough() {
        assert!(detect(env(&[("SSH_CONNECTION", "10.0.0.2 51234 10.0.0.1 22")])));
        assert!(detect(env(&[("SSH_CLIENT", "10.0.0.2 51234 22")])));
        assert!(detect(env(&[("SSH_TTY", "/dev/pts/3")])));
    }

    /// A cleared variable is not a connection: `SSH_TTY=` means the wrapper
    /// took it away.
    #[test]
    fn an_empty_marker_does_not_count() {
        assert!(!detect(env(&[("SSH_TTY", ""), ("SSH_CLIENT", "")])));
        assert!(detect(env(&[("SSH_TTY", ""), ("SSH_CLIENT", "10.0.0.2 1 22")])));
    }
}
