//! Commands: the shared vocabulary of keybindings, `wv exec`, and the wire.
//!
//! A [`Command`] is parsed from argv (`wv exec split-window -h -t dev:1`) and
//! travels to the session server as a serialized value, so the parser lives
//! here rather than in the CLI: a keybinding and a script must mean exactly
//! the same thing.
//!
//! # tmux names and weave aliases
//!
//! Each command has a tmux name plus the short weave name it had before, kept
//! as an alias so existing scripts and keybindings keep working. See
//! `docs/TMUX_PARITY.md` for the full matrix.

pub mod target;

use serde::{Deserialize, Serialize};

use crate::layout::geometry::{Direction, Split};
pub use target::{Extreme, PaneRef, Target, TargetError, TargetKind, WindowRef};

/// A command, with everything it needs to run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Split a pane in two and spawn a shell in the new half.
    SplitWindow { split: Split, target: Target },
    /// Move focus.
    SelectPane { selector: PaneSelector },
    /// Switch to another window (today: a workspace).
    SelectWindow { target: Target },
    /// Close a pane.
    KillPane { target: Target },
    /// Leave the session running and disconnect the client.
    DetachClient,
    /// Shut the session down, killing every pane.
    KillSession { target: Target },
}

/// How `select-pane` picks its new focus.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaneSelector {
    /// Geometric neighbour: `-L`, `-R`, `-U`, `-D`.
    Direction(Direction),
    /// An addressed pane: `-t`.
    Target(Target),
    /// The previously focused pane: `-l`.
    Last,
}

/// Why an argv could not be turned into a [`Command`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CommandError {
    #[error("no command given")]
    Empty,
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    #[error("`{command}` does not accept `{flag}`")]
    UnknownFlag { command: String, flag: String },
    #[error("`{command} {flag}` is not supported yet ({plan})")]
    UnsupportedFlag {
        command: String,
        flag: String,
        plan: String,
    },
    #[error("missing value for `{flag}`")]
    MissingValue { flag: String },
    #[error("`{command}` does not take the argument `{argument}`")]
    UnexpectedArgument { command: String, argument: String },
    #[error("`{command}` accepts only one of {flags}")]
    ConflictingFlags { command: String, flags: String },
    #[error("invalid target for `{command}`: {source}")]
    Target {
        command: String,
        #[source]
        source: TargetError,
    },
}

/// Every command name and alias, for error messages and completion.
pub const COMMAND_NAMES: &[&str] = &[
    "split-window",
    "select-pane",
    "select-window",
    "kill-pane",
    "detach-client",
    "kill-session",
];

/// The pre-target weave names, still accepted.
pub const ALIAS_NAMES: &[&str] = &[
    "split-h",
    "split-v",
    "focus-left",
    "focus-right",
    "focus-up",
    "focus-down",
    "close",
    "detach",
    "quit",
    "workspace-1..9",
];

impl Command {
    /// Parse a full argv, starting at the command name.
    pub fn parse<I, S>(argv: I) -> Result<Self, CommandError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let argv = argv
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<_>>();
        let (name, rest) = argv.split_first().ok_or(CommandError::Empty)?;

        match name.as_str() {
            "split-window" | "splitw" => parse_split_window(name, rest, None),
            // The weave aliases pin the axis, so `-h`/`-v` are not accepted
            // alongside them: `split-h` already said which way.
            "split-h" => parse_split_window(name, rest, Some(Split::Horizontal)),
            "split-v" => parse_split_window(name, rest, Some(Split::Vertical)),

            "select-pane" | "selectp" => parse_select_pane(name, rest, None),
            "focus-left" => parse_select_pane(name, rest, Some(Direction::Left)),
            "focus-right" => parse_select_pane(name, rest, Some(Direction::Right)),
            "focus-up" => parse_select_pane(name, rest, Some(Direction::Up)),
            "focus-down" => parse_select_pane(name, rest, Some(Direction::Down)),

            "select-window" | "selectw" => parse_select_window(name, rest, None),

            "kill-pane" | "killp" | "close" => Ok(Self::KillPane {
                target: parse_target_only(name, rest, TargetKind::Pane)?,
            }),

            "detach-client" | "detach" => {
                reject_extra_args(name, rest)?;
                Ok(Self::DetachClient)
            }

            "kill-session" | "quit" => Ok(Self::KillSession {
                target: parse_target_only(name, rest, TargetKind::Session)?,
            }),

            other => parse_workspace_alias(other)
                .ok_or_else(|| CommandError::UnknownCommand(other.to_owned())),
        }
    }

    /// Parse a whitespace-separated command line.
    ///
    /// Convenience for config files and tests; `wv exec` uses [`Command::parse`]
    /// so the shell keeps ownership of quoting.
    pub fn parse_str(line: &str) -> Result<Self, CommandError> {
        Self::parse(line.split_whitespace())
    }
}

