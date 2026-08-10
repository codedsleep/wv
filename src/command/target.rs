//! tmux-style target addressing: `session:window.pane`.
//!
//! A target names *where* a command acts. Every component is optional and an
//! omitted component means "the current one", so `-t :2` is window 2 of this
//! session and `-t %7` is one specific pane wherever it lives.
//!
//! Grammar, in the order the parser tries it:
//!
//! | Form | Meaning |
//! |---|---|
//! | `%N` | pane by stable id |
//! | `@N` | window by stable id |
//! | `sess:win.pane` | fully qualified; any part may be empty |
//! | `win.pane` | window and pane in this session |
//! | bare token | resolved against the command's [`TargetKind`] |
//!
//! Session names are validated to ASCII letters, digits, `-` and `_`
//! (`session::paths::validate_session_name`), so `:` and `.` never appear
//! inside one and the split is unambiguous.

use serde::{Deserialize, Serialize};

/// What a bare, unpunctuated target token means for a given command.
///
/// `wv kill-pane -t 2` means pane 2, while `wv select-window -t 2` means
/// window 2. tmux resolves the same ambiguity the same way — by the command.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TargetKind {
    Session,
    Window,
    Pane,
}

/// A parsed `session:window.pane` address.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub session: Option<String>,
    pub window: Option<WindowRef>,
    pub pane: Option<PaneRef>,
}

/// How a target names a window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WindowRef {
    /// Positional index as shown in the status bar (`-t :2`).
    Index(u32),
    /// Stable id that survives renumbering (`-t @4`).
    Id(u32),
    /// Window name (`-t :build`).
    Name(String),
    /// `+` or `{next}`.
    Next,
    /// `-` or `{previous}`.
    Previous,
    /// `!` or `{last}`.
    Last,
    /// `{start}` — the lowest-numbered window.
    Start,
    /// `{end}` — the highest-numbered window.
    End,
}

/// How a target names a pane.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaneRef {
    /// Positional index within its window (`-t .1`).
    Index(u32),
    /// Stable id that survives closing other panes (`-t %7`).
    Id(u64),
    /// `+` or `{next}` — next pane in layout order, wrapping.
    Next,
    /// `-` or `{previous}` — previous pane in layout order, wrapping.
    Previous,
    /// `!` or `{last}` — the previously focused pane.
    Last,
    /// `{top}`, `{bottom}`, `{left}`, `{right}` — extreme pane by geometry.
    Extreme(Extreme),
}

/// Geometric extremes addressable as `{top}`, `{bottom}`, `{left}`, `{right}`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Extreme {
    Top,
    Bottom,
    Left,
    Right,
}

/// Why a target string could not be parsed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TargetError {
    #[error("empty target: expected something like `session:window.pane`")]
    Empty,
    #[error("invalid pane id `{0}`: expected `%` followed by a number")]
    PaneId(String),
    #[error("invalid window id `{0}`: expected `@` followed by a number")]
    WindowId(String),
    #[error("invalid pane `{0}`: expected an index, `%id`, `+`, `-`, `!`, or `{{top}}`-style token")]
    Pane(String),
    #[error("unknown target token `{{{0}}}`")]
    Token(String),
    #[error("invalid session name `{0}`: only ASCII letters, digits, hyphens, and underscores are allowed")]
    SessionName(String),
}

impl Target {
    /// A target with every component omitted: "wherever we are now".
    pub fn current() -> Self {
        Self::default()
    }

    /// True when this target names nothing and so means the current pane.
    pub fn is_current(&self) -> bool {
        self.session.is_none() && self.window.is_none() && self.pane.is_none()
    }

    /// Parse a `-t` value. `kind` decides what a bare token like `2` means.
    pub fn parse(value: &str, kind: TargetKind) -> Result<Self, TargetError> {
        if value.is_empty() {
            return Err(TargetError::Empty);
        }

        // `%N` and `@N` are absolute: they carry their own scope, so they are
        // accepted whole regardless of what the command expected.
        if let Some(rest) = value.strip_prefix('%') {
            return Ok(Self {
                pane: Some(PaneRef::Id(parse_number(rest).ok_or_else(|| {
                    TargetError::PaneId(value.to_owned())
                })?)),
                ..Self::default()
            });
        }
        if let Some(rest) = value.strip_prefix('@') {
            let id = parse_number(rest)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| TargetError::WindowId(value.to_owned()))?;
            return Ok(Self {
                window: Some(WindowRef::Id(id)),
                ..Self::default()
            });
        }

