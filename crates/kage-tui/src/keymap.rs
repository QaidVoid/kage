//! Keymap dispatch for the editor: crossterm key conversion, the
//! modes that apply to an editing state, the pending-sequence state
//! machine, the action names and the help rows built from the live
//! table.

use std::time::{Duration, Instant};

use kage_core::config::EditorMode;
use kage_core::keymap::{Key, KeyCode, Keymap, Lookup, Mode, Mods, Rhs};
use ratatui::crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyModifiers};

use crate::input::InputAction;

/// Convert a crossterm key event into a keymap [`Key`]. Shift folds
/// into characters as in [`Key::new`], an uppercase character implies
/// shift, and `BackTab` is `<S-Tab>`. Returns `None` for keys the
/// notation cannot name.
#[must_use]
pub fn key_from_event(event: &KeyEvent) -> Option<Key> {
    let mut mods = Mods::NONE;
    for (flag, m) in [
        (KeyModifiers::CONTROL, Mods::CTRL),
        (KeyModifiers::ALT, Mods::ALT),
        (KeyModifiers::SHIFT, Mods::SHIFT),
        (KeyModifiers::SUPER, Mods::SUPER),
    ] {
        if event.modifiers.contains(flag) {
            mods = mods | m;
        }
    }
    let code = match event.code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Esc => KeyCode::Esc,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::BackTab => {
            mods = mods | Mods::SHIFT;
            KeyCode::Tab
        }
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Delete => KeyCode::Delete,
        CtKeyCode::Up => KeyCode::Up,
        CtKeyCode::Down => KeyCode::Down,
        CtKeyCode::Left => KeyCode::Left,
        CtKeyCode::Right => KeyCode::Right,
        CtKeyCode::Home => KeyCode::Home,
        CtKeyCode::End => KeyCode::End,
        CtKeyCode::PageUp => KeyCode::PageUp,
        CtKeyCode::PageDown => KeyCode::PageDown,
        CtKeyCode::Insert => KeyCode::Insert,
        CtKeyCode::F(n @ 1..=12) => KeyCode::F(n),
        _ => return None,
    };
    Some(Key::new(code, mods))
}

/// Convert a keymap [`Key`] back into a crossterm key event, so keys
/// the keymap replays reach the editor grammar as they arrived.
#[must_use]
pub fn event_from_key(key: Key) -> KeyEvent {
    let mods = key.mods();
    let mut modifiers = KeyModifiers::NONE;
    for (m, flag) in [
        (Mods::CTRL, KeyModifiers::CONTROL),
        (Mods::ALT, KeyModifiers::ALT),
        (Mods::SHIFT, KeyModifiers::SHIFT),
        (Mods::SUPER, KeyModifiers::SUPER),
    ] {
        if mods.contains(m) {
            modifiers |= flag;
        }
    }
    let code = match key.code() {
        KeyCode::Char(c) => CtKeyCode::Char(c),
        KeyCode::Enter => CtKeyCode::Enter,
        KeyCode::Esc => CtKeyCode::Esc,
        KeyCode::Tab if mods.contains(Mods::SHIFT) => CtKeyCode::BackTab,
        KeyCode::Tab => CtKeyCode::Tab,
        KeyCode::Backspace => CtKeyCode::Backspace,
        KeyCode::Delete => CtKeyCode::Delete,
        KeyCode::Up => CtKeyCode::Up,
        KeyCode::Down => CtKeyCode::Down,
        KeyCode::Left => CtKeyCode::Left,
        KeyCode::Right => CtKeyCode::Right,
        KeyCode::Home => CtKeyCode::Home,
        KeyCode::End => CtKeyCode::End,
        KeyCode::PageUp => CtKeyCode::PageUp,
        KeyCode::PageDown => CtKeyCode::PageDown,
        KeyCode::Insert => CtKeyCode::Insert,
        KeyCode::F(n) => CtKeyCode::F(n),
    };
    KeyEvent::new(code, modifiers)
}

