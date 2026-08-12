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
//!
//! The modifier the root chords hang off is not fixed: see [`Leader`], which a
//! weave running inside another one moves to `Ctrl+Alt` so the outer instance
//! stops swallowing its keys.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command::target::{PaneRef, WindowRef};
use crate::command::{swap_toward, Command, PaneSelector, ResizeChange, Target};
use crate::layout::geometry::{Direction, Split};

/// The table consulted when no prefix has been pressed.
pub const ROOT_TABLE: &str = "root";
/// The table consulted immediately after the prefix key.
pub const PREFIX_TABLE: &str = "prefix";

/// The modifier weave's own chords hang off.
///
/// Normally `Alt`. A weave running inside another one — over SSH, almost
/// always — cannot use it: the outer instance matches `Alt+v` in its own root
/// table and the key never reaches the inner session.
///
/// The nested leader is `Ctrl+Alt` rather than plain `Ctrl` because `Ctrl`
/// alone is not weave's to take. `C-d`, `C-h`, `C-l`, `C-r`, `C-v` and `C-q`
/// all mean something to the shell in the pane, and a nested session that ate
/// them would cost the user their shell to save its own keys. Nothing sends
/// `Ctrl+Alt` chords, so they are free, and the outer weave passes them
/// through: its root table holds `Alt`, and a key event carrying `Ctrl` as
/// well does not match it.
///
/// See [`crate::input::nesting`] for how the choice is made.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Leader {
    Alt,
    CtrlAlt,
}

impl Leader {
    pub const fn modifier(self) -> KeyModifiers {
        match self {
            Self::Alt => KeyModifiers::ALT,
            Self::CtrlAlt => KeyModifiers::CONTROL.union(KeyModifiers::ALT),
        }
    }

    /// How the leader is written in a message to the user.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alt => "Alt",
            Self::CtrlAlt => "Ctrl+Alt",
        }
    }
}

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
    /// The modifier the root table's chords currently hang off.
    leader: Leader,
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

    pub fn leader(&self) -> Leader {
        self.leader
    }

    /// Move every root-table chord from the current leader onto `leader`.
    ///
    /// The rule is by modifier, not by binding: every root binding carrying
    /// all of the old leader's modifiers moves, whoever bound it. That keeps a
    /// user's own `bind -n M-s` working nested, as `C-M-s`. A binding on a
    /// modifier the leader does not use — a hand-bound `C-t` — stays put in
    /// both directions, since it carries no `Alt` either way. Bindings in the
    /// prefix table are left alone; they are reached through the prefix key,
    /// not the leader.
    ///
    /// Returns whether anything changed, so a caller can skip the work of
    /// announcing a move that did not happen.
    pub fn set_leader(&mut self, leader: Leader) -> bool {
        if self.leader == leader {
            return false;
        }

        let from = self.leader.modifier();
        let to = leader.modifier();
        self.leader = leader;

        let Some(root) = self.tables.get_mut(ROOT_TABLE) else {
            return true;
        };

        let mut moved: Vec<(KeyEvent, Binding)> = root
            .iter()
            .filter(|(key, _)| key.modifiers.contains(from))
            .map(|(key, binding)| {
                let mut key = *key;
                key.modifiers = key.modifiers.difference(from).union(to);
                (key, binding.clone())
            })
            .collect();
        // Two chords can land on one key — `M-q` moving onto a `C-q` that was
        // already bound. Sorting first makes which one wins the same on every
        // run rather than a matter of hash order.
        moved.sort_by_key(|(key, _)| format_key(key));

        root.retain(|key, _| !key.modifiers.contains(from));
        root.extend(moved);

        true
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
        // Alt+Shift+hjkl moves the focused pane itself, Hyprland-style: it
        // trades places with the neighbour in that direction and focus travels
        // with it. Terminals differ on whether Shift survives as a modifier —
        // some fold it into the uppercase char — so register both spellings.
        for (lower, upper, direction) in [
            ('h', 'H', Direction::Left),
            ('j', 'J', Direction::Down),
            ('k', 'K', Direction::Up),
            ('l', 'L', Direction::Right),
        ] {
            self.set_binding(alt_char(upper), swap_toward(direction));
            self.set_binding(
                KeyEvent::new(KeyCode::Char(upper), KeyModifiers::ALT | KeyModifiers::SHIFT),
                swap_toward(direction),
            );
            // Some terminals report Alt+Shift+h as lowercase plus SHIFT.
            self.set_binding(
                KeyEvent::new(KeyCode::Char(lower), KeyModifiers::ALT | KeyModifiers::SHIFT),
                swap_toward(direction),
            );
        }
        self.set_binding(alt_char('q'), kill_pane());
        // The goto picker. `;` because it is under the right hand and free of
        // any tmux or readline meaning, and because reaching another session
        // should cost one chord, not a detach and a re-attach.
        self.set_binding(alt_char(';'), Command::ChooseTree);
        self.set_binding(alt_char('d'), Command::DetachClient { target: None, all: false });
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

        // Renaming needs a name typed in, so these open a prompt rather than
        // running the rename directly. Both are prefilled with the current
        // name, so you edit rather than retype.
        self.set_binding(alt_char('r'), rename_window_prompt());
        // Some terminals fold Shift into the uppercase char and drop the SHIFT
        // modifier, as with Alt+Shift+Q; register both spellings.
        self.set_binding(alt_char('R'), rename_session_prompt());
        self.set_binding(
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            rename_session_prompt(),
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
        bind('d', Command::DetachClient { target: None, all: false });
        bind(
            'z',
            Command::ResizePane {
                target: Target::current(),
                change: ResizeChange::ToggleZoom,
            },
        );
        // tmux's own rename keys, for the same commands.
        bind(',', rename_window_prompt());
        bind('$', rename_session_prompt());
        bind('n', relative_window(WindowRef::Next));
        bind('p', relative_window(WindowRef::Previous));
        bind('l', relative_window(WindowRef::Last));
        bind('o', select_pane(PaneRef::Next));
        // tmux's two choose-* keys, both opening weave's one picker.
        bind('s', Command::ChooseTree);
        bind('w', Command::ChooseTree);
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
            leader: Leader::Alt,
        };

        keymap.install_root_defaults();
        keymap.install_prefix_defaults();

        keymap
    }
}

