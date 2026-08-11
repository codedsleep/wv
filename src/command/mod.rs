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

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::layout::geometry::{Direction, Split};
pub use target::{Extreme, PaneRef, Target, TargetError, TargetKind, WindowRef};

/// A command, with everything it needs to run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Split a pane in two and spawn something in the new half.
    SplitWindow {
        split: Split,
        target: Target,
        /// What to run. `None` means the user's shell.
        command: Option<SpawnCommand>,
        /// `-d`: leave focus where it is.
        detached: bool,
        /// How big the new pane should be. `None` splits evenly.
        size: Option<SplitSize>,
    },
    /// Move focus.
    SelectPane { selector: PaneSelector },
    /// Switch to another window.
    SelectWindow {
        target: Target,
        /// Create the window if it does not exist yet.
        ///
        /// tmux's `select-window` fails on a missing window, and so does ours.
        /// The `workspace-N` aliases and the `Alt+N` bindings set this, which
        /// is what keeps "jump to window 5, making it if need be" working the
        /// way it always has in weave.
        create: bool,
    },
    /// Create a window and switch to it.
    NewWindow {
        /// Where to put it. `None` means the lowest-numbered free window.
        target: Target,
        name: Option<String>,
        command: Option<SpawnCommand>,
        /// `-d`: make it but stay where you are.
        detached: bool,
    },
    /// Close a window and everything in it.
    KillWindow { target: Target },
    /// Name a window, pinning it against automatic renaming.
    RenameWindow { target: Target, name: String },
    /// Resize a pane, or toggle it filling its window.
    ResizePane {
        target: Target,
        change: ResizeChange,
    },
    /// Exchange two panes' positions.
    SwapPane {
        source: Target,
        target: Target,
        /// Keep focus on the pane that was focused, not on where it moved to.
        keep_focus: bool,
    },
    /// Cycle every pane through the window's layout positions.
    RotateWindow { target: Target, reverse: bool },
    /// Rearrange a window's panes into a named shape.
    SelectLayout { target: Target, layout: LayoutPreset },
    /// Read a pane's visible screen back out.
    CapturePane {
        target: Target,
        /// First and last visible line to include, zero-based.
        start: Option<u16>,
        end: Option<u16>,
    },
    /// List panes, windows or sessions, one formatted line each.
    List { scope: ListScope, format: Option<String> },
    /// Bind a key to a command.
    BindKey {
        table: String,
        key: String,
        repeat: bool,
        command: Vec<String>,
    },
    /// Remove a binding, or every binding in a table.
    UnbindKey {
        table: String,
        key: Option<String>,
        all: bool,
    },
    /// Show the bindings, one per line.
    ListKeys { table: Option<String> },
    /// Set an option.
    SetOption {
        name: String,
        value: String,
        unset: bool,
    },
    /// Show the options, one per line.
    ShowOptions { name: Option<String> },
    /// Move a pane into a window of its own.
    BreakPane {
        source: Target,
        target: Target,
        name: Option<String>,
        detached: bool,
    },
    /// Move a pane out of its window and into another.
    JoinPane {
        source: Target,
        target: Target,
        split: Split,
        detached: bool,
    },
    /// Run a shell command outside any pane.
    RunShell {
        command: String,
        /// `-b`: do not wait for it to finish.
        background: bool,
    },
    /// Run a weave command depending on a shell command's exit status.
    IfShell {
        condition: String,
        then_command: Vec<String>,
        else_command: Option<Vec<String>>,
        background: bool,
    },
    /// Block until a channel is signalled, or signal one.
    WaitFor { channel: String, action: WaitAction },
    /// Close a pane, or every pane except it.
    KillPane {
        target: Target,
        /// `-a`: kill every *other* pane in the window instead.
        all_but_target: bool,
    },
    /// Leave the session running and disconnect the client.
    DetachClient,
    /// Shut the session down, killing every pane.
    KillSession { target: Target },
    /// Type keys into a pane, as if the user had pressed them.
    SendKeys {
        target: Target,
        /// Each argument is a key name (`C-c`, `Enter`) or, when it is not one,
        /// literal text to type.
        keys: Vec<String>,
        /// `-l`: treat every argument as literal text, even `Enter`.
        literal: bool,
    },
    /// Restart a pane's process in place, keeping its position in the layout.
    RespawnPane {
        target: Target,
        /// `-k`: kill the existing process instead of refusing while it lives.
        kill: bool,
        command: Option<SpawnCommand>,
    },
    /// Print a message back to the caller.
    ///
    /// `display-message -p` is how a script asks the session a question. The
    /// message is a format string, so `-p "#{pane_current_path}"` reads a
    /// value back out.
    DisplayMessage {
        message: String,
        target: Target,
        /// `-p`: return the text to the caller. Without it the message goes to
        /// the status line for the attached client to read.
        print: bool,
    },
}

/// How big a new pane should be, from `-p` or `-l`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    /// A percentage of the pane being split.
    Percent(u16),
    /// A number of cells along the split axis.
    Cells(u16),
}

/// What to run in a pane, and where.
///
/// A `None` program means the user's shell; `cwd` overrides where it starts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpawnCommand {
    /// The command line, already split into words by the caller's shell.
    /// Empty means "run the default shell".
    pub argv: Vec<String>,
    /// `-c`: the directory to start in.
    pub cwd: Option<PathBuf>,
}

impl SpawnCommand {
    pub fn is_empty(&self) -> bool {
        self.argv.is_empty() && self.cwd.is_none()
    }
}

/// What a `resize-pane` should do.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResizeChange {
    /// Move the nearest boundary in a direction by some cells.
    By { direction: Direction, cells: u16 },
    /// Set the pane's width.
    Width(u16),
    /// Set the pane's height.
    Height(u16),
    /// Toggle the pane filling its window.
    ToggleZoom,
}