/// The input action a `kage.action` name runs, or `None` for a name
/// not in [`kage_core::keymap::ACTIONS`]. `arg` is the line count of
/// `Scroll`, saturated to `i32`.
#[must_use]
pub fn action(name: &str, arg: Option<i64>) -> Option<InputAction> {
    Some(match name {
        "Cancel" => InputAction::Cancel,
        "BeginCommand" => InputAction::BeginCommand,
        "BeginSearch" => InputAction::BeginSearch,
        "ScrollToTop" => InputAction::ScrollToTop,
        "ScrollToBottom" => InputAction::ScrollToBottom,
        "ToggleFold" => InputAction::ToggleFold,
        "UnfoldAll" => InputAction::UnfoldAll,
        "FoldAll" => InputAction::FoldAll,
        "Yank" => InputAction::Yank,
        "ClearSelection" => InputAction::ClearSelection,
        "OpenModelPicker" => InputAction::OpenModelPicker,
        "OpenSessionPicker" => InputAction::OpenSessionPicker,
        "OpenCommandPalette" => InputAction::OpenCommandPalette,
        "SearchNext" => InputAction::SearchNext,
        "SearchPrev" => InputAction::SearchPrev,
        "YankFocusedBlock" => InputAction::YankFocusedBlock,
        "CycleThinkingLevel" => InputAction::CycleThinkingLevel,
        "CyclePane" => InputAction::CyclePane,
        "FocusPrev" => InputAction::FocusPrev,
        "FocusNext" => InputAction::FocusNext,
        "Scroll" => {
            let lines = arg
                .unwrap_or(0)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            InputAction::Scroll(i32::try_from(lines).unwrap_or_default())
        }
        "OpenHelp" => InputAction::OpenHelp,
        "OpenJumpPicker" => InputAction::OpenJumpPicker,
        "AttachClipboardImage" => InputAction::AttachClipboardImage,
        "EnterVisual" => InputAction::EnterVisual,
        "QueuePrompt" => InputAction::QueuePrompt,
        "OpenAgents" => InputAction::OpenAgents,
        _ => return None,
    })
}

/// The editing state a key arrives in, which selects the keymap modes
/// to search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditState {
    /// Vim normal mode with the conversation pane focused.
    NormalBuffer,
    /// Vim normal mode with the input pane focused.
    NormalInput,
    /// Vim insert mode or the modeless editor.
    Insert,
    /// Visual selection.
    Visual,
}

impl EditState {
    /// Modes to search, highest priority first.
    #[must_use]
    pub const fn modes(self) -> &'static [Mode] {
        match self {
            Self::NormalBuffer => &[Mode::Buffer, Mode::Normal, Mode::Global],
            Self::NormalInput => &[Mode::Normal, Mode::Global],
            Self::Insert => &[Mode::Insert, Mode::Global],
            Self::Visual => &[Mode::Visual, Mode::Global],
        }
    }
}

/// One outcome of feeding keys to a [`Sequencer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A mapping matched: run its rhs.
    Fire(Rhs),
    /// No mapping matched: pass these keys to the editor grammar in
    /// order.
    Replay(Vec<Key>),
    /// Keys are buffered. Call [`Sequencer::tick`] at `deadline`.
    Pending {
        /// When the buffer resolves on its own.
        deadline: Instant,
    },
}

/// The pending-sequence state machine. It buffers keys while they are
/// a prefix of a longer mapping and resolves them on a mismatch or
/// after the timeout.
#[derive(Clone, Debug)]
pub struct Sequencer {
    keys: Vec<Key>,
    deadline: Option<Instant>,
    timeout: Duration,
}

