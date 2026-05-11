//! Command enum + parsing.

#![allow(dead_code)]

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    SplitH,
    SplitV,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    Close,
    Quit,
}

impl Command {
    #[allow(clippy::should_implement_trait)]
    pub const fn from_str(s: &str) -> Option<Self> {
        match s.as_bytes() {
            b"split-h" => Some(Self::SplitH),
            b"split-v" => Some(Self::SplitV),
            b"focus-left" => Some(Self::FocusLeft),
            b"focus-right" => Some(Self::FocusRight),
            b"focus-up" => Some(Self::FocusUp),
            b"focus-down" => Some(Self::FocusDown),
            b"close" => Some(Self::Close),
            b"quit" => Some(Self::Quit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Command;

    #[test]
    fn parses_kebab_case_commands() {
        assert_eq!(Command::from_str("split-h"), Some(Command::SplitH));
        assert_eq!(Command::from_str("split-v"), Some(Command::SplitV));
        assert_eq!(Command::from_str("focus-left"), Some(Command::FocusLeft));
        assert_eq!(Command::from_str("focus-right"), Some(Command::FocusRight));
        assert_eq!(Command::from_str("focus-up"), Some(Command::FocusUp));
        assert_eq!(Command::from_str("focus-down"), Some(Command::FocusDown));
        assert_eq!(Command::from_str("close"), Some(Command::Close));
        assert_eq!(Command::from_str("quit"), Some(Command::Quit));
        assert_eq!(Command::from_str("split_h"), None);
    }
}
