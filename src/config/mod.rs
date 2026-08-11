//! Config: the TOML schema, the tmux-syntax file, and the option registry.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;
use serde::Deserialize;

pub mod options;
pub mod tmux_conf;

use crate::command::Command;
use crate::input::keymap::Keymap;
pub use options::{OptionError, Options};

pub const DEFAULT_TARGET_FPS: u16 = 160;
pub const MIN_TARGET_FPS: u16 = 30;
pub const MAX_TARGET_FPS: u16 = 240;

#[derive(Clone)]
pub struct Config {
    pub keymap: Keymap,
    pub options: Options,
    pub ui: UiConfig,
    pub theme: ThemeConfig,
    /// Config lines that could not be honoured, with where they came from.
    ///
    /// Collected rather than fatal: one unsupported line in a long
    /// `.tmux.conf` should not cost you the other forty.
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiConfig {
    pub border_color: Color,
    pub status_bar: bool,
    pub pane_titles: bool,
    pub target_fps: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeConfig {
    pub border_focused: Color,
    pub border_unfocused: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    pub accent: Color,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid key `{0}`")]
    InvalidKey(String),
    #[error("invalid command `{line}`: {source}")]
    InvalidCommand {
        line: String,
        #[source]
        source: crate::command::CommandError,
    },
    #[error("toml parse failed: {0}")]
    Toml(#[from] toml::de::Error),
}

impl Config {
    /// Load the TOML config, then the tmux-syntax one over it.
    ///
    /// Both are optional. The `.conf` file is applied second and therefore
    /// wins, because it is the imperative one: its lines are commands, and a
    /// command that runs later is the one that took effect.
    pub fn load() -> Self {
        let mut config = Self::load_toml();
        config.apply_conf_file(&tmux_conf::conf_path());

        for diagnostic in &config.diagnostics {
            tracing::warn!("{diagnostic}");
        }

        config
    }

    fn load_toml() -> Self {
        let path = config_path();
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                tracing::warn!("failed to read config {}: {error}", path.display());
                return Self::default();
            }
        };

        match Self::from_toml_str(&source) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!("failed to parse config {}: {error}", path.display());
                Self::default()
            }
        }
    }

    /// Apply a tmux-syntax config file, if it is there.
    pub fn apply_conf_file(&mut self, path: &std::path::Path) {
        let lines = match tmux_conf::load(path) {
            Ok(lines) => lines,
            Err(tmux_conf::ConfError::Io { source, .. })
                if source.kind() == ErrorKind::NotFound =>
            {
                return;
            }
            Err(error) => {
                self.diagnostics.push(error.to_string());
                return;
            }
        };

        for line in lines {
            if let Err(message) = self.apply_conf_line(&line.words) {
                self.diagnostics.push(format!(
                    "{}:{}: {message}",
                    line.source.display(),
                    line.number
                ));
            }
        }
    }

    /// Apply one config line.
    ///
    /// Only the commands that configure weave are honoured here — a config
    /// file is read before any session exists, so a `split-window` in one has
    /// nothing to act on.
    fn apply_conf_line(&mut self, words: &[String]) -> Result<(), String> {
        let command = Command::parse(words).map_err(|error| error.to_string())?;

        match command {
            Command::SetOption { name, value, unset } => {
                let value = if unset {
                    options::spec(&name).map_or("", |spec| spec.default)
                } else {
                    &value
                };
                let spec = self.options.set(&name, value).map_err(|e| e.to_string())?;
                self.apply_option(spec.name);
                if let options::OptionStatus::Inert(reason) = spec.status {
                    return Err(format!("`{name}` is accepted but does nothing: {reason}"));
                }
                Ok(())
            }
            Command::BindKey {
                table,
                key,
                repeat,
                command,
            } => {
                let key = crate::input::keys::parse_binding_key(&key)
                    .ok_or_else(|| format!("`{key}` is not a key name"))?;
                let bound = Command::parse(&command).map_err(|error| error.to_string())?;
                let binding = if repeat {
                    crate::input::keymap::Binding::repeating(bound)
                } else {
                    crate::input::keymap::Binding::new(bound)
                };
                self.keymap.bind(&table, key, binding);
                Ok(())
            }
            Command::UnbindKey { table, key, all } => {
                if all {
                    self.keymap.unbind_all(&table);
                    return Ok(());
                }
                let key = key.ok_or("nothing to unbind")?;
                let parsed = crate::input::keys::parse_binding_key(&key)
                    .ok_or_else(|| format!("`{key}` is not a key name"))?;
                self.keymap.unbind(&table, parsed);
                Ok(())
            }
            other => Err(format!(
                "`{}` cannot be used in a config file; it needs a running session",
                config_command_name(&other)
            )),
        }
    }

    /// Push a config-time option into the settings that read it.
    fn apply_option(&mut self, name: &str) {
        match name {
            "prefix" | "prefix2" => {
                let keys = ["prefix", "prefix2"]
                    .into_iter()
                    .filter_map(|option| self.options.get(option))
                    .filter(|value| !value.is_empty())
                    .filter_map(crate::input::keys::parse_binding_key)
                    .collect::<Vec<_>>();
                self.keymap.set_prefix(&keys);
            }
            "status" => self.ui.status_bar = self.options.flag("status"),
            "pane-border-status" => self.ui.pane_titles = self.options.flag("pane-border-status"),
            "target-fps" => {
                if let Some(fps) = self.options.number("target-fps") {
                    self.ui.target_fps =
                        normalize_target_fps(u16::try_from(fps).unwrap_or(DEFAULT_TARGET_FPS));
                }
            }
            _ => {}
        }
    }

    pub fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        let raw = toml::from_str::<RawConfig>(source)?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        // A binding value is a whole command line, so a config can say
        // `"Alt+s" = "split-window -h -t %1"`, not just a bare command name.
        for (key, line) in raw.keymap.bindings {
            let key = parse_key(&key)?;
            let command =
                Command::parse_str(&line).map_err(|source| ConfigError::InvalidCommand {
                    line: line.clone(),
                    source,
                })?;
            config.keymap.set_binding(key, command);
        }

        let preset = ThemePreset::from_name(raw.theme.preset.as_deref());
        let preset_theme = preset.theme();
        config.theme = preset_theme;

        if let Some(border_color) = raw.ui.border_color.as_deref() {
            let color = parse_color_or_default(border_color);
            config.ui.border_color = color;
            config.theme.border_focused = color;
        }
        if let Some(status_bar) = raw.ui.status_bar {
            config.ui.status_bar = status_bar;
        }
        if let Some(pane_titles) = raw.ui.pane_titles {
            config.ui.pane_titles = pane_titles;
        }
        if let Some(target_fps) = raw.ui.target_fps {
            config.ui.target_fps = normalize_target_fps(target_fps);
        }
        config.theme.apply_overrides(raw.theme, preset_theme);

        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keymap: Keymap::default(),
            options: Options::default(),
            diagnostics: Vec::new(),
            ui: UiConfig {
                border_color: Color::Cyan,
                status_bar: true,
                pane_titles: false,
                target_fps: DEFAULT_TARGET_FPS,
            },
            theme: ThemePreset::TokyoNight.theme(),
        }
    }
}