/// What a `wait-for` does to its channel.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WaitAction {
    /// Block until someone signals the channel.
    Wait,
    /// Release everyone waiting on it.
    Signal,
}

/// What a `list-*` command enumerates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ListScope {
    /// Panes of one window, or of every window with `-a`.
    Panes { target: Target, all: bool },
    /// Windows of this session.
    Windows { target: Target },
    /// Every live session.
    Sessions,
}

/// tmux's named layouts.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LayoutPreset {
    EvenHorizontal,
    EvenVertical,
    MainVertical,
    MainHorizontal,
    Tiled,
}

impl LayoutPreset {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "even-horizontal" => Self::EvenHorizontal,
            "even-vertical" => Self::EvenVertical,
            "main-vertical" => Self::MainVertical,
            "main-horizontal" => Self::MainHorizontal,
            "tiled" => Self::Tiled,
            _ => return None,
        })
    }
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
    #[error("`{command} {flag}` expects {expected}, not `{value}`")]
    InvalidValue {
        command: String,
        flag: String,
        value: String,
        expected: String,
    },
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
    "display-message",
    "send-keys",
    "respawn-pane",
    "new-window",
    "kill-window",
    "rename-window",
    "resize-pane",
    "swap-pane",
    "rotate-window",
    "select-layout",
    "capture-pane",
    "list-panes",
    "list-windows",
    "list-sessions",
    "bind-key",
    "unbind-key",
    "list-keys",
    "set-option",
    "show-options",
    "break-pane",
    "join-pane",
    "run-shell",
    "if-shell",
    "wait-for",
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
            "next-window" | "nextw" => Ok(relative_window(WindowRef::Next)),
            "previous-window" | "prevw" => Ok(relative_window(WindowRef::Previous)),
            "last-window" => Ok(relative_window(WindowRef::Last)),

            "new-window" | "neww" => parse_new_window(name, rest),
            "kill-window" | "killw" => Ok(Self::KillWindow {
                target: parse_target_only(name, rest, TargetKind::Window)?,
            }),
            "rename-window" | "renamew" => parse_rename_window(name, rest),

            "resize-pane" | "resizep" => parse_resize_pane(name, rest),
            "swap-pane" | "swapp" => parse_swap_pane(name, rest),
            "rotate-window" | "rotatew" => parse_rotate_window(name, rest),
            "select-layout" | "selectl" => parse_select_layout(name, rest),

            "capture-pane" | "capturep" => parse_capture_pane(name, rest),
            "list-panes" | "lsp" => parse_list(name, rest, ListKind::Panes),
            "list-windows" | "lsw" => parse_list(name, rest, ListKind::Windows),
            "list-sessions" | "ls" => parse_list(name, rest, ListKind::Sessions),

            "bind-key" | "bind" => parse_bind_key(name, rest),
            "unbind-key" | "unbind" => parse_unbind_key(name, rest),
            "list-keys" | "lsk" => parse_list_keys(name, rest),
            "set-option" | "set" | "setw" | "set-window-option" => parse_set_option(name, rest),
            "show-options" | "show" | "showw" => parse_show_options(name, rest),

            "break-pane" | "breakp" => parse_break_pane(name, rest),
            "join-pane" | "joinp" | "move-pane" | "movep" => parse_join_pane(name, rest),
            "run-shell" | "run" => parse_run_shell(name, rest),
            "if-shell" | "if" => parse_if_shell(name, rest),
            "wait-for" | "wait" => parse_wait_for(name, rest),
            "set-environment" | "setenv" => Err(unsupported(
                name,
                "set-environment",
                "PR 9: per-session environment",
            )),

            "kill-pane" | "killp" | "close" => {
                let (target, all_but_target) =
                    parse_target_only_with_all(name, rest, TargetKind::Pane)?;
                Ok(Self::KillPane {
                    target,
                    all_but_target,
                })
            }

            "detach-client" | "detach" => {
                reject_extra_args(name, rest)?;
                Ok(Self::DetachClient)
            }

            "kill-session" | "quit" => Ok(Self::KillSession {
                target: parse_target_only(name, rest, TargetKind::Session)?,
            }),

            "display-message" | "display" => parse_display_message(name, rest),

            "send-keys" | "send" => parse_send_keys(name, rest),

            "respawn-pane" | "respawnp" => parse_respawn_pane(name, rest),

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

/// Whether an argument is a flag rather than a value.
///
/// A lone `-` is not: it is the name of the minus key (`bind - split-window`)
/// and a relative target (`-t -`). Only `-x` and longer are flags.
fn is_flag(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
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
    let mut detached = false;
    let mut size = None;
    let mut spawn = SpawnCommand::default();
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
            "-c" => spawn.cwd = Some(PathBuf::from(next_value(name, &mut args, "-c")?)),
            "-d" => detached = true,
            "-p" => {
                let value = next_value(name, &mut args, "-p")?;
                size = Some(SplitSize::Percent(parse_size(name, "-p", &value)?));
            }
            "-l" => {
                let value = next_value(name, &mut args, "-l")?;
                // tmux accepts `-l 30%` as a spelling of `-p 30`.
                size = Some(match value.strip_suffix('%') {
                    Some(percent) => SplitSize::Percent(parse_size(name, "-l", percent)?),
                    None => SplitSize::Cells(parse_size(name, "-l", &value)?),
                });
            }
            "-f" => return Err(unsupported(name, arg, "not planned: full-width splits need a layout model weave does not have")),
            "-b" => return Err(unsupported(name, arg, "not planned: splits always place the new pane second")),
            "-P" | "-F" => return Err(unsupported(name, arg, "PR 9: printing the new pane")),
            "--" => {
                // Everything after `--` is the command, even if it looks like
                // a flag: `split-window -- ls -la`.
                spawn.argv.extend(args.cloned());
                break;
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                // The first bare word starts the command; the rest are its
                // arguments, flags and all.
                spawn.argv.push(other.to_owned());
                spawn.argv.extend(args.cloned());
                break;
            }
        }
    }

    Ok(Command::SplitWindow {
        // tmux defaults to a stacked split when neither flag is given.
        split: split.unwrap_or_else(|| split_from_tmux_flag(false)),
        target: target.unwrap_or_default(),
        command: (!spawn.is_empty()).then_some(spawn),
        detached,
        size,
    })
}

