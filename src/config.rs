//! TOML config schema + loader.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;
use serde::Deserialize;

use crate::command::Command;
use crate::input::keymap::Keymap;

pub const DEFAULT_TARGET_FPS: u16 = 160;
pub const MIN_TARGET_FPS: u16 = 30;
pub const MAX_TARGET_FPS: u16 = 240;

#[derive(Clone)]
pub struct Config {
    pub keymap: Keymap,
    pub ui: UiConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiConfig {
    pub border_color: Color,
    pub status_bar: bool,
    pub target_fps: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid key `{0}`")]
    InvalidKey(String),
    #[error("invalid command `{0}`")]
    InvalidCommand(String),
    #[error("toml parse failed: {0}")]
    Toml(#[from] toml::de::Error),
}

impl Config {
    pub fn load() -> Self {
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

    pub fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        let raw = toml::from_str::<RawConfig>(source)?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        if let Some(prefix) = raw.keymap.prefix {
            config.keymap.set_prefix(parse_key(&prefix)?);
        }

        for (key, command) in raw.keymap.bindings {
            let key = parse_key(&key)?;
            let command = Command::from_str(&command)
                .ok_or_else(|| ConfigError::InvalidCommand(command.clone()))?;
            config.keymap.set_binding(key, command);
        }

        if let Some(border_color) = raw.ui.border_color {
            config.ui.border_color = parse_color_or_default(&border_color);
        }
        if let Some(status_bar) = raw.ui.status_bar {
            config.ui.status_bar = status_bar;
        }
        if let Some(target_fps) = raw.ui.target_fps {
            config.ui.target_fps = normalize_target_fps(target_fps);
        }

        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keymap: Keymap::default(),
            ui: UiConfig {
                border_color: Color::Cyan,
                status_bar: true,
                target_fps: DEFAULT_TARGET_FPS,
            },
        }
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
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawKeymap {
    prefix: Option<String>,
    bindings: HashMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawUi {
    border_color: Option<String>,
    status_bar: Option<bool>,
    target_fps: Option<u16>,
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use crossterm::style::Color;

    use super::{parse_key, Config, ConfigError};
    use crate::command::Command;

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    #[test]
    fn default_config_matches_default_keymap() {
        let config = Config::default();

        assert!(config
            .keymap
            .is_prefix(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)));
        assert_eq!(config.keymap.command_for(&key('s')), Some(Command::SplitH));
        assert_eq!(
            config.keymap.command_for(&key('h')),
            Some(Command::FocusLeft)
        );
        assert_eq!(config.ui.border_color, Color::Cyan);
        assert!(config.ui.status_bar);
        assert_eq!(config.ui.target_fps, 160);
    }

    #[test]
    fn sample_toml_parses_and_overrides_defaults() {
        let config = Config::from_toml_str(
            r#"
            [keymap]
            prefix = "Alt+x"

            [keymap.bindings]
            s = "split-v"

            [ui]
            border_color = "magenta"
            status_bar = false
            target_fps = 120
            "#,
        )
        .expect("sample config parses");

        assert!(config
            .keymap
            .is_prefix(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)));
        assert_eq!(config.keymap.command_for(&key('s')), Some(Command::SplitV));
        assert_eq!(
            config.keymap.command_for(&key('h')),
            Some(Command::FocusLeft)
        );
        assert_eq!(config.ui.border_color, Color::Magenta);
        assert!(!config.ui.status_bar);
        assert_eq!(config.ui.target_fps, 120);
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
            r#"
            [ui]
            target_fps = 1
            "#,
        )
        .expect("low fps config parses");
        let high = Config::from_toml_str(
            r#"
            [ui]
            target_fps = 999
            "#,
        )
        .expect("high fps config parses");

        assert_eq!(low.ui.target_fps, 30);
        assert_eq!(high.ui.target_fps, 240);
    }
}
