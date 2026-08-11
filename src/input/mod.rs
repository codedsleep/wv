//! Input module exports.
//!
//! Keys reach a pane in one of two encodings. The legacy one is what every
//! terminal has always sent and every program understands, but it cannot
//! express most modified keys: there is no byte sequence for Shift+Enter, and
//! ESC is the same `\x1b` that starts every escape sequence, so a program has
//! to disambiguate it by timing.
//!
//! The kitty keyboard protocol fixes both, and a pane's program opts into it
//! (see [`crate::term::query`]). Keys are then encoded against the flags that
//! pane asked for, which is why encoding takes them as an argument.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub mod keymap;
pub mod keys;

/// Kitty keyboard flag bit 1: report keys unambiguously, ESC included.
///
/// This is the only flag whose encoding weave implements. The others ask for
/// key release events and for all keys as escape sequences, neither of which
/// changes how the keys weave does send are spelled.
pub const DISAMBIGUATE_ESCAPE_CODES: u8 = 0b1;

/// Encode a key the way a pane with no protocol enabled expects it.
pub fn encode(event: &KeyEvent) -> Option<Vec<u8>> {
    encode_with(event, 0)
}

/// Encode a key for a pane, honouring the kitty keyboard `flags` it asked for.
///
/// Modified keys that the legacy encoding cannot express are sent in the
/// protocol's `CSI code ; modifiers u` form whatever the flags say. Nothing is
/// lost by doing so — the alternative is dropping the modifier and sending a
/// key the user did not press — and it is what tmux does with `extended-keys`
/// turned on.
pub fn encode_with(event: &KeyEvent, flags: u8) -> Option<Vec<u8>> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    let disambiguate = flags & DISAMBIGUATE_ESCAPE_CODES != 0;
    let modifiers = encode_modifiers(event.modifiers);

    match event.code {
        KeyCode::Char(ch) => Some(encode_char(ch, event.modifiers, disambiguate)),
        // A program that asked to tell ESC apart gets the form that does. The
        // bare byte is right for everyone else.
        KeyCode::Esc if disambiguate => Some(csi_u(27, modifiers)),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        // These three have a legacy encoding only while unmodified.
        KeyCode::Enter if modifiers > 1 => Some(csi_u(13, modifiers)),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace if modifiers > 1 => Some(csi_u(127, modifiers)),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Tab if modifiers > 1 => Some(csi_u(9, modifiers)),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Up => Some(csi_arrow(b'A', modifiers)),
        KeyCode::Down => Some(csi_arrow(b'B', modifiers)),
        KeyCode::Right => Some(csi_arrow(b'C', modifiers)),
        KeyCode::Left => Some(csi_arrow(b'D', modifiers)),
        // Unmodified Home and End keep the SS3 spelling they have always had
        // here; modified, they take the CSI form that has room for parameters.
        KeyCode::Home if modifiers > 1 => Some(csi_arrow(b'H', modifiers)),
        KeyCode::Home => Some(b"\x1bOH".to_vec()),
        KeyCode::End if modifiers > 1 => Some(csi_arrow(b'F', modifiers)),
        KeyCode::End => Some(b"\x1bOF".to_vec()),
        KeyCode::PageUp => Some(csi_tilde(5, modifiers)),
        KeyCode::PageDown => Some(csi_tilde(6, modifiers)),
        KeyCode::Insert => Some(csi_tilde(2, modifiers)),
        KeyCode::Delete => Some(csi_tilde(3, modifiers)),
        KeyCode::F(number) => encode_function_key(number, modifiers),
        _ => None,
    }
}

/// The protocol's modifier parameter: a bitmask, biased by one so that "no
/// modifiers" is 1 and can be told from an absent parameter.
fn encode_modifiers(modifiers: KeyModifiers) -> u8 {
    let mut encoded = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        encoded |= 0b1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        encoded |= 0b10;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        encoded |= 0b100;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        encoded |= 0b1000;
    }

    encoded + 1
}

/// `CSI code ; modifiers u`, with the modifier parameter left off when there
/// are none — the shorter form every parser accepts.
fn csi_u(code: u32, modifiers: u8) -> Vec<u8> {
    if modifiers <= 1 {
        return format!("\x1b[{code}u").into_bytes();
    }

    format!("\x1b[{code};{modifiers}u").into_bytes()
}

/// `CSI 1 ; modifiers <final>` for the arrow and cursor keys. Unmodified, the
/// parameters are omitted, which is the sequence they have always sent.
fn csi_arrow(final_byte: u8, modifiers: u8) -> Vec<u8> {
    let final_char = char::from(final_byte);
    if modifiers <= 1 {
        return format!("\x1b[{final_char}").into_bytes();
    }

    format!("\x1b[1;{modifiers}{final_char}").into_bytes()
}