/// tmux's `-h`/`-v` name how the panes end up sitting; weave's [`Split`] names
/// the axis being divided. They are opposites, and getting this backwards is
/// the classic porting bug: `split-window -h` puts panes **side by side**,
/// which divides the width, which weave calls [`Split::Vertical`].
const fn split_from_tmux_flag(horizontal: bool) -> Split {
    if horizontal {
        Split::Vertical
    } else {
        Split::Horizontal
    }
}

fn parse_split_window(
    name: &str,
    args: &[String],
    fixed: Option<Split>,
) -> Result<Command, CommandError> {
    let mut split = fixed;
    let mut target = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "-v" if fixed.is_some() => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: arg.clone(),
                });
            }
            "-h" => set_split(name, &mut split, Split::Vertical)?,
            "-v" => set_split(name, &mut split, Split::Horizontal)?,
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Pane)?),
            // Sizing, cwd, detached-spawn and shell commands all land in later
            // PRs; say so rather than silently ignoring them.
            "-p" | "-l" | "-f" => return Err(unsupported(name, arg, "PR 5: pane sizing")),
            "-c" | "-d" | "-b" => {
                return Err(unsupported(name, arg, "PR 3: spawn control"));
            }
            other if other.starts_with('-') => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                return Err(CommandError::UnsupportedFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                    plan: "PR 3: running a command in the new pane".to_owned(),
                });
            }
        }
    }

    Ok(Command::SplitWindow {
        // tmux defaults to a stacked split when neither flag is given.
        split: split.unwrap_or_else(|| split_from_tmux_flag(false)),
        target: target.unwrap_or_default(),
    })
}

fn set_split(name: &str, slot: &mut Option<Split>, split: Split) -> Result<(), CommandError> {
    if slot.is_some_and(|existing| existing != split) {
        return Err(CommandError::ConflictingFlags {
            command: name.to_owned(),
            flags: "`-h` and `-v`".to_owned(),
        });
    }
    *slot = Some(split);
    Ok(())
}

fn parse_select_pane(
    name: &str,
    args: &[String],
    fixed: Option<Direction>,
) -> Result<Command, CommandError> {
    let mut selector = fixed.map(PaneSelector::Direction);
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        let next = match arg.as_str() {
            "-L" => PaneSelector::Direction(Direction::Left),
            "-R" => PaneSelector::Direction(Direction::Right),
            "-U" => PaneSelector::Direction(Direction::Up),
            "-D" => PaneSelector::Direction(Direction::Down),
            "-l" => PaneSelector::Last,
            "-t" => PaneSelector::Target(next_target(name, &mut args, "-t", TargetKind::Pane)?),
            "-Z" => return Err(unsupported(name, arg, "PR 5: zoom")),
            "-T" | "-P" | "-g" => {
                return Err(unsupported(name, arg, "PR 7: pane titles and styles"));
            }
            other if other.starts_with('-') => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                return Err(CommandError::UnexpectedArgument {
                    command: name.to_owned(),
                    argument: other.to_owned(),
                });
            }
        };

        if selector.replace(next).is_some() {
            return Err(CommandError::ConflictingFlags {
                command: name.to_owned(),
                flags: "`-L`, `-R`, `-U`, `-D`, `-l` and `-t`".to_owned(),
            });
        }
    }

    Ok(Command::SelectPane {
        // A bare `select-pane` addresses the current pane, which is a no-op
        // now but becomes meaningful once `-Z` and `-T` land.
        selector: selector.unwrap_or_else(|| PaneSelector::Target(Target::current())),
    })
}

fn parse_select_window(
    name: &str,
    args: &[String],
    fixed: Option<Target>,
) -> Result<Command, CommandError> {
    let mut target = fixed;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        let next = match arg.as_str() {
            "-t" => next_target(name, &mut args, "-t", TargetKind::Window)?,
            "-n" => window_target(WindowRef::Next),
            "-p" => window_target(WindowRef::Previous),
            "-l" => window_target(WindowRef::Last),
            "-T" => return Err(unsupported(name, arg, "PR 5: zoom toggle")),
            other if other.starts_with('-') => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                return Err(CommandError::UnexpectedArgument {
                    command: name.to_owned(),
                    argument: other.to_owned(),
                });
            }
        };

        if target.replace(next).is_some() {
            return Err(CommandError::ConflictingFlags {
                command: name.to_owned(),
                flags: "`-t`, `-n`, `-p` and `-l`".to_owned(),
            });
        }
    }

    Ok(Command::SelectWindow {
        target: target.unwrap_or_default(),
    })
}

