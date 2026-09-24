//! Key notation, mappings and the keymap table shared by the TUI and
//! the plugin runtime.
//!
//! Notation follows Vim (`<C-l>`, `<M-p>`, `<S-Tab>`, `<CR>`, `gg`,
//! `<leader>m`) and also accepts the chord form (`ctrl+shift+x`, `f5`)
//! for a single key. Characters are case-sensitive: shift is folded
//! into the character (`<S-a>` is `A`), while named keys keep it
//! (`<S-Tab>`).
//!
//! The table is plain data. Lookup is longest match over a list of
//! modes, so the caller can drive a pending-sequence state machine.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::{BitOr, Bound};

/// A keymap mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode {
    /// `n`: vim normal mode, in either pane.
    Normal,
    /// `b`: vim normal mode with the conversation pane focused.
    Buffer,
    /// `i`: vim insert mode and the modeless editor.
    Insert,
    /// `v`: visual mode.
    Visual,
    /// `g`: any editing state outside an overlay.
    Global,
}

impl Mode {
    /// Every mode, in table order.
    pub const ALL: [Self; 5] = [
        Self::Normal,
        Self::Buffer,
        Self::Insert,
        Self::Visual,
        Self::Global,
    ];

    /// Parse a mode letter (`n`, `b`, `i`, `v`, `g`).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// The mode letter.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "n",
            Self::Buffer => "b",
            Self::Insert => "i",
            Self::Visual => "v",
            Self::Global => "g",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Modifier set of a [`Key`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mods(u8);

impl Mods {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// Control.
    pub const CTRL: Self = Self(1);
    /// Alt (`M-` in Vim notation).
    pub const ALT: Self = Self(1 << 1);
    /// Shift. Only kept on named keys.
    pub const SHIFT: Self = Self(1 << 2);
    /// Super (`D-` in Vim notation).
    pub const SUPER: Self = Self(1 << 3);

    /// Whether every modifier in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modifier is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl BitOr for Mods {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// The key part of a [`Key`], without modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyCode {
    /// A character. Space is `Char(' ')`.
    Char(char),
    /// Enter (`<CR>`).
    Enter,
    /// Escape.
    Esc,
    /// Tab. Shift+Tab is `Tab` with [`Mods::SHIFT`].
    Tab,
    /// Backspace (`<BS>`).
    Backspace,
    /// Delete (`<Del>`).
    Delete,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// Function key `F1` to `F12`.
    F(u8),
}

/// Named keys as `(Vim name, code)`. The first name for a code is the
/// display form.
const NAMED: &[(&str, KeyCode)] = &[
    ("CR", KeyCode::Enter),
    ("Enter", KeyCode::Enter),
    ("Return", KeyCode::Enter),
    ("Esc", KeyCode::Esc),
    ("Tab", KeyCode::Tab),
    ("BS", KeyCode::Backspace),
    ("Backspace", KeyCode::Backspace),
    ("Del", KeyCode::Delete),
    ("Delete", KeyCode::Delete),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PageUp", KeyCode::PageUp),
    ("PageDown", KeyCode::PageDown),
    ("Insert", KeyCode::Insert),
    ("Space", KeyCode::Char(' ')),
    ("lt", KeyCode::Char('<')),
    ("gt", KeyCode::Char('>')),
    ("Bslash", KeyCode::Char('\\')),
    ("Bar", KeyCode::Char('|')),
];

/// Key names accepted as the last token of the chord form.
const CHORD_NAMED: &[(&str, KeyCode)] = &[
    ("enter", KeyCode::Enter),
    ("esc", KeyCode::Esc),
    ("tab", KeyCode::Tab),
    ("space", KeyCode::Char(' ')),
    ("backspace", KeyCode::Backspace),
    ("delete", KeyCode::Delete),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
    ("insert", KeyCode::Insert),
];

/// Modifier words accepted in the chord form.
const CHORD_MODS: &[(&str, Mods)] = &[
    ("ctrl", Mods::CTRL),
    ("control", Mods::CTRL),
    ("alt", Mods::ALT),
    ("option", Mods::ALT),
    ("opt", Mods::ALT),
    ("shift", Mods::SHIFT),
    ("super", Mods::SUPER),
    ("cmd", Mods::SUPER),
    ("command", Mods::SUPER),
    ("meta", Mods::SUPER),
    ("win", Mods::SUPER),
];

/// One key press: a code plus modifiers, normalized so equal presses
/// compare equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    code: KeyCode,
    mods: Mods,
}

impl Key {
    /// Build a key. Shift on a character is folded into the character
    /// (`a` with shift becomes `A`) and dropped from the modifiers.
    #[must_use]
    pub fn new(code: KeyCode, mods: Mods) -> Self {
        match code {
            KeyCode::Char(c) if mods.contains(Mods::SHIFT) => Self {
                code: KeyCode::Char(c.to_ascii_uppercase()),
                mods: mods.without(Mods::SHIFT),
            },
            _ => Self { code, mods },
        }
    }

