//! Key tables, the prefix state machine, and the default bindings.
//!
//! tmux is modal: most keys reach the pane, and a prefix key (`C-b`) switches
//! to a table where the next key is a command. weave was direct-chord only —
//! `Alt+v` splits, everything else passes through. Both now coexist:
//!
//! - The **root** table holds keys that act with no prefix. weave's `Alt`
//!   chords live here, as does tmux's `bind -n`.
//! - The **prefix** table holds keys that act after the prefix. tmux's default
//!   bindings live here, so `C-b %` splits.
//!
//! A key in no table reaches the pane, which is what makes typing work.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command::target::{PaneRef, WindowRef};
use crate::command::{Command, PaneSelector, ResizeChange, Target};
use crate::layout::geometry::{Direction, Split};

/// The table consulted when no prefix has been pressed.
pub const ROOT_TABLE: &str = "root";
/// The table consulted immediately after the prefix key.
pub const PREFIX_TABLE: &str = "prefix";

/// A bound key: what it runs, and whether it can repeat without the prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    pub command: Command,
    /// tmux's `-r`: after this fires, stay in the prefix table briefly so the
    /// key can be pressed again without repeating the prefix.
    pub repeat: bool,
}

impl Binding {
    pub fn new(command: Command) -> Self {
        Self {
            command,
            repeat: false,
        }
    }

    pub fn repeating(command: Command) -> Self {
        Self {
            command,
            repeat: true,
        }
    }
}

#[derive(Clone)]
pub struct Keymap {
    tables: HashMap<String, HashMap<KeyEvent, Binding>>,
    /// The keys that switch into the prefix table. Two, because tmux has
    /// `prefix` and `prefix2`.
    prefix: Vec<KeyEvent>,
}

impl Keymap {
    /// Bind `key` in `table`, replacing whatever was there.
    pub fn bind(&mut self, table: &str, key: KeyEvent, binding: Binding) {
        let Some(key) = normalize_key(&key) else {
            return;
        };
        self.tables
            .entry(table.to_owned())
            .or_default()
            .insert(key, binding);
    }

    /// Bind in the root table, the shape weave's config has always had.
    pub fn set_binding(&mut self, key: KeyEvent, command: Command) {
        self.bind(ROOT_TABLE, key, Binding::new(command));
    }

    pub fn unbind(&mut self, table: &str, key: KeyEvent) -> bool {
        let Some(key) = normalize_key(&key) else {
            return false;
        };
        self.tables
            .get_mut(table)
            .is_some_and(|bindings| bindings.remove(&key).is_some())
    }

    /// Drop every binding in a table.
    pub fn unbind_all(&mut self, table: &str) {
        self.tables.remove(table);
    }

    /// What `event` runs in `table`, if anything.
    pub fn lookup(&self, table: &str, event: &KeyEvent) -> Option<&Binding> {
        let key = normalize_key(event)?;
        self.tables.get(table)?.get(&key)
    }

    /// The command bound in the root table.
    pub fn command_for(&self, event: &KeyEvent) -> Option<Command> {
        self.lookup(ROOT_TABLE, event)
            .map(|binding| binding.command.clone())
    }

    /// Whether this key switches into the prefix table.
    pub fn is_prefix(&self, event: &KeyEvent) -> bool {
        normalize_key(event).is_some_and(|key| self.prefix.contains(&key))
    }

    pub fn set_prefix(&mut self, keys: &[KeyEvent]) {
        self.prefix = keys.iter().filter_map(normalize_key).collect();
    }

    pub fn prefix_keys(&self) -> &[KeyEvent] {
        &self.prefix
    }

