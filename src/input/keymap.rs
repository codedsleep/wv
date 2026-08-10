//! `Keymap` and default key bindings.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command::target::WindowRef;
use crate::command::{Command, PaneSelector, Target};
use crate::layout::geometry::{Direction, Split};

#[derive(Clone)]
pub struct Keymap {
    bindings: HashMap<KeyEvent, Command>,
}

impl Keymap {
    pub fn set_binding(&mut self, key: KeyEvent, command: Command) {
        if let Some(key) = normalize_key(&key) {
            self.bindings.insert(key, command);
        }
    }

    /// The command bound to `event`, if any.
    ///
    /// Commands carry owned arguments now, so this clones rather than copies;
    /// it runs once per keypress, not per frame.
    pub fn command_for(&self, event: &KeyEvent) -> Option<Command> {
        self.bindings.get(&normalize_key(event)?).cloned()
    }
}

impl Default for Keymap {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert(alt_char('h'), focus(Direction::Left));
        bindings.insert(alt_char('j'), focus(Direction::Down));
        bindings.insert(alt_char('k'), focus(Direction::Up));
        bindings.insert(alt_char('l'), focus(Direction::Right));
        bindings.insert(
            alt_char('q'),
            Command::KillPane {
                target: Target::current(),
            },
        );
        bindings.insert(alt_char('d'), Command::DetachClient);
        bindings.insert(alt_char('v'), split(Split::Vertical));
        // Some terminals fold Shift into the uppercase char and drop the SHIFT
        // modifier; kitty-style protocols keep it. Register both so Alt+Shift+Q
        // reliably quits.
        bindings.insert(
            alt_char('Q'),
            Command::KillSession {
                target: Target::current(),
            },
        );
        bindings.insert(
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            Command::KillSession {
                target: Target::current(),
            },
        );
        bindings.insert(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            split(Split::Horizontal),
        );
        for n in 1u32..=9 {
            let digit = char::from(b'0' + u8::try_from(n).expect("1..=9 fits a byte"));
            bindings.insert(alt_char(digit), select_window(n));
        }

        Self { bindings }
    }
}

fn focus(direction: Direction) -> Command {
    Command::SelectPane {
        selector: PaneSelector::Direction(direction),
    }
}

fn split(split: Split) -> Command {
    Command::SplitWindow {
        split,
        target: Target::current(),
    }
}

fn select_window(index: u32) -> Command {
    Command::SelectWindow {
        target: Target {
            window: Some(WindowRef::Index(index)),
            ..Target::default()
        },
    }
}

fn normalize_key(event: &KeyEvent) -> Option<KeyEvent> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    Some(KeyEvent::new(event.code, event.modifiers))
}

fn alt_char(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::Keymap;
    use crate::command::Command;

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn alt_shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT | KeyModifiers::SHIFT)
    }

    /// The defaults are asserted against the parsed alias forms, which pins
    /// two things at once: the binding, and the alias meaning the same thing
    /// as the key. If `split-h` ever drifts from Alt+Enter, this fails.
    fn command(line: &str) -> Command {
        Command::parse_str(line).expect("alias parses")
    }

    #[test]
    fn default_alt_enter_splits_horizontally() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Enter)),
            Some(command("split-h"))
        );
    }

    #[test]
    fn default_alt_v_splits_vertically() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('v'))),
            Some(command("split-v"))
        );
    }

    #[test]
    fn default_alt_h_focuses_left() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('h'))),
            Some(command("focus-left"))
        );
    }

    #[test]
    fn default_alt_q_closes_pane() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('q'))),
            Some(command("close"))
        );
    }

    #[test]
    fn default_alt_d_detaches() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('d'))),
            Some(command("detach"))
        );
    }

    #[test]
    fn default_alt_shift_q_quits_with_or_without_shift_modifier() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('Q'))),
            Some(command("quit"))
        );
        assert_eq!(
            keymap.command_for(&alt_shift(KeyCode::Char('Q'))),
            Some(command("quit"))
        );
    }

    #[test]
    fn default_alt_digits_switch_workspaces() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('1'))),
            Some(command("workspace-1"))
        );
        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('9'))),
            Some(command("workspace-9"))
        );
        assert_eq!(keymap.command_for(&alt(KeyCode::Char('0'))), None);
    }

    #[test]
    fn unbound_key_returns_none() {
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&alt(KeyCode::Char('z'))), None);
    }

    #[test]
    fn default_alt_g_has_no_goto_window_binding() {
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&alt(KeyCode::Char('g'))), None);
    }

    #[test]
    fn unmodified_letter_passes_through() {
        // Plain letters now flow to the focused PTY; only Alt+ bindings trigger commands.
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            None
        );
    }
}