/// A command's name, for the "not usable in a config file" message.
fn config_command_name(command: &Command) -> &'static str {
    match command {
        Command::SplitWindow { .. } => "split-window",
        Command::SelectPane { .. } => "select-pane",
        Command::SelectWindow { .. } => "select-window",
        Command::KillPane { .. } => "kill-pane",
        Command::DetachClient { .. } => "detach-client",
        Command::RefreshClient => "refresh-client",
        Command::RenameSession { .. } => "rename-session",
        Command::CommandPrompt { .. } => "command-prompt",
        Command::KillSession { .. } => "kill-session",
        Command::DisplayMessage { .. } => "display-message",
        Command::SendKeys { .. } => "send-keys",
        Command::RespawnPane { .. } => "respawn-pane",
        Command::NewWindow { .. } => "new-window",
        Command::KillWindow { .. } => "kill-window",
        Command::RenameWindow { .. } => "rename-window",
        Command::ResizePane { .. } => "resize-pane",
        Command::SwapPane { .. } => "swap-pane",
        Command::RotateWindow { .. } => "rotate-window",
        Command::SelectLayout { .. } => "select-layout",
        Command::CapturePane { .. } => "capture-pane",
        Command::List { .. } => "list",
        Command::ListKeys { .. } => "list-keys",
        Command::ShowOptions { .. } => "show-options",
        Command::BindKey { .. } => "bind-key",
        Command::UnbindKey { .. } => "unbind-key",
        Command::SetOption { .. } => "set-option",
        Command::BreakPane { .. } => "break-pane",
        Command::JoinPane { .. } => "join-pane",
        Command::RunShell { .. } => "run-shell",
        Command::IfShell { .. } => "if-shell",
        Command::WaitFor { .. } => "wait-for",
    }
}