    /// The key code.
    #[must_use]
    pub fn code(self) -> KeyCode {
        self.code
    }

    /// The modifiers.
    #[must_use]
    pub fn mods(self) -> Mods {
        self.mods
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut mods = self.mods;
        let name = match self.code {
            KeyCode::Char(c) if mods.is_empty() && !matches!(c, ' ' | '<') => {
                return write!(f, "{c}");
            }
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                mods = mods | Mods::SHIFT;
                c.to_ascii_lowercase().to_string()
            }
            KeyCode::Char(c) if !matches!(c, ' ' | '<' | '>') => c.to_string(),
            KeyCode::F(n) => format!("F{n}"),
            code => NAMED
                .iter()
                .find(|(_, named)| *named == code)
                .map_or_else(String::new, |(name, _)| (*name).to_owned()),
        };
        f.write_str("<")?;
        for (flag, prefix) in [
            (Mods::CTRL, "C-"),
            (Mods::ALT, "M-"),
            (Mods::SHIFT, "S-"),
            (Mods::SUPER, "D-"),
        ] {
            if mods.contains(flag) {
                f.write_str(prefix)?;
            }
        }
        write!(f, "{name}>")
    }
}

/// Format a key sequence in Vim notation.
#[must_use]
pub fn display_keys(keys: &[Key]) -> String {
    keys.iter().map(ToString::to_string).collect()
}

/// Parse a key sequence. `<leader>` expands to `leader`, which must be
/// one key in the same notation. A string in chord form
/// (`ctrl+shift+x`, `f5`, `enter`) is one key.
pub fn parse_keys(notation: &str, leader: &str) -> Result<Vec<Key>, KeymapError> {
    parse(notation, Some(leader))
}

/// Parse exactly one key, as the `leader` option holds. `<leader>` is
/// not allowed here.
pub fn parse_key(notation: &str) -> Result<Key, KeymapError> {
    match parse(notation, None)?.as_slice() {
        [key] => Ok(*key),
        _ => Err(notation_error(notation, "expected a single key")),
    }
}

fn parse(notation: &str, leader: Option<&str>) -> Result<Vec<Key>, KeymapError> {
    if notation.is_empty() {
        return Err(notation_error(notation, "empty key sequence"));
    }
    if let Some(key) = parse_chord(notation)? {
        return Ok(vec![key]);
    }
    let mut keys = Vec::new();
    let mut rest = notation;
    while let Some(c) = rest.chars().next() {
        let bracket = if c == '<' {
            rest.find('>').filter(|&end| end > 1)
        } else {
            None
        };
        if let Some(end) = bracket {
            let inner = &rest[1..end];
            if inner.eq_ignore_ascii_case("leader") {
                let Some(leader) = leader else {
                    return Err(notation_error(notation, "<leader> is not allowed here"));
                };
                keys.push(parse_key(leader)?);
            } else {
                keys.push(
                    parse_bracketed(inner).ok_or_else(|| {
                        notation_error(notation, &format!("unknown key <{inner}>"))
                    })?,
                );
            }
            rest = &rest[end + 1..];
            continue;
        }
        keys.push(Key::new(KeyCode::Char(c), Mods::NONE));
        rest = &rest[c.len_utf8()..];
    }
    Ok(keys)
}