fn parse_size(command: &str, flag: &str, value: &str) -> Result<u16, CommandError> {
    value.parse().map_err(|_| CommandError::InvalidValue {
        command: command.to_owned(),
        flag: flag.to_owned(),
        value: value.to_owned(),
        expected: "a number".to_owned(),
    })
}

fn parse_send_keys(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut literal = false;
    let mut keys = Vec::new();
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => {
                if target
                    .replace(next_target(name, &mut args, "-t", TargetKind::Pane)?)
                    .is_some()
                {
                    return Err(CommandError::ConflictingFlags {
                        command: name.to_owned(),
                        flags: "`-t`".to_owned(),
                    });
                }
            }
            "-l" => literal = true,
            "-H" => return Err(unsupported(name, arg, "PR 9: hex key arguments")),
            "-R" | "-M" | "-X" | "-N" => {
                return Err(unsupported(
                    name,
                    arg,
                    "not planned: these drive copy mode, which weave does not have",
                ));
            }
            "--" => {
                keys.extend(args.cloned());
                break;
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => keys.push(other.to_owned()),
        }
    }

    if keys.is_empty() {
        return Err(CommandError::MissingValue {
            flag: "keys to send".to_owned(),
        });
    }

    Ok(Command::SendKeys {
        target: target.unwrap_or_default(),
        keys,
        literal,
    })
}

fn parse_respawn_pane(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut kill = false;
    let mut spawn = SpawnCommand::default();
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Pane)?),
            "-k" => kill = true,
            "-c" => spawn.cwd = Some(PathBuf::from(next_value(name, &mut args, "-c")?)),
            "-e" => return Err(unsupported(name, arg, "PR 7: per-pane environment")),
            "--" => {
                spawn.argv.extend(args.cloned());
                break;
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                spawn.argv.push(other.to_owned());
                spawn.argv.extend(args.cloned());
                break;
            }
        }
    }

    Ok(Command::RespawnPane {
        target: target.unwrap_or_default(),
        kill,
        command: (!spawn.is_empty()).then_some(spawn),
    })
}

fn parse_resize_pane(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut change = None;
    let mut args = args.iter().peekable();

    while let Some(arg) = args.next() {
        let next = match arg.as_str() {
            "-t" => {
                target = Some(next_target(name, &mut args, "-t", TargetKind::Pane)?);
                continue;
            }
            "-Z" => ResizeChange::ToggleZoom,
            // A direction takes an optional count, so a bare `-L` means one
            // cell and `-L 5` means five. Only a number counts as the count.
            "-L" => ResizeChange::By {
                direction: Direction::Left,
                cells: optional_count(&mut args),
            },
            "-R" => ResizeChange::By {
                direction: Direction::Right,
                cells: optional_count(&mut args),
            },
            "-U" => ResizeChange::By {
                direction: Direction::Up,
                cells: optional_count(&mut args),
            },
            "-D" => ResizeChange::By {
                direction: Direction::Down,
                cells: optional_count(&mut args),
            },
            "-x" => {
                let value = next_value(name, &mut args, "-x")?;
                ResizeChange::Width(parse_size(name, "-x", &value)?)
            }
            "-y" => {
                let value = next_value(name, &mut args, "-y")?;
                ResizeChange::Height(parse_size(name, "-y", &value)?)
            }
            "-M" => return Err(unsupported(name, arg, "not planned: weave has no mouse support")),
            other if is_flag(other) => {
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

        if change.replace(next).is_some() {
            return Err(CommandError::ConflictingFlags {
                command: name.to_owned(),
                flags: "`-L`, `-R`, `-U`, `-D`, `-x`, `-y` and `-Z`".to_owned(),
            });
        }
    }

    let change = change.ok_or_else(|| CommandError::MissingValue {
        flag: "a direction, `-x`, `-y` or `-Z`".to_owned(),
    })?;

    Ok(Command::ResizePane {
        target: target.unwrap_or_default(),
        change,
    })
}

/// tmux's resize directions take an optional count; default to one cell.
fn optional_count<'a, I>(args: &mut std::iter::Peekable<I>) -> u16
where
    I: Iterator<Item = &'a String>,
{
    let Some(next) = args.peek() else {
        return 1;
    };
    let Ok(count) = next.parse::<u16>() else {
        return 1;
    };
    args.next();

    count
}

fn parse_swap_pane(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut source = None;
    let mut target = None;
    let mut keep_focus = false;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" => source = Some(next_target(name, &mut args, "-s", TargetKind::Pane)?),
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Pane)?),
            "-d" => keep_focus = true,
            "-U" => target = Some(relative_pane(PaneRef::Previous)),
            "-D" => target = Some(relative_pane(PaneRef::Next)),
            other if is_flag(other) => {
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

    Ok(Command::SwapPane {
        source: source.unwrap_or_default(),
        target: target.unwrap_or_default(),
        keep_focus,
    })
}

fn relative_pane(pane: PaneRef) -> Target {
    Target {
        pane: Some(pane),
        ..Target::default()
    }
}

fn parse_rotate_window(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut reverse = false;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Window)?),
            "-U" => reverse = true,
            "-D" => reverse = false,
            "-Z" => return Err(unsupported(name, arg, "not planned: rotate does not unzoom")),
            other if is_flag(other) => {
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

    Ok(Command::RotateWindow {
        target: target.unwrap_or_default(),
        reverse,
    })
}