pub fn config_path() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join("weave/config.toml");
    }

    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".config/weave/config.toml"),
        |home| PathBuf::from(home).join(".config/weave/config.toml"),
    )
}

pub fn parse_key(value: &str) -> Result<KeyEvent, ConfigError> {
    let trimmed = value.trim();
    let parts = trimmed.split('+').collect::<Vec<_>>();

    match parts.as_slice() {
        [key] => parse_key_with_modifiers(key, KeyModifiers::NONE),
        [modifier, key] => {
            let modifier = match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                _ => return Err(ConfigError::InvalidKey(value.to_owned())),
            };
            parse_key_with_modifiers(key, modifier)
        }
        _ => Err(ConfigError::InvalidKey(value.to_owned())),
    }
}

fn parse_key_with_modifiers(key: &str, modifiers: KeyModifiers) -> Result<KeyEvent, ConfigError> {
    let key = key.trim();
    let code = match key.to_ascii_lowercase().as_str() {
        "space" => KeyCode::Char(' '),
        "esc" | "escape" => KeyCode::Esc,
        function if function.starts_with('f') => {
            let number = function[1..]
                .parse::<u8>()
                .map_err(|_| ConfigError::InvalidKey(key.to_owned()))?;
            KeyCode::F(number)
        }
        character => {
            let mut chars = character.chars();
            let Some(ch) = chars.next() else {
                return Err(ConfigError::InvalidKey(key.to_owned()));
            };
            if chars.next().is_some() {
                return Err(ConfigError::InvalidKey(key.to_owned()));
            }
            KeyCode::Char(ch)
        }
    };

    Ok(KeyEvent::new(code, modifiers))
}