        let (session, rest) = match value.split_once(':') {
            Some((session, rest)) => (non_empty(session), rest),
            None => (None, value),
        };
        if let Some(name) = session {
            validate_session_name(name)?;
        }

        // With a session given, everything after `:` addresses a window even
        // when the command wanted a pane, matching tmux's reading of `dev:1`.
        let effective_kind = if session.is_some() && kind == TargetKind::Session {
            TargetKind::Session
        } else {
            kind
        };

        let (window, pane) = parse_window_and_pane(rest, effective_kind)?;

        Ok(Self {
            session: session.map(str::to_owned),
            window,
            pane,
        })
    }
}

fn parse_window_and_pane(
    rest: &str,
    kind: TargetKind,
) -> Result<(Option<WindowRef>, Option<PaneRef>), TargetError> {
    if rest.is_empty() {
        return Ok((None, None));
    }

    if let Some((window, pane)) = rest.split_once('.') {
        let window = match non_empty(window) {
            Some(window) => Some(parse_window(window)?),
            None => None,
        };
        let pane = match non_empty(pane) {
            Some(pane) => Some(parse_pane(pane)?),
            None => None,
        };
        return Ok((window, pane));
    }

    match kind {
        TargetKind::Pane => Ok((None, Some(parse_pane(rest)?))),
        TargetKind::Window | TargetKind::Session => Ok((Some(parse_window(rest)?), None)),
    }
}

fn parse_window(value: &str) -> Result<WindowRef, TargetError> {
    if let Some(rest) = value.strip_prefix('@') {
        let id = parse_number(rest)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| TargetError::WindowId(value.to_owned()))?;
        return Ok(WindowRef::Id(id));
    }

    match value {
        "+" => return Ok(WindowRef::Next),
        "-" => return Ok(WindowRef::Previous),
        "!" => return Ok(WindowRef::Last),
        _ => {}
    }

    if let Some(token) = braced(value) {
        return match token {
            "next" => Ok(WindowRef::Next),
            "previous" | "prev" => Ok(WindowRef::Previous),
            "last" => Ok(WindowRef::Last),
            "start" => Ok(WindowRef::Start),
            "end" => Ok(WindowRef::End),
            other => Err(TargetError::Token(other.to_owned())),
        };
    }

    if let Some(index) = parse_number(value).and_then(|n| u32::try_from(n).ok()) {
        return Ok(WindowRef::Index(index));
    }

    // Anything else is a window name. Names are user data, so they are the
    // fallback rather than a parse error.
    Ok(WindowRef::Name(value.to_owned()))
}

fn parse_pane(value: &str) -> Result<PaneRef, TargetError> {
    if let Some(rest) = value.strip_prefix('%') {
        return Ok(PaneRef::Id(parse_number(rest).ok_or_else(|| {
            TargetError::PaneId(value.to_owned())
        })?));
    }

    match value {
        "+" => return Ok(PaneRef::Next),
        "-" => return Ok(PaneRef::Previous),
        "!" => return Ok(PaneRef::Last),
        _ => {}
    }

    if let Some(token) = braced(value) {
        return match token {
            "next" => Ok(PaneRef::Next),
            "previous" | "prev" => Ok(PaneRef::Previous),
            "last" => Ok(PaneRef::Last),
            "top" => Ok(PaneRef::Extreme(Extreme::Top)),
            "bottom" => Ok(PaneRef::Extreme(Extreme::Bottom)),
            "left" => Ok(PaneRef::Extreme(Extreme::Left)),
            "right" => Ok(PaneRef::Extreme(Extreme::Right)),
            other => Err(TargetError::Token(other.to_owned())),
        };
    }

    parse_number(value)
        .and_then(|n| u32::try_from(n).ok())
        .map(PaneRef::Index)
        .ok_or_else(|| TargetError::Pane(value.to_owned()))
}

