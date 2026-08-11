//! tmux key names → [`KeyEvent`].
//!
//! `send-keys C-c` has to put the same byte on the PTY that pressing Ctrl-C
//! would. Rather than write a second encoder, this parses a tmux key name into
//! the same [`KeyEvent`] the terminal would deliver and hands it to
//! [`crate::input::encode`] — so typed and scripted keys cannot drift apart.
//!
//! Modifier prefixes stack in any order: `C-` (or `^`) for control, `M-` (or
//! `Alt-`) for meta, `S-` for shift. Names are matched case-insensitively.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Parse a key name as `bind-key` means it, where a bare character is a key.
///
/// `send-keys` and `bind-key` disagree about a lone `c`: for `send-keys` it is
/// text to type, for `bind-key` it is the C key. Same names otherwise.
pub fn parse_binding_key(name: &str) -> Option<KeyEvent> {
    if let Some(key) = parse_key_name(name) {
        return Some(key);
    }

    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Some(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        _ => None,
    }
}

/// Parse one tmux key name.
///
/// Returns `None` for anything that is not a key name; `send-keys` treats that
/// as literal text to type, which is what makes `send-keys 'npm run dev' Enter`
/// work.
pub fn parse_key_name(name: &str) -> Option<KeyEvent> {
    if name.is_empty() {
        return None;
    }

    let (modifiers, rest) = strip_modifiers(name);

    // A bare single character is only a key name when a modifier asked for it.
    // Without that, `a` is text to type, not a key to press.
    let mut chars = rest.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if modifiers.is_empty() {
            return None;
        }
        return Some(KeyEvent::new(KeyCode::Char(ch), modifiers));
    }

    let code = named_key(rest)?;

    Some(KeyEvent::new(code, modifiers))
}

/// Peel `C-`, `M-`, `S-` and `^` off the front of a key name.
fn strip_modifiers(name: &str) -> (KeyModifiers, &str) {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = name;

    loop {
        if let Some(stripped) = rest.strip_prefix('^') {
            modifiers |= KeyModifiers::CONTROL;
            rest = stripped;
            continue;
        }

        let lowered = rest.to_ascii_lowercase();
        if let Some(stripped) = lowered
            .strip_prefix("c-")
            .or_else(|| lowered.strip_prefix("ctrl-"))
        {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[rest.len() - stripped.len()..];
            continue;
        }
        if let Some(stripped) = lowered
            .strip_prefix("m-")
            .or_else(|| lowered.strip_prefix("alt-"))
        {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[rest.len() - stripped.len()..];
            continue;
        }
        if let Some(stripped) = lowered.strip_prefix("s-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[rest.len() - stripped.len()..];
            continue;
        }

        return (modifiers, rest);
    }
}

fn named_key(name: &str) -> Option<KeyCode> {
    // tmux's own spellings first, then the ones people reach for anyway.
    let code = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "btab" | "backtab" => KeyCode::BackTab,
        "bspace" | "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "ppage" | "pageup" | "pgup" => KeyCode::PageUp,
        "npage" | "pagedown" | "pgdn" => KeyCode::PageDown,
        "ic" | "insert" => KeyCode::Insert,
        "dc" | "delete" | "del" => KeyCode::Delete,
        function if function.starts_with('f') => {
            let number = function[1..].parse::<u8>().ok()?;
            if (1..=12).contains(&number) {
                KeyCode::F(number)
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some(code)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::parse_key_name;
    use crate::input::encode;

    fn bytes(name: &str) -> Vec<u8> {
        let key = parse_key_name(name).expect("key name parses");
        encode(&key).expect("key encodes")
    }

    #[test]
    fn control_keys_encode_to_control_bytes() {
        assert_eq!(bytes("C-c"), vec![0x03]);
        assert_eq!(bytes("^C"), vec![0x03]);
        assert_eq!(bytes("C-d"), vec![0x04]);
    }

    #[test]
    fn named_keys_encode_like_the_real_key() {
        assert_eq!(bytes("Enter"), b"\r".to_vec());
        assert_eq!(bytes("Escape"), b"\x1b".to_vec());
        assert_eq!(bytes("Up"), b"\x1b[A".to_vec());
        assert_eq!(bytes("Space"), b" ".to_vec());
        assert_eq!(bytes("Tab"), b"\t".to_vec());
        assert_eq!(bytes("BSpace"), b"\x7f".to_vec());
        assert_eq!(bytes("F5"), b"\x1b[15~".to_vec());
    }

    #[test]
    fn names_and_modifier_prefixes_are_case_insensitive() {
        assert_eq!(parse_key_name("enter"), parse_key_name("ENTER"));
        assert_eq!(parse_key_name("c-c"), parse_key_name("C-c"));
    }

    /// The character's own case is preserved, but control folds it away, so
    /// `C-C` and `C-c` still put the same byte on the wire — as in tmux.
    #[test]
    fn control_folds_the_case_of_its_character() {
        assert_eq!(bytes("C-C"), bytes("C-c"));
    }

    #[test]
    fn modifiers_stack() {
        assert_eq!(
            parse_key_name("M-C-x"),
            Some(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            ))
        );
    }

    #[test]
    fn meta_prefixes_the_escape_byte() {
        assert_eq!(bytes("M-x"), vec![0x1b, b'x']);
    }

    /// `bind-key c` means the C key, while `send-keys c` types a letter.
    #[test]
    fn binding_keys_accept_a_bare_character() {
        use super::parse_binding_key;

        assert_eq!(
            parse_binding_key("c"),
            Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        );
        assert_eq!(parse_binding_key("Enter"), parse_key_name("Enter"));
        assert_eq!(parse_binding_key("C-a"), parse_key_name("C-a"));
        // Still not a key: more than one character and no modifier.
        assert_eq!(parse_binding_key("hello"), None);
    }

    /// A bare word is text to type, not a key — this is what lets
    /// `send-keys 'npm run dev' Enter` do the obvious thing.
    #[test]
    fn plain_text_is_not_a_key_name() {
        assert_eq!(parse_key_name("a"), None);
        assert_eq!(parse_key_name("npm run dev"), None);
        assert_eq!(parse_key_name("hello"), None);
        assert_eq!(parse_key_name(""), None);
    }

    #[test]
    fn unknown_function_keys_are_not_key_names() {
        assert_eq!(parse_key_name("F13"), None);
        assert_eq!(parse_key_name("F0"), None);
    }
}