fn parse_select_layout(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut layout = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Window)?),
            "-n" => layout = Some(LayoutPreset::Tiled),
            "-p" | "-o" | "-E" => {
                return Err(unsupported(
                    name,
                    arg,
                    "not planned: weave keeps no layout history to step through",
                ));
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                let preset = LayoutPreset::parse(other).ok_or_else(|| {
                    CommandError::InvalidValue {
                        command: name.to_owned(),
                        flag: "layout".to_owned(),
                        value: other.to_owned(),
                        expected: "even-horizontal, even-vertical, main-vertical, \
                                   main-horizontal or tiled"
                            .to_owned(),
                    }
                })?;
                layout = Some(preset);
            }
        }
    }

    let layout = layout.ok_or_else(|| CommandError::MissingValue {
        flag: "a layout name".to_owned(),
    })?;

    Ok(Command::SelectLayout {
        target: target.unwrap_or_default(),
        layout,
    })
}

/// What kind of thing a `list-*` enumerates, before its flags are read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ListKind {
    Panes,
    Windows,
    Sessions,
}

fn parse_list(name: &str, args: &[String], kind: ListKind) -> Result<Command, CommandError> {
    let mut target = None;
    let mut format = None;
    let mut all = false;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => {
                let target_kind = match kind {
                    ListKind::Panes => TargetKind::Window,
                    ListKind::Windows | ListKind::Sessions => TargetKind::Session,
                };
                target = Some(next_target(name, &mut args, "-t", target_kind)?);
            }
            "-F" => format = Some(next_value(name, &mut args, "-F")?),
            "-a" => all = true,
            "-s" => return Err(unsupported(name, arg, "not planned: weave lists one session")),
            "-f" => return Err(unsupported(name, arg, "PR 9: list filters")),
            other if is_flag(other) => {
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

    let scope = match kind {
        ListKind::Panes => ListScope::Panes {
            target: target.unwrap_or_default(),
            all,
        },
        ListKind::Windows => ListScope::Windows {
            target: target.unwrap_or_default(),
        },
        ListKind::Sessions => ListScope::Sessions,
    };

    Ok(Command::List { scope, format })
}

fn parse_capture_pane(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut start = None;
    let mut end = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Pane)?),
            // `-p` prints to stdout, which is the only thing weave does with a
            // capture, so it is accepted and implied.
            "-p" => {}
            "-S" => {
                let value = next_value(name, &mut args, "-S")?;
                start = Some(parse_capture_line(name, "-S", &value)?);
            }
            "-E" => {
                let value = next_value(name, &mut args, "-E")?;
                end = Some(parse_capture_line(name, "-E", &value)?);
            }
            "-e" | "-C" => {
                return Err(unsupported(
                    name,
                    arg,
                    "PR 9: capturing escape sequences rather than text",
                ));
            }
            "-J" | "-N" | "-T" => {
                return Err(unsupported(name, arg, "PR 9: capture line joining"));
            }
            "-b" | "-a" => {
                return Err(unsupported(
                    name,
                    arg,
                    "not planned: paste buffers went with copy mode",
                ));
            }
            other if is_flag(other) => {
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

    Ok(Command::CapturePane {
        target: target.unwrap_or_default(),
        start,
        end,
    })
}

/// Parse a capture line number, refusing the negative and `-` forms that mean
/// "reach into the scrollback" — weave has none, and silently clamping them to
/// the visible screen would hand a script less than it asked for.
fn parse_capture_line(command: &str, flag: &str, value: &str) -> Result<u16, CommandError> {
    if value == "-" || value.starts_with('-') {
        return Err(CommandError::UnsupportedFlag {
            command: command.to_owned(),
            flag: format!("{flag} {value}"),
            plan: "not planned: weave keeps no scrollback, so only the visible \
                   screen can be captured"
                .to_owned(),
        });
    }

    value.parse().map_err(|_| CommandError::InvalidValue {
        command: command.to_owned(),
        flag: flag.to_owned(),
        value: value.to_owned(),
        expected: "a line number on the visible screen".to_owned(),
    })
}

fn parse_bind_key(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut table = None;
    let mut root = false;
    let mut repeat = false;
    let mut args = args.iter();
    let mut key = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-T" => table = Some(next_value(name, &mut args, "-T")?),
            "-n" => root = true,
            "-r" => repeat = true,
            "-N" => return Err(unsupported(name, arg, "PR 9: binding descriptions")),
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                key = Some(other.to_owned());
                break;
            }
        }
    }

    let key = key.ok_or_else(|| CommandError::MissingValue {
        flag: "a key to bind".to_owned(),
    })?;
    let command: Vec<String> = args.cloned().collect();
    if command.is_empty() {
        return Err(CommandError::MissingValue {
            flag: "a command to bind the key to".to_owned(),
        });
    }

    // `-n` and `-T` name the same thing two ways; `-n` is the root table.
    let table = match (root, table) {
        (true, _) => "root".to_owned(),
        (false, Some(table)) => table,
        (false, None) => "prefix".to_owned(),
    };

    Ok(Command::BindKey {
        table,
        key,
        repeat,
        command,
    })
}

fn parse_unbind_key(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut table = None;
    let mut root = false;
    let mut all = false;
    let mut key = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-T" => table = Some(next_value(name, &mut args, "-T")?),
            "-n" => root = true,
            "-a" => all = true,
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                if key.replace(other.to_owned()).is_some() {
                    return Err(CommandError::UnexpectedArgument {
                        command: name.to_owned(),
                        argument: other.to_owned(),
                    });
                }
            }
        }
    }

    if key.is_none() && !all {
        return Err(CommandError::MissingValue {
            flag: "a key to unbind, or `-a`".to_owned(),
        });
    }

    let table = match (root, table) {
        (true, _) => "root".to_owned(),
        (false, Some(table)) => table,
        (false, None) => "prefix".to_owned(),
    };

    Ok(Command::UnbindKey { table, key, all })
}

