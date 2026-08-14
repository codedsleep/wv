//! The option registry behind `set-option` and `show-options`.
//!
//! A real `.tmux.conf` sets options weave has no equivalent for. Erroring on
//! all of them would make an otherwise-portable config fail on its first line;
//! ignoring them all would let a config quietly not work. So options are in
//! one of three states, and the registry says which:
//!
//! - **live** — weave reads it, and setting it changes behaviour.
//! - **inert** — a real tmux option weave accepts and stores so
//!   `show-options` round-trips, but nothing reads. Setting one logs a warning.
//! - **unknown** — not a tmux option at all. Rejected, because it is a typo.

use std::collections::BTreeMap;

/// What kind of value an option holds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OptionKind {
    Flag,
    Number,
    String,
    Key,
    /// One of a fixed set of words, stored lowercased.
    Choice(&'static [&'static str]),
}

/// Whether weave actually reads an option.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OptionStatus {
    Live,
    /// Accepted and stored, but nothing reads it. The reason is for the
    /// warning, so a user learns why their config line did nothing.
    Inert(&'static str),
}

#[derive(Debug)]
pub struct OptionSpec {
    pub name: &'static str,
    pub kind: OptionKind,
    pub status: OptionStatus,
    pub default: &'static str,
}

/// What `nested-keys` accepts.
pub const NESTED_KEYS_VALUES: &[&str] = &["auto", "on", "off"];