impl Sequencer {
    /// An empty sequencer that waits `timeout` for a longer match.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            keys: Vec::new(),
            deadline: None,
            timeout,
        }
    }

    /// Change the wait for a longer match. A pending deadline keeps
    /// its value.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Drop any buffered keys.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.deadline = None;
    }

    /// Keys buffered while they are a prefix of a longer mapping.
    #[must_use]
    pub fn pending(&self) -> &[Key] {
        &self.keys
    }

    /// When the buffered keys resolve on their own, if any are
    /// buffered.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Feed one key looked up in `modes`. A [`Step::Pending`], if
    /// present, is the last step.
    pub fn feed(&mut self, keymap: &Keymap, modes: &[Mode], key: Key, now: Instant) -> Vec<Step> {
        let mut steps = Vec::new();
        self.push(keymap, modes, key, &mut steps);
        self.finish(now, steps)
    }

    /// Resolve the buffer if its deadline has passed: fire the longest
    /// exact prefix and re-feed the rest, or replay the keys.
    pub fn tick(&mut self, keymap: &Keymap, modes: &[Mode], now: Instant) -> Vec<Step> {
        if self.deadline.is_none_or(|deadline| now < deadline) {
            return Vec::new();
        }
        let mut steps = Vec::new();
        self.flush(keymap, modes, &mut steps);
        self.finish(now, steps)
    }

    fn push(&mut self, keymap: &Keymap, modes: &[Mode], key: Key, steps: &mut Vec<Step>) {
        self.keys.push(key);
        match keymap.lookup(modes, &self.keys) {
            Lookup::Exact(mapping) => {
                self.keys.clear();
                steps.push(Step::Fire(mapping.rhs.clone()));
            }
            Lookup::Prefix { .. } => {}
            Lookup::None => self.flush(keymap, modes, steps),
        }
    }

    fn flush(&mut self, keymap: &Keymap, modes: &[Mode], steps: &mut Vec<Step>) {
        let keys = std::mem::take(&mut self.keys);
        let longest = (1..=keys.len())
            .rev()
            .find_map(|n| match keymap.lookup(modes, &keys[..n]) {
                Lookup::Exact(mapping)
                | Lookup::Prefix {
                    exact: Some(mapping),
                } => Some((n, mapping)),
                _ => None,
            });
        let Some((n, mapping)) = longest else {
            steps.push(Step::Replay(keys));
            return;
        };
        steps.push(Step::Fire(mapping.rhs.clone()));
        for &key in &keys[n..] {
            self.push(keymap, modes, key, steps);
        }
    }

    fn finish(&mut self, now: Instant, mut steps: Vec<Step>) -> Vec<Step> {
        self.deadline = (!self.keys.is_empty()).then(|| now + self.timeout);
        if let Some(deadline) = self.deadline {
            steps.push(Step::Pending { deadline });
        }
        steps
    }
}

/// A help section: mappings sharing a `group`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpGroup {
    /// Group name, `other` for mappings without one.
    pub name: String,
    /// Rows in first-set order.
    pub rows: Vec<HelpRow>,
}

/// One help row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpRow {
    /// Key sequence in Vim notation.
    pub lhs: String,
    /// What the mapping does.
    pub desc: String,
}

/// One key as user-facing text shows it: `<C-p>` reads `ctrl+p`,
/// `<S-Tab>` reads `shift+tab`, and a plain character stays itself.
#[must_use]
pub fn key_label(key: &Key) -> String {
    let vim = key.to_string();
    let Some(mut inner) = vim.strip_prefix('<').and_then(|v| v.strip_suffix('>')) else {
        return vim;
    };
    let mut out = String::new();
    loop {
        let (prefix, rest) = match inner.split_at_checked(2) {
            Some(("C-", rest)) => ("ctrl+", rest),
            Some(("M-", rest)) => ("alt+", rest),
            Some(("S-", rest)) => ("shift+", rest),
            Some(("D-", rest)) => ("super+", rest),
            _ => break,
        };
        out.push_str(prefix);
        inner = rest;
    }
    match inner {
        "CR" => out.push_str("enter"),
        "BS" => out.push_str("backspace"),
        name => out.push_str(&name.to_lowercase()),
    }
    out
}

/// A key sequence as user-facing text shows it: plain characters run
/// together (`gg`), and a sequence with a named key or chord is spaced
/// (`ctrl+w j`).
#[must_use]
pub fn key_labels(keys: &[Key]) -> String {
    let labels: Vec<String> = keys.iter().map(key_label).collect();
    if labels.iter().all(|l| l.chars().count() == 1) {
        labels.concat()
    } else {
        labels.join(" ")
    }
}