fn parse_list_keys(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut table = None;
    // `-T` consumes the next word, so this reads the iterator by hand.
    let mut args = args.iter();

    #[allow(clippy::while_let_on_iterator)]
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-T" => table = Some(next_value(name, &mut args, "-T")?),
            "-N" | "-P" | "-1" => {
                return Err(unsupported(name, arg, "PR 9: binding descriptions"));
            }
            other if is_flag(other) => {
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

    Ok(Command::ListKeys { table })
}

fn parse_set_option(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut unset = false;
    let mut positional = Vec::new();

    for arg in args {
        match arg.as_str() {
            // Scope flags are accepted and ignored: weave has one session, so
            // global, window and pane options are the same set.
            "-g" | "-w" | "-p" | "-s" | "-q" | "-a" => {}
            "-u" => unset = true,
            "-o" | "-F" => return Err(unsupported(name, arg, "PR 9: conditional option setting")),
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => positional.push(other.to_owned()),
        }
    }

    let mut positional = positional.into_iter();
    let option = positional.next().ok_or_else(|| CommandError::MissingValue {
        flag: "an option name".to_owned(),
    })?;
    // tmux allows a flag option to be set with no value, meaning "on".
    let value = positional.next().unwrap_or_else(|| "on".to_owned());

    if let Some(extra) = positional.next() {
        return Err(CommandError::UnexpectedArgument {
            command: name.to_owned(),
            argument: extra,
        });
    }

    Ok(Command::SetOption {
        name: option,
        value,
        unset,
    })
}

fn parse_show_options(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut option = None;

    for arg in args {
        match arg.as_str() {
            "-g" | "-w" | "-p" | "-s" | "-q" | "-v" | "-A" => {}
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                if option.replace(other.to_owned()).is_some() {
                    return Err(CommandError::UnexpectedArgument {
                        command: name.to_owned(),
                        argument: other.to_owned(),
                    });
                }
            }
        }
    }

    Ok(Command::ShowOptions { name: option })
}

fn parse_break_pane(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut source = None;
    let mut target = None;
    let mut window_name = None;
    let mut detached = false;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" => source = Some(next_target(name, &mut args, "-s", TargetKind::Pane)?),
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Window)?),
            "-n" => window_name = Some(next_value(name, &mut args, "-n")?),
            "-d" => detached = true,
            "-P" | "-F" => return Err(unsupported(name, arg, "not planned: printing the new window")),
            "-a" | "-b" => {
                return Err(unsupported(
                    name,
                    arg,
                    "not planned: windows are fixed slots, so there is nothing to insert before or after",
                ));
            }
            other if is_flag(other) => {
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

    Ok(Command::BreakPane {
        source: source.unwrap_or_default(),
        target: target.unwrap_or_default(),
        name: window_name,
        detached,
    })
}

fn parse_join_pane(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut source = None;
    let mut target = None;
    let mut split = None;
    let mut detached = false;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" => source = Some(next_target(name, &mut args, "-s", TargetKind::Pane)?),
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Pane)?),
            // Same inversion as `split-window`: `-h` is side by side.
            "-h" => split = Some(Split::Vertical),
            "-v" => split = Some(Split::Horizontal),
            "-d" => detached = true,
            "-p" | "-l" => return Err(unsupported(name, arg, "not planned: joined panes split evenly")),
            "-b" | "-f" => {
                return Err(unsupported(name, arg, "not planned: the joined pane always goes second"));
            }
            other if is_flag(other) => {
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

    Ok(Command::JoinPane {
        source: source.unwrap_or_default(),
        target: target.unwrap_or_default(),
        split: split.unwrap_or(Split::Horizontal),
        detached,
    })
}

fn parse_run_shell(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut background = false;
    let mut command = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-b" => background = true,
            "-t" => {
                // Accepted and ignored: a run-shell has no pane to act in.
                let _ = next_value(name, &mut args, "-t")?;
            }
            "-d" | "-C" => return Err(unsupported(name, arg, "not planned: delayed and in-session commands")),
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                if command.replace(other.to_owned()).is_some() {
                    return Err(CommandError::UnexpectedArgument {
                        command: name.to_owned(),
                        argument: other.to_owned(),
                    });
                }
            }
        }
    }

    let command = command.ok_or_else(|| CommandError::MissingValue {
        flag: "a shell command".to_owned(),
    })?;

    Ok(Command::RunShell {
        command,
        background,
    })
}

fn parse_if_shell(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut background = false;
    let mut positional = Vec::new();
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-b" => background = true,
            "-F" => return Err(unsupported(name, arg, "not planned: format conditions")),
            "-t" => {
                let _ = next_value(name, &mut args, "-t")?;
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => positional.push(other.to_owned()),
        }
    }

    let mut positional = positional.into_iter();
    let condition = positional.next().ok_or_else(|| CommandError::MissingValue {
        flag: "a shell command to test".to_owned(),
    })?;
    let then_command = positional.next().ok_or_else(|| CommandError::MissingValue {
        flag: "a command to run when the test succeeds".to_owned(),
    })?;
    let else_command = positional.next();

    // The branches are whole command lines in one argument, as tmux writes
    // them: `if-shell "test -d /srv" "new-window -c /srv"`.
    Ok(Command::IfShell {
        condition,
        then_command: shell_words(&then_command),
        else_command: else_command.as_deref().map(shell_words),
        background,
    })
}

/// Split a quoted command line the way a config file would.
fn shell_words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