/// Every option weave will accept.
pub const OPTIONS: &[OptionSpec] = &[
    // --- live ---
    OptionSpec {
        name: "prefix",
        kind: OptionKind::Key,
        status: OptionStatus::Live,
        default: "C-b",
    },
    OptionSpec {
        name: "prefix2",
        kind: OptionKind::Key,
        status: OptionStatus::Live,
        default: "",
    },
    // Whether weave's own chords hang off Ctrl instead of Alt, so a weave
    // inside another weave can still be driven. `auto` looks at whether the
    // attached terminal is on the far side of an SSH connection.
    OptionSpec {
        name: "nested-keys",
        kind: OptionKind::Choice(NESTED_KEYS_VALUES),
        status: OptionStatus::Live,
        default: "auto",
    },
    // The prefix key to use while nested. The usual `C-b` is no good there:
    // the outer weave is bound to it and consumes it first. Set it empty to
    // keep `prefix` even when nested.
    OptionSpec {
        name: "nested-prefix",
        kind: OptionKind::Key,
        status: OptionStatus::Live,
        default: "C-a",
    },
    OptionSpec {
        name: "status",
        kind: OptionKind::Flag,
        status: OptionStatus::Live,
        default: "on",
    },
    OptionSpec {
        name: "pane-border-status",
        kind: OptionKind::Flag,
        status: OptionStatus::Live,
        default: "off",
    },
    OptionSpec {
        name: "repeat-time",
        kind: OptionKind::Number,
        status: OptionStatus::Live,
        default: "500",
    },
    OptionSpec {
        name: "default-shell",
        kind: OptionKind::String,
        status: OptionStatus::Live,
        default: "",
    },
    OptionSpec {
        name: "automatic-rename",
        kind: OptionKind::Flag,
        status: OptionStatus::Live,
        default: "on",
    },
    // weave's own, spelled tmux-style so one config can set everything.
    OptionSpec {
        name: "target-fps",
        kind: OptionKind::Number,
        status: OptionStatus::Live,
        default: "160",
    },
    // --- inert: real tmux options weave has no behaviour for ---
    OptionSpec {
        name: "base-index",
        kind: OptionKind::Number,
        status: OptionStatus::Inert("weave's windows are fixed slots numbered 1-9"),
        default: "1",
    },
    OptionSpec {
        name: "pane-base-index",
        kind: OptionKind::Number,
        status: OptionStatus::Inert("pane indices always start at 0"),
        default: "0",
    },
    OptionSpec {
        name: "history-limit",
        kind: OptionKind::Number,
        status: OptionStatus::Inert("weave keeps no scrollback"),
        default: "0",
    },
    OptionSpec {
        name: "mouse",
        kind: OptionKind::Flag,
        status: OptionStatus::Inert("mouse support is not implemented yet"),
        default: "off",
    },
    OptionSpec {
        name: "escape-time",
        kind: OptionKind::Number,
        status: OptionStatus::Inert("weave reads keys through crossterm, which times escapes itself"),
        default: "500",
    },
    OptionSpec {
        name: "default-command",
        kind: OptionKind::String,
        status: OptionStatus::Inert("panes run the shell or an explicit command"),
        default: "",
    },
    OptionSpec {
        name: "status-left",
        kind: OptionKind::String,
        status: OptionStatus::Inert("the status bar is not format-driven yet"),
        default: "",
    },
    OptionSpec {
        name: "status-right",
        kind: OptionKind::String,
        status: OptionStatus::Inert("the status bar is not format-driven yet"),
        default: "",
    },
    OptionSpec {
        name: "status-style",
        kind: OptionKind::String,
        status: OptionStatus::Inert("status colours come from the theme"),
        default: "",
    },
    OptionSpec {
        name: "status-position",
        kind: OptionKind::String,
        status: OptionStatus::Inert("the status bar is always on the bottom row"),
        default: "bottom",
    },
    OptionSpec {
        name: "status-interval",
        kind: OptionKind::Number,
        status: OptionStatus::Inert("the status bar redraws every frame"),
        default: "1",
    },
    OptionSpec {
        name: "renumber-windows",
        kind: OptionKind::Flag,
        status: OptionStatus::Inert("window indices are fixed slots and never shift"),
        default: "off",
    },
    OptionSpec {
        name: "aggressive-resize",
        kind: OptionKind::Flag,
        status: OptionStatus::Inert("weave has one client, so panes always match it"),
        default: "off",
    },
    OptionSpec {
        name: "set-titles",
        kind: OptionKind::Flag,
        status: OptionStatus::Inert("weave does not set the host terminal's title"),
        default: "off",
    },
    OptionSpec {
        name: "mode-keys",
        kind: OptionKind::String,
        status: OptionStatus::Inert("copy mode is out of scope"),
        default: "emacs",
    },
    OptionSpec {
        name: "status-keys",
        kind: OptionKind::String,
        status: OptionStatus::Inert("weave has no command prompt"),
        default: "emacs",
    },
    // Whether the status bar draws its segments with the powerline glyphs.
    // They live in the private-use area, so a terminal without a patched font
    // renders them as tofu; turning this off falls back to plain separators.
    OptionSpec {
        name: "status-powerline",
        kind: OptionKind::Flag,
        status: OptionStatus::Live,
        default: "on",
    },
    // Agent status. Not tmux options — weave-specific.
    OptionSpec {
        name: "agent-status",
        kind: OptionKind::Flag,
        status: OptionStatus::Live,
        default: "on",
    },
    OptionSpec {
        name: "agent-commands",
        kind: OptionKind::String,
        status: OptionStatus::Live,
        default: "claude,codex,opencode",
    },
    // How long after its last output an agent still counts as working. Long
    // enough to bridge the gaps an agent leaves while it thinks, short enough
    // that a finished one goes grey while you are still looking.
    OptionSpec {
        name: "agent-activity-time",
        kind: OptionKind::Number,
        status: OptionStatus::Live,
        default: "2000",
    },
    // Ring the terminal bell when an agent stops. The host terminal decides
    // what that is — a sound, a flash, a desktop notification — which is the
    // point: weave does not need an audio device to be heard.
    OptionSpec {
        name: "agent-bell",
        kind: OptionKind::Flag,
        status: OptionStatus::Live,
        default: "on",
    },
    // How long an agent has to have been working before stopping is worth a
    // bell. The screen is the activity signal, and a footer repainting its
    // clock moves it for one poll, which would otherwise read as a whole turn
    // beginning and ending. Long enough to rule those out, short enough that a
    // quick answer still rings.
    OptionSpec {
        name: "agent-minimum-run",
        kind: OptionKind::Number,
        status: OptionStatus::Live,
        default: "3000",
    },
    // Text that means an agent has stopped to ask you something. Matched
    // case-insensitively against the bottom of the pane; the defaults cover
    // the prompts Claude Code and Codex stop at.
    OptionSpec {
        name: "agent-waiting-patterns",
        kind: OptionKind::String,
        status: OptionStatus::Live,
        default: "do you want,(y/n),(y/N),proceed?,continue?,esc to cancel",
    },
    // Text that means an agent is still mid-turn, however still its screen is.
    // A tool call that takes a minute prints nothing while it runs, which the
    // activity window alone reads as the turn having ended — so the footer the
    // agent shows while it can still be interrupted is believed over the
    // silence. Matched case-insensitively against the bottom of the pane, like
    // `agent-waiting-patterns`.
    OptionSpec {
        name: "agent-working-patterns",
        kind: OptionKind::String,
        status: OptionStatus::Live,
        default: "to interrupt,esc to stop",
    },
    // Text that means the pane is showing an agent's own viewer — a transcript
    // being scrolled through — rather than its live state. History is full of
    // questions that were answered long ago, and scrolling moves the screen
    // exactly like output does, so both signals lie there; while the bottom of
    // the pane matches, the last live reading stands instead of a new one.
    // The defaults cover Claude Code's ctrl+o transcript and Codex's overlay.
    OptionSpec {
        name: "agent-viewer-patterns",
        kind: OptionKind::String,
        status: OptionStatus::Live,
        default: "showing detailed transcript,home/end to jump",
    },
];

pub fn spec(name: &str) -> Option<&'static OptionSpec> {
    OPTIONS.iter().find(|option| option.name == name)
}

/// Why an option could not be set.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OptionError {
    #[error("unknown option `{0}`")]
    Unknown(String),
    #[error("`{name}` expects {expected}, not `{value}`")]
    BadValue {
        name: String,
        value: String,
        expected: String,
    },
}

/// The options a session is running with.
#[derive(Clone, Debug)]
pub struct Options {
    values: BTreeMap<&'static str, String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            values: OPTIONS
                .iter()
                .map(|option| (option.name, option.default.to_owned()))
                .collect(),
        }
    }
}

