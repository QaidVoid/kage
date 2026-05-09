//! Modal input state machine: Normal / Insert.
//!
//! Vim-flavoured key handling: `i` enters insert, `Esc` returns to
//! normal, `j`/`k`/`gg`/`G` scroll the buffer, `y` copies the active
//! screen-cell selection, `/` starts a search, `:` starts a command.
//! Each key resolves to zero or more [`InputAction`]s the host applies
//! to its [`crate::Buffer`] / clipboard / command runner.
//!
//! The state machine is intentionally pure: input is a [`KeyEvent`],
//! output is a [`Vec<InputAction>`]. Side effects live in the host.
//!
//! Selection lives outside this state machine: the host paints a
//! cell-based screen overlay driven by mouse drag. `Esc` clears the
//! selection through [`InputAction::ClearSelection`].

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Editing mode of the input area.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    /// Vim-style normal mode: navigation, folding, mode entry.
    #[default]
    Normal,
    /// Free-text insert mode for the prompt input.
    Insert,
    /// Keyboard-driven cell selection. The host paints a screen
    /// overlay anchored at the position the user pressed `v`; this
    /// mode's keys (`h`/`j`/`k`/`l`, arrows, `y`, `Esc`) move the
    /// cursor end of that selection or finalise it.
    Visual,
}

/// Vim-style window focus: which "pane" Normal-mode keys (and the
/// hardware cursor) belong to. `Pane::Input` is the bordered input
/// card at the bottom of the screen; `Pane::Buffer` is the read-only
/// conversation buffer above it. `Ctrl-w` toggles between them, and
/// mouse clicks inside a region focus that pane.
///
/// The pane is purely a routing flag in Stage B (PR-2): keystrokes
/// behave the same regardless. Stage C (vim ops on the input pane)
/// uses it to decide whether `j`/`k` scrolls the buffer or moves the
/// input cursor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pane {
    /// The bordered input card. Default focus: text and cursor live
    /// here so the user can type immediately on startup.
    #[default]
    Input,
    /// The conversation buffer. Block focus and scroll act on this
    /// pane's content.
    Buffer,
}

impl Pane {
    /// Return the other pane. Stage B's `Ctrl-w` toggles using this.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Input => Self::Buffer,
            Self::Buffer => Self::Input,
        }
    }
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
    /// Yank the active screen-cell selection into the OSC52
    /// clipboard. The host walks its rendered cell grid to extract
    /// the text, so the copy matches what's painted (including
    /// across tool blocks).
    Yank,
    /// Drop the active screen-cell selection without copying.
    /// Triggered by `Esc` when the user wants to abandon a drag.
    ClearSelection,
    /// Cancel the in-flight turn.
    Cancel,
    /// Open the in-TUI model picker overlay.
    OpenModelPicker,
    /// Open the in-TUI session picker overlay so the user can resume
    /// an earlier conversation in place.
    OpenSessionPicker,
    /// Move focus to the previous (older) foldable block.
    FocusPrev,
    /// Move focus to the next (newer) foldable block.
    FocusNext,
    /// Open the slash command palette overlay (leading `/` typed in
    /// Insert mode against an empty prompt).
    OpenCommandPalette,
    /// Jump focus to the next block matching the active search.
    SearchNext,
    /// Jump focus to the previous block matching the active search.
    SearchPrev,
    /// Enter keyboard visual select. The host anchors a selection at
    /// a sensible position (the focused block's first visible row)
    /// and switches mode to [`Mode::Visual`].
    EnterVisual,
    /// Move the visual cursor by one column to the left.
    VisualLeft,
    /// Move the visual cursor by one column to the right.
    VisualRight,
    /// Move the visual cursor up by one row.
    VisualUp,
    /// Move the visual cursor down by one row.
    VisualDown,
    /// Snap the visual cursor to the start of the current row.
    VisualLineStart,
    /// Snap the visual cursor to the end of the current row.
    VisualLineEnd,
    /// Yank the entire content of the focused block (vim-style
    /// `Y`). Selection state is left alone.
    YankFocusedBlock,
    /// Toggle vim-style window focus between the input card and the
    /// conversation buffer (`Ctrl-w` in Normal mode).
    CyclePane,
    /// Force focus to a specific pane (e.g. mouse click in a
    /// particular region).
    FocusPane(Pane),
}

/// Cap on retained history entries. The host's persistence layer is
/// expected to truncate to the same bound when it serializes.
pub const HISTORY_MAX: usize = 1000;