fn parse_wait_for(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut action = WaitAction::Wait;
    let mut channel = None;

    for arg in args {
        match arg.as_str() {
            "-S" => action = WaitAction::Signal,
            "-L" | "-U" => {
                return Err(unsupported(
                    name,
                    arg,
                    "not planned: weave has signals but no wait-for locks",
                ));
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                if channel.replace(other.to_owned()).is_some() {
                    return Err(CommandError::UnexpectedArgument {
                        command: name.to_owned(),
                        argument: other.to_owned(),
                    });
                }
            }
        }
    }

    let channel = channel.ok_or_else(|| CommandError::MissingValue {
        flag: "a channel name".to_owned(),
    })?;

    Ok(Command::WaitFor { channel, action })
}

fn next_value<'a, I>(command: &str, args: &mut I, flag: &str) -> Result<String, CommandError>
where
    I: Iterator<Item = &'a String>,
{
    args.next()
        .cloned()
        .ok_or_else(|| CommandError::MissingValue {
            flag: format!("{command} {flag}"),
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
            other if is_flag(other) => {
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
            other if is_flag(other) => {
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
        create: false,
    })
}

fn relative_window(window: WindowRef) -> Command {
    Command::SelectWindow {
        target: window_target(window),
        create: false,
    }
}

fn parse_new_window(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut window_name = None;
    let mut detached = false;
    let mut spawn = SpawnCommand::default();
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Window)?),
            "-n" => window_name = Some(next_value(name, &mut args, "-n")?),
            "-c" => spawn.cwd = Some(PathBuf::from(next_value(name, &mut args, "-c")?)),
            "-d" => detached = true,
            "-a" | "-b" => return Err(unsupported(name, arg, "not planned: windows are fixed slots, so there is nothing to insert before or after")),
            "-k" => return Err(unsupported(name, arg, "PR 9: replacing an existing window")),
            "-P" | "-F" => return Err(unsupported(name, arg, "PR 9: printing the new window")),
            "-S" => return Err(unsupported(name, arg, "PR 9: select if the window already exists")),
            "--" => {
                spawn.argv.extend(args.cloned());
                break;
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                spawn.argv.push(other.to_owned());
                spawn.argv.extend(args.cloned());
                break;
            }
        }
    }

    Ok(Command::NewWindow {
        target: target.unwrap_or_default(),
        name: window_name,
        command: (!spawn.is_empty()).then_some(spawn),
        detached,
    })
}

fn parse_rename_window(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut target = None;
    let mut new_name = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => target = Some(next_target(name, &mut args, "-t", TargetKind::Window)?),
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                if new_name.replace(other.to_owned()).is_some() {
                    return Err(CommandError::UnexpectedArgument {
                        command: name.to_owned(),
                        argument: other.to_owned(),
                    });
                }
            }
        }
    }

    let new_name = new_name.ok_or_else(|| CommandError::MissingValue {
        flag: "a new window name".to_owned(),
    })?;

    Ok(Command::RenameWindow {
        target: target.unwrap_or_default(),
        name: new_name,
    })
}

fn parse_display_message(name: &str, args: &[String]) -> Result<Command, CommandError> {
    let mut print = false;
    let mut target = None;
    let mut message = None;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-p" => print = true,
            "-t" => {
                if target
                    .replace(next_target(name, &mut args, "-t", TargetKind::Pane)?)
                    .is_some()
                {
                    return Err(CommandError::ConflictingFlags {
                        command: name.to_owned(),
                        flags: "`-t`".to_owned(),
                    });
                }
            }
            // tmux's `-F` names the format explicitly; the bare argument is
            // already treated as one, so this is just the other spelling.
            "-F" => message = Some(next_value(name, &mut args, "-F")?),
            "-v" | "-a" | "-I" | "-N" | "-c" | "-d" => {
                return Err(unsupported(name, arg, "PR 9: message routing and verbose output"));
            }
            other if is_flag(other) => {
                return Err(CommandError::UnknownFlag {
                    command: name.to_owned(),
                    flag: other.to_owned(),
                });
            }
            other => {
                if message.replace(other.to_owned()).is_some() {
                    return Err(CommandError::UnexpectedArgument {
                        command: name.to_owned(),
                        argument: other.to_owned(),
                    });
                }
            }
        }
    }

    Ok(Command::DisplayMessage {
        message: message.unwrap_or_default(),
        target: target.unwrap_or_default(),
        print,
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
        // `Alt+3` has always opened window 3 whether or not it existed.
        create: true,
    })
}

fn window_target(window: WindowRef) -> Target {
    Target {
        window: Some(window),
        ..Target::default()
    }
}

