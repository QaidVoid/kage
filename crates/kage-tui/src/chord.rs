//! Parse canonical keybinding chords and match them against terminal
//! key events.
//!
//! The plugin runtime normalises chords to a canonical lowercase form
//! (`ctrl+shift+x`, `f5`, `alt+enter`) before the host ever sees them
//! (see `kage_plugin::keybindings::normalize_chord`). This module is
//! the host-side counterpart: it parses that canonical form into a
//! [`Chord`] and tests whether a crossterm [`KeyEvent`] triggers it.
//!
//! Character matching is intentionally lenient about shift: terminals
//! disagree on whether `Ctrl+Shift+P` arrives as `Char('P')` or
//! `Char('p')` + `SHIFT`. A chord's `shift` requirement is satisfied
//! by either an explicit `SHIFT` modifier or an uppercase character,
//! and the event character is compared case-insensitively.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Modifiers a chord can require; other crossterm modifier bits
/// (keypad, caps-lock, etc.) are masked out before comparison.
const RELEVANT: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT)
    .union(KeyModifiers::SUPER);

/// A parsed keybinding chord: a key plus the modifier set that must
/// accompany it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chord {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Chord {
    /// Parse a canonical chord string. Returns `None` only if the
    /// grammar drifted from `kage_plugin`'s normaliser (guarded by a
    /// shared-token test), so the caller can safely skip it.
    #[must_use]
    pub fn parse(canonical: &str) -> Option<Self> {
        let parts: Vec<&str> = canonical.split('+').collect();
        let (key, mods) = parts.split_last()?;
        let mut chord = Self {
            code: key_code(key)?,
            mods: KeyModifiers::NONE,
        };
        for m in mods {
            chord.mods |= match *m {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" => KeyModifiers::SUPER,
                _ => return None,
            };
        }
        Some(chord)
    }

    /// Whether `key` (a key-press event) triggers this chord.
    #[must_use]
    pub fn matches(&self, key: &KeyEvent) -> bool {
        let mut mods = key.modifiers.intersection(RELEVANT);
        let code_matches = match (self.code, key.code) {
            (KeyCode::Char(want), KeyCode::Char(got)) => {
                if got.is_uppercase() {
                    mods |= KeyModifiers::SHIFT;
                }
                want.eq_ignore_ascii_case(&got)
            }
            (a, b) => a == b,
        };
        code_matches && mods == self.mods
    }
}

/// Map a canonical key token to a crossterm [`KeyCode`].
fn key_code(token: &str) -> Option<KeyCode> {
    if token.chars().count() == 1 {
        return Some(KeyCode::Char(token.chars().next()?));
    }
    if let Some(n) = token.strip_prefix('f') {
        let n: u8 = n.parse().ok()?;
        return (1..=12).contains(&n).then_some(KeyCode::F(n));
    }
    Some(match token {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_char_matches() {
        let c = Chord::parse("ctrl+s").unwrap();
        assert!(c.matches(&ev(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        assert!(!c.matches(&ev(KeyCode::Char('s'), KeyModifiers::NONE)));
        assert!(!c.matches(&ev(KeyCode::Char('x'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn shift_satisfied_by_uppercase_or_modifier() {
        let c = Chord::parse("ctrl+shift+p").unwrap();
        assert!(c.matches(&ev(KeyCode::Char('P'), KeyModifiers::CONTROL)));
        assert!(c.matches(&ev(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!c.matches(&ev(KeyCode::Char('p'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn named_and_function_keys() {
        assert!(
            Chord::parse("f5")
                .unwrap()
                .matches(&ev(KeyCode::F(5), KeyModifiers::NONE))
        );
        assert!(
            Chord::parse("alt+enter")
                .unwrap()
                .matches(&ev(KeyCode::Enter, KeyModifiers::ALT))
        );
        assert!(
            Chord::parse("space")
                .unwrap()
                .matches(&ev(KeyCode::Char(' '), KeyModifiers::NONE))
        );
    }

    #[test]
    fn extra_modifier_does_not_match() {
        let c = Chord::parse("ctrl+s").unwrap();
        assert!(!c.matches(&ev(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
    }

    #[test]
    fn every_plugin_named_token_parses() {
        for tok in kage_plugin::keybindings::NAMED_KEYS {
            assert!(
                Chord::parse(tok).is_some(),
                "host cannot parse named key `{tok}` the plugin layer accepts"
            );
        }
        for n in 1..=12 {
            assert!(Chord::parse(&format!("f{n}")).is_some());
        }
        assert!(Chord::parse("ctrl+alt+shift+super+a").is_some());
    }
}
