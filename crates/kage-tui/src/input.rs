//! Modal input state machine: Normal / Insert / Visual.
//!
//! Vim-flavoured key handling: `i` enters insert, `Esc` returns to
//! normal, `j`/`k`/`gg`/`G` scroll the buffer, `v` enters visual, `y`
//! yanks the selection, `/` starts a search, `:` starts a command. Each
//! key resolves to zero or more [`InputAction`]s the host applies to
//! its [`crate::Buffer`] / clipboard / command runner.
//!
//! The state machine is intentionally pure: input is a [`KeyEvent`],
//! output is a [`Vec<InputAction>`]. Side effects live in the host.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Editing mode of the input area.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    /// Vim-style normal mode: navigation, folding, mode entry.
    #[default]
    Normal,
    /// Free-text insert mode for the prompt input.
    Insert,
    /// Visual selection over conversation lines.
    Visual,
}

/// Side effect produced by a key press.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputAction {
    /// Mode transition; host updates its rendering accordingly.
    EnterMode(Mode),
    /// Submit the input buffer as a user prompt; clears the buffer.
    Submit(String),
    /// Scroll the conversation buffer by `delta` lines (negative = up).
    Scroll(i32),
    /// Snap the conversation scroll to the top.
    ScrollToTop,
    /// Snap the conversation scroll to the bottom.
    ScrollToBottom,
    /// Toggle the fold state of the focused block.
    ToggleFold,
    /// Open every foldable block.
    UnfoldAll,
    /// Close every foldable block.
    FoldAll,
    /// Begin a `:` command. Host opens a command line.
    BeginCommand,
    /// Begin a `/` search. Host opens a search overlay.
    BeginSearch,
    /// Yank the current visual selection into the OSC52 clipboard.
    Yank,
    /// Cancel the in-flight turn.
    Cancel,
    /// Open the in-TUI model picker overlay.
    OpenModelPicker,
}

/// Tracks the editing mode, the prompt text, and any pending leader key
/// (e.g. `g` waiting for the second `g` of `gg`).
#[derive(Debug, Default)]
pub struct InputState {
    mode: Mode,
    text: String,
    cursor: usize,
    pending: Option<char>,
}