/// Parse the inside of `<...>`: modifier prefixes, then a key name or
/// a single character. As in Vim, a letter after `C-` alone is
/// case-insensitive (`<C-L>` is `<C-l>`); Ctrl with Shift is `<C-S-l>`.
fn parse_bracketed(inner: &str) -> Option<Key> {
    let mut mods = Mods::NONE;
    let mut name = inner;
    while name.len() > 2 && name.as_bytes()[1] == b'-' {
        mods = mods
            | match name.as_bytes()[0].to_ascii_uppercase() {
                b'C' => Mods::CTRL,
                b'M' | b'A' => Mods::ALT,
                b'S' => Mods::SHIFT,
                b'D' => Mods::SUPER,
                _ => return None,
            };
        name = &name[2..];
    }
    let mut chars = name.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) if mods.contains(Mods::CTRL) && !mods.contains(Mods::SHIFT) => {
            KeyCode::Char(c.to_ascii_lowercase())
        }
        (Some(c), None) => KeyCode::Char(c),
        _ => named_code(name)?,
    };
    Some(Key::new(code, mods))
}

fn named_code(name: &str) -> Option<KeyCode> {
    if let Some(n) = name
        .strip_prefix(['F', 'f'])
        .and_then(|n| n.parse::<u8>().ok())
    {
        return (1..=12).contains(&n).then_some(KeyCode::F(n));
    }
    NAMED
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, code)| *code)
}

/// Parse the chord form. `Ok(None)` means the string is not a chord
/// and should be read as Vim notation.
fn parse_chord(raw: &str) -> Result<Option<Key>, KeymapError> {
    let parts: Vec<String> = raw.split('+').map(str::to_ascii_lowercase).collect();
    let chord_mod = |token: &str| {
        CHORD_MODS
            .iter()
            .find(|(word, _)| *word == token)
            .map(|(_, m)| *m)
    };
    let Some((last, mod_tokens)) = parts.split_last() else {
        return Ok(None);
    };
    if mod_tokens.is_empty() {
        return Ok(chord_named(last).map(|code| Key::new(code, Mods::NONE)));
    }
    if chord_mod(&mod_tokens[0]).is_none() {
        return Ok(None);
    }
    let mut mods = Mods::NONE;
    for token in mod_tokens {
        mods = mods
            | chord_mod(token)
                .ok_or_else(|| notation_error(raw, &format!("unknown modifier `{token}`")))?;
    }
    let code =
        chord_code(last).ok_or_else(|| notation_error(raw, &format!("unknown key `{last}`")))?;
    Ok(Some(Key::new(code, mods)))
}

fn chord_code(token: &str) -> Option<KeyCode> {
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(KeyCode::Char(c)),
        _ => chord_named(token),
    }
}

fn chord_named(token: &str) -> Option<KeyCode> {
    if let Some(n) = token.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=12).contains(&n).then_some(KeyCode::F(n));
    }
    CHORD_NAMED
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, code)| *code)
}

fn notation_error(input: &str, reason: &str) -> KeymapError {
    KeymapError::Notation {
        input: input.to_owned(),
        reason: reason.to_owned(),
    }
}

/// A named Rust action a mapping can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionDef {
    /// Action name, as in `action:Name` and `kage.action`.
    pub name: &'static str,
    /// Whether the action takes an integer argument.
    pub arg: bool,
    /// One-line description.
    pub doc: &'static str,
}

const fn action(name: &'static str, doc: &'static str) -> ActionDef {
    ActionDef {
        name,
        arg: false,
        doc,
    }
}