impl Options {
    /// Validate and store a value, returning the spec so the caller can apply
    /// live options and warn about inert ones.
    pub fn set(&mut self, name: &str, value: &str) -> Result<&'static OptionSpec, OptionError> {
        let spec = spec(name).ok_or_else(|| OptionError::Unknown(name.to_owned()))?;
        let normalized = normalize(spec, value)?;
        self.values.insert(spec.name, normalized);

        Ok(spec)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn flag(&self, name: &str) -> bool {
        matches!(self.get(name), Some("on"))
    }

    pub fn number(&self, name: &str) -> Option<u64> {
        self.get(name)?.parse().ok()
    }

    /// `name value` lines, for `show-options`.
    pub fn show(&self) -> Vec<String> {
        self.values
            .iter()
            .map(|(name, value)| format!("{name} {value}"))
            .collect()
    }
}

/// Check a value against its option's kind, normalising as tmux does.
fn normalize(spec: &'static OptionSpec, value: &str) -> Result<String, OptionError> {
    let bad = |expected: &str| OptionError::BadValue {
        name: spec.name.to_owned(),
        value: value.to_owned(),
        expected: expected.to_owned(),
    };

    match spec.kind {
        OptionKind::Choice(allowed) => {
            let lowered = value.to_ascii_lowercase();
            if allowed.contains(&lowered.as_str()) {
                Ok(lowered)
            } else {
                Err(bad(&list_words(allowed)))
            }
        }
        OptionKind::Flag => match value {
            // tmux accepts several spellings for each; store one.
            "on" | "yes" | "1" | "true" => Ok("on".to_owned()),
            "off" | "no" | "0" | "false" => Ok("off".to_owned()),
            _ => Err(bad("on or off")),
        },
        OptionKind::Number => value
            .parse::<u64>()
            .map(|number| number.to_string())
            .map_err(|_| bad("a number")),
        OptionKind::String | OptionKind::Key => Ok(value.to_owned()),
    }
}

/// `["auto", "on", "off"]` → `auto, on or off`, for the error message.
fn list_words(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::{spec, OptionError, OptionStatus, Options};

    #[test]
    fn flags_accept_the_spellings_tmux_does() {
        let mut options = Options::default();

        for on in ["on", "yes", "1", "true"] {
            options.set("status", on).expect("valid flag");
            assert!(options.flag("status"), "{on} should be on");
        }
        for off in ["off", "no", "0", "false"] {
            options.set("status", off).expect("valid flag");
            assert!(!options.flag("status"), "{off} should be off");
        }
    }

    #[test]
    fn a_bad_flag_value_says_what_it_wanted() {
        let mut options = Options::default();

        let error = options.set("status", "maybe").expect_err("not a flag");
        assert_eq!(
            error,
            OptionError::BadValue {
                name: "status".to_owned(),
                value: "maybe".to_owned(),
                expected: "on or off".to_owned(),
            }
        );
    }

    #[test]
    fn nested_keys_takes_auto_on_or_off_and_says_so() {
        let mut options = Options::default();

        assert_eq!(options.get("nested-keys"), Some("auto"));
        options.set("nested-keys", "ON").expect("a listed value");
        assert_eq!(options.get("nested-keys"), Some("on"));

        let error = options
            .set("nested-keys", "sometimes")
            .expect_err("not a listed value")
            .to_string();
        assert!(error.contains("auto, on or off"), "{error}");
    }

    #[test]
    fn numbers_are_validated() {
        let mut options = Options::default();

        options.set("repeat-time", "300").expect("valid number");
        assert_eq!(options.number("repeat-time"), Some(300));
        assert!(options.set("repeat-time", "soon").is_err());
    }

    /// A typo must fail; a real tmux option weave ignores must not.
    #[test]
    fn unknown_options_are_rejected_but_inert_ones_are_accepted() {
        let mut options = Options::default();

        assert!(matches!(
            options.set("statuss", "on"),
            Err(OptionError::Unknown(_))
        ));

        let applied = options.set("mouse", "on").expect("a real tmux option");
        assert!(matches!(applied.status, OptionStatus::Inert(_)));
        assert_eq!(options.get("mouse"), Some("on"));
    }

    #[test]
    fn every_default_is_valid_for_its_own_kind() {
        let mut options = Options::default();

        for option in super::OPTIONS {
            if option.default.is_empty() {
                continue;
            }
            options
                .set(option.name, option.default)
                .unwrap_or_else(|error| panic!("{}: {error}", option.name));
        }
    }

    #[test]
    fn show_options_round_trips_what_was_set() {
        let mut options = Options::default();
        options.set("prefix", "C-a").expect("valid key");

        let shown = options.show();
        assert!(
            shown.iter().any(|line| line == "prefix C-a"),
            "{shown:?}"
        );
    }

    #[test]
    fn inert_options_carry_a_reason() {
        let history = spec("history-limit").expect("known option");
        match history.status {
            OptionStatus::Inert(reason) => assert!(reason.contains("scrollback"), "{reason}"),
            OptionStatus::Live => panic!("history-limit does nothing yet"),
        }
    }
}
