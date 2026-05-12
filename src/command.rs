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
    Detach,
    Quit,
    SwitchWorkspace(u8),
}

impl Command {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.as_bytes() {
            b"split-h" => Some(Self::SplitH),
            b"split-v" => Some(Self::SplitV),
            b"focus-left" => Some(Self::FocusLeft),
            b"focus-right" => Some(Self::FocusRight),
            b"focus-up" => Some(Self::FocusUp),
            b"focus-down" => Some(Self::FocusDown),
            b"close" => Some(Self::Close),
            b"detach" => Some(Self::Detach),
            b"quit" => Some(Self::Quit),
            other => parse_switch_workspace(other),
        }
    }
}

fn parse_switch_workspace(bytes: &[u8]) -> Option<Command> {
    let rest = bytes.strip_prefix(b"workspace-")?;
    if rest.len() != 1 {
        return None;
    }
    match rest[0] {
        b'1'..=b'9' => Some(Command::SwitchWorkspace(rest[0] - b'0')),
        _ => None,
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
        assert_eq!(Command::from_str("detach"), Some(Command::Detach));
        assert_eq!(Command::from_str("quit"), Some(Command::Quit));
        assert_eq!(Command::from_str("split_h"), None);
    }

    #[test]
    fn parses_workspace_commands() {
        assert_eq!(
            Command::from_str("workspace-1"),
            Some(Command::SwitchWorkspace(1))
        );
        assert_eq!(
            Command::from_str("workspace-9"),
            Some(Command::SwitchWorkspace(9))
        );
        assert_eq!(Command::from_str("workspace-0"), None);
        assert_eq!(Command::from_str("workspace-10"), None);
    }
}