/// Every action a mapping can name.
pub const ACTIONS: &[ActionDef] = &[
    action("Cancel", "Cancel the in-flight turn."),
    action("BeginCommand", "Open the `:` command line."),
    action("BeginSearch", "Open the `/` search line."),
    action("ScrollToTop", "Scroll the conversation to the top."),
    action("ScrollToBottom", "Scroll the conversation to the bottom."),
    action("ToggleFold", "Toggle the fold of the focused block."),
    action("UnfoldAll", "Open every foldable block."),
    action("FoldAll", "Close every foldable block."),
    action("Yank", "Copy the active selection to the clipboard."),
    action("ClearSelection", "Drop the active selection."),
    action("OpenModelPicker", "Open the model picker."),
    action("OpenSessionPicker", "Open the session picker."),
    action("OpenCommandPalette", "Open the slash command palette."),
    action("SearchNext", "Focus the next search match."),
    action("SearchPrev", "Focus the previous search match."),
    action("YankFocusedBlock", "Copy the focused block."),
    action("CycleThinkingLevel", "Cycle the thinking level."),
    action(
        "CyclePane",
        "Toggle focus between the input and the conversation.",
    ),
    action("FocusPrev", "Focus the previous foldable block."),
    action("FocusNext", "Focus the next foldable block."),
    ActionDef {
        name: "Scroll",
        arg: true,
        doc: "Scroll the conversation by a line count (negative scrolls up).",
    },
    action("OpenHelp", "Open the keyboard reference."),
    action("OpenJumpPicker", "Open the jump-to-message picker."),
    action(
        "AttachClipboardImage",
        "Attach an image from the clipboard.",
    ),
    action("EnterVisual", "Enter visual selection."),
    action(
        "QueuePrompt",
        "Queue the prompt until the run ends. Does nothing while idle.",
    ),
];

/// Look up an action by name.
#[must_use]
pub fn find_action(name: &str) -> Option<&'static ActionDef> {
    ACTIONS.iter().find(|a| a.name == name)
}

/// What a mapping does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rhs {
    /// Run a Rust action from [`ACTIONS`].
    Action {
        /// Action name.
        name: &'static str,
        /// Integer argument, present exactly when the action takes one.
        arg: Option<i64>,
    },
    /// Run a command line through the command executor.
    Command(String),
    /// Call a Lua handler by id.
    Lua(u64),
    /// Swallow the key.
    Nop,
}

impl Rhs {
    /// Build an action rhs, checking the name and the argument.
    pub fn action(name: &str, arg: Option<i64>) -> Result<Self, KeymapError> {
        let def = find_action(name).ok_or_else(|| KeymapError::UnknownAction(name.to_owned()))?;
        match (def.arg, arg) {
            (true, None) => return Err(KeymapError::MissingArg(def.name)),
            (false, Some(_)) => return Err(KeymapError::UnexpectedArg(def.name)),
            _ => {}
        }
        Ok(Self::Action {
            name: def.name,
            arg,
        })
    }
}

/// Owner of the mappings in the embedded `_defaults.lua`.
pub const OWNER_DEFAULTS: &str = "defaults";
/// Owner of the mappings from `[keybindings] bindings` in `config.toml`.
pub const OWNER_TOML: &str = "config.toml";
/// Owner of the mappings set by the trusted `init.lua`.
pub const OWNER_USER: &str = "init.lua";

/// One mapping in the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// What the mapping does.
    pub rhs: Rhs,
    /// Description shown in help. Mappings without one are hidden there.
    pub desc: Option<String>,
    /// Help section.
    pub group: Option<String>,
    /// Who set it: `defaults`, a plugin stem, `config.toml` or `init.lua`.
    pub owner: String,
}

impl Mapping {
    /// Whether the user set it, in `init.lua` or `config.toml`.
    #[must_use]
    pub fn user_owned(&self) -> bool {
        self.owner == OWNER_USER || self.owner == OWNER_TOML
    }
}

/// Result of [`Keymap::lookup`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// The sequence maps and no longer mapping extends it.
    Exact(&'a Mapping),
    /// A longer mapping extends the sequence. `exact` is the mapping
    /// for the sequence itself, if any.
    Prefix {
        /// Mapping for the sequence itself.
        exact: Option<&'a Mapping>,
    },
    /// Nothing maps the sequence or extends it.
    None,
}

/// A table entry, as returned by [`Keymap::entries`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Mode of the mapping.
    pub mode: Mode,
    /// Key sequence.
    pub lhs: &'a [Key],
    /// The mapping.
    pub mapping: &'a Mapping,
}

#[derive(Clone, Debug)]
struct Slot {
    order: u64,
    mapping: Mapping,
}

/// The keymap table: one map per mode, last set wins.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    modes: [BTreeMap<Vec<Key>, Slot>; 5],
    next_order: u64,
    generation: u64,
}

