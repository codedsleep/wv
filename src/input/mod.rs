//! Input module exports.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub mod keymap;

pub fn encode(event: &KeyEvent) -> Option<Vec<u8>> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    match event.code {
        KeyCode::Char(ch) => Some(encode_char(ch, event.modifiers)),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1bOH".to_vec()),
        KeyCode::End => Some(b"\x1bOF".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(number) => encode_function_key(number),
        _ => None,
    }
}

fn encode_char(ch: char, modifiers: KeyModifiers) -> Vec<u8> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return encode_ctrl_char(ch);
    }

    let mut bytes = Vec::new();
    if modifiers.contains(KeyModifiers::ALT) {
        bytes.push(0x1b);
    }
    bytes.extend(ch.to_string().as_bytes());

    bytes
}

fn encode_ctrl_char(ch: char) -> Vec<u8> {
    if ch.is_ascii_alphabetic() {
        let byte = ch.to_ascii_lowercase() as u8 - b'a' + 1;
        return vec![byte];
    }

    ch.to_string().into_bytes()
}

fn encode_function_key(number: u8) -> Option<Vec<u8>> {
    let bytes = match number {
        1 => &b"\x1bOP"[..],
        2 => &b"\x1bOQ"[..],
        3 => &b"\x1bOR"[..],
        4 => &b"\x1bOS"[..],
        5 => &b"\x1b[15~"[..],
        6 => &b"\x1b[17~"[..],
        7 => &b"\x1b[18~"[..],
        8 => &b"\x1b[19~"[..],
        9 => &b"\x1b[20~"[..],
        10 => &b"\x1b[21~"[..],
        11 => &b"\x1b[23~"[..],
        12 => &b"\x1b[24~"[..],
        _ => return None,
    };

    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::encode;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn encode_plain_char_returns_utf8() {
        assert_eq!(
            encode(&key(KeyCode::Char('é'), KeyModifiers::NONE)),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn encode_ctrl_a_returns_control_byte() {
        assert_eq!(
            encode(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
    }

    #[test]
    fn encode_ctrl_c_returns_control_byte() {
        assert_eq!(
            encode(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn encode_up_returns_escape_sequence() {
        assert_eq!(
            encode(&key(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn encode_enter_returns_carriage_return() {
        assert_eq!(
            encode(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn encode_release_returns_none() {
        let event = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };

        assert_eq!(encode(&event), None);
    }
}