    /// Every binding, table by table, sorted for stable `list-keys` output.
    pub fn all_bindings(&self) -> Vec<(String, KeyEvent, Binding)> {
        let mut out: Vec<(String, KeyEvent, Binding)> = self
            .tables
            .iter()
            .flat_map(|(table, bindings)| {
                bindings
                    .iter()
                    .map(move |(key, binding)| (table.clone(), *key, binding.clone()))
            })
            .collect();
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| format_key(&a.1).cmp(&format_key(&b.1)))
        });

        out
    }

    /// weave's own Alt chords, unchanged: they act without a prefix.
    fn install_root_defaults(&mut self) {
        self.set_binding(alt_char('h'), focus(Direction::Left));
        self.set_binding(alt_char('j'), focus(Direction::Down));
        self.set_binding(alt_char('k'), focus(Direction::Up));
        self.set_binding(alt_char('l'), focus(Direction::Right));
        self.set_binding(alt_char('q'), kill_pane());
        self.set_binding(alt_char('d'), Command::DetachClient);
        self.set_binding(alt_char('v'), split(Split::Vertical));
        // Some terminals fold Shift into the uppercase char and drop the SHIFT
        // modifier; kitty-style protocols keep it. Register both so Alt+Shift+Q
        // reliably quits.
        self.set_binding(alt_char('Q'), kill_session());
        self.set_binding(
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            kill_session(),
        );
        self.set_binding(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            split(Split::Horizontal),
        );

        for number in 1..=9u32 {
            let digit = char::from(b'0' + u8::try_from(number).expect("1..=9 fits a byte"));
            self.set_binding(alt_char(digit), select_window(number, true));
        }
    }

    /// tmux's defaults, so muscle memory works after `C-b`.
    fn install_prefix_defaults(&mut self) {
        let mut bind = |ch: char, command: Command| {
            let key = plain(ch);
            self.tables
                .entry(PREFIX_TABLE.to_owned())
                .or_default()
                .insert(key, Binding::new(command));
        };

        bind(
            'c',
            Command::NewWindow {
                target: Target::current(),
                name: None,
                command: None,
                detached: false,
            },
        );
        bind('%', split(Split::Vertical));
        bind('"', split(Split::Horizontal));
        bind('x', kill_pane());
        bind(
            '&',
            Command::KillWindow {
                target: Target::current(),
            },
        );
        bind('d', Command::DetachClient);
        bind(
            'z',
            Command::ResizePane {
                target: Target::current(),
                change: ResizeChange::ToggleZoom,
            },
        );
        bind('n', relative_window(WindowRef::Next));
        bind('p', relative_window(WindowRef::Previous));
        bind('l', relative_window(WindowRef::Last));
        bind('o', select_pane(PaneRef::Next));
        bind('}', swap_pane(PaneRef::Next));
        bind('{', swap_pane(PaneRef::Previous));

        for number in 0..=9u32 {
            let digit = char::from(b'0' + u8::try_from(number).expect("0..=9 fits a byte"));
            bind(digit, select_window(number, false));
        }

        // Arrows move focus. `C-b l` is tmux's last-window, so `h`/`j`/`k` get
        // the vi spelling but `l` does not — the arrow covers it.
        for (code, ch, direction) in [
            (KeyCode::Left, Some('h'), Direction::Left),
            (KeyCode::Down, Some('j'), Direction::Down),
            (KeyCode::Up, Some('k'), Direction::Up),
            (KeyCode::Right, None, Direction::Right),
        ] {
            self.bind(
                PREFIX_TABLE,
                KeyEvent::new(code, KeyModifiers::NONE),
                Binding::new(focus(direction)),
            );
            if let Some(ch) = ch {
                self.bind(PREFIX_TABLE, plain(ch), Binding::new(focus(direction)));
            }

            // Repeating resizes: hold the key rather than re-pressing prefix.
            self.bind(
                PREFIX_TABLE,
                KeyEvent::new(code, KeyModifiers::CONTROL),
                Binding::repeating(Command::ResizePane {
                    target: Target::current(),
                    change: ResizeChange::By {
                        direction,
                        cells: 1,
                    },
                }),
            );
        }
    }
}

impl Default for Keymap {
    fn default() -> Self {
        let mut keymap = Self {
            tables: HashMap::new(),
            prefix: vec![ctrl('b')],
        };

        keymap.install_root_defaults();
        keymap.install_prefix_defaults();

        keymap
    }
}

fn focus(direction: Direction) -> Command {
    Command::SelectPane {
        selector: PaneSelector::Direction(direction),
    }
}

fn select_pane(pane: PaneRef) -> Command {
    Command::SelectPane {
        selector: PaneSelector::Target(Target {
            pane: Some(pane),
            ..Target::default()
        }),
    }
}

fn swap_pane(pane: PaneRef) -> Command {
    Command::SwapPane {
        source: Target::current(),
        target: Target {
            pane: Some(pane),
            ..Target::default()
        },
        keep_focus: false,
    }
}

fn split(split: Split) -> Command {
    Command::SplitWindow {
        split,
        target: Target::current(),
        command: None,
        detached: false,
        size: None,
    }
}

fn kill_pane() -> Command {
    Command::KillPane {
        target: Target::current(),
    }
}

fn kill_session() -> Command {
    Command::KillSession {
        target: Target::current(),
    }
}