fn parse_color_or_default(value: &str) -> Color {
    if let Some(color) = parse_hex_color(value) {
        return color;
    }

    match value.trim().to_ascii_lowercase().as_str() {
        "cyan" => Color::Cyan,
        "darkgrey" | "darkgray" => Color::DarkGrey,
        "red" => Color::Red,
        "white" => Color::White,
        "black" => Color::Black,
        "blue" => Color::Blue,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "magenta" => Color::Magenta,
        unknown => {
            tracing::warn!("unknown border color `{unknown}`, falling back to cyan");
            Color::Cyan
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThemePreset {
    Nord,
    TokyoNight,
}

impl ThemePreset {
    fn from_name(name: Option<&str>) -> Self {
        match name.map(str::trim) {
            None | Some("" | "tokyonight") => Self::TokyoNight,
            Some("nord") => Self::Nord,
            Some(unknown) => {
                tracing::warn!("unknown theme preset `{unknown}`, falling back to tokyonight");
                Self::TokyoNight
            }
        }
    }

    const fn theme(self) -> ThemeConfig {
        match self {
            Self::Nord => ThemeConfig {
                border_focused: rgb(0x88, 0xc0, 0xd0),
                border_unfocused: rgb(0x3b, 0x42, 0x52),
                status_fg: rgb(0xec, 0xef, 0xf4),
                status_bg: rgb(0x2e, 0x34, 0x40),
                accent: rgb(0xbf, 0x61, 0x6a),
            },
            Self::TokyoNight => ThemeConfig {
                border_focused: rgb(0x7d, 0xcf, 0xff),
                border_unfocused: rgb(0x41, 0x48, 0x68),
                status_fg: rgb(0xc0, 0xca, 0xf5),
                status_bg: rgb(0x1a, 0x1b, 0x26),
                accent: rgb(0xf7, 0x76, 0x8e),
            },
        }
    }
}

impl ThemeConfig {
    fn apply_overrides(&mut self, raw: RawTheme, preset: Self) {
        if let Some(value) = raw.border_focused {
            self.border_focused =
                parse_theme_color_or_fallback("border_focused", &value, preset.border_focused);
        }
        if let Some(value) = raw.border_unfocused {
            self.border_unfocused =
                parse_theme_color_or_fallback("border_unfocused", &value, preset.border_unfocused);
        }
        if let Some(value) = raw.status_fg {
            self.status_fg = parse_theme_color_or_fallback("status_fg", &value, preset.status_fg);
        }
        if let Some(value) = raw.status_bg {
            self.status_bg = parse_theme_color_or_fallback("status_bg", &value, preset.status_bg);
        }
        if let Some(value) = raw.accent {
            self.accent = parse_theme_color_or_fallback("accent", &value, preset.accent);
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

fn parse_theme_color_or_fallback(key: &str, value: &str, fallback: Color) -> Color {
    parse_hex_color(value).unwrap_or_else(|| {
        tracing::warn!("malformed theme color `{key}` = `{value}`, falling back to preset value");
        fallback
    })
}

fn parse_hex_color(value: &str) -> Option<Color> {
    let value = value.trim();
    let hex = value.strip_prefix('#')?;
    let bytes = hex.as_bytes();
    if bytes.len() != 6 {
        return None;
    }

    let r = parse_hex_byte(bytes[0], bytes[1])?;
    let g = parse_hex_byte(bytes[2], bytes[3])?;
    let b = parse_hex_byte(bytes[4], bytes[5])?;
    Some(rgb(r, g, b))
}

fn parse_hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some((parse_hex_nibble(hi)? << 4) | parse_hex_nibble(lo)?)
}

fn parse_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn normalize_target_fps(value: u16) -> u16 {
    let clamped = value.clamp(MIN_TARGET_FPS, MAX_TARGET_FPS);
    if clamped != value {
        tracing::warn!(
            "target_fps {value} outside supported range {MIN_TARGET_FPS}..={MAX_TARGET_FPS}, using {clamped}"
        );
    }
    clamped
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    keymap: RawKeymap,
    ui: RawUi,
    theme: RawTheme,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawKeymap {
    bindings: HashMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawUi {
    border_color: Option<String>,
    status_bar: Option<bool>,
    pane_titles: Option<bool>,
    target_fps: Option<u16>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawTheme {
    preset: Option<String>,
    border_focused: Option<String>,
    border_unfocused: Option<String>,
    status_fg: Option<String>,
    status_bg: Option<String>,
    accent: Option<String>,
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use crossterm::style::Color;

    use super::{parse_key, Config, ConfigError};
    use crate::command::Command;

    fn alt(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT)
    }

    fn command(line: &str) -> Command {
        Command::parse_str(line).expect("command parses")
    }

    /// The end-to-end shape of PR 7: a tmux-syntax file changes the prefix,
    /// rebinds keys and reports what it could not honour.
    #[test]
    fn a_tmux_syntax_config_is_applied_and_reports_what_it_could_not() {
        let dir = std::env::temp_dir().join(format!("weave-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("weave.conf");
        std::fs::write(
            &path,
            "# comment\n\
             set -g prefix C-a\n\
             unbind -n M-v\n\
             bind -n M-s split-window -h\n\
             bind -r H resize-pane -L 5\n\
             set -g history-limit 5000\n\
             set -g not-an-option yes\n",
        )
        .expect("write config");

        let mut config = Config::default();
        config.apply_conf_file(&path);

        // The prefix moved.
        assert!(config.keymap.is_prefix(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL
        )));
        // A new root binding took, and an old one went.
        assert_eq!(
            config.keymap.command_for(&alt('s')),
            Some(command("split-window -h"))
        );
        assert_eq!(config.keymap.command_for(&alt('v')), None);
        // `-r` survived the round trip.
        let binding = config
            .keymap
            .lookup("prefix", &KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE))
            .expect("H is bound");
        assert!(binding.repeat);

        // Two diagnostics: one inert option, one that does not exist.
        assert_eq!(config.diagnostics.len(), 2, "{:?}", config.diagnostics);
        assert!(config.diagnostics[0].contains("history-limit"));
        assert!(config.diagnostics[1].contains("not-an-option"));
        // An inert option is still stored, so `show-options` round-trips.
        assert_eq!(config.options.get("history-limit"), Some("5000"));

        std::fs::remove_dir_all(dir).expect("clean up");
    }

    /// A config that names a session command is refused with an explanation
    /// rather than silently doing nothing at startup.
    #[test]
    fn session_commands_are_refused_in_a_config_file() {
        let mut config = Config::default();

        let error = config
            .apply_conf_line(&["split-window".to_owned(), "-h".to_owned()])
            .expect_err("not usable in a config");

        assert!(error.contains("needs a running session"), "{error}");
    }

    /// The option and the TOML key both reach the same setting, so someone who
    /// wants titles back can turn them on either way.
    #[test]
    fn pane_border_status_turns_titles_back_on() {
        let mut config = Config::default();
        assert!(!config.ui.pane_titles);

        config
            .apply_conf_line(&[
                "set-option".to_owned(),
                "-g".to_owned(),
                "pane-border-status".to_owned(),
                "on".to_owned(),
            ])
            .expect("a live option");

        assert!(config.ui.pane_titles);
    }

    #[test]
    fn a_missing_config_file_is_not_an_error() {
        let mut config = Config::default();
        config.apply_conf_file(std::path::Path::new("/nonexistent/weave.conf"));

        assert!(config.diagnostics.is_empty());
    }

    #[test]
    fn default_config_matches_default_keymap() {
        let config = Config::default();

        assert_eq!(
            config.keymap.command_for(&alt('h')),
            Some(command("focus-left"))
        );
        assert_eq!(config.keymap.command_for(&alt('q')), Some(command("close")));
        assert_eq!(config.ui.border_color, Color::Cyan);
        assert!(config.ui.status_bar);
        assert!(!config.ui.pane_titles);
        assert_eq!(config.ui.target_fps, 160);
        assert_eq!(
            config.theme.border_focused,
            Color::Rgb {
                r: 0x7d,
                g: 0xcf,
                b: 0xff
            }
        );
        assert_eq!(
            config.theme.border_unfocused,
            Color::Rgb {
                r: 0x41,
                g: 0x48,
                b: 0x68
            }
        );
    }

    #[test]
    fn sample_toml_parses_and_overrides_defaults() {
        let config = Config::from_toml_str(
            r#"
            [keymap.bindings]
            "Alt+s" = "split-v"

            [ui]
            border_color = "magenta"
            status_bar = false
            pane_titles = false
            target_fps = 120
            "#,
        )
        .expect("sample config parses");

        assert_eq!(config.keymap.command_for(&alt('s')), Some(command("split-v")));
        assert_eq!(
            config.keymap.command_for(&alt('h')),
            Some(command("focus-left"))
        );
        assert_eq!(config.ui.border_color, Color::Magenta);
        assert_eq!(config.theme.border_focused, Color::Magenta);
        assert!(!config.ui.status_bar);
        assert!(!config.ui.pane_titles);
        assert_eq!(config.ui.target_fps, 120);
    }

    #[test]
    /// Off by default — a title over every pane is noise when the pane's own
    /// content already says what it is — but still available to anyone who
    /// wants it.
    fn pane_titles_default_disabled_and_can_be_enabled() {
        let default = Config::from_toml_str("").expect("empty config parses");
        let enabled = Config::from_toml_str(
            r"
            [ui]
            pane_titles = true
            ",
        )
        .expect("pane titles config parses");

        assert!(!default.ui.pane_titles);
        assert!(enabled.ui.pane_titles);
    }

    #[test]
    fn theme_defaults_to_tokyonight_preset() {
        let config = Config::from_toml_str("").expect("empty config parses");

        assert_eq!(
            config.theme.status_bg,
            Color::Rgb {
                r: 0x1a,
                g: 0x1b,
                b: 0x26
            }
        );
        assert_eq!(
            config.theme.accent,
            Color::Rgb {
                r: 0xf7,
                g: 0x76,
                b: 0x8e
            }
        );
    }

    #[test]
    fn theme_preset_overrides_default_preset() {
        let config = Config::from_toml_str(
            r#"
            [theme]
            preset = "nord"
            "#,
        )
        .expect("theme config parses");

        assert_eq!(
            config.theme.border_focused,
            Color::Rgb {
                r: 0x88,
                g: 0xc0,
                b: 0xd0
            }
        );
        assert_eq!(
            config.theme.status_bg,
            Color::Rgb {
                r: 0x2e,
                g: 0x34,
                b: 0x40
            }
        );
    }

    #[test]
    fn theme_per_key_override_wins_over_preset_and_ui_border_color() {
        let config = Config::from_toml_str(
            r##"
            [ui]
            border_color = "magenta"

            [theme]
            preset = "nord"
            border_focused = "#010203"
            status_fg = "#aabbcc"
            "##,
        )
        .expect("theme config parses");

        assert_eq!(config.theme.border_focused, Color::Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            config.theme.border_unfocused,
            Color::Rgb {
                r: 0x3b,
                g: 0x42,
                b: 0x52
            }
        );
        assert_eq!(
            config.theme.status_fg,
            Color::Rgb {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc
            }
        );
    }

    #[test]
    fn malformed_theme_hex_falls_back_to_preset_value() {
        let config = Config::from_toml_str(
            r##"
            [theme]
            preset = "nord"
            border_focused = "not-hex"
            status_bg = "#12345"
            "##,
        )
        .expect("theme config parses");

        assert_eq!(
            config.theme.border_focused,
            Color::Rgb {
                r: 0x88,
                g: 0xc0,
                b: 0xd0
            }
        );
        assert_eq!(
            config.theme.status_bg,
            Color::Rgb {
                r: 0x2e,
                g: 0x34,
                b: 0x40
            }
        );
    }

    #[test]
    fn unknown_theme_preset_falls_back_to_tokyonight() {
        let config = Config::from_toml_str(
            r#"
            [theme]
            preset = "unknown"
            "#,
        )
        .expect("theme config parses");

        assert_eq!(
            config.theme.border_focused,
            Color::Rgb {
                r: 0x7d,
                g: 0xcf,
                b: 0xff
            }
        );
        assert_eq!(
            config.theme.status_bg,
            Color::Rgb {
                r: 0x1a,
                g: 0x1b,
                b: 0x26
            }
        );
    }

    #[test]
    fn malformed_toml_parse_returns_error() {
        assert!(matches!(
            Config::from_toml_str("not valid toml ="),
            Err(ConfigError::Toml(_))
        ));
    }

    #[test]
    fn parse_key_handles_supported_forms() {
        assert_eq!(
            parse_key("Ctrl+Space").expect("ctrl space parses"),
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
        );
        assert_eq!(
            parse_key("Esc").expect("esc parses"),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("s").expect("letter parses"),
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("F1").expect("function key parses"),
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)
        );
        assert!(parse_key("garbage").is_err());
    }

    #[test]
    fn target_fps_clamps_to_supported_range() {
        let low = Config::from_toml_str(
            r"
            [ui]
            target_fps = 1
            ",
        )
        .expect("low fps config parses");
        let high = Config::from_toml_str(
            r"
            [ui]
            target_fps = 999
            ",
        )
        .expect("high fps config parses");

        assert_eq!(low.ui.target_fps, 30);
        assert_eq!(high.ui.target_fps, 240);
    }
}
