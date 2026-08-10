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
        default: "on",
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
        expected: &'static str,
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
    let bad = |expected| OptionError::BadValue {
        name: spec.name.to_owned(),
        value: value.to_owned(),
        expected,
    };

    match spec.kind {
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
                expected: "on or off",
            }
        );
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