/// Reject anything the session layer would refuse to name a socket after, so a
/// bad session in a target fails at parse time rather than at connect time.
fn validate_session_name(name: &str) -> Result<(), TargetError> {
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(TargetError::SessionName(name.to_owned()))
    }
}

fn braced(value: &str) -> Option<&str> {
    value.strip_prefix('{')?.strip_suffix('}')
}

fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Parse a decimal number, rejecting the sign forms `+`/`-` handle themselves.
fn parse_number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Extreme, PaneRef, Target, TargetError, TargetKind, WindowRef};

    fn pane(value: &str) -> Target {
        Target::parse(value, TargetKind::Pane).expect("target parses")
    }

    fn window(value: &str) -> Target {
        Target::parse(value, TargetKind::Window).expect("target parses")
    }

    #[test]
    fn bare_token_follows_the_command_kind() {
        assert_eq!(pane("2").pane, Some(PaneRef::Index(2)));
        assert_eq!(pane("2").window, None);
        assert_eq!(window("2").window, Some(WindowRef::Index(2)));
        assert_eq!(window("2").pane, None);
    }

    #[test]
    fn parses_fully_qualified_targets() {
        let target = pane("dev:build.1");
        assert_eq!(target.session.as_deref(), Some("dev"));
        assert_eq!(target.window, Some(WindowRef::Name("build".to_owned())));
        assert_eq!(target.pane, Some(PaneRef::Index(1)));
    }

    #[test]
    fn omitted_components_stay_none() {
        let target = pane("dev:");
        assert_eq!(target.session.as_deref(), Some("dev"));
        assert_eq!(target.window, None);
        assert_eq!(target.pane, None);

        let target = pane(":2.1");
        assert_eq!(target.session, None);
        assert_eq!(target.window, Some(WindowRef::Index(2)));
        assert_eq!(target.pane, Some(PaneRef::Index(1)));

        let target = pane(".3");
        assert_eq!(target.window, None);
        assert_eq!(target.pane, Some(PaneRef::Index(3)));
    }

    #[test]
    fn absolute_ids_ignore_the_command_kind() {
        assert_eq!(window("%7").pane, Some(PaneRef::Id(7)));
        assert_eq!(window("%7").window, None);
        assert_eq!(pane("@4").window, Some(WindowRef::Id(4)));
        assert_eq!(pane("@4").pane, None);
    }

    #[test]
    fn parses_relative_and_braced_tokens() {
        assert_eq!(pane("+").pane, Some(PaneRef::Next));
        assert_eq!(pane("-").pane, Some(PaneRef::Previous));
        assert_eq!(pane("!").pane, Some(PaneRef::Last));
        assert_eq!(pane("{last}").pane, Some(PaneRef::Last));
        assert_eq!(pane("{top}").pane, Some(PaneRef::Extreme(Extreme::Top)));
        assert_eq!(window("{end}").window, Some(WindowRef::End));
        assert_eq!(window("+").window, Some(WindowRef::Next));
    }

    #[test]
    fn window_names_fall_back_from_indices() {
        assert_eq!(
            window("build").window,
            Some(WindowRef::Name("build".to_owned()))
        );
        assert_eq!(window("2").window, Some(WindowRef::Index(2)));
    }

    #[test]
    fn rejects_malformed_targets() {
        assert_eq!(Target::parse("", TargetKind::Pane), Err(TargetError::Empty));
        assert!(matches!(
            Target::parse("%x", TargetKind::Pane),
            Err(TargetError::PaneId(_))
        ));
        assert!(matches!(
            Target::parse("@x", TargetKind::Window),
            Err(TargetError::WindowId(_))
        ));
        assert!(matches!(
            Target::parse("{sideways}", TargetKind::Pane),
            Err(TargetError::Token(_))
        ));
        // A pane component is never a free-form name: `.build` is a typo, not
        // a pane called "build".
        assert!(matches!(
            Target::parse(".build", TargetKind::Pane),
            Err(TargetError::Pane(_))
        ));
        assert!(matches!(
            Target::parse("bad name:1", TargetKind::Pane),
            Err(TargetError::SessionName(_))
        ));
    }

    #[test]
    fn current_target_is_empty() {
        assert!(Target::current().is_current());
        assert!(!pane("%1").is_current());
    }
}