/// `workspace-1` .. `workspace-9`, the pre-window way to switch.
fn parse_workspace_alias(name: &str) -> Option<Command> {
    let rest = name.strip_prefix("workspace-")?;
    let index = match rest.as_bytes() {
        [digit @ b'1'..=b'9'] => u32::from(digit - b'0'),
        _ => return None,
    };

    Some(Command::SelectWindow {
        target: Target {
            window: Some(WindowRef::Index(index)),
            ..Target::default()
        },
    })
}

fn window_target(window: WindowRef) -> Target {
    Target {
        window: Some(window),
        ..Target::default()
    }
}

fn parse_target_only(
    name: &str,
    args: &[String],
    kind: TargetKind,
) -> Result<Target, CommandError> {
    let mut target = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => {
                if target
                    .replace(next_target(name, &mut args, "-t", kind)?)
                    .is_some()
                {
                    return Err(CommandError::ConflictingFlags {
                        command: name.to_owned(),
                        flags: "`-t`".to_owned(),
                    });
                }
            }
            "-a" => return Err(unsupported(name, arg, "PR 5: kill all but the target")),
            other if other.starts_with('-') => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                return Err(CommandError::UnexpectedArgument {
                    command: name.to_owned(),
                    argument: other.to_owned(),
                });
            }
        }
    }

    Ok(target.unwrap_or_default())
}

fn reject_extra_args(name: &str, args: &[String]) -> Result<(), CommandError> {
    match args.first() {
        None => Ok(()),
        Some(arg) if arg == "-a" || arg == "-P" || arg == "-t" => {
            Err(unsupported(name, arg, "PR 10: multi-client attach"))
        }
        Some(arg) if arg.starts_with('-') => Err(CommandError::UnknownFlag {
            command: name.to_owned(),
            flag: arg.clone(),
        }),
        Some(arg) => Err(CommandError::UnexpectedArgument {
            command: name.to_owned(),
            argument: arg.clone(),
        }),
    }
}

fn next_target<'a, I>(
    command: &str,
    args: &mut I,
    flag: &str,
    kind: TargetKind,
) -> Result<Target, CommandError>
where
    I: Iterator<Item = &'a String>,
{
    let value = args.next().ok_or_else(|| CommandError::MissingValue {
        flag: flag.to_owned(),
    })?;

    Target::parse(value, kind).map_err(|source| CommandError::Target {
        command: command.to_owned(),
        source,
    })
}