fn select_window(index: u32, create: bool) -> Command {
    Command::SelectWindow {
        target: Target {
            window: Some(WindowRef::Index(index)),
            ..Target::default()
        },
        create,
    }
}

fn relative_window(window: WindowRef) -> Command {
    Command::SelectWindow {
        target: Target {
            window: Some(window),
            ..Target::default()
        },
        create: false,
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

fn plain(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
}

fn ctrl(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}

/// Render a key the way tmux writes it, for `list-keys`.
pub fn format_key(key: &KeyEvent) -> String {
    let mut out = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("C-");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        out.push_str("M-");
    }

    let name = match key.code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Escape".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "BTab".to_owned(),
        KeyCode::Backspace => "BSpace".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PPage".to_owned(),
        KeyCode::PageDown => "NPage".to_owned(),
        KeyCode::Insert => "IC".to_owned(),
        KeyCode::Delete => "DC".to_owned(),
        KeyCode::F(number) => format!("F{number}"),
        other => format!("{other:?}"),
    };
    out.push_str(&name);

    out
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{format_key, Binding, Keymap, PREFIX_TABLE, ROOT_TABLE};
    use crate::command::Command;

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn alt_shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT | KeyModifiers::SHIFT)
    }

    fn plain(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    /// The defaults are asserted against the parsed alias forms, which pins
    /// two things at once: the binding, and the alias meaning the same thing
    /// as the key.
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
    fn unmodified_letter_passes_through_the_root_table() {
        // Plain letters reach the PTY; only bound keys trigger commands, and
        // the tmux defaults live behind the prefix.
        let keymap = Keymap::default();

        assert_eq!(keymap.command_for(&plain('c')), None);
        assert_eq!(keymap.command_for(&plain('h')), None);
    }

    #[test]
    fn ctrl_b_is_the_default_prefix() {
        let keymap = Keymap::default();

        assert!(keymap.is_prefix(&KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL
        )));
        assert!(!keymap.is_prefix(&plain('b')));
    }

    /// tmux muscle memory: `C-b %` and `C-b "` split, `C-b c` makes a window.
    #[test]
    fn tmux_defaults_live_in_the_prefix_table() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap
                .lookup(PREFIX_TABLE, &plain('%'))
                .map(|binding| &binding.command),
            Some(&command("split-window -h"))
        );
        assert_eq!(
            keymap
                .lookup(PREFIX_TABLE, &plain('"'))
                .map(|binding| &binding.command),
            Some(&command("split-window -v"))
        );
        assert_eq!(
            keymap
                .lookup(PREFIX_TABLE, &plain('x'))
                .map(|binding| &binding.command),
            Some(&command("kill-pane"))
        );
        assert!(keymap.lookup(PREFIX_TABLE, &plain('c')).is_some());
    }

    /// `Alt+N` creates a window if the slot is empty; the prefix digits follow
    /// tmux and only select, so `C-b 3` on an empty slot fails rather than
    /// quietly making one.
    #[test]
    fn prefix_digits_select_without_creating() {
        let keymap = Keymap::default();

        let Some(Command::SelectWindow { create, .. }) = keymap
            .lookup(PREFIX_TABLE, &plain('3'))
            .map(|binding| binding.command.clone())
        else {
            panic!("expected a select-window");
        };
        assert!(!create, "the prefix table follows tmux, not Alt+N");
    }

    #[test]
    fn resize_bindings_repeat() {
        let keymap = Keymap::default();

        let binding = keymap
            .lookup(
                PREFIX_TABLE,
                &KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
            )
            .expect("C-b C-Left resizes");
        assert!(binding.repeat, "resize should be a repeating binding");
    }

    #[test]
    fn binding_and_unbinding_round_trip() {
        let mut keymap = Keymap::default();
        let key = plain('Z');

        keymap.bind(ROOT_TABLE, key, Binding::new(command("detach")));
        assert_eq!(keymap.command_for(&key), Some(command("detach")));

        assert!(keymap.unbind(ROOT_TABLE, key));
        assert_eq!(keymap.command_for(&key), None);
        assert!(!keymap.unbind(ROOT_TABLE, key), "already gone");
    }

    #[test]
    fn formats_keys_the_way_tmux_writes_them() {
        assert_eq!(format_key(&plain('a')), "a");
        assert_eq!(
            format_key(&KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            "C-b"
        );
        assert_eq!(format_key(&alt(KeyCode::Char('h'))), "M-h");
        assert_eq!(format_key(&plain(' ')), "Space");
        assert_eq!(
            format_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            "Enter"
        );
    }
}