impl InputState {
    /// Construct a state in [`Mode::Normal`] with an empty prompt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current editing mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Current prompt-input text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor in the prompt text.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// True if there is a pending two-key sequence waiting on its second
    /// keystroke.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Drive the state machine forward by one key.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Visual => self.handle_visual(key),
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) -> Vec<InputAction> {
        if let Some(prev) = self.pending.take() {
            return Self::handle_pending(prev, key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('i' | 'a') => self.enter_mode(Mode::Insert),
            KeyCode::Char('v') => self.enter_mode(Mode::Visual),
            KeyCode::Char('j') | KeyCode::Down => vec![InputAction::Scroll(1)],
            KeyCode::Char('k') | KeyCode::Up => vec![InputAction::Scroll(-1)],
            KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right => {
                vec![InputAction::Scroll(0)]
            }
            KeyCode::PageDown => vec![InputAction::Scroll(10)],
            KeyCode::PageUp => vec![InputAction::Scroll(-10)],
            KeyCode::Char('G') => vec![InputAction::ScrollToBottom],
            KeyCode::Char('g') => {
                self.pending = Some('g');
                Vec::new()
            }
            KeyCode::Char('z') => {
                self.pending = Some('z');
                Vec::new()
            }
            KeyCode::Char('o') if ctrl => vec![InputAction::ToggleFold],
            KeyCode::Char(':') => vec![InputAction::BeginCommand],
            KeyCode::Char('/') => vec![InputAction::BeginSearch],
            KeyCode::Char('c') if ctrl => vec![InputAction::Cancel],
            KeyCode::Char('p') if ctrl => vec![InputAction::OpenModelPicker],
            _ => Vec::new(),
        }
    }

    fn handle_pending(prev: char, key: KeyEvent) -> Vec<InputAction> {
        match (prev, key.code) {
            ('g', KeyCode::Char('g')) => vec![InputAction::ScrollToTop],
            ('z', KeyCode::Char('o' | 'c')) => vec![InputAction::ToggleFold],
            ('z', KeyCode::Char('R')) => vec![InputAction::UnfoldAll],
            ('z', KeyCode::Char('M')) => vec![InputAction::FoldAll],
            _ => Vec::new(),
        }
    }

    fn handle_insert(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match key.code {
            KeyCode::Esc => self.enter_mode(Mode::Normal),
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.insert_char('\n');
                    Vec::new()
                } else if self.text.is_empty() {
                    Vec::new()
                } else {
                    let text = std::mem::take(&mut self.text);
                    self.cursor = 0;
                    let mut actions = vec![InputAction::Submit(text)];
                    actions.extend(self.enter_mode(Mode::Normal));
                    actions
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                Vec::new()
            }
            KeyCode::Left => {
                self.move_cursor(-1);
                Vec::new()
            }
            KeyCode::Right => {
                self.move_cursor(1);
                Vec::new()
            }
            KeyCode::Home => {
                self.cursor = 0;
                Vec::new()
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                Vec::new()
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_visual(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match key.code {
            KeyCode::Esc => self.enter_mode(Mode::Normal),
            KeyCode::Char('y') => {
                let mut actions = vec![InputAction::Yank];
                actions.extend(self.enter_mode(Mode::Normal));
                actions
            }
            KeyCode::Char('j') | KeyCode::Down => vec![InputAction::Scroll(1)],
            KeyCode::Char('k') | KeyCode::Up => vec![InputAction::Scroll(-1)],
            _ => Vec::new(),
        }
    }

    fn enter_mode(&mut self, mode: Mode) -> Vec<InputAction> {
        if self.mode == mode {
            return Vec::new();
        }
        self.mode = mode;
        self.pending = None;
        vec![InputAction::EnterMode(mode)]
    }

    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    fn move_cursor(&mut self, delta: i32) {
        let target = i64::try_from(self.cursor).unwrap_or(0) + i64::from(delta);
        if target < 0 {
            self.cursor = 0;
        } else if let Ok(pos) = usize::try_from(target) {
            self.cursor = pos.min(self.text.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn shift_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
    }

    #[test]
    fn i_enters_insert_then_esc_returns_to_normal() {
        let mut state = InputState::new();
        let acts = state.handle_key(key(KeyCode::Char('i')));
        assert_eq!(acts, vec![InputAction::EnterMode(Mode::Insert)]);
        assert_eq!(state.mode(), Mode::Insert);
        let acts = state.handle_key(key(KeyCode::Esc));
        assert_eq!(acts, vec![InputAction::EnterMode(Mode::Normal)]);
    }

    #[test]
    fn typing_in_insert_appends_to_text() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        for c in "hello".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(state.text(), "hello");
        assert_eq!(state.cursor(), 5);
    }

    #[test]
    fn enter_in_insert_submits_and_returns_to_normal() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        for c in "hi".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        let acts = state.handle_key(key(KeyCode::Enter));
        assert_eq!(
            acts,
            vec![
                InputAction::Submit("hi".into()),
                InputAction::EnterMode(Mode::Normal),
            ],
        );
        assert_eq!(state.text(), "");
        assert_eq!(state.mode(), Mode::Normal);
    }

    #[test]
    fn enter_on_empty_buffer_in_insert_is_a_no_op() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        let acts = state.handle_key(key(KeyCode::Enter));
        assert!(acts.is_empty());
        assert_eq!(state.mode(), Mode::Insert);
    }

    #[test]
    fn shift_enter_inserts_a_newline_in_insert() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.handle_key(key(KeyCode::Char('a')));
        state.handle_key(shift_enter());
        state.handle_key(key(KeyCode::Char('b')));
        assert_eq!(state.text(), "a\nb");
    }

    #[test]
    fn jk_scroll_in_normal() {
        let mut state = InputState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('j'))),
            vec![InputAction::Scroll(1)]
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('k'))),
            vec![InputAction::Scroll(-1)]
        );
    }

    #[test]
    fn gg_scrolls_to_top_only_after_second_g() {
        let mut state = InputState::new();
        let first = state.handle_key(key(KeyCode::Char('g')));
        assert!(first.is_empty());
        assert!(state.has_pending());
        let second = state.handle_key(key(KeyCode::Char('g')));
        assert_eq!(second, vec![InputAction::ScrollToTop]);
        assert!(!state.has_pending());
    }

    #[test]
    fn capital_g_scrolls_to_bottom_immediately() {
        let mut state = InputState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('G'))),
            vec![InputAction::ScrollToBottom]
        );
    }

    #[test]
    fn z_prefix_handles_fold_keys() {
        let mut state = InputState::new();
        for (suffix, expected) in [
            ('o', InputAction::ToggleFold),
            ('c', InputAction::ToggleFold),
            ('R', InputAction::UnfoldAll),
            ('M', InputAction::FoldAll),
        ] {
            state.handle_key(key(KeyCode::Char('z')));
            let acts = state.handle_key(key(KeyCode::Char(suffix)));
            assert_eq!(acts, vec![expected.clone()]);
        }
    }

    #[test]
    fn colon_and_slash_open_command_and_search() {
        let mut state = InputState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char(':'))),
            vec![InputAction::BeginCommand]
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('/'))),
            vec![InputAction::BeginSearch]
        );
    }

    #[test]
    fn ctrl_c_in_normal_emits_cancel() {
        let mut state = InputState::new();
        assert_eq!(state.handle_key(ctrl('c')), vec![InputAction::Cancel]);
    }

    #[test]
    fn visual_mode_y_yanks_and_returns_to_normal() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('v')));
        assert_eq!(state.mode(), Mode::Visual);
        let acts = state.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            acts,
            vec![InputAction::Yank, InputAction::EnterMode(Mode::Normal),]
        );
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        for c in "abc".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        state.handle_key(key(KeyCode::Backspace));
        assert_eq!(state.text(), "ab");
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn left_right_move_cursor_in_insert() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        for c in "abc".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        state.handle_key(key(KeyCode::Left));
        state.handle_key(key(KeyCode::Left));
        assert_eq!(state.cursor(), 1);
        state.handle_key(key(KeyCode::Char('X')));
        assert_eq!(state.text(), "aXbc");
    }

    #[test]
    fn home_and_end_jump_in_insert() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        for c in "abc".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        state.handle_key(key(KeyCode::Home));
        assert_eq!(state.cursor(), 0);
        state.handle_key(key(KeyCode::End));
        assert_eq!(state.cursor(), 3);
    }
}