/// `CSI number ; modifiers ~` for the keypad-style keys.
fn csi_tilde(number: u8, modifiers: u8) -> Vec<u8> {
    if modifiers <= 1 {
        return format!("\x1b[{number}~").into_bytes();
    }

    format!("\x1b[{number};{modifiers}~").into_bytes()
}

fn encode_char(ch: char, modifiers: KeyModifiers, disambiguate: bool) -> Vec<u8> {
    // A program that asked for the protocol wants Ctrl+C as `CSI 99;5u`, not
    // as the `\x03` control byte the legacy encoding collapses it to.
    if disambiguate && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) {
        return csi_u(u32::from(ch.to_ascii_lowercase()), encode_modifiers(modifiers));
    }

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

fn encode_function_key(number: u8, modifiers: u8) -> Option<Vec<u8>> {
    // F1-F4 are SS3 sequences unmodified and CSI ones when modified, because
    // SS3 has nowhere to put a parameter.
    if let Some(final_byte) = match number {
        1 => Some(b'P'),
        2 => Some(b'Q'),
        3 => Some(b'R'),
        4 => Some(b'S'),
        _ => None,
    } {
        let final_char = char::from(final_byte);
        if modifiers <= 1 {
            return Some(format!("\x1bO{final_char}").into_bytes());
        }
        return Some(format!("\x1b[1;{modifiers}{final_char}").into_bytes());
    }

    let number = match number {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };

    Some(csi_tilde(number, modifiers))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{encode, encode_with};

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

    /// The whole point of the protocol: a program that asked to tell ESC apart
    /// gets a sequence that cannot be confused with the start of another one.
    #[test]
    fn esc_is_unambiguous_only_for_a_pane_that_asked_for_it() {
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(encode(&esc), Some(b"\x1b".to_vec()));
        assert_eq!(
            encode_with(&esc, super::DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1b[27u".to_vec())
        );
    }

    /// Shift+Enter has no legacy encoding at all, so dropping the modifier
    /// would send a plain Enter — a different keypress than the one made. It
    /// goes out in the protocol's form whether or not the pane asked.
    #[test]
    fn modified_enter_is_sent_in_csi_u_form_regardless_of_flags() {
        let shift_enter = key(KeyCode::Enter, KeyModifiers::SHIFT);

        assert_eq!(encode(&shift_enter), Some(b"\x1b[13;2u".to_vec()));
        assert_eq!(
            encode_with(&shift_enter, super::DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1b[13;2u".to_vec())
        );
    }

    #[test]
    fn unmodified_keys_keep_their_legacy_encoding_under_the_protocol() {
        for (code, expected) in [
            (KeyCode::Enter, &b"\r"[..]),
            (KeyCode::Tab, &b"\t"[..]),
            (KeyCode::Backspace, &b"\x7f"[..]),
            (KeyCode::Up, &b"\x1b[A"[..]),
            (KeyCode::Home, &b"\x1bOH"[..]),
        ] {
            let event = key(code, KeyModifiers::NONE);
            assert_eq!(
                encode_with(&event, super::DISAMBIGUATE_ESCAPE_CODES),
                Some(expected.to_vec()),
                "{code:?} should keep its legacy encoding"
            );
        }
    }

    /// Modified arrows have had a legacy encoding since xterm; it was the
    /// modifier being dropped that lost them, not the lack of a spelling.
    #[test]
    fn modified_arrows_carry_their_modifier() {
        assert_eq!(
            encode(&key(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            encode(&key(KeyCode::Left, KeyModifiers::SHIFT | KeyModifiers::ALT)),
            Some(b"\x1b[1;4D".to_vec())
        );
    }

    /// Ctrl+C collapses to a control byte for a pane using the legacy
    /// encoding, and stays a distinct key for one that asked for the protocol.
    #[test]
    fn ctrl_char_encoding_follows_the_panes_flags() {
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(encode(&ctrl_c), Some(vec![0x03]));
        assert_eq!(
            encode_with(&ctrl_c, super::DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1b[99;5u".to_vec())
        );
    }

    #[test]
    fn plain_typing_is_untouched_by_the_protocol() {
        let typed = key(KeyCode::Char('a'), KeyModifiers::NONE);
        let shifted = key(KeyCode::Char('A'), KeyModifiers::SHIFT);

        assert_eq!(
            encode_with(&typed, super::DISAMBIGUATE_ESCAPE_CODES),
            Some(b"a".to_vec())
        );
        assert_eq!(
            encode_with(&shifted, super::DISAMBIGUATE_ESCAPE_CODES),
            Some(b"A".to_vec())
        );
    }

    #[test]
    fn modified_function_keys_switch_to_the_parameterised_form() {
        assert_eq!(
            encode(&key(KeyCode::F(1), KeyModifiers::NONE)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode(&key(KeyCode::F(1), KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5P".to_vec())
        );
        assert_eq!(
            encode(&key(KeyCode::F(5), KeyModifiers::SHIFT)),
            Some(b"\x1b[15;2~".to_vec())
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