/// `Alt+R` / `C-b ,` — ask for a window name, prefilled with the current one.
fn rename_window_prompt() -> Command {
    Command::CommandPrompt {
        prompt: Some("rename-window:".to_owned()),
        initial: Some("#W".to_owned()),
        template: "rename-window %%".to_owned(),
    }
}

/// `Alt+Shift+R` / `C-b $` — ask for a session name, prefilled.
fn rename_session_prompt() -> Command {
    Command::CommandPrompt {
        prompt: Some("rename-session:".to_owned()),
        initial: Some("#S".to_owned()),
        template: "rename-session %%".to_owned(),
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
        all_but_target: false,
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

    use super::{
        focus, format_key, swap_toward, Binding, Keymap, Leader, PREFIX_TABLE, ROOT_TABLE,
    };
    use crate::command::Command;
    use crate::layout::geometry::Direction;

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
    fn default_alt_shift_hjkl_swaps_the_focused_pane_with_its_neighbour() {
        let keymap = Keymap::default();

        for (upper, lower, direction) in [
            ('H', 'h', Direction::Left),
            ('J', 'j', Direction::Down),
            ('K', 'k', Direction::Up),
            ('L', 'l', Direction::Right),
        ] {
            let expected = Some(swap_toward(direction));
            assert_eq!(keymap.command_for(&alt(KeyCode::Char(upper))), expected);
            assert_eq!(
                keymap.command_for(&alt_shift(KeyCode::Char(upper))),
                expected
            );
            assert_eq!(
                keymap.command_for(&alt_shift(KeyCode::Char(lower))),
                expected
            );
        }
    }

    #[test]
    fn default_alt_shift_l_does_not_shadow_alt_l_focus() {
        let keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('l'))),
            Some(focus(Direction::Right))
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

    /// `C-M-x`, the nested leader.
    fn ctrl_alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::ALT)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// The whole point of nesting: the chords move to Ctrl+Alt, where the weave
    /// running outside this one will not have eaten them first.
    #[test]
    fn the_nested_leader_moves_every_root_chord_to_ctrl_alt() {
        let mut keymap = Keymap::default();

        assert!(keymap.set_leader(Leader::CtrlAlt));

        assert_eq!(keymap.leader(), Leader::CtrlAlt);
        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Char('v'))),
            Some(command("split-v"))
        );
        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Char('h'))),
            Some(command("focus-left"))
        );
        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Enter)),
            Some(command("split-h"))
        );
        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Char('1'))),
            Some(command("workspace-1"))
        );
        // And the Alt spellings are gone, so the outer weave's chords are not
        // shadowed by an inner one that also answers to them.
        assert_eq!(keymap.command_for(&alt(KeyCode::Char('v'))), None);
        assert_eq!(keymap.command_for(&alt(KeyCode::Char('h'))), None);
    }

    /// The keys the shell in the pane needs are exactly the ones a plain-Ctrl
    /// leader would have taken, so this is the assertion that says why the
    /// nested leader is Ctrl+Alt and not Ctrl.
    #[test]
    fn the_nested_leader_leaves_the_shells_ctrl_keys_alone() {
        let mut keymap = Keymap::default();

        keymap.set_leader(Leader::CtrlAlt);

        for ch in ['d', 'h', 'l', 'r', 'v', 'q', 'c'] {
            assert_eq!(
                keymap.command_for(&ctrl_key(KeyCode::Char(ch))),
                None,
                "C-{ch} belongs to the pane, not to weave"
            );
        }
    }

    /// Shift rides along: `M-S-h` becomes `C-M-S-h`, not `C-M-h`.
    #[test]
    fn the_nested_leader_keeps_the_other_modifiers() {
        let mut keymap = Keymap::default();

        keymap.set_leader(Leader::CtrlAlt);

        assert_eq!(
            keymap.command_for(&KeyEvent::new(
                KeyCode::Char('h'),
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT
            )),
            Some(swap_toward(Direction::Left))
        );
        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Char('h'))),
            Some(focus(Direction::Left)),
            "the unshifted chord still focuses"
        );
    }

    /// The prefix table is reached through the prefix key, not the leader, so
    /// nesting must leave it exactly as it was.
    #[test]
    fn the_nested_leader_leaves_the_prefix_table_alone() {
        let mut keymap = Keymap::default();

        keymap.set_leader(Leader::CtrlAlt);

        assert_eq!(
            keymap
                .lookup(PREFIX_TABLE, &plain('%'))
                .map(|binding| &binding.command),
            Some(&command("split-window -h"))
        );
        assert!(keymap
            .lookup(PREFIX_TABLE, &KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL))
            .is_some_and(|binding| binding.repeat));
    }

    /// Detaching the SSH client and reattaching locally puts the chords back.
    #[test]
    fn the_leader_moves_back_and_reports_when_it_did_not_move() {
        let mut keymap = Keymap::default();

        assert!(keymap.set_leader(Leader::CtrlAlt));
        assert!(!keymap.set_leader(Leader::CtrlAlt), "already there");
        assert!(keymap.set_leader(Leader::Alt));

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('v'))),
            Some(command("split-v"))
        );
        assert_eq!(keymap.command_for(&ctrl_alt(KeyCode::Char('v'))), None);
    }

    /// The goto picker, on the chord and on both of tmux's choose-* keys — and
    /// it relocates when nested like every other root chord.
    #[test]
    fn the_picker_is_bound_to_alt_semicolon_and_follows_the_leader() {
        let mut keymap = Keymap::default();

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char(';'))),
            Some(Command::ChooseTree)
        );
        for ch in ['s', 'w'] {
            assert_eq!(
                keymap
                    .lookup(PREFIX_TABLE, &KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                    .map(|binding| binding.command.clone()),
                Some(Command::ChooseTree),
                "C-b {ch}"
            );
        }

        keymap.set_leader(Leader::CtrlAlt);

        assert_eq!(keymap.command_for(&alt(KeyCode::Char(';'))), None);
        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Char(';'))),
            Some(Command::ChooseTree),
            "over SSH the picker is Ctrl+Alt+; so the outer weave keeps Alt+;"
        );
    }

    /// A binding the user added moves with the built-in ones — nesting must not
    /// leave half of someone's config unreachable. One on a modifier the leader
    /// does not use stays exactly where it was put, in both directions.
    #[test]
    fn a_user_bound_chord_moves_with_the_leader_and_a_ctrl_one_does_not() {
        let mut keymap = Keymap::default();
        keymap.bind(
            ROOT_TABLE,
            alt(KeyCode::Char('s')),
            Binding::new(command("split-v")),
        );
        keymap.bind(
            ROOT_TABLE,
            ctrl_key(KeyCode::Char('t')),
            Binding::new(command("split-h")),
        );

        keymap.set_leader(Leader::CtrlAlt);

        assert_eq!(
            keymap.command_for(&ctrl_alt(KeyCode::Char('s'))),
            Some(command("split-v"))
        );
        assert_eq!(
            keymap.command_for(&ctrl_key(KeyCode::Char('t'))),
            Some(command("split-h")),
            "C-t carries no Alt, so the leader has no claim on it"
        );

        keymap.set_leader(Leader::Alt);

        assert_eq!(
            keymap.command_for(&alt(KeyCode::Char('s'))),
            Some(command("split-v"))
        );
        assert_eq!(
            keymap.command_for(&ctrl_key(KeyCode::Char('t'))),
            Some(command("split-h")),
            "and moving back must not drag it onto M-t"
        );
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