/// Tracks the editing mode, the prompt text, the prompt history, and
/// any pending leader key (e.g. `g` waiting for the second `g` of `gg`).
#[derive(Debug, Default)]
pub struct InputState {
    mode: Mode,
    text: String,
    cursor: usize,
    pending: Option<char>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_stash: Option<String>,
    focused_pane: Pane,
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

    /// Currently focused vim-style pane. See [`Pane`] for semantics.
    #[must_use]
    pub fn focused_pane(&self) -> Pane {
        self.focused_pane
    }

    /// Force focus to `pane`. Returns whether the focus actually
    /// changed; callers can use that signal to redraw or emit a
    /// cursor-style escape only when necessary.
    pub fn set_focused_pane(&mut self, pane: Pane) -> bool {
        if self.focused_pane == pane {
            return false;
        }
        self.focused_pane = pane;
        true
    }

    /// Toggle the focused pane. Same return convention as
    /// [`Self::set_focused_pane`] (always `true`, since the toggle
    /// changes state by definition; the bool stays for symmetry with
    /// the setter so callers can use them interchangeably).
    pub fn toggle_focused_pane(&mut self) -> bool {
        self.focused_pane = self.focused_pane.opposite();
        true
    }

    /// Read-only slice of history entries, oldest first.
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Replace the in-memory history (e.g. when seeding from the
    /// persisted file at startup). Truncated to [`HISTORY_MAX`]
    /// entries, keeping the most recent.
    pub fn set_history(&mut self, entries: Vec<String>) {
        self.history = entries;
        if self.history.len() > HISTORY_MAX {
            let drop = self.history.len() - HISTORY_MAX;
            self.history.drain(..drop);
        }
        self.history_cursor = None;
        self.history_stash = None;
    }

    /// Append `entry` to the history, deduping against the most recent
    /// entry and skipping empty strings. Truncates to [`HISTORY_MAX`]
    /// from the front when full.
    pub fn push_history(&mut self, entry: &str) {
        if entry.is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == entry) {
            return;
        }
        self.history.push(entry.to_owned());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.history_stash = Some(self.text.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(idx) => idx - 1,
        };
        self.history_cursor = Some(next);
        self.text.clone_from(&self.history[next]);
        self.cursor = self.text.len();
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.history.len() {
            let next = idx + 1;
            self.history_cursor = Some(next);
            self.text.clone_from(&self.history[next]);
            self.cursor = self.text.len();
        } else {
            self.history_cursor = None;
            self.text = self.history_stash.take().unwrap_or_default();
            self.cursor = self.text.len();
        }
    }

    fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_stash = None;
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
            KeyCode::Char('v') => vec![InputAction::EnterVisual],
            // Lowercase `y` yanks an existing selection (mouse or
            // visual mode left it behind). Capital `Y` yanks the
            // focused block whole, vim's "yank line" gesture
            // adapted for our block-stream layout.
            KeyCode::Char('y') => vec![InputAction::Yank],
            KeyCode::Char('Y') => vec![InputAction::YankFocusedBlock],
            KeyCode::Esc => vec![InputAction::ClearSelection],
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
            KeyCode::Char('w') if ctrl => vec![InputAction::CyclePane],
            KeyCode::Char(':') => vec![InputAction::BeginCommand],
            KeyCode::Char('/') => vec![InputAction::BeginSearch],
            KeyCode::Char('c') if ctrl => vec![InputAction::Cancel],
            KeyCode::Char('p') if ctrl => vec![InputAction::OpenModelPicker],
            KeyCode::Char('r') if ctrl => vec![InputAction::OpenSessionPicker],
            KeyCode::Char('[') => vec![InputAction::FocusPrev],
            KeyCode::Char(']') => vec![InputAction::FocusNext],
            KeyCode::Char('n') => vec![InputAction::SearchNext],
            KeyCode::Char('N') => vec![InputAction::SearchPrev],
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
            KeyCode::Esc => {
                self.reset_history_navigation();
                self.enter_mode(Mode::Normal)
            }
            KeyCode::Enter => {
                // Many terminals (wezterm, ghostty in default config,
                // some xterm variants) transmit `Shift+Enter` as
                // `Esc<Enter>` which crossterm decodes as `Alt+Enter`.
                // Accept either modifier so the user-visible Shift+Enter
                // gesture inserts a newline regardless of which mapping
                // the terminal uses.
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.insert_char('\n');
                    Vec::new()
                } else if self.text.is_empty() {
                    Vec::new()
                } else {
                    let text = std::mem::take(&mut self.text);
                    self.cursor = 0;
                    self.push_history(&text);
                    self.reset_history_navigation();
                    let mut actions = vec![InputAction::Submit(text)];
                    actions.extend(self.enter_mode(Mode::Normal));
                    actions
                }
            }
            KeyCode::Up => {
                // Multi-line input: walk a row up first; only fall
                // through to history when the cursor is already on the
                // top row of the current draft.
                if !self.move_cursor_up() {
                    self.history_prev();
                }
                Vec::new()
            }
            KeyCode::Down => {
                if !self.move_cursor_down() {
                    self.history_next();
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                self.reset_history_navigation();
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
            KeyCode::Char('/') if self.text.is_empty() => {
                vec![InputAction::OpenCommandPalette]
            }
            KeyCode::Char(c) => {
                self.reset_history_navigation();
                self.insert_char(c);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_visual(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match key.code {
            KeyCode::Esc => {
                let mut actions = vec![InputAction::ClearSelection];
                actions.extend(self.enter_mode(Mode::Normal));
                actions
            }
            KeyCode::Char('y') => {
                let mut actions = vec![InputAction::Yank];
                actions.extend(self.enter_mode(Mode::Normal));
                actions
            }
            KeyCode::Char('h') | KeyCode::Left => vec![InputAction::VisualLeft],
            KeyCode::Char('l') | KeyCode::Right => vec![InputAction::VisualRight],
            KeyCode::Char('j') | KeyCode::Down => vec![InputAction::VisualDown],
            KeyCode::Char('k') | KeyCode::Up => vec![InputAction::VisualUp],
            KeyCode::Char('0') | KeyCode::Home => vec![InputAction::VisualLineStart],
            KeyCode::Char('$') | KeyCode::End => vec![InputAction::VisualLineEnd],
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

    /// Set the editing mode externally (e.g. mouse drag entering
    /// visual mode). Returns whether the mode actually changed.
    pub fn switch_mode(&mut self, mode: Mode) -> bool {
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        self.pending = None;
        true
    }

    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Insert pasted text at the cursor when in [`Mode::Insert`]. No-op
    /// in other modes so a stray paste in normal mode does not mutate
    /// the prompt. The paste is preserved verbatim, including newlines,
    /// so a multi-line paste does not auto-submit.
    pub fn paste(&mut self, text: &str) {
        if self.mode != Mode::Insert {
            return;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
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

    /// Move the cursor up one row inside the current text. Returns true
    /// if a move happened; false when the cursor is already on the top
    /// row (caller falls through to history navigation).
    fn move_cursor_up(&mut self) -> bool {
        let prefix = &self.text[..self.cursor];
        let Some(curr_line_start) = prefix.rfind('\n').map(|i| i + 1) else {
            return false;
        };
        let col_chars = self.text[curr_line_start..self.cursor].chars().count();
        let prev_line_end = curr_line_start - 1;
        let prev_line_start = self.text[..prev_line_end].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = byte_offset_at_column(&self.text[prev_line_start..prev_line_end], col_chars)
            + prev_line_start;
        true
    }

    /// Move the cursor down one row inside the current text. Returns
    /// true if a move happened; false when the cursor is on the last
    /// row (caller falls through to history navigation).
    fn move_cursor_down(&mut self) -> bool {
        let curr_line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let col_chars = self.text[curr_line_start..self.cursor].chars().count();
        let curr_line_end = self.text[self.cursor..].find('\n').map(|i| self.cursor + i);
        let Some(end) = curr_line_end else {
            return false;
        };
        let next_line_start = end + 1;
        let next_line_end = self.text[next_line_start..]
            .find('\n')
            .map_or(self.text.len(), |i| next_line_start + i);
        self.cursor = byte_offset_at_column(&self.text[next_line_start..next_line_end], col_chars)
            + next_line_start;
        true
    }
}

/// Byte offset on `line` where the visual column `col` falls. Clamps to
/// the line's length when the column is past the end.
fn byte_offset_at_column(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map_or(line.len(), |(idx, _)| idx)
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

    fn alt_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
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
    fn alt_enter_also_inserts_a_newline_for_terminals_that_remap_shift_enter() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.handle_key(key(KeyCode::Char('a')));
        state.handle_key(alt_enter());
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
    fn normal_y_emits_yank() {
        let mut state = InputState::new();
        let acts = state.handle_key(key(KeyCode::Char('y')));
        assert_eq!(acts, vec![InputAction::Yank]);
    }

    #[test]
    fn normal_esc_emits_clear_selection() {
        let mut state = InputState::new();
        let acts = state.handle_key(key(KeyCode::Esc));
        assert_eq!(acts, vec![InputAction::ClearSelection]);
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
    fn paste_in_insert_inserts_verbatim_with_newlines() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.handle_key(key(KeyCode::Char('a')));
        state.paste("multi\nline\npaste");
        state.handle_key(key(KeyCode::Char('z')));
        assert_eq!(state.text(), "amulti\nline\npastez");
    }

    #[test]
    fn submit_pushes_text_into_history_skipping_dupes() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        for c in "foo".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        state.handle_key(key(KeyCode::Enter));
        // Re-enter, type the same prompt again.
        state.handle_key(key(KeyCode::Char('i')));
        for c in "foo".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        state.handle_key(key(KeyCode::Enter));
        assert_eq!(state.history(), &["foo".to_owned()]);
    }

    #[test]
    fn up_in_insert_walks_back_through_history() {
        let mut state = InputState::new();
        state.set_history(vec!["alpha".into(), "beta".into(), "gamma".into()]);
        state.handle_key(key(KeyCode::Char('i')));
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "gamma");
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "beta");
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "alpha");
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "alpha", "stops at oldest");
    }

    #[test]
    fn down_in_insert_returns_to_stashed_draft() {
        let mut state = InputState::new();
        state.set_history(vec!["one".into(), "two".into()]);
        state.handle_key(key(KeyCode::Char('i')));
        for c in "draft".chars() {
            state.handle_key(key(KeyCode::Char(c)));
        }
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "two");
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.text(), "draft");
    }

    #[test]
    fn up_in_multiline_input_moves_cursor_within_text_first() {
        let mut state = InputState::new();
        state.set_history(vec!["history-entry".into()]);
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("first\nsecond");
        // Cursor is at end of "second" (col 6). Up should land on row 0.
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "first\nsecond", "text untouched");
        // Now another Up — already on row 0, should walk history.
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "history-entry");
    }

    #[test]
    fn down_in_multiline_input_moves_cursor_before_walking_history() {
        let mut state = InputState::new();
        state.set_history(vec!["older".into(), "newer".into()]);
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("aa\nbb");
        // Move cursor to start of first row.
        state.handle_key(key(KeyCode::Home));
        for _ in 0..5 {
            // Reach top row, col 0 — but we already are there after Home.
        }
        // Down — moves cursor to row 1, no history walk.
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.text(), "aa\nbb");
        // Down again — last row reached, history walks forward (no
        // cursor was set up by a prior Up so this is a no-op).
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.text(), "aa\nbb");
    }

    #[test]
    fn up_clamps_column_to_shorter_previous_row() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hi\nlonger-line");
        // Cursor at end of "longer-line" (col 11). Previous row is "hi"
        // (2 chars). Up should land at the end of "hi" (col 2 = byte 2).
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn typing_after_history_walk_resets_navigation() {
        let mut state = InputState::new();
        state.set_history(vec!["a".into(), "b".into()]);
        state.handle_key(key(KeyCode::Char('i')));
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.text(), "b");
        state.handle_key(key(KeyCode::Char('z')));
        assert_eq!(state.text(), "bz");
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.text(), "bz", "down with no cursor is a no-op");
    }

    #[test]
    fn set_history_truncates_to_cap() {
        let mut state = InputState::new();
        let entries: Vec<String> = (0..(HISTORY_MAX + 5)).map(|i| format!("e{i}")).collect();
        state.set_history(entries);
        assert_eq!(state.history().len(), HISTORY_MAX);
        assert_eq!(state.history().first().unwrap(), "e5");
    }

    #[test]
    fn paste_in_normal_is_ignored() {
        let mut state = InputState::new();
        state.paste("oops");
        assert_eq!(state.text(), "");
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

    #[test]
    fn default_pane_focus_is_input() {
        let state = InputState::new();
        assert_eq!(state.focused_pane(), Pane::Input);
    }

    #[test]
    fn ctrl_w_in_normal_emits_cycle_pane() {
        let mut state = InputState::new();
        let acts = state.handle_key(ctrl('w'));
        assert_eq!(acts, vec![InputAction::CyclePane]);
    }

    #[test]
    fn toggle_focused_pane_round_trips() {
        let mut state = InputState::new();
        assert!(state.toggle_focused_pane());
        assert_eq!(state.focused_pane(), Pane::Buffer);
        assert!(state.toggle_focused_pane());
        assert_eq!(state.focused_pane(), Pane::Input);
    }

    #[test]
    fn set_focused_pane_reports_changes() {
        let mut state = InputState::new();
        assert!(!state.set_focused_pane(Pane::Input)); // no change
        assert!(state.set_focused_pane(Pane::Buffer));
        assert!(!state.set_focused_pane(Pane::Buffer)); // no change
    }
}
