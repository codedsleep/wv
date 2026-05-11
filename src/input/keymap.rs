//! `Keymap` and default key bindings.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command::Command;

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

    pub fn command_for(&self, event: &KeyEvent) -> Option<Command> {
        self.bindings.get(&normalize_key(event)?).copied()
    }
}

impl Default for Keymap {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert(alt_char('h'), Command::FocusLeft);
        bindings.insert(alt_char('j'), Command::FocusDown);
        bindings.insert(alt_char('k'), Command::FocusUp);
        bindings.insert(alt_char('l'), Command::FocusRight);
        bindings.insert(alt_char('q'), Command::Close);
        bindings.insert(alt_char('d'), Command::Detach);
        bindings.insert(alt_char('v'), Command::SplitV);
        // Some terminals fold Shift into the uppercase char and drop the SHIFT
        // modifier; kitty-style protocols keep it. Register both so Alt+Shift+Q
        // reliably quits.
        bindings.insert(alt_char('Q'), Command::Quit);
        bindings.insert(
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            Command::Quit,
        );
        bindings.insert(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            Command::SplitH,
        );

        Self { bindings }
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

    #[test]
    fn default_alt_enter_splits_horizontally() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Enter)),
            Some(Command::SplitH)
        );
    }

    #[test]
    fn default_alt_v_splits_vertically() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('v'))),
            Some(Command::SplitV)
        );
    }

    #[test]
    fn default_alt_h_focuses_left() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('h'))),
            Some(Command::FocusLeft)
        );
    }

    #[test]
    fn default_alt_q_closes_pane() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('q'))),
            Some(Command::Close)
        );
    }

    #[test]
    fn default_alt_d_detaches() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('d'))),
            Some(Command::Detach)
        );
    }

    #[test]
    fn default_alt_shift_q_quits_with_or_without_shift_modifier() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('Q'))),
            Some(Command::Quit)
        );
        assert_eq!(
            keymap.command_for(&alt_shift(KeyCode::Char('Q'))),
            Some(Command::Quit)
        );
    }

    #[test]
    fn unbound_key_returns_none() {
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&alt(KeyCode::Char('z'))), None);
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
