//! Keyboard shortcut parsing.
//!
//! `cmd+s` means Command on macOS and Super on Linux. Silently rewriting it to
//! Ctrl on Linux would be a lie that works often enough to be trusted and then
//! fails somewhere important, so `cmd` always means the Meta/Super key and
//! `accel` is provided for "whatever this platform's menu modifier is".

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::models::backend::Platform;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ChordParseError {
    #[error("shortcut is empty")]
    Empty,
    #[error("shortcut has modifiers but no key")]
    MissingKey,
    #[error("shortcut names more than one non-modifier key")]
    MultipleKeys,
    #[error("unknown key name")]
    UnknownKey,
    #[error("function key number is out of range (F1-F24)")]
    FunctionKeyOutOfRange,
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Command on macOS, Super/Windows on Linux.
    pub meta: bool,
    /// Platform-dependent menu accelerator, unresolved. Resolved by
    /// [`Modifiers::resolve`] at the platform boundary.
    pub accel: bool,
}

impl Modifiers {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            accel: false,
        }
    }

    /// Collapses `accel` into the concrete modifier this platform uses for
    /// menu accelerators.
    #[must_use]
    pub const fn resolve(self, platform: Platform) -> Self {
        if !self.accel {
            return Self {
                accel: false,
                ..self
            };
        }
        match platform {
            Platform::Macos => Self {
                meta: true,
                accel: false,
                ..self
            },
            Platform::Linux => Self {
                ctrl: true,
                accel: false,
                ..self
            },
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.meta && !self.accel
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    Return,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
}

impl NamedKey {
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        let lower = token.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix('f')
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            let number: u8 = rest.parse().ok()?;
            return (1..=24).contains(&number).then_some(Self::Function(number));
        }
        match lower.as_str() {
            "return" | "enter" | "cr" => Some(Self::Return),
            "tab" => Some(Self::Tab),
            "escape" | "esc" => Some(Self::Escape),
            "space" | "spc" => Some(Self::Space),
            "backspace" | "bs" => Some(Self::Backspace),
            "delete" | "del" => Some(Self::Delete),
            "insert" | "ins" => Some(Self::Insert),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "home" => Some(Self::Home),
            "end" => Some(Self::End),
            "pageup" | "pgup" | "page_up" => Some(Self::PageUp),
            "pagedown" | "pgdn" | "page_down" => Some(Self::PageDown),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_string(self) -> String {
        match self {
            Self::Return => "return".to_owned(),
            Self::Tab => "tab".to_owned(),
            Self::Escape => "escape".to_owned(),
            Self::Space => "space".to_owned(),
            Self::Backspace => "backspace".to_owned(),
            Self::Delete => "delete".to_owned(),
            Self::Insert => "insert".to_owned(),
            Self::Up => "up".to_owned(),
            Self::Down => "down".to_owned(),
            Self::Left => "left".to_owned(),
            Self::Right => "right".to_owned(),
            Self::Home => "home".to_owned(),
            Self::End => "end".to_owned(),
            Self::PageUp => "pageup".to_owned(),
            Self::PageDown => "pagedown".to_owned(),
            Self::Function(n) => format!("f{n}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

/// A parsed keyboard shortcut.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Chord {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl Chord {
    /// Parses `ctrl+shift+p`, `cmd+s`, `accel+s`, `alt+F4`, `Return`.
    ///
    /// Separators are `+` or `-`, so `ctrl-c` works too. A trailing literal
    /// `+` is understood as the plus character (`ctrl++`).
    pub fn parse(input: &str) -> Result<Self, ChordParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ChordParseError::Empty);
        }

        let tokens = split_tokens(trimmed);
        let mut modifiers = Modifiers::none();
        let mut key: Option<Key> = None;

        for token in tokens {
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "ctl" => modifiers.ctrl = true,
                "alt" | "option" | "opt" => modifiers.alt = true,
                "shift" => modifiers.shift = true,
                "cmd" | "command" | "meta" | "super" | "win" => modifiers.meta = true,
                "accel" => modifiers.accel = true,
                _ => {
                    if key.is_some() {
                        return Err(ChordParseError::MultipleKeys);
                    }
                    key = Some(parse_key(&token)?);
                }
            }
        }

        match key {
            Some(key) => Ok(Self { modifiers, key }),
            None if modifiers.is_empty() => Err(ChordParseError::UnknownKey),
            None => Err(ChordParseError::MissingKey),
        }
    }

    /// Resolves `accel` against the running platform.
    #[must_use]
    pub const fn resolve(self, platform: Platform) -> Self {
        Self {
            modifiers: self.modifiers.resolve(platform),
            key: self.key,
        }
    }
}