/// Build help rows from `keymap`: mappings with a `desc`, in the modes
/// that apply to `editor`, grouped by `group` in first-seen order.
/// Rows repeated across modes with the same keys and text show once.
#[must_use]
pub fn help_groups(keymap: &Keymap, editor: EditorMode) -> Vec<HelpGroup> {
    let mut groups: Vec<HelpGroup> = Vec::new();
    for entry in keymap.entries() {
        let applies = match editor {
            EditorMode::Vim => true,
            EditorMode::Modeless => EditState::Insert.modes().contains(&entry.mode),
        };
        let Some(desc) = entry.mapping.desc.as_ref().filter(|_| applies) else {
            continue;
        };
        let name = entry.mapping.group.as_deref().unwrap_or("other");
        let index = groups
            .iter()
            .position(|g| g.name == name)
            .unwrap_or_else(|| {
                groups.push(HelpGroup {
                    name: name.to_owned(),
                    rows: Vec::new(),
                });
                groups.len() - 1
            });
        let row = HelpRow {
            lhs: key_labels(entry.lhs),
            desc: desc.clone(),
        };
        if !groups[index].rows.contains(&row) {
            groups[index].rows.push(row);
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_core::keymap::{Mapping, parse_key, parse_keys};

    const TIMEOUT: Duration = Duration::from_secs(1);

    fn keys(s: &str) -> Vec<Key> {
        parse_keys(s, "\\").unwrap()
    }

    fn key(s: &str) -> Key {
        parse_key(s).unwrap()
    }

    fn ev(code: CtKeyCode, mods: KeyModifiers) -> Option<Key> {
        key_from_event(&KeyEvent::new(code, mods))
    }

    fn cmd(s: &str) -> Rhs {
        Rhs::Command(s.to_owned())
    }

    fn mapping(rhs: Rhs, desc: Option<&str>, group: Option<&str>) -> Mapping {
        Mapping {
            rhs,
            desc: desc.map(str::to_owned),
            group: group.map(str::to_owned),
            owner: "test".to_owned(),
        }
    }

    fn table(maps: &[(Mode, &str, &str)]) -> Keymap {
        let mut km = Keymap::new();
        for &(mode, lhs, rhs) in maps {
            km.set(mode, keys(lhs), mapping(cmd(rhs), None, None));
        }
        km
    }

    fn feed_all(
        seq: &mut Sequencer,
        km: &Keymap,
        modes: &[Mode],
        input: &str,
        now: Instant,
    ) -> Vec<Step> {
        keys(input)
            .into_iter()
            .flat_map(|k| seq.feed(km, modes, k, now))
            .collect()
    }

    #[test]
    fn events_convert_like_parsed_notation() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;
        assert_eq!(ev(CtKeyCode::Char('g'), none), Some(key("g")));
        assert_eq!(ev(CtKeyCode::Char('A'), none), Some(key("A")));
        assert_eq!(ev(CtKeyCode::Char('A'), shift), Some(key("A")));
        assert_eq!(ev(CtKeyCode::Char('a'), shift), Some(key("<S-a>")));
        assert_eq!(ev(CtKeyCode::Char('!'), shift), Some(key("!")));
        assert_eq!(ev(CtKeyCode::Char(' '), none), Some(key("<Space>")));
        assert_eq!(ev(CtKeyCode::Char('l'), ctrl), Some(key("<C-l>")));
        assert_eq!(ev(CtKeyCode::Char('l'), ctrl | shift), Some(key("<C-S-l>")));
        assert_eq!(ev(CtKeyCode::Char('L'), ctrl | shift), Some(key("<C-S-l>")));
        assert_eq!(ev(CtKeyCode::Char('L'), ctrl), Some(key("<C-S-l>")));
        assert_ne!(ev(CtKeyCode::Char('l'), ctrl), Some(key("<C-S-l>")));
        assert_eq!(ev(CtKeyCode::BackTab, none), Some(key("<S-Tab>")));
        assert_eq!(ev(CtKeyCode::BackTab, shift), Some(key("<S-Tab>")));
        assert_eq!(ev(CtKeyCode::Tab, shift), Some(key("<S-Tab>")));
        assert_eq!(ev(CtKeyCode::Enter, KeyModifiers::ALT), Some(key("<M-CR>")));
        assert_eq!(
            ev(CtKeyCode::Char('k'), KeyModifiers::SUPER),
            Some(key("<D-k>"))
        );
        assert_eq!(ev(CtKeyCode::Up, ctrl), Some(key("<C-Up>")));
        assert_eq!(ev(CtKeyCode::F(5), none), Some(key("<F5>")));
        assert_eq!(ev(CtKeyCode::F(13), none), None);
        assert_eq!(ev(CtKeyCode::CapsLock, none), None);
    }

    #[test]
    fn replayed_keys_round_trip_to_events() {
        for notation in [
            "g", "G", "<C-l>", "<C-S-l>", "<S-Tab>", "<M-CR>", "<Space>", "<F5>", "<C-Up>", "<D-k>",
        ] {
            let k = key(notation);
            assert_eq!(key_from_event(&event_from_key(k)), Some(k), "{notation}");
        }
        assert_eq!(event_from_key(key("<S-Tab>")).code, CtKeyCode::BackTab);
    }

    #[test]
    fn every_action_name_maps_to_an_input_action() {
        for def in kage_core::keymap::ACTIONS {
            let arg = def.arg.then_some(-3);
            assert!(
                action(def.name, arg).is_some(),
                "{} has no action",
                def.name
            );
        }
        assert_eq!(action("Scroll", Some(-10)), Some(InputAction::Scroll(-10)));
        assert_eq!(
            action("Scroll", Some(i64::MAX)),
            Some(InputAction::Scroll(i32::MAX))
        );
        assert_eq!(action("Nope", None), None);
    }

    #[test]
    fn clear_drops_buffered_keys() {
        let km = table(&[(Mode::Normal, "gw", "pane")]);
        let modes = EditState::NormalInput.modes();
        let mut seq = Sequencer::new(TIMEOUT);
        seq.feed(&km, modes, key("g"), Instant::now());
        seq.clear();
        assert_eq!(seq.deadline(), None);
        assert_eq!(
            seq.feed(&km, modes, key("w"), Instant::now()),
            [Step::Replay(keys("w"))]
        );
    }

    #[test]
    fn edit_states_search_their_modes() {
        use Mode::{Buffer, Global, Insert, Normal, Visual};
        assert_eq!(EditState::NormalBuffer.modes(), [Buffer, Normal, Global]);
        assert_eq!(EditState::NormalInput.modes(), [Normal, Global]);
        assert_eq!(EditState::Insert.modes(), [Insert, Global]);
        assert_eq!(EditState::Visual.modes(), [Visual, Global]);
    }

    #[test]
    fn exact_match_fires_and_unmapped_key_replays() {
        let km = table(&[(Mode::Global, "<C-p>", "models")]);
        let modes = EditState::Insert.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        assert_eq!(
            seq.feed(&km, modes, key("<C-p>"), now),
            [Step::Fire(cmd("models"))]
        );
        assert_eq!(
            seq.feed(&km, modes, key("x"), now),
            [Step::Replay(keys("x"))]
        );
        assert_eq!(seq.deadline(), None);
    }

    #[test]
    fn prefix_waits_and_completion_fires() {
        let km = table(&[(Mode::Buffer, "gg", "top")]);
        let modes = EditState::NormalBuffer.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        assert_eq!(
            seq.feed(&km, modes, key("g"), now),
            [Step::Pending {
                deadline: now + TIMEOUT
            }]
        );
        assert_eq!(seq.deadline(), Some(now + TIMEOUT));
        let later = now + Duration::from_millis(300);
        assert_eq!(
            seq.feed(&km, modes, key("g"), later),
            [Step::Fire(cmd("top"))]
        );
        assert_eq!(seq.deadline(), None);
    }

    #[test]
    fn each_buffered_key_restarts_the_deadline() {
        let km = table(&[(Mode::Normal, "abc", "long")]);
        let modes = EditState::NormalInput.modes();
        let now = Instant::now();
        let later = now + Duration::from_millis(600);
        let mut seq = Sequencer::new(TIMEOUT);
        seq.feed(&km, modes, key("a"), now);
        assert_eq!(
            seq.feed(&km, modes, key("b"), later),
            [Step::Pending {
                deadline: later + TIMEOUT
            }]
        );
        assert!(seq.tick(&km, modes, now + TIMEOUT).is_empty());
    }

    #[test]
    fn exact_prefix_fires_after_the_timeout() {
        let km = table(&[
            (Mode::Global, "<leader>", "short"),
            (Mode::Normal, "<leader>m", "long"),
        ]);
        let modes = EditState::NormalInput.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        assert_eq!(
            seq.feed(&km, modes, key("\\"), now),
            [Step::Pending {
                deadline: now + TIMEOUT
            }]
        );
        assert!(
            seq.tick(&km, modes, now + Duration::from_millis(999))
                .is_empty()
        );
        assert_eq!(
            seq.tick(&km, modes, now + TIMEOUT),
            [Step::Fire(cmd("short"))]
        );
        assert_eq!(seq.deadline(), None);
        assert!(seq.tick(&km, modes, now + TIMEOUT * 2).is_empty());
    }

    #[test]
    fn exact_prefix_yields_to_the_longer_mapping() {
        let km = table(&[
            (Mode::Global, "<leader>", "short"),
            (Mode::Normal, "<leader>m", "long"),
        ]);
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        assert_eq!(
            feed_all(&mut seq, &km, EditState::NormalInput.modes(), "\\m", now),
            [
                Step::Pending {
                    deadline: now + TIMEOUT
                },
                Step::Fire(cmd("long"))
            ]
        );
    }

    #[test]
    fn mismatch_fires_longest_exact_prefix_and_refeeds_the_rest() {
        let km = table(&[
            (Mode::Normal, "a", "a"),
            (Mode::Normal, "abcd", "abcd"),
            (Mode::Normal, "c", "c"),
            (Mode::Normal, "zz", "zz"),
        ]);
        let modes = EditState::NormalInput.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        feed_all(&mut seq, &km, modes, "abc", now);
        assert_eq!(
            seq.feed(&km, modes, key("x"), now),
            [
                Step::Fire(cmd("a")),
                Step::Replay(keys("b")),
                Step::Fire(cmd("c")),
                Step::Replay(keys("x")),
            ]
        );
        assert_eq!(seq.deadline(), None);

        feed_all(&mut seq, &km, modes, "ab", now);
        assert_eq!(
            seq.feed(&km, modes, key("z"), now),
            [
                Step::Fire(cmd("a")),
                Step::Replay(keys("b")),
                Step::Pending {
                    deadline: now + TIMEOUT
                },
            ]
        );
        assert_eq!(seq.feed(&km, modes, key("z"), now), [Step::Fire(cmd("zz"))]);
    }

    #[test]
    fn mismatch_without_exact_prefix_replays_in_order() {
        let km = table(&[(Mode::Normal, "zo", "open"), (Mode::Normal, "gw", "yank")]);
        let modes = EditState::NormalInput.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        assert_eq!(
            feed_all(&mut seq, &km, modes, "zx", now),
            [
                Step::Pending {
                    deadline: now + TIMEOUT
                },
                Step::Replay(keys("zx"))
            ]
        );
        assert_eq!(
            feed_all(&mut seq, &km, modes, "gg", now),
            [
                Step::Pending {
                    deadline: now + TIMEOUT
                },
                Step::Replay(keys("gg"))
            ]
        );
    }

    #[test]
    fn timeout_without_exact_match_replays() {
        let km = table(&[(Mode::Normal, "zo", "open")]);
        let modes = EditState::NormalInput.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        seq.feed(&km, modes, key("z"), now);
        assert_eq!(
            seq.tick(&km, modes, now + TIMEOUT),
            [Step::Replay(keys("z"))]
        );
    }

    #[test]
    fn timeout_fires_exact_prefix_and_refeeds_the_rest() {
        let km = table(&[(Mode::Normal, "a", "a"), (Mode::Normal, "abc", "abc")]);
        let modes = EditState::NormalInput.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        feed_all(&mut seq, &km, modes, "ab", now);
        let expired = now + TIMEOUT;
        assert_eq!(
            seq.tick(&km, modes, expired),
            [Step::Fire(cmd("a")), Step::Replay(keys("b"))]
        );
    }

    #[test]
    fn modes_follow_the_edit_state() {
        let km = table(&[(Mode::Buffer, "gg", "top"), (Mode::Insert, "<C-n>", "next")]);
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        let buffer = feed_all(&mut seq, &km, EditState::NormalBuffer.modes(), "gg", now);
        assert_eq!(buffer.last(), Some(&Step::Fire(cmd("top"))));
        let input = feed_all(&mut seq, &km, EditState::NormalInput.modes(), "gg", now);
        assert_eq!(input, [Step::Replay(keys("g")), Step::Replay(keys("g"))]);
        assert_eq!(
            seq.feed(&km, EditState::Visual.modes(), key("<C-n>"), now),
            [Step::Replay(keys("<C-n>"))]
        );
    }

    #[test]
    fn zero_timeout_resolves_on_the_next_tick() {
        let km = table(&[
            (Mode::Global, "<leader>", "short"),
            (Mode::Global, "<leader>m", "long"),
        ]);
        let modes = EditState::Insert.modes();
        let now = Instant::now();
        let mut seq = Sequencer::new(TIMEOUT);
        seq.set_timeout(Duration::ZERO);
        assert_eq!(
            seq.feed(&km, modes, key("\\"), now),
            [Step::Pending { deadline: now }]
        );
        assert_eq!(seq.tick(&km, modes, now), [Step::Fire(cmd("short"))]);
    }

    #[test]
    fn help_groups_rows_by_first_seen_group() {
        let mut km = Keymap::new();
        let set = |km: &mut Keymap, mode, lhs: &str, desc, group| {
            km.set(mode, keys(lhs), mapping(Rhs::Nop, desc, group));
        };
        set(
            &mut km,
            Mode::Global,
            "<C-p>",
            Some("model picker"),
            Some("pickers"),
        );
        set(
            &mut km,
            Mode::Buffer,
            "gg",
            Some("scroll to top"),
            Some("buffer"),
        );
        set(
            &mut km,
            Mode::Insert,
            "<PageUp>",
            Some("scroll up"),
            Some("buffer"),
        );
        set(
            &mut km,
            Mode::Normal,
            "<PageUp>",
            Some("scroll up"),
            Some("buffer"),
        );
        set(
            &mut km,
            Mode::Global,
            "<C-s>",
            Some("session picker"),
            Some("pickers"),
        );
        set(&mut km, Mode::Visual, "y", Some("yank"), None);
        set(&mut km, Mode::Global, "<leader>x", None, Some("hidden"));

        let row = |lhs: &str, desc: &str| HelpRow {
            lhs: lhs.to_owned(),
            desc: desc.to_owned(),
        };
        let group = |name: &str, rows: Vec<HelpRow>| HelpGroup {
            name: name.to_owned(),
            rows,
        };
        assert_eq!(
            help_groups(&km, EditorMode::Vim),
            [
                group(
                    "pickers",
                    vec![
                        row("ctrl+p", "model picker"),
                        row("ctrl+s", "session picker")
                    ]
                ),
                group(
                    "buffer",
                    vec![row("gg", "scroll to top"), row("pageup", "scroll up")]
                ),
                group("other", vec![row("y", "yank")]),
            ]
        );
        assert_eq!(
            help_groups(&km, EditorMode::Modeless),
            [
                group(
                    "pickers",
                    vec![
                        row("ctrl+p", "model picker"),
                        row("ctrl+s", "session picker")
                    ]
                ),
                group("buffer", vec![row("pageup", "scroll up")]),
            ]
        );
    }

    #[test]
    fn key_labels_use_the_footer_notation() {
        for (vim, label) in [
            ("<C-p>", "ctrl+p"),
            ("<S-Tab>", "shift+tab"),
            ("<M-CR>", "alt+enter"),
            ("<C-S-l>", "ctrl+shift+l"),
            ("<BS>", "backspace"),
            ("gg", "gg"),
            ("<C-w>j", "ctrl+w j"),
        ] {
            assert_eq!(key_labels(&keys(vim)), label, "{vim}");
        }
    }
}
