//! Reading a tmux-syntax config file.
//!
//! A config is a list of commands, one per line, in the same language
//! `wv exec` speaks — so this module only has to do three things: split lines
//! into words the way a shell would, expand the `set`/`bind` aliases tmux
//! configs are actually written with, and hand the result to
//! [`crate::command::Command::parse`].
//!
//! ```text
//! # ~/.config/weave/weave.conf
//! set -g prefix C-a
//! unbind C-b
//! bind -n M-h select-pane -L
//! bind '|' split-window -h
//! source-file ~/.config/weave/extra.conf
//! ```
//!
//! Lines weave cannot honour are collected as diagnostics rather than aborting
//! the file: one unsupported option in a long config should not cost you the
//! other forty lines.

use std::path::{Path, PathBuf};

/// One line of a config, already split and de-aliased.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLine {
    pub source: PathBuf,
    pub number: usize,
    pub words: Vec<String>,
}

/// Something in a config file that could not be honoured.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub source: PathBuf,
    pub number: usize,
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.source.display(),
            self.number,
            self.message
        )
    }
}

/// Why a config file could not be read at all.
#[derive(Debug, thiserror::Error)]
pub enum ConfError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}:{number}: {message}")]
    Parse {
        path: PathBuf,
        number: usize,
        message: String,
    },
}

/// How deep `source-file` may nest before we call it a loop.
const MAX_SOURCE_DEPTH: usize = 8;

/// Read a config file, following `source-file`.
pub fn load(path: &Path) -> Result<Vec<ConfigLine>, ConfError> {
    let mut lines = Vec::new();
    load_into(path, 0, &mut lines)?;

    Ok(lines)
}

fn load_into(
    path: &Path,
    depth: usize,
    out: &mut Vec<ConfigLine>,
) -> Result<(), ConfError> {
    if depth > MAX_SOURCE_DEPTH {
        return Err(ConfError::Parse {
            path: path.to_owned(),
            number: 0,
            message: format!("`source-file` nested more than {MAX_SOURCE_DEPTH} deep"),
        });
    }

    let text = std::fs::read_to_string(path).map_err(|source| ConfError::Io {
        path: path.to_owned(),
        source,
    })?;

    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let words = split_words(raw).map_err(|message| ConfError::Parse {
            path: path.to_owned(),
            number,
            message,
        })?;
        let Some(words) = expand_aliases(words) else {
            continue;
        };

        // `source-file` is handled here rather than as a command because it
        // pulls in more config rather than acting on a session.
        if words[0] == "source-file" || words[0] == "source" {
            for target in &words[1..] {
                let nested = resolve_relative(path, target);
                load_into(&nested, depth + 1, out)?;
            }
            continue;
        }

        out.push(ConfigLine {
            source: path.to_owned(),
            number,
            words,
        });
    }

    Ok(())
}

/// `source-file` paths are relative to the file that names them, and `~`
/// means the home directory as it does in a shell.
fn resolve_relative(parent: &Path, target: &str) -> PathBuf {
    let expanded = match target.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map_or_else(|| PathBuf::from(target), |home| PathBuf::from(home).join(rest)),
        None => PathBuf::from(target),
    };

    if expanded.is_absolute() {
        return expanded;
    }

    parent
        .parent()
        .map_or_else(|| expanded.clone(), |dir| dir.join(&expanded))
}

/// Rewrite the short names tmux configs are written with.
///
/// Returns `None` for a line with nothing on it.
fn expand_aliases(mut words: Vec<String>) -> Option<Vec<String>> {
    let first = words.first()?;

    let full = match first.as_str() {
        "set" | "set-option" | "setw" | "set-window-option" => "set-option",
        "setenv" | "set-environment" => "set-environment",
        "bind" | "bind-key" => "bind-key",
        "unbind" | "unbind-key" => "unbind-key",
        "source" | "source-file" => "source-file",
        _ => return Some(words),
    };
    full.clone_into(&mut words[0]);

    Some(words)
}