/// Splits on `+`/`-`.
///
/// A separator that arrives when no token is being accumulated is the key
/// itself rather than a delimiter, which is what makes `ctrl++` mean the plus
/// key while `ctrl+` stays malformed.
fn split_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in input.chars() {
        if matches!(ch, '+' | '-') {
            if current.is_empty() {
                tokens.push(ch.to_string());
            } else {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Parses one key token.
///
/// A bare letter is lowercased, so `cmd+S` and `cmd+s` are the same chord and
/// case is carried by an explicit `shift`.
fn parse_key(token: &str) -> Result<Key, ChordParseError> {
    if let Some(named) = NamedKey::parse(token) {
        return Ok(Key::Named(named));
    }
    let lower = token.to_ascii_lowercase();
    if lower.starts_with('f') && lower.len() > 1 && lower[1..].chars().all(|c| c.is_ascii_digit()) {
        return Err(ChordParseError::FunctionKeyOutOfRange);
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(single), None) => Ok(Key::Char(single.to_ascii_lowercase())),
        _ => Err(ChordParseError::UnknownKey),
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.modifiers.ctrl {
            parts.push("ctrl".to_owned());
        }
        if self.modifiers.alt {
            parts.push("alt".to_owned());
        }
        if self.modifiers.shift {
            parts.push("shift".to_owned());
        }
        if self.modifiers.meta {
            parts.push("cmd".to_owned());
        }
        if self.modifiers.accel {
            parts.push("accel".to_owned());
        }
        parts.push(match self.key {
            Key::Char(c) => c.to_string(),
            Key::Named(named) => named.as_string(),
        });
        formatter.write_str(&parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(input: &str) -> Chord {
        Chord::parse(input).expect("parses")
    }

    #[test]
    fn a_simple_command_shortcut_parses() {
        let c = chord("cmd+s");
        assert!(c.modifiers.meta);
        assert!(!c.modifiers.ctrl);
        assert_eq!(c.key, Key::Char('s'));
    }

    #[test]
    fn multiple_modifiers_combine() {
        let c = chord("ctrl+shift+p");
        assert!(c.modifiers.ctrl);
        assert!(c.modifiers.shift);
        assert!(!c.modifiers.alt);
        assert_eq!(c.key, Key::Char('p'));
    }

    #[test]
    fn function_keys_parse_and_are_range_checked() {
        assert_eq!(chord("alt+F4").key, Key::Named(NamedKey::Function(4)));
        assert_eq!(chord("F12").key, Key::Named(NamedKey::Function(12)));
        assert_eq!(
            Chord::parse("F25"),
            Err(ChordParseError::FunctionKeyOutOfRange)
        );
        assert_eq!(
            Chord::parse("F0"),
            Err(ChordParseError::FunctionKeyOutOfRange)
        );
    }

    #[test]
    fn a_bare_named_key_needs_no_modifiers() {
        assert_eq!(chord("Return").key, Key::Named(NamedKey::Return));
        assert_eq!(chord("escape").key, Key::Named(NamedKey::Escape));
        assert!(chord("Return").modifiers.is_empty());
    }

    #[test]
    fn key_names_are_case_insensitive_and_accept_common_aliases() {
        assert_eq!(chord("ENTER").key, Key::Named(NamedKey::Return));
        assert_eq!(chord("esc").key, Key::Named(NamedKey::Escape));
        assert_eq!(chord("pgdn").key, Key::Named(NamedKey::PageDown));
        assert_eq!(chord("Ctrl+C").key, Key::Char('c'));
    }

    #[test]
    fn hyphen_works_as_a_separator_for_emacs_style_input() {
        assert_eq!(chord("ctrl-c"), chord("ctrl+c"));
    }

    #[test]
    fn a_trailing_separator_is_treated_as_the_key_itself() {
        assert_eq!(chord("ctrl++").key, Key::Char('+'));
        assert_eq!(chord("ctrl+-").key, Key::Char('-'));
    }

    #[test]
    fn cmd_means_super_on_linux_rather_than_being_rewritten_to_ctrl() {
        // Silently mapping cmd to ctrl would produce a shortcut the user never
        // asked for, and would work often enough to be trusted.
        let resolved = chord("cmd+s").resolve(Platform::Linux);
        assert!(resolved.modifiers.meta);
        assert!(!resolved.modifiers.ctrl);
    }

    #[test]
    fn accel_resolves_to_the_platform_menu_modifier() {
        let mac = chord("accel+s").resolve(Platform::Macos);
        assert!(mac.modifiers.meta);
        assert!(!mac.modifiers.ctrl);
        assert!(!mac.modifiers.accel);

        let linux = chord("accel+s").resolve(Platform::Linux);
        assert!(linux.modifiers.ctrl);
        assert!(!linux.modifiers.meta);
        assert!(!linux.modifiers.accel);
    }

    #[test]
    fn resolving_a_chord_without_accel_changes_nothing() {
        let before = chord("ctrl+shift+p");
        assert_eq!(before.resolve(Platform::Macos), before);
        assert_eq!(before.resolve(Platform::Linux), before);
    }

    #[test]
    fn malformed_shortcuts_are_rejected_with_a_specific_reason() {
        assert_eq!(Chord::parse(""), Err(ChordParseError::Empty));
        assert_eq!(Chord::parse("   "), Err(ChordParseError::Empty));
        assert_eq!(Chord::parse("ctrl+"), Err(ChordParseError::MissingKey));
        assert_eq!(Chord::parse("ctrl+a+b"), Err(ChordParseError::MultipleKeys));
        assert_eq!(Chord::parse("nonsense"), Err(ChordParseError::UnknownKey));
    }

    #[test]
    fn display_round_trips_through_parse() {
        for input in [
            "ctrl+c",
            "cmd+s",
            "ctrl+shift+p",
            "alt+f4",
            "return",
            "accel+s",
        ] {
            let parsed = chord(input);
            let rendered = parsed.to_string();
            assert_eq!(chord(&rendered), parsed, "round trip failed for {input}");
        }
    }
}