impl Keymap {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `lhs` in `mode`, replacing any existing mapping, which is
    /// returned. A replaced mapping keeps its position in
    /// [`Self::entries`].
    pub fn set(&mut self, mode: Mode, lhs: Vec<Key>, mapping: Mapping) -> Option<Mapping> {
        let map = &mut self.modes[mode as usize];
        self.generation += 1;
        if let Some(slot) = map.get_mut(&lhs) {
            return Some(std::mem::replace(&mut slot.mapping, mapping));
        }
        map.insert(
            lhs,
            Slot {
                order: self.next_order,
                mapping,
            },
        );
        self.next_order += 1;
        None
    }

    /// Remove the mapping for `lhs` in `mode`.
    pub fn del(&mut self, mode: Mode, lhs: &[Key]) -> Result<Mapping, KeymapError> {
        let slot = self.modes[mode as usize]
            .remove(lhs)
            .ok_or_else(|| KeymapError::NoMapping {
                mode,
                lhs: display_keys(lhs),
            })?;
        self.generation += 1;
        Ok(slot.mapping)
    }

    /// Remove every mapping.
    pub fn clear(&mut self) {
        for map in &mut self.modes {
            map.clear();
        }
        self.generation += 1;
    }

    /// Longest-match lookup of `seq` across `modes`. When several modes
    /// map `seq`, the first in `modes` wins.
    #[must_use]
    pub fn lookup(&self, modes: &[Mode], seq: &[Key]) -> Lookup<'_> {
        let mut exact = None;
        let mut prefix = false;
        for &mode in modes {
            let map = &self.modes[mode as usize];
            if exact.is_none() {
                exact = map.get(seq).map(|slot| &slot.mapping);
            }
            prefix = prefix
                || map
                    .range::<[Key], _>((Bound::Excluded(seq), Bound::Unbounded))
                    .next()
                    .is_some_and(|(lhs, _)| lhs.starts_with(seq));
        }
        match (exact, prefix) {
            (_, true) => Lookup::Prefix { exact },
            (Some(mapping), false) => Lookup::Exact(mapping),
            (None, false) => Lookup::None,
        }
    }

    /// Every mapping, in the order it was first set.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry<'_>> {
        let mut entries: Vec<(u64, Entry<'_>)> = Mode::ALL
            .into_iter()
            .flat_map(|mode| {
                self.modes[mode as usize].iter().map(move |(lhs, slot)| {
                    (
                        slot.order,
                        Entry {
                            mode,
                            lhs,
                            mapping: &slot.mapping,
                        },
                    )
                })
            })
            .collect();
        entries.sort_by_key(|(order, _)| *order);
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Counter bumped on every change.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Errors from key notation and the keymap table.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    /// The key notation does not parse.
    #[error("invalid key notation `{input}`: {reason}")]
    Notation {
        /// The notation as given.
        input: String,
        /// Why it failed.
        reason: String,
    },
    /// `del` found no mapping.
    #[error("no mapping for {lhs} in mode {mode}")]
    NoMapping {
        /// Mode searched.
        mode: Mode,
        /// Key sequence in Vim notation.
        lhs: String,
    },
    /// The action name is not in [`ACTIONS`].
    #[error("unknown action `{0}`")]
    UnknownAction(String),
    /// The action needs an integer argument and got none.
    #[error("action `{0}` needs an integer argument")]
    MissingArg(&'static str),
    /// The action takes no argument and got one.
    #[error("action `{0}` takes no argument")]
    UnexpectedArg(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(s: &str) -> Vec<Key> {
        parse_keys(s, "\\").unwrap()
    }

    fn ch(c: char) -> Key {
        Key::new(KeyCode::Char(c), Mods::NONE)
    }

    fn map(owner: &str) -> Mapping {
        Mapping {
            rhs: Rhs::Nop,
            desc: None,
            group: None,
            owner: owner.to_owned(),
        }
    }

    #[test]
    fn vim_notation_parses() {
        assert_eq!(keys("gg"), [ch('g'), ch('g')]);
        assert_eq!(keys("<C-l>"), [Key::new(KeyCode::Char('l'), Mods::CTRL)]);
        assert_eq!(keys("<M-p>"), keys("<A-p>"));
        assert_eq!(keys("<S-Tab>"), [Key::new(KeyCode::Tab, Mods::SHIFT)]);
        assert_eq!(keys("<cr>"), [Key::new(KeyCode::Enter, Mods::NONE)]);
        assert_eq!(keys("<Space>"), [ch(' ')]);
        assert_eq!(keys("<F5>"), [Key::new(KeyCode::F(5), Mods::NONE)]);
        assert_eq!(keys("<S-a>"), [ch('A')]);
        assert_eq!(keys("<C-S-p>"), [Key::new(KeyCode::Char('P'), Mods::CTRL)]);
        assert_eq!(keys("<C-->"), [Key::new(KeyCode::Char('-'), Mods::CTRL)]);
        assert_eq!(keys("<"), [ch('<')]);
        assert_eq!(keys("<>"), [ch('<'), ch('>')]);
        assert_ne!(keys("n"), keys("N"));
    }

    #[test]
    fn leader_expands_at_parse_time() {
        assert_eq!(keys("<leader>m"), [ch('\\'), ch('m')]);
        assert_eq!(
            parse_keys("<Leader>m", "<Space>").unwrap(),
            [ch(' '), ch('m')]
        );
        assert_eq!(parse_keys("<leader>", ",").unwrap(), [ch(',')]);
        assert!(parse_keys("<leader>x", "ab").is_err());
        assert!(parse_key("<leader>").is_err());
    }

    #[test]
    fn chord_form_is_one_key() {
        assert_eq!(keys("ctrl+shift+x"), keys("<C-S-x>"));
        assert_eq!(keys("Ctrl+S"), keys("<C-s>"));
        assert_eq!(keys("alt+enter"), keys("<M-CR>"));
        assert_eq!(keys("shift+tab"), keys("<S-Tab>"));
        assert_eq!(keys("f5"), keys("<F5>"));
        assert_eq!(keys("space"), keys("<Space>"));
        assert_eq!(keys("cmd+k"), keys("<D-k>"));
        assert_eq!(keys("g+"), [ch('g'), ch('+')]);
        assert_eq!(keys("+"), [ch('+')]);
    }

    #[test]
    fn display_round_trips() {
        for (input, shown) in [
            ("gg", "gg"),
            ("<C-l>", "<C-l>"),
            ("<M-p>", "<M-p>"),
            ("<S-Tab>", "<S-Tab>"),
            ("<CR>", "<CR>"),
            ("<Enter>", "<CR>"),
            ("<Esc>", "<Esc>"),
            (" ", "<Space>"),
            ("<lt>", "<lt>"),
            ("<C-gt>", "<C-gt>"),
            ("<C-Space>", "<C-Space>"),
            ("<F12>", "<F12>"),
            ("<S-a>", "A"),
            ("ctrl+shift+x", "<C-S-x>"),
            ("<C-->", "<C-->"),
            ("<PageUp>", "<PageUp>"),
            ("zM", "zM"),
            ("\\m", "\\m"),
            ("<C-M-S-D-Up>", "<C-M-S-D-Up>"),
        ] {
            let parsed = keys(input);
            let text = display_keys(&parsed);
            assert_eq!(text, shown, "{input}");
            assert_eq!(keys(&text), parsed, "{input}");
        }
    }

    #[test]
    fn notation_errors() {
        for bad in [
            "",
            "<Foo>",
            "<X-a>",
            "<F13>",
            "ctrl+nope",
            "ctrl+bogus+x",
            "ctrl+",
        ] {
            assert!(
                matches!(parse_keys(bad, "\\"), Err(KeymapError::Notation { .. })),
                "{bad:?} should fail"
            );
        }
        assert!(parse_key("gg").is_err());
        assert_eq!(parse_key("<Space>").unwrap(), ch(' '));
    }

    #[test]
    fn mode_letters_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Mode::parse("x"), None);
    }

    #[test]
    fn lookup_is_longest_match() {
        let mut km = Keymap::new();
        km.set(Mode::Normal, keys("gw"), map("a"));
        km.set(Mode::Buffer, keys("gg"), map("b"));
        km.set(Mode::Normal, keys("zM"), map("c"));

        assert_eq!(
            km.lookup(&[Mode::Buffer, Mode::Normal], &keys("g")),
            Lookup::Prefix { exact: None }
        );
        assert!(matches!(
            km.lookup(&[Mode::Buffer, Mode::Normal], &keys("gg")),
            Lookup::Exact(m) if m.owner == "b"
        ));
        assert_eq!(km.lookup(&[Mode::Normal], &keys("gg")), Lookup::None);
        assert_eq!(km.lookup(&[Mode::Normal], &keys("zx")), Lookup::None);
        assert_eq!(km.lookup(&[Mode::Insert], &keys("gw")), Lookup::None);
    }

    #[test]
    fn exact_match_that_is_also_a_prefix() {
        let mut km = Keymap::new();
        km.set(Mode::Global, keys("<leader>"), map("short"));
        km.set(Mode::Normal, keys("<leader>m"), map("long"));
        assert!(matches!(
            km.lookup(&[Mode::Normal, Mode::Global], &keys("\\")),
            Lookup::Prefix { exact: Some(m) } if m.owner == "short"
        ));
        assert!(matches!(
            km.lookup(&[Mode::Global], &keys("\\")),
            Lookup::Exact(m) if m.owner == "short"
        ));
    }

    #[test]
    fn earlier_mode_wins_and_set_replaces() {
        let mut km = Keymap::new();
        km.set(Mode::Global, keys("<C-p>"), map("global"));
        km.set(Mode::Insert, keys("<C-p>"), map("insert"));
        assert!(matches!(
            km.lookup(&[Mode::Insert, Mode::Global], &keys("<C-p>")),
            Lookup::Exact(m) if m.owner == "insert"
        ));
        let replaced = km.set(Mode::Insert, keys("<C-p>"), map("user"));
        assert_eq!(replaced.map(|m| m.owner), Some("insert".to_owned()));
        assert!(matches!(
            km.lookup(&[Mode::Insert], &keys("<C-p>")),
            Lookup::Exact(m) if m.owner == "user"
        ));
    }

    #[test]
    fn del_removes_and_missing_errors() {
        let mut km = Keymap::new();
        km.set(Mode::Global, keys("<C-s>"), map("defaults"));
        let generation = km.generation();
        assert_eq!(
            km.del(Mode::Global, &keys("<C-s>")).unwrap().owner,
            "defaults"
        );
        assert!(km.generation() > generation);
        assert_eq!(
            km.del(Mode::Global, &keys("<C-s>")),
            Err(KeymapError::NoMapping {
                mode: Mode::Global,
                lhs: "<C-s>".to_owned()
            })
        );
        assert!(km.del(Mode::Normal, &keys("x")).is_err());
    }

    #[test]
    fn entries_keep_first_set_order() {
        let mut km = Keymap::new();
        km.set(Mode::Normal, keys("z"), map("1"));
        km.set(Mode::Global, keys("a"), map("2"));
        km.set(Mode::Buffer, keys("m"), map("3"));
        km.set(Mode::Normal, keys("z"), map("4"));
        let owners: Vec<&str> = km
            .entries()
            .iter()
            .map(|e| e.mapping.owner.as_str())
            .collect();
        assert_eq!(owners, ["4", "2", "3"]);
        km.clear();
        assert!(km.entries().is_empty());
    }

    #[test]
    fn actions_hold_the_rebindable_names() {
        assert_eq!(ACTIONS.len(), 26);
        for (i, a) in ACTIONS.iter().enumerate() {
            assert!(
                ACTIONS[..i].iter().all(|b| b.name != a.name),
                "duplicate {}",
                a.name
            );
        }
        assert_eq!(
            Rhs::action("Scroll", Some(-10)),
            Ok(Rhs::Action {
                name: "Scroll",
                arg: Some(-10)
            })
        );
        assert!(Rhs::action("Scroll", None).is_err());
        assert!(Rhs::action("Cancel", Some(1)).is_err());
        assert!(matches!(
            Rhs::action("Nope", None),
            Err(KeymapError::UnknownAction(_))
        ));
    }

    #[test]
    fn ctrl_letters_ignore_case_unless_shift_is_named() {
        let key = |n: &str| parse_key(n).unwrap();
        assert_eq!(key("<C-L>"), key("<C-l>"));
        assert_ne!(key("<C-S-l>"), key("<C-l>"));
        assert_eq!(
            key("<C-S-l>"),
            Key::new(KeyCode::Char('l'), Mods::CTRL | Mods::SHIFT)
        );
        assert_ne!(key("<M-L>"), key("<M-l>"));
    }
}