fn unsupported(command: &str, flag: &str, plan: &str) -> CommandError {
    CommandError::UnsupportedFlag {
        command: command.to_owned(),
        flag: flag.to_owned(),
        plan: plan.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, CommandError, PaneSelector, Target, WindowRef};
    use crate::command::target::PaneRef;
    use crate::layout::geometry::{Direction, Split};

    fn parse(line: &str) -> Command {
        Command::parse_str(line).expect("command parses")
    }

    #[test]
    fn tmux_split_flags_are_the_opposite_of_weave_split_axes() {
        // The porting trap: `-h` means side-by-side, which divides the width,
        // which weave calls a vertical split.
        assert_eq!(
            parse("split-window -h"),
            Command::SplitWindow {
                split: Split::Vertical,
                target: Target::current(),
            }
        );
        assert_eq!(
            parse("split-window -v"),
            Command::SplitWindow {
                split: Split::Horizontal,
                target: Target::current(),
            }
        );
    }

    #[test]
    fn split_window_defaults_to_stacked_like_tmux() {
        assert_eq!(
            parse("split-window"),
            Command::SplitWindow {
                split: Split::Horizontal,
                target: Target::current(),
            }
        );
    }

    #[test]
    fn weave_split_aliases_keep_their_original_axes() {
        // `split-h` has always meant "divide the height"; it must not start
        // meaning tmux's `-h` now that both spellings exist.
        assert_eq!(
            parse("split-h"),
            Command::SplitWindow {
                split: Split::Horizontal,
                target: Target::current(),
            }
        );
        assert_eq!(
            parse("split-v"),
            Command::SplitWindow {
                split: Split::Vertical,
                target: Target::current(),
            }
        );
    }

    #[test]
    fn split_aliases_reject_an_axis_flag() {
        assert!(matches!(
            Command::parse_str("split-h -v"),
            Err(CommandError::UnknownFlag { .. })
        ));
    }

    #[test]
    fn split_window_takes_a_target() {
        let Command::SplitWindow { target, .. } = parse("split-window -h -t %4") else {
            panic!("expected a split");
        };
        assert_eq!(target.pane, Some(PaneRef::Id(4)));
    }

    #[test]
    fn focus_aliases_map_to_select_pane_directions() {
        for (line, direction) in [
            ("focus-left", Direction::Left),
            ("focus-right", Direction::Right),
            ("focus-up", Direction::Up),
            ("focus-down", Direction::Down),
        ] {
            assert_eq!(
                parse(line),
                Command::SelectPane {
                    selector: PaneSelector::Direction(direction),
                }
            );
        }
    }

    #[test]
    fn select_pane_accepts_direction_last_and_target() {
        assert_eq!(
            parse("select-pane -L"),
            Command::SelectPane {
                selector: PaneSelector::Direction(Direction::Left),
            }
        );
        assert_eq!(
            parse("select-pane -l"),
            Command::SelectPane {
                selector: PaneSelector::Last,
            }
        );
        let Command::SelectPane {
            selector: PaneSelector::Target(target),
        } = parse("select-pane -t :2.1")
        else {
            panic!("expected a targeted select-pane");
        };
        assert_eq!(target.window, Some(WindowRef::Index(2)));
        assert_eq!(target.pane, Some(PaneRef::Index(1)));
    }

    #[test]
    fn select_pane_rejects_two_selectors() {
        assert!(matches!(
            Command::parse_str("select-pane -L -R"),
            Err(CommandError::ConflictingFlags { .. })
        ));
    }

    #[test]
    fn workspace_aliases_select_windows_by_index() {
        assert_eq!(
            parse("workspace-3"),
            Command::SelectWindow {
                target: Target {
                    window: Some(WindowRef::Index(3)),
                    ..Target::default()
                },
            }
        );
        assert!(Command::parse_str("workspace-0").is_err());
        assert!(Command::parse_str("workspace-10").is_err());
    }

    #[test]
    fn select_window_relative_flags() {
        for (line, window) in [
            ("select-window -n", WindowRef::Next),
            ("select-window -p", WindowRef::Previous),
            ("select-window -l", WindowRef::Last),
        ] {
            assert_eq!(
                parse(line),
                Command::SelectWindow {
                    target: Target {
                        window: Some(window),
                        ..Target::default()
                    },
                }
            );
        }
    }

    #[test]
    fn close_and_quit_aliases_survive() {
        assert_eq!(
            parse("close"),
            Command::KillPane {
                target: Target::current(),
            }
        );
        assert_eq!(parse("detach"), Command::DetachClient);
        assert_eq!(
            parse("quit"),
            Command::KillSession {
                target: Target::current(),
            }
        );
    }

    #[test]
    fn planned_flags_say_which_pr_brings_them() {
        let error = Command::parse_str("split-window -h -p 30").expect_err("not supported yet");
        let message = error.to_string();
        assert!(message.contains("-p"), "{message}");
        assert!(message.contains("PR 5"), "{message}");

        let error = Command::parse_str("split-window -c /srv").expect_err("not supported yet");
        assert!(error.to_string().contains("PR 3"), "{error}");
    }

    #[test]
    fn shell_command_arguments_are_named_as_planned() {
        let error = Command::parse_str("split-window npm run dev").expect_err("not supported yet");
        assert!(error.to_string().contains("PR 3"), "{error}");
    }

    #[test]
    fn unknown_commands_and_flags_are_rejected() {
        assert!(matches!(
            Command::parse_str("split-pane"),
            Err(CommandError::UnknownCommand(_))
        ));
        assert!(matches!(
            Command::parse_str("kill-pane -x"),
            Err(CommandError::UnknownFlag { .. })
        ));
        assert!(matches!(
            Command::parse_str("kill-pane -t"),
            Err(CommandError::MissingValue { .. })
        ));
        assert!(matches!(Command::parse_str(""), Err(CommandError::Empty)));
    }

    #[test]
    fn invalid_targets_name_the_command() {
        let error = Command::parse_str("kill-pane -t %oops").expect_err("bad target");
        assert!(error.to_string().contains("kill-pane"), "{error}");
    }
}
