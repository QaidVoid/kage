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

/// Vim-style operator pending after `d`, `c`, or `y`. Combines with
/// a motion or a doubled key (`dd`, `cc`, `yy`) to act on a range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operator {
    /// `d` - delete the range, save to register.
    Delete,
    /// `c` - delete the range, save to register, enter Insert.
    Change,
    /// `y` - copy the range to register, leave cursor and text.
    Yank,
}

impl Operator {
    fn from_key(c: char) -> Option<Self> {
        match c {
            'd' => Some(Self::Delete),
            'c' => Some(Self::Change),
            'y' => Some(Self::Yank),
            _ => None,
        }
    }

    fn double_key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Change => 'c',
            Self::Yank => 'y',
        }
    }
}

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
    /// Vim operator awaiting a motion or doubled key. When set, the
    /// next keystroke either resolves the operator (motion / linewise
    /// `dd`-style / Esc cancel) or extends the count.
    pending_op: Option<Operator>,
    /// Count accumulated from leading digits before an operator or
    /// motion. `Some(3)` after pressing `3`, `Some(15)` after `15`.
    /// Multiplies whatever follows; reset after the action runs.
    pending_count: Option<usize>,
    /// `true` after `r` was pressed; the next character literally
    /// replaces the char at the cursor.
    awaiting_replace: bool,
    /// Last yanked / cut text. Inserted by `p` / `P` (Stage C.4).
    register: String,
    /// `true` when `register` was filled by a linewise op (`dd`, `yy`,
    /// etc.), so `p` pastes on a new line below the cursor instead of
    /// inserting inline.
    register_linewise: bool,
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
        // Awaiting `r{ch}` replacement: the next char literally
        // replaces the char at the cursor. Esc cancels.
        if self.awaiting_replace {
            self.awaiting_replace = false;
            if matches!(key.code, KeyCode::Esc) {
                return vec![InputAction::ClearSelection];
            }
            if let KeyCode::Char(c) = key.code {
                self.replace_char_at_cursor(c);
            }
            return Vec::new();
        }

        if let Some(prev) = self.pending.take() {
            return self.handle_pending(prev, key);
        }

        // Operator pending (`d`, `c`, `y`): the next key is either a
        // motion, a doubled operator key for linewise, a count
        // multiplier, or Esc.
        if self.pending_op.is_some() {
            return self.handle_op_pending(key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Cross-pane keys: behaviour is identical regardless of which
        // pane has window focus.
        match key.code {
            KeyCode::Esc => {
                self.pending_count = None;
                return vec![InputAction::ClearSelection];
            }
            KeyCode::Char(':') => return vec![InputAction::BeginCommand],
            KeyCode::Char('/') => return vec![InputAction::BeginSearch],
            KeyCode::Char('o') if ctrl => return vec![InputAction::ToggleFold],
            KeyCode::Char('w') if ctrl => return vec![InputAction::CyclePane],
            KeyCode::Char('c') if ctrl => return vec![InputAction::Cancel],
            KeyCode::Char('p') if ctrl => return vec![InputAction::OpenModelPicker],
            KeyCode::Char('r') if ctrl => return vec![InputAction::OpenSessionPicker],
            KeyCode::Char('[') => return vec![InputAction::FocusPrev],
            KeyCode::Char(']') => return vec![InputAction::FocusNext],
            KeyCode::Char('n') => return vec![InputAction::SearchNext],
            KeyCode::Char('N') => return vec![InputAction::SearchPrev],
            KeyCode::Char('g') => {
                self.pending = Some('g');
                return Vec::new();
            }
            KeyCode::Char('z') => {
                self.pending = Some('z');
                return Vec::new();
            }
            _ => {}
        }

        match self.focused_pane {
            Pane::Buffer => self.handle_normal_buffer(key),
            Pane::Input => self.handle_normal_input(key),
        }
    }

    /// Operator pending: `d`/`c`/`y` was pressed and the next key
    /// must complete the action. Handles the doubled-key linewise
    /// case (`dd`/`cc`/`yy`), motion-driven ranges (`dw`, `c$`,
    /// `yh`), digit counts (`d3w`), and Esc cancel.
    fn handle_op_pending(&mut self, key: KeyEvent) -> Vec<InputAction> {
        let Some(op) = self.pending_op else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Esc => {
                self.pending_op = None;
                self.pending_count = None;
                return vec![InputAction::ClearSelection];
            }
            // Counts after the operator: `d3w` etc. Multiply the
            // existing pre-operator count by the post-operator one.
            KeyCode::Char(c @ '0'..='9') => {
                if c == '0' && self.pending_count.is_none() {
                    // `d0` is "delete to line start", not "count 0".
                } else {
                    self.accumulate_count(c);
                    return Vec::new();
                }
            }
            KeyCode::Char(c) if c == op.double_key() => {
                let count = self.pending_count.take().unwrap_or(1);
                self.pending_op = None;
                return self.apply_op_linewise(op, count);
            }
            _ => {}
        }
        // Try resolving as a motion key.
        let count = self.pending_count.take().unwrap_or(1);
        if let KeyCode::Char(motion_key) = key.code
            && let Some(range) = self.motion_operator_range(motion_key, count)
        {
            self.pending_op = None;
            return self.apply_op_charwise(op, range);
        }
        // Unrecognised key cancels the operator (vim convention).
        self.pending_op = None;
        Vec::new()
    }

    /// Normal-mode keys that act on the conversation buffer (scroll,
    /// fold, yank-selection, enter buffer-cell visual). Insert-mode
    /// entry from here auto-switches focus to the input pane so the
    /// user lands in a typable card.
    fn handle_normal_buffer(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match key.code {
            KeyCode::Char('i' | 'a') => {
                self.focused_pane = Pane::Input;
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('v') => vec![InputAction::EnterVisual],
            // Lowercase `y` yanks an existing selection (mouse or
            // visual mode left it behind). Capital `Y` yanks the
            // focused block whole, vim's "yank line" gesture adapted
            // for our block-stream layout.
            KeyCode::Char('y') => vec![InputAction::Yank],
            KeyCode::Char('Y') => vec![InputAction::YankFocusedBlock],
            KeyCode::Char('j') | KeyCode::Down => vec![InputAction::Scroll(1)],
            KeyCode::Char('k') | KeyCode::Up => vec![InputAction::Scroll(-1)],
            KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right => {
                vec![InputAction::Scroll(0)]
            }
            KeyCode::PageDown => vec![InputAction::Scroll(10)],
            KeyCode::PageUp => vec![InputAction::Scroll(-10)],
            KeyCode::Char('G') => vec![InputAction::ScrollToBottom],
            _ => Vec::new(),
        }
    }

    /// Normal-mode keys that act on the input card: vim-style motions
    /// (`h`/`l`/`0`/`$`/`^`/`w`/`b`/`e`/`j`/`k`/`G`), single-char
    /// edits (`x`/`X`/`r`/`D`/`C`/`Y`), operator entry (`d`/`c`/`y`
    /// followed by a motion or doubled key), the count prefix
    /// (`3dw`, `5j`), and the insert-entry variants
    /// (`i`/`a`/`I`/`A`/`o`/`O`). Cursor movement and edits mutate
    /// state in place; mode transitions return an [`InputAction`]
    /// for the host to react to.
    #[allow(clippy::too_many_lines)]
    fn handle_normal_input(&mut self, key: KeyEvent) -> Vec<InputAction> {
        // Count prefix: digits 1-9 always, `0` only after a count has
        // already started (otherwise `0` is the "go to line start"
        // motion).
        if let KeyCode::Char(c @ '0'..='9') = key.code {
            if !(c == '0' && self.pending_count.is_none()) {
                self.accumulate_count(c);
                return Vec::new();
            }
        }

        // Operator entry: stash the operator and wait for a motion or
        // doubled key. Counts that came before stay in
        // `pending_count`; they multiply with whatever follows.
        if let KeyCode::Char(c) = key.code
            && let Some(op) = Operator::from_key(c)
        {
            self.pending_op = Some(op);
            return Vec::new();
        }

        // `r{ch}` replace: stash a flag; the next keystroke is the
        // literal replacement char.
        if matches!(key.code, KeyCode::Char('r')) {
            self.awaiting_replace = true;
            self.pending_count = None;
            return Vec::new();
        }

        let count = self.pending_count.take().unwrap_or(1);

        match key.code {
            // Insert-entry variants. Vim doesn't repeat insert-entry
            // by count in a meaningful way for our use, so we drop
            // the count silently.
            KeyCode::Char('i') => self.enter_mode(Mode::Insert),
            KeyCode::Char('a') => {
                if let Some((_, w)) = char_at(&self.text, self.cursor) {
                    self.cursor += w;
                }
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('I') => {
                let start = current_line_start(&self.text, self.cursor);
                self.cursor = first_non_whitespace_at(&self.text, start);
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('A') => {
                self.cursor = current_line_end(&self.text, self.cursor);
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('o') => {
                let end = current_line_end(&self.text, self.cursor);
                self.text.insert(end, '\n');
                self.cursor = end + 1;
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('O') => {
                let start = current_line_start(&self.text, self.cursor);
                self.text.insert(start, '\n');
                self.cursor = start;
                self.enter_mode(Mode::Insert)
            }
            // Single-char edits.
            KeyCode::Char('x') => {
                for _ in 0..count {
                    self.delete_char_at_cursor();
                }
                Vec::new()
            }
            KeyCode::Char('X') => {
                for _ in 0..count {
                    self.backspace();
                }
                Vec::new()
            }
            // Vim's line-shorthand operators.
            KeyCode::Char('D') => self.apply_op_charwise(
                Operator::Delete,
                (self.cursor, current_line_end(&self.text, self.cursor)),
            ),
            KeyCode::Char('C') => self.apply_op_charwise(
                Operator::Change,
                (self.cursor, current_line_end(&self.text, self.cursor)),
            ),
            KeyCode::Char('Y') => self.apply_op_linewise(Operator::Yank, count),
            // Charwise motions (cursor movement only).
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor =
                    self.cursor_after_char_move(-i32::try_from(count).unwrap_or(i32::MAX));
                Vec::new()
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor = self.cursor_after_char_move(i32::try_from(count).unwrap_or(i32::MAX));
                Vec::new()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                for _ in 0..count {
                    if !self.move_cursor_down() {
                        break;
                    }
                }
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                for _ in 0..count {
                    if !self.move_cursor_up() {
                        break;
                    }
                }
                Vec::new()
            }
            KeyCode::Char('0') | KeyCode::Home => {
                self.cursor = current_line_start(&self.text, self.cursor);
                Vec::new()
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.cursor = current_line_end(&self.text, self.cursor);
                Vec::new()
            }
            KeyCode::Char('^') => {
                let start = current_line_start(&self.text, self.cursor);
                self.cursor = first_non_whitespace_at(&self.text, start);
                Vec::new()
            }
            KeyCode::Char('w') => {
                for _ in 0..count {
                    self.cursor = vim_word_forward(&self.text, self.cursor);
                }
                Vec::new()
            }
            KeyCode::Char('b') => {
                for _ in 0..count {
                    self.cursor = backward_word_start(&self.text, self.cursor);
                }
                Vec::new()
            }
            KeyCode::Char('e') => {
                for _ in 0..count {
                    self.cursor = vim_word_end(&self.text, self.cursor);
                }
                Vec::new()
            }
            KeyCode::Char('G') => {
                self.cursor = self.text.len();
                Vec::new()
            }
            KeyCode::Char('v') => vec![InputAction::EnterVisual],
            KeyCode::PageDown => vec![InputAction::Scroll(10)],
            KeyCode::PageUp => vec![InputAction::Scroll(-10)],
            _ => Vec::new(),
        }
    }

    /// Walk the count digit `c` into [`Self::pending_count`]. Caps
    /// the accumulator at `usize::MAX / 10` so absurd input can't
    /// overflow.
    fn accumulate_count(&mut self, c: char) {
        let digit = (c as u32).saturating_sub('0' as u32) as usize;
        let next = self
            .pending_count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit);
        self.pending_count = Some(next);
    }

    /// Replace the char at the cursor with `c` (vim's `r{ch}`). If
    /// the cursor sits past the last char (empty input or end of
    /// line), no-op rather than appending - matches vim's refusal to
    /// extend a line on `r`.
    fn replace_char_at_cursor(&mut self, c: char) {
        let Some((_, w)) = char_at(&self.text, self.cursor) else {
            return;
        };
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.replace_range(self.cursor..self.cursor + w, s);
    }

    /// Cursor target after moving `delta` chars, clamped to text
    /// bounds. Negative `delta` walks left.
    fn cursor_after_char_move(&self, delta: i32) -> usize {
        let mut pos = self.cursor;
        if delta >= 0 {
            for _ in 0..delta {
                let Some((_, w)) = char_at(&self.text, pos) else {
                    break;
                };
                pos += w;
            }
        } else {
            for _ in 0..(-delta) {
                let Some((_, w)) = prev_char(&self.text, pos) else {
                    break;
                };
                pos -= w;
            }
        }
        pos
    }

    /// Compute the byte range an operator (`d`/`c`/`y`) consumes for
    /// motion `motion_key` applied `count` times. Returns
    /// `(start, end)` with `start <= end`. Charwise motions only;
    /// linewise (`j`/`k` with operator) is handled separately. `e`
    /// is inclusive (range extends one char past the word's end).
    fn motion_operator_range(&self, motion_key: char, count: usize) -> Option<(usize, usize)> {
        let count = count.max(1);
        let target: usize = match motion_key {
            'h' => self.cursor_after_char_move(-i32::try_from(count).unwrap_or(i32::MAX)),
            'l' => self.cursor_after_char_move(i32::try_from(count).unwrap_or(i32::MAX)),
            '0' => current_line_start(&self.text, self.cursor),
            '$' => current_line_end(&self.text, self.cursor),
            '^' => {
                let s = current_line_start(&self.text, self.cursor);
                first_non_whitespace_at(&self.text, s)
            }
            'w' => {
                let mut p = self.cursor;
                for _ in 0..count {
                    p = vim_word_forward(&self.text, p);
                }
                p
            }
            'b' => {
                let mut p = self.cursor;
                for _ in 0..count {
                    p = backward_word_start(&self.text, p);
                }
                p
            }
            'e' => {
                let mut p = self.cursor;
                for _ in 0..count {
                    p = vim_word_end(&self.text, p);
                }
                if let Some((_, w)) = char_at(&self.text, p) {
                    p + w
                } else {
                    p
                }
            }
            'G' => self.text.len(),
            _ => return None,
        };
        let range = if self.cursor <= target {
            (self.cursor, target)
        } else {
            (target, self.cursor)
        };
        Some(range)
    }

    /// Apply a charwise operator on `range`. Saves the consumed text
    /// to the register and updates cursor / text per op semantics.
    fn apply_op_charwise(&mut self, op: Operator, range: (usize, usize)) -> Vec<InputAction> {
        let (s, e) = range;
        if s >= e || e > self.text.len() {
            return Vec::new();
        }
        self.register = self.text[s..e].to_string();
        self.register_linewise = false;
        match op {
            Operator::Yank => Vec::new(),
            Operator::Delete => {
                self.text.drain(s..e);
                self.cursor = s;
                Vec::new()
            }
            Operator::Change => {
                self.text.drain(s..e);
                self.cursor = s;
                self.enter_mode(Mode::Insert)
            }
        }
    }

    /// Apply a linewise operator covering `count` lines starting from
    /// the cursor's line. `dd` removes the line and its trailing
    /// newline, `yy` only copies, `cc` removes the line content but
    /// preserves the surrounding newline structure and enters Insert.
    fn apply_op_linewise(&mut self, op: Operator, count: usize) -> Vec<InputAction> {
        let count = count.max(1);
        let line_start = current_line_start(&self.text, self.cursor);
        let mut end = line_start;
        for i in 0..count {
            let content_end = current_line_end(&self.text, end);
            end = if matches!(op, Operator::Change) && i == count - 1 {
                content_end
            } else if content_end < self.text.len() {
                content_end + 1
            } else {
                content_end
            };
        }
        self.register = self.text[line_start..end].to_string();
        self.register_linewise = true;
        match op {
            Operator::Yank => Vec::new(),
            Operator::Delete => {
                self.text.drain(line_start..end);
                self.cursor = line_start.min(self.text.len());
                Vec::new()
            }
            Operator::Change => {
                self.text.drain(line_start..end);
                self.cursor = line_start;
                self.enter_mode(Mode::Insert)
            }
        }
    }

    fn handle_pending(&mut self, prev: char, key: KeyEvent) -> Vec<InputAction> {
        match (prev, key.code) {
            ('g', KeyCode::Char('g')) => match self.focused_pane {
                Pane::Buffer => vec![InputAction::ScrollToTop],
                Pane::Input => {
                    self.cursor = 0;
                    Vec::new()
                }
            },
            // `gw` is an ergonomic alternative to `<C-w>` for users
            // who'd rather not press a modifier; both toggle pane.
            ('g', KeyCode::Char('w')) => vec![InputAction::CyclePane],
            ('z', KeyCode::Char('o' | 'c')) => vec![InputAction::ToggleFold],
            ('z', KeyCode::Char('R')) => vec![InputAction::UnfoldAll],
            ('z', KeyCode::Char('M')) => vec![InputAction::FoldAll],
            _ => Vec::new(),
        }
    }

    /// Vim-style `x`: delete the char at the cursor. If the deletion
    /// leaves the cursor past the end of its line, snap it to the
    /// last char on that line (vim convention).
    fn delete_char_at_cursor(&mut self) {
        let Some((c, w)) = char_at(&self.text, self.cursor) else {
            return;
        };
        if c == '\n' {
            // Vim's `x` does not eat newlines; ignore.
            return;
        }
        self.text.drain(self.cursor..self.cursor + w);
        let line_end = current_line_end(&self.text, self.cursor);
        let line_start = current_line_start(&self.text, self.cursor);
        if self.cursor > line_end {
            self.cursor = line_end;
        }
        if self.cursor == line_end && line_end > line_start {
            if let Some((_, pw)) = prev_char(&self.text, self.cursor) {
                self.cursor -= pw;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_insert(&mut self, key: KeyEvent) -> Vec<InputAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Readline / Emacs-style word and line edits. Match shells
        // (bash, zsh, fish): Ctrl+W deletes back to whitespace
        // ("unix-word-rubout"), Alt+Backspace deletes back to the
        // previous alphanumeric boundary ("backward-kill-word"),
        // Alt+d deletes forward, Alt+b/Alt+f move by word, and
        // Ctrl+a/e/u/k operate on the current visual line.
        if ctrl && !alt {
            match key.code {
                KeyCode::Char('w') => {
                    self.reset_history_navigation();
                    let to = unix_word_rubout_start(&self.text, self.cursor);
                    self.delete_range(to, self.cursor);
                    return Vec::new();
                }
                KeyCode::Char('a') => {
                    self.cursor = current_line_start(&self.text, self.cursor);
                    return Vec::new();
                }
                KeyCode::Char('e') => {
                    self.cursor = current_line_end(&self.text, self.cursor);
                    return Vec::new();
                }
                KeyCode::Char('u') => {
                    self.reset_history_navigation();
                    let start = current_line_start(&self.text, self.cursor);
                    self.delete_range(start, self.cursor);
                    return Vec::new();
                }
                KeyCode::Char('k') => {
                    self.reset_history_navigation();
                    let end = current_line_end(&self.text, self.cursor);
                    self.delete_range(self.cursor, end);
                    return Vec::new();
                }
                _ => {}
            }
        }
        if alt && !ctrl {
            match key.code {
                KeyCode::Backspace => {
                    self.reset_history_navigation();
                    let to = backward_word_start(&self.text, self.cursor);
                    self.delete_range(to, self.cursor);
                    return Vec::new();
                }
                KeyCode::Delete | KeyCode::Char('d') => {
                    self.reset_history_navigation();
                    let to = forward_word_end(&self.text, self.cursor);
                    self.delete_range(self.cursor, to);
                    return Vec::new();
                }
                KeyCode::Char('b') | KeyCode::Left => {
                    self.cursor = backward_word_start(&self.text, self.cursor);
                    return Vec::new();
                }
                KeyCode::Char('f') | KeyCode::Right => {
                    self.cursor = forward_word_end(&self.text, self.cursor);
                    return Vec::new();
                }
                _ => {}
            }
        }

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

    /// Remove `text[start..end]` and clamp the cursor to the deletion
    /// point. Used by the Emacs-style edits in [`Self::handle_insert`].
    fn delete_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.text.len() {
            return;
        }
        self.text.drain(start..end);
        if self.cursor >= end {
            self.cursor -= end - start;
        } else if self.cursor > start {
            self.cursor = start;
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

/// True for "word constituent" chars in the readline / Emacs sense:
/// alphanumerics plus underscore. Word boundary motions (`Alt+b`,
/// `Alt+f`) and `Alt+Backspace` / `Alt+d` use this.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Char immediately before byte position `byte`, plus its UTF-8 width.
/// `None` when `byte == 0`.
fn prev_char(text: &str, byte: usize) -> Option<(char, usize)> {
    text[..byte].chars().next_back().map(|c| (c, c.len_utf8()))
}

/// Char at byte position `byte`, plus its UTF-8 width. `None` at
/// end-of-text.
fn char_at(text: &str, byte: usize) -> Option<(char, usize)> {
    text[byte..].chars().next().map(|c| (c, c.len_utf8()))
}

/// Position of the start of the word containing or immediately
/// preceding `cursor`. Walks left over non-word chars first, then
/// over word chars, mirroring readline's `backward-word`.
pub(crate) fn backward_word_start(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if is_word_char(c) {
            break;
        }
        i -= w;
    }
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if !is_word_char(c) {
            break;
        }
        i -= w;
    }
    i
}

/// Position one past the end of the word containing or immediately
/// following `cursor`. Walks right over non-word chars first, then
/// over word chars, mirroring readline's `forward-word`.
pub(crate) fn forward_word_end(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while let Some((c, w)) = char_at(text, i) {
        if is_word_char(c) {
            break;
        }
        i += w;
    }
    while let Some((c, w)) = char_at(text, i) {
        if !is_word_char(c) {
            break;
        }
        i += w;
    }
    i
}

/// Position of the start of the run of non-whitespace immediately
/// before `cursor`, mirroring readline's `unix-word-rubout` (the one
/// `Ctrl+W` uses in shells: it splits on whitespace only, so
/// `foo-bar` is one word).
pub(crate) fn unix_word_rubout_start(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if !c.is_whitespace() {
            break;
        }
        i -= w;
    }
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if c.is_whitespace() {
            break;
        }
        i -= w;
    }
    i
}

/// Byte offset of the start of the visual line (between newlines)
/// containing `cursor`.
pub(crate) fn current_line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte offset of the end of the visual line (between newlines)
/// containing `cursor`. The newline itself is not included.
pub(crate) fn current_line_end(text: &str, cursor: usize) -> usize {
    text[cursor..].find('\n').map_or(text.len(), |i| cursor + i)
}

/// Byte offset of the first non-whitespace char at or after `start`,
/// staying on the same logical line. Used by vim's `^` motion.
pub(crate) fn first_non_whitespace_at(text: &str, start: usize) -> usize {
    let mut i = start;
    while let Some((c, w)) = char_at(text, i) {
        if c == '\n' || !c.is_whitespace() {
            return i;
        }
        i += w;
    }
    i
}

/// Vim-style `w`: forward to the start of the next word. From the
/// cursor's current word, skip remaining word chars, then skip
/// non-word chars (whitespace and punctuation), and land on the
/// first char of the next word.
pub(crate) fn vim_word_forward(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while let Some((c, w)) = char_at(text, i) {
        if !is_word_char(c) {
            break;
        }
        i += w;
    }
    while let Some((c, w)) = char_at(text, i) {
        if is_word_char(c) {
            break;
        }
        i += w;
    }
    i
}

/// Vim-style `e`: forward to the *end* of the current word. Vim
/// places the cursor on the last char of the word (not after it),
/// so the byte offset returned is the last word-char's start, not
/// the position past it.
pub(crate) fn vim_word_end(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return cursor;
    }
    let mut i = cursor;
    if let Some((_, w)) = char_at(text, i) {
        i += w;
    }
    while let Some((c, w)) = char_at(text, i) {
        if is_word_char(c) {
            break;
        }
        i += w;
    }
    let mut last_word_pos = i;
    while let Some((c, w)) = char_at(text, i) {
        if !is_word_char(c) {
            break;
        }
        last_word_pos = i;
        i += w;
    }
    last_word_pos
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
    fn jk_scroll_in_normal_when_buffer_focused() {
        // Stage C made j/k pane-aware: they only emit Scroll(...)
        // when the buffer pane has window focus. With default Input
        // focus they move the input cursor instead.
        let mut state = InputState::new();
        state.set_focused_pane(Pane::Buffer);
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
    fn jk_move_input_cursor_when_input_focused() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("first\nsecond");
        state.handle_key(key(KeyCode::Esc));
        // Cursor at end of "second" (byte 12). j is no-op (no line
        // below); k goes up one row at the same column.
        assert!(state.handle_key(key(KeyCode::Char('j'))).is_empty());
        assert_eq!(state.cursor(), 12);
        let acts = state.handle_key(key(KeyCode::Char('k')));
        assert!(acts.is_empty());
        assert_eq!(state.cursor(), 5);
    }

    #[test]
    fn gg_scrolls_to_top_when_buffer_focused() {
        let mut state = InputState::new();
        state.set_focused_pane(Pane::Buffer);
        let first = state.handle_key(key(KeyCode::Char('g')));
        assert!(first.is_empty());
        assert!(state.has_pending());
        let second = state.handle_key(key(KeyCode::Char('g')));
        assert_eq!(second, vec![InputAction::ScrollToTop]);
        assert!(!state.has_pending());
    }

    #[test]
    fn gg_jumps_input_cursor_to_start_when_input_focused() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello");
        state.handle_key(key(KeyCode::Esc));
        assert_eq!(state.cursor(), 5);
        state.handle_key(key(KeyCode::Char('g')));
        let acts = state.handle_key(key(KeyCode::Char('g')));
        assert!(acts.is_empty());
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn capital_g_scrolls_to_bottom_when_buffer_focused() {
        let mut state = InputState::new();
        state.set_focused_pane(Pane::Buffer);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('G'))),
            vec![InputAction::ScrollToBottom]
        );
    }

    #[test]
    fn capital_g_jumps_input_cursor_to_end_when_input_focused() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello world");
        state.handle_key(key(KeyCode::Esc));
        // After Esc cursor moves to byte 11 (end of "world"); but
        // vim's `G` jumps to text end regardless. Verify by moving
        // back first.
        state.handle_key(key(KeyCode::Char('h')));
        assert!(state.cursor() < 11);
        state.handle_key(key(KeyCode::Char('G')));
        assert_eq!(state.cursor(), 11);
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
    fn normal_y_emits_yank_when_buffer_focused() {
        // Stage C.2 made `y` an operator-entry in the input pane;
        // buffer-pane keeps the original "yank current selection"
        // semantics.
        let mut state = InputState::new();
        state.set_focused_pane(Pane::Buffer);
        let acts = state.handle_key(key(KeyCode::Char('y')));
        assert_eq!(acts, vec![InputAction::Yank]);
    }

    #[test]
    fn normal_y_in_input_pane_starts_yank_operator() {
        let mut state = InputState::new();
        let acts = state.handle_key(key(KeyCode::Char('y')));
        assert!(acts.is_empty());
        assert!(state.pending_op.is_some());
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

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn alt_code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn alt_backspace_kills_word_back_in_insert() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello world");
        state.handle_key(alt_code(KeyCode::Backspace));
        assert_eq!(state.text(), "hello ");
        assert_eq!(state.cursor(), 6);
        // Again deletes the trailing space and the prior word.
        state.handle_key(alt_code(KeyCode::Backspace));
        assert_eq!(state.text(), "");
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn ctrl_w_kills_back_to_whitespace() {
        // unix-word-rubout: hyphen is part of the word, only spaces
        // separate.
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo-bar baz");
        state.handle_key(ctrl('w'));
        assert_eq!(state.text(), "foo-bar ");
        state.handle_key(ctrl('w'));
        assert_eq!(state.text(), "");
    }

    #[test]
    fn alt_d_kills_word_forward() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello world");
        // Move cursor to start.
        for _ in 0..11 {
            state.handle_key(key(KeyCode::Left));
        }
        state.handle_key(alt('d'));
        assert_eq!(state.text(), " world");
    }

    #[test]
    fn alt_b_alt_f_navigate_words() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo bar baz");
        // Cursor at end. Alt+b lands at start of "baz".
        state.handle_key(alt('b'));
        assert_eq!(state.cursor(), 8);
        state.handle_key(alt('b'));
        assert_eq!(state.cursor(), 4);
        state.handle_key(alt('b'));
        assert_eq!(state.cursor(), 0);
        state.handle_key(alt('f'));
        assert_eq!(state.cursor(), 3);
        state.handle_key(alt('f'));
        assert_eq!(state.cursor(), 7);
    }

    #[test]
    fn ctrl_a_e_jump_to_line_edges() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("first\nsecond line");
        // Cursor at end. Ctrl+a -> start of current line ("second...").
        state.handle_key(ctrl('a'));
        assert_eq!(state.cursor(), 6);
        state.handle_key(ctrl('e'));
        assert_eq!(state.cursor(), state.text().len());
    }

    #[test]
    fn ctrl_u_deletes_to_line_start() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("first\nsecond line");
        state.handle_key(ctrl('u'));
        assert_eq!(state.text(), "first\n");
        assert_eq!(state.cursor(), 6);
    }

    #[test]
    fn h_l_move_cursor_in_input_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("abcd");
        state.handle_key(key(KeyCode::Esc));
        // Esc keeps cursor where it was. Move left twice with `h`.
        state.handle_key(key(KeyCode::Char('h')));
        state.handle_key(key(KeyCode::Char('h')));
        assert_eq!(state.cursor(), 2);
        state.handle_key(key(KeyCode::Char('l')));
        assert_eq!(state.cursor(), 3);
    }

    #[test]
    fn dollar_zero_caret_jump_to_line_edges_in_input_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("  hello world");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        assert_eq!(state.cursor(), 0);
        state.handle_key(key(KeyCode::Char('^')));
        assert_eq!(state.cursor(), 2);
        state.handle_key(key(KeyCode::Char('$')));
        assert_eq!(state.cursor(), state.text().len());
    }

    #[test]
    fn vim_w_b_e_word_motions_in_input_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo bar baz");
        state.handle_key(key(KeyCode::Esc));
        // Cursor at end after Esc + at-end clamp = 11.
        state.handle_key(key(KeyCode::Char('0')));
        assert_eq!(state.cursor(), 0);
        state.handle_key(key(KeyCode::Char('w')));
        assert_eq!(state.cursor(), 4);
        state.handle_key(key(KeyCode::Char('e')));
        assert_eq!(state.cursor(), 6);
        state.handle_key(key(KeyCode::Char('b')));
        assert_eq!(state.cursor(), 4);
    }

    #[test]
    fn x_deletes_char_at_cursor_in_input_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("abcd");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('x')));
        assert_eq!(state.text(), "bcd");
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn capital_x_deletes_char_before_cursor_in_input_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("abcd");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('X')));
        assert_eq!(state.text(), "abc");
    }

    #[test]
    fn lowercase_a_advances_cursor_then_inserts() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("ab");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        // After Esc + 0, cursor at start. `a` advances by one then enters Insert.
        state.handle_key(key(KeyCode::Char('a')));
        assert_eq!(state.mode(), Mode::Insert);
        assert_eq!(state.cursor(), 1);
        state.handle_key(key(KeyCode::Char('X')));
        assert_eq!(state.text(), "aXb");
    }

    #[test]
    fn capital_a_jumps_to_end_of_line_then_inserts() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello\nworld");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('k')));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('A')));
        assert_eq!(state.mode(), Mode::Insert);
        assert_eq!(state.cursor(), 5); // end of "hello"
    }

    #[test]
    fn capital_o_opens_line_above() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('O')));
        assert_eq!(state.mode(), Mode::Insert);
        // Text now starts with a newline.
        assert!(state.text().starts_with('\n'));
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn lowercase_o_opens_line_below() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('o')));
        assert_eq!(state.mode(), Mode::Insert);
        assert_eq!(state.text(), "hello\n");
        assert_eq!(state.cursor(), 6);
    }

    #[test]
    fn dw_deletes_word_in_input_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo bar baz");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('d')));
        state.handle_key(key(KeyCode::Char('w')));
        assert_eq!(state.text(), "bar baz");
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.register, "foo ");
    }

    #[test]
    fn dd_deletes_current_line() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("first\nsecond\nthird");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('g')));
        state.handle_key(key(KeyCode::Char('g')));
        // gg lands at byte 0; dd deletes "first\n".
        state.handle_key(key(KeyCode::Char('d')));
        state.handle_key(key(KeyCode::Char('d')));
        assert_eq!(state.text(), "second\nthird");
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.register, "first\n");
        assert!(state.register_linewise);
    }

    #[test]
    fn cw_deletes_word_and_enters_insert() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo bar");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('c')));
        state.handle_key(key(KeyCode::Char('w')));
        assert_eq!(state.mode(), Mode::Insert);
        assert_eq!(state.text(), "bar");
    }

    #[test]
    fn yw_yanks_word_into_register() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello world");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('y')));
        state.handle_key(key(KeyCode::Char('w')));
        // Text unchanged, cursor unchanged, register holds "hello ".
        assert_eq!(state.text(), "hello world");
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.register, "hello ");
        assert!(!state.register_linewise);
    }

    #[test]
    fn count_prefix_repeats_motion() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("a b c d e");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('3')));
        state.handle_key(key(KeyCode::Char('w')));
        // 3 word jumps from start: a -> b -> c -> d. Cursor at "d".
        assert_eq!(state.cursor(), 6);
    }

    #[test]
    fn count_prefix_with_operator_3dw() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo bar baz qux");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('3')));
        state.handle_key(key(KeyCode::Char('d')));
        state.handle_key(key(KeyCode::Char('w')));
        // Delete 3 words: "foo bar baz " -> leaves "qux".
        assert_eq!(state.text(), "qux");
    }

    #[test]
    fn capital_d_deletes_to_end_of_line() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello world");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('5'))); // count 5 -> jumps over
        state.handle_key(key(KeyCode::Char('l')));
        state.handle_key(key(KeyCode::Char('D')));
        assert_eq!(state.text(), "hello");
    }

    #[test]
    fn r_replaces_char_at_cursor() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("foo");
        state.handle_key(key(KeyCode::Esc));
        state.handle_key(key(KeyCode::Char('0')));
        state.handle_key(key(KeyCode::Char('r')));
        state.handle_key(key(KeyCode::Char('B')));
        assert_eq!(state.text(), "Boo");
        // Mode unchanged.
        assert_eq!(state.mode(), Mode::Normal);
    }

    #[test]
    fn d_then_esc_cancels_operator() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("hello");
        state.handle_key(key(KeyCode::Esc));
        // `d` then Esc cancels.
        state.handle_key(key(KeyCode::Char('d')));
        assert!(state.pending_op.is_some());
        state.handle_key(key(KeyCode::Esc));
        assert!(state.pending_op.is_none());
        assert_eq!(state.text(), "hello");
    }

    #[test]
    fn gw_in_normal_emits_cycle_pane() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('g')));
        let acts = state.handle_key(key(KeyCode::Char('w')));
        assert_eq!(acts, vec![InputAction::CyclePane]);
    }

    #[test]
    fn ctrl_k_deletes_to_line_end() {
        let mut state = InputState::new();
        state.handle_key(key(KeyCode::Char('i')));
        state.paste("first\nsecond");
        // Move cursor to start of "second".
        for _ in 0..6 {
            state.handle_key(key(KeyCode::Left));
        }
        assert_eq!(state.cursor(), 6);
        state.handle_key(ctrl('k'));
        assert_eq!(state.text(), "first\n");
    }
}