/// Split a config line into words, honouring quotes and `#` comments.
fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut has_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // A `#` outside quotes starts a comment, but only at a word
            // boundary — `bind '#' ...` and `-F "#{pane_id}"` must survive.
            '#' if quote.is_none() && !has_word => break,
            '\\' => match chars.next() {
                Some(escaped) => {
                    current.push(escaped);
                    has_word = true;
                }
                None => return Err("line ends with a trailing backslash".to_owned()),
            },
            '\'' | '"' if quote.is_none() => {
                quote = Some(ch);
                has_word = true;
            }
            ch if quote == Some(ch) => quote = None,
            ch if ch.is_whitespace() && quote.is_none() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            ch => {
                current.push(ch);
                has_word = true;
            }
        }
    }

    if quote.is_some() {
        return Err("unclosed quote".to_owned());
    }
    if has_word {
        words.push(current);
    }

    Ok(words)
}

/// Where a tmux-syntax config lives.
pub fn conf_path() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join("weave/weave.conf");
    }

    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".config/weave/weave.conf"),
        |home| PathBuf::from(home).join(".config/weave/weave.conf"),
    )
}

#[cfg(test)]
mod tests {
    use super::{expand_aliases, split_words};

    fn words(line: &str) -> Vec<String> {
        split_words(line).expect("line splits")
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(words("set -g prefix C-a"), ["set", "-g", "prefix", "C-a"]);
        assert_eq!(words("   spaced   out  "), ["spaced", "out"]);
        assert!(words("").is_empty());
    }

    #[test]
    fn comments_and_blank_lines_vanish() {
        assert!(words("# a comment").is_empty());
        assert!(words("    # indented").is_empty());
        assert_eq!(words("bind x kill-pane # trailing"), ["bind", "x", "kill-pane"]);
    }

    /// `#` is a real key name and the first character of every format string,
    /// so a comment must only start at a word boundary.
    #[test]
    fn a_hash_inside_a_word_is_not_a_comment() {
        assert_eq!(
            words("display-message -p \"#{pane_id}\""),
            ["display-message", "-p", "#{pane_id}"]
        );
        assert_eq!(words("bind '#' kill-pane"), ["bind", "#", "kill-pane"]);
    }

    #[test]
    fn quotes_group_words() {
        assert_eq!(
            words("bind X new-window -n 'my window'"),
            ["bind", "X", "new-window", "-n", "my window"]
        );
        assert_eq!(words("a \"b c\" d"), ["a", "b c", "d"]);
    }

    #[test]
    fn an_empty_quoted_string_is_still_a_word() {
        assert_eq!(words("set -g status-left ''"), ["set", "-g", "status-left", ""]);
    }

    #[test]
    fn backslashes_escape() {
        assert_eq!(words(r"bind \; kill-pane"), ["bind", ";", "kill-pane"]);
        assert_eq!(words(r"a\ b"), ["a b"]);
    }

    #[test]
    fn unclosed_quotes_are_an_error() {
        assert!(split_words("bind X 'oops").is_err());
        assert!(split_words(r"trailing \").is_err());
    }

    #[test]
    fn aliases_expand_to_full_command_names() {
        for (short, full) in [
            ("set", "set-option"),
            ("setw", "set-option"),
            ("bind", "bind-key"),
            ("unbind", "unbind-key"),
            ("source", "source-file"),
        ] {
            let expanded = expand_aliases(vec![short.to_owned(), "x".to_owned()])
                .expect("a non-empty line");
            assert_eq!(expanded[0], full, "{short} should expand to {full}");
        }
    }

    #[test]
    fn unknown_commands_pass_through_untouched() {
        let expanded =
            expand_aliases(vec!["split-window".to_owned(), "-h".to_owned()]).expect("a line");
        assert_eq!(expanded, ["split-window", "-h"]);
    }
}