/// Parse a command whose only arguments are `-t` and, for `kill-pane`, `-a`.
///
/// Returns the target and whether `-a` was given.
fn parse_target_only_with_all(
    name: &str,
    args: &[String],
    kind: TargetKind,
) -> Result<(Target, bool), CommandError> {
    let mut target = None;
    let mut all_but_target = false;
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
            "-a" => all_but_target = true,
            other if is_flag(other) => {
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

    Ok((target.unwrap_or_default(), all_but_target))
}

fn parse_target_only(
    name: &str,
    args: &[String],
    kind: TargetKind,
) -> Result<Target, CommandError> {
    let (target, all) = parse_target_only_with_all(name, args, kind)?;
    if all {
        return Err(CommandError::UnknownFlag {
            command: name.to_owned(),
            flag: "-a".to_owned(),
        });
    }

    Ok(target)
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
    use super::{
        Command, CommandError, LayoutPreset, ListScope, PaneSelector, ResizeChange, SplitSize,
        Target, WindowRef,
    };
    use crate::command::target::PaneRef;
    use crate::layout::geometry::{Direction, Split};

    fn parse(line: &str) -> Command {
        Command::parse_str(line).expect("command parses")
    }

    impl Command {
        fn as_select_window_target(&self) -> Option<WindowRef> {
            match self {
                Self::SelectWindow { target, .. } => target.window.clone(),
                _ => None,
            }
        }
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
                command: None,
                detached: false,
                size: None,
            }
        );
        assert_eq!(
            parse("split-window -v"),
            Command::SplitWindow {
                split: Split::Horizontal,
                target: Target::current(),
                command: None,
                detached: false,
                size: None,
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
                command: None,
                detached: false,
                size: None,
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
                command: None,
                detached: false,
                size: None,
            }
        );
        assert_eq!(
            parse("split-v"),
            Command::SplitWindow {
                split: Split::Vertical,
                target: Target::current(),
                command: None,
                detached: false,
                size: None,
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
                create: true,
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
                    create: false,
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
                all_but_target: false,
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
        let error = Command::parse_str("new-window -S").expect_err("not supported yet");
        let message = error.to_string();
        assert!(message.contains("-S"), "{message}");
        assert!(message.contains("PR 9"), "{message}");
    }

    /// A flag weave will never grow says so, rather than naming a PR that is
    /// not coming.
    #[test]
    fn dropped_features_say_not_planned() {
        let error = Command::parse_str("resize-pane -M").expect_err("no mouse");
        assert!(error.to_string().contains("not planned"), "{error}");
    }

    #[test]
    fn split_window_sizes_the_new_pane() {
        let Command::SplitWindow { size, .. } = parse("split-window -h -p 30") else {
            panic!("expected a split");
        };
        assert_eq!(size, Some(SplitSize::Percent(30)));

        let Command::SplitWindow { size, .. } = parse("split-window -l 20") else {
            panic!("expected a split");
        };
        assert_eq!(size, Some(SplitSize::Cells(20)));

        // tmux accepts `-l 30%` as another way to say `-p 30`.
        let Command::SplitWindow { size, .. } = parse("split-window -l 30%") else {
            panic!("expected a split");
        };
        assert_eq!(size, Some(SplitSize::Percent(30)));
    }

    #[test]
    fn a_bad_size_says_what_it_wanted() {
        let error = Command::parse_str("split-window -p wide").expect_err("not a number");
        assert!(error.to_string().contains("a number"), "{error}");
    }

    /// tmux's resize directions take an optional count, so `-L` is one cell.
    #[test]
    fn resize_directions_default_to_one_cell() {
        assert_eq!(
            parse("resize-pane -L"),
            Command::ResizePane {
                target: Target::current(),
                change: ResizeChange::By {
                    direction: Direction::Left,
                    cells: 1,
                },
            }
        );
        assert_eq!(
            parse("resize-pane -R 5"),
            Command::ResizePane {
                target: Target::current(),
                change: ResizeChange::By {
                    direction: Direction::Right,
                    cells: 5,
                },
            }
        );
    }

    #[test]
    fn resize_pane_takes_absolute_sizes_and_zoom() {
        assert_eq!(
            parse("resize-pane -x 40"),
            Command::ResizePane {
                target: Target::current(),
                change: ResizeChange::Width(40),
            }
        );
        assert_eq!(
            parse("resize-pane -Z"),
            Command::ResizePane {
                target: Target::current(),
                change: ResizeChange::ToggleZoom,
            }
        );
    }

    #[test]
    fn resize_pane_needs_to_be_told_what_to_do() {
        assert!(matches!(
            Command::parse_str("resize-pane -t %1"),
            Err(CommandError::MissingValue { .. })
        ));
    }

    #[test]
    fn swap_pane_directions_are_relative_targets() {
        let Command::SwapPane { target, .. } = parse("swap-pane -U") else {
            panic!("expected a swap");
        };
        assert_eq!(target.pane, Some(PaneRef::Previous));
    }

    #[test]
    fn select_layout_names_its_presets() {
        for (line, expected) in [
            ("select-layout even-horizontal", LayoutPreset::EvenHorizontal),
            ("select-layout even-vertical", LayoutPreset::EvenVertical),
            ("select-layout main-vertical", LayoutPreset::MainVertical),
            ("select-layout main-horizontal", LayoutPreset::MainHorizontal),
            ("select-layout tiled", LayoutPreset::Tiled),
        ] {
            assert_eq!(
                parse(line),
                Command::SelectLayout {
                    target: Target::current(),
                    layout: expected,
                }
            );
        }
    }

    #[test]
    fn an_unknown_layout_lists_the_ones_that_exist() {
        let error = Command::parse_str("select-layout spiral").expect_err("no such layout");
        assert!(error.to_string().contains("even-horizontal"), "{error}");
    }

    /// The first bare word starts the command line; everything after it
    /// belongs to that command, flags included.
    #[test]
    fn a_trailing_command_line_becomes_the_pane_process() {
        let Command::SplitWindow { command, .. } = parse("split-window -h npm run dev") else {
            panic!("expected a split");
        };
        let command = command.expect("a command was given");
        assert_eq!(command.argv, vec!["npm", "run", "dev"]);
        assert_eq!(command.cwd, None);
    }

    #[test]
    fn a_command_keeps_flags_that_look_like_weaves() {
        let Command::SplitWindow { command, .. } = parse("split-window ls -la -t") else {
            panic!("expected a split");
        };
        assert_eq!(
            command.expect("a command was given").argv,
            vec!["ls", "-la", "-t"]
        );
    }

    #[test]
    fn a_double_dash_starts_the_command_even_for_a_leading_flag() {
        let Command::SplitWindow { command, .. } = parse("split-window -- -weird-program") else {
            panic!("expected a split");
        };
        assert_eq!(
            command.expect("a command was given").argv,
            vec!["-weird-program"]
        );
    }

    #[test]
    fn split_window_takes_a_cwd_and_a_detached_flag() {
        let Command::SplitWindow {
            command, detached, ..
        } = parse("split-window -d -c /srv")
        else {
            panic!("expected a split");
        };
        assert!(detached);
        assert_eq!(
            command.expect("a cwd was given").cwd,
            Some(std::path::PathBuf::from("/srv"))
        );
    }

    #[test]
    fn send_keys_collects_key_names_and_text() {
        assert_eq!(
            parse("send-keys -t %2 npm Enter"),
            Command::SendKeys {
                target: Target {
                    pane: Some(PaneRef::Id(2)),
                    ..Target::default()
                },
                keys: vec!["npm".to_owned(), "Enter".to_owned()],
                literal: false,
            }
        );
    }

    #[test]
    fn send_keys_literal_flag_is_carried_through() {
        let Command::SendKeys { literal, keys, .. } = parse("send-keys -l Enter") else {
            panic!("expected send-keys");
        };
        assert!(literal);
        assert_eq!(keys, vec!["Enter"]);
    }

    #[test]
    fn send_keys_needs_something_to_send() {
        assert!(matches!(
            Command::parse_str("send-keys -t %1"),
            Err(CommandError::MissingValue { .. })
        ));
    }

    #[test]
    fn respawn_pane_requires_k_to_be_meaningful_later() {
        assert_eq!(
            parse("respawn-pane -k -t %1"),
            Command::RespawnPane {
                target: Target {
                    pane: Some(PaneRef::Id(1)),
                    ..Target::default()
                },
                kill: true,
                command: None,
            }
        );
    }

    #[test]
    fn display_message_p_prints_and_without_it_shows() {
        assert_eq!(
            parse("display-message -p hello"),
            Command::DisplayMessage {
                message: "hello".to_owned(),
                target: Target::current(),
                print: true,
            }
        );
        // Without `-p` the message goes to the status line instead.
        let Command::DisplayMessage { print, .. } = parse("display-message hello") else {
            panic!("expected a display-message");
        };
        assert!(!print);
    }

    #[test]
    fn display_message_takes_a_target_and_defaults_to_empty() {
        let Command::DisplayMessage { message, target, .. } = parse("display-message -p -t %3")
        else {
            panic!("expected a display-message");
        };
        assert_eq!(message, "");
        assert_eq!(target.pane, Some(PaneRef::Id(3)));
    }

    #[test]
    fn display_message_takes_a_format_either_way() {
        let bare = parse("display-message -p #{pane_id}");
        let flagged = parse("display-message -p -F #{pane_id}");
        assert_eq!(bare, flagged);
    }

    #[test]
    fn capture_pane_takes_a_line_range() {
        assert_eq!(
            parse("capture-pane -p -t %2 -S 0 -E 4"),
            Command::CapturePane {
                target: Target {
                    pane: Some(PaneRef::Id(2)),
                    ..Target::default()
                },
                start: Some(0),
                end: Some(4),
            }
        );
    }

    /// Scrollback is out of scope, so `-S -` must fail loudly rather than
    /// quietly returning only the visible screen.
    #[test]
    fn capture_pane_refuses_history_ranges() {
        let error = Command::parse_str("capture-pane -p -S -").expect_err("no history");
        assert!(error.to_string().contains("scrollback"), "{error}");

        let error = Command::parse_str("capture-pane -p -S -20").expect_err("no history");
        assert!(error.to_string().contains("scrollback"), "{error}");
    }

    #[test]
    fn list_commands_carry_their_scope_and_format() {
        assert_eq!(
            parse("list-panes -a -F #{pane_id}"),
            Command::List {
                scope: ListScope::Panes {
                    target: Target::current(),
                    all: true,
                },
                format: Some("#{pane_id}".to_owned()),
            }
        );
        assert_eq!(
            parse("list-sessions"),
            Command::List {
                scope: ListScope::Sessions,
                format: None,
            }
        );
    }

    /// `-` is the minus key, not a flag — `bind - split-window -v` is a line
    /// real tmux configs contain.
    #[test]
    fn a_lone_dash_is_a_value_not_a_flag() {
        assert_eq!(
            parse("bind-key - split-window -v"),
            Command::BindKey {
                table: "prefix".to_owned(),
                key: "-".to_owned(),
                repeat: false,
                command: vec!["split-window".to_owned(), "-v".to_owned()],
            }
        );
        assert_eq!(
            parse("select-window -t -").as_select_window_target(),
            Some(WindowRef::Previous)
        );
    }

    #[test]
    fn bind_key_defaults_to_the_prefix_table_and_n_means_root() {
        let Command::BindKey { table, .. } = parse("bind-key x kill-pane") else {
            panic!("expected a binding");
        };
        assert_eq!(table, "prefix");

        let Command::BindKey { table, repeat, .. } = parse("bind-key -n -r M-h select-pane -L")
        else {
            panic!("expected a binding");
        };
        assert_eq!(table, "root");
        assert!(repeat);
    }

    #[test]
    fn bind_key_needs_a_key_and_a_command() {
        assert!(matches!(
            Command::parse_str("bind-key"),
            Err(CommandError::MissingValue { .. })
        ));
        assert!(matches!(
            Command::parse_str("bind-key x"),
            Err(CommandError::MissingValue { .. })
        ));
    }

    #[test]
    fn set_option_defaults_a_missing_value_to_on() {
        assert_eq!(
            parse("set-option -g mouse"),
            Command::SetOption {
                name: "mouse".to_owned(),
                value: "on".to_owned(),
                unset: false,
            }
        );
    }

    /// Scope flags are accepted and ignored: weave has one session, so global,
    /// window and pane options are the same set.
    #[test]
    fn set_option_scope_flags_are_accepted() {
        for line in [
            "set -g status on",
            "setw -g status on",
            "set -w status on",
            "set -p status on",
        ] {
            assert_eq!(
                Command::parse_str(line).expect("parses"),
                Command::SetOption {
                    name: "status".to_owned(),
                    value: "on".to_owned(),
                    unset: false,
                }
            );
        }
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
