//! `Keymap`, `Mode`, bindings.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command::Command;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Normal,
    Prefix,
}

impl Mode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Prefix => "PREFIX",
        }
    }
}

#[derive(Clone)]
pub struct Keymap {
    prefix: KeyEvent,
    bindings: HashMap<KeyEvent, Command>,
}

impl Keymap {
    pub fn set_prefix(&mut self, prefix: KeyEvent) {
        self.prefix = normalize_key(&prefix).unwrap_or(prefix);
    }

    pub fn set_binding(&mut self, key: KeyEvent, command: Command) {
        if let Some(key) = normalize_key(&key) {
            self.bindings.insert(key, command);
        }
    }

    pub fn command_for(&self, event: &KeyEvent) -> Option<Command> {
        self.bindings.get(&normalize_key(event)?).copied()
    }

    pub fn is_prefix(&self, event: &KeyEvent) -> bool {
        normalize_key(event).is_some_and(|key| key == self.prefix)
    }
}

impl Default for Keymap {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        bindings.insert(char_key('s'), Command::SplitH);
        bindings.insert(char_key('v'), Command::SplitV);
        bindings.insert(char_key('h'), Command::FocusLeft);
        bindings.insert(char_key('j'), Command::FocusDown);
        bindings.insert(char_key('k'), Command::FocusUp);
        bindings.insert(char_key('l'), Command::FocusRight);
        bindings.insert(char_key('x'), Command::Close);
        bindings.insert(char_key('q'), Command::Quit);

        Self {
            prefix: KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
            bindings,
        }
    }
}

fn normalize_key(event: &KeyEvent) -> Option<KeyEvent> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    Some(KeyEvent::new(event.code, event.modifiers))
}

fn char_key(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Keymap, Mode};
    use crate::command::Command;

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    #[test]
    fn default_s_binding_splits_horizontally() {
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&key('s')), Some(Command::SplitH));
    }

    #[test]
    fn default_h_binding_focuses_left() {
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&key('h')), Some(Command::FocusLeft));
    }

    #[test]
    fn unbound_key_returns_none() {
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&key('z')), None);
    }

    #[test]
    fn default_prefix_is_ctrl_space() {
        let keymap = Keymap::default();

        assert!(keymap.is_prefix(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)));
    }

    #[test]
    fn mode_labels_match_status_bar_text() {
        assert_eq!(Mode::Normal.label(), "NORMAL");
        assert_eq!(Mode::Prefix.label(), "PREFIX");
    }
}
