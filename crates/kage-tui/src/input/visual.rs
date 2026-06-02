//! Visual-mode selection and mode transitions.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl InputState {
    pub(crate) fn handle_visual(&mut self, key: KeyEvent) -> Vec<InputAction> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('s')) {
            return vec![InputAction::OpenSessionPicker];
        }
        if self.visual_anchor.is_some() {
            return self.handle_visual_input(key);
        }
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

    /// Handle Visual-mode keys when an input-pane char-visual
    /// selection is active. Motions extend the cursor (anchor stays
    /// put); operators (`d`/`c`/`y`) act on the
    /// `[min(anchor, cursor), max + 1)` range and exit Visual.
    pub(crate) fn handle_visual_input(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match key.code {
            KeyCode::Esc => {
                self.visual_anchor = None;
                let mut actions = vec![InputAction::ClearSelection];
                actions.extend(self.enter_mode(Mode::Normal));
                return actions;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor = self.cursor_after_char_move(-1);
                return Vec::new();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor = self.cursor_after_char_move(1);
                return Vec::new();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_cursor_down();
                return Vec::new();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_cursor_up();
                return Vec::new();
            }
            KeyCode::Char('0') | KeyCode::Home => {
                self.cursor = current_line_start(&self.text, self.cursor);
                return Vec::new();
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.cursor = current_line_end(&self.text, self.cursor);
                return Vec::new();
            }
            KeyCode::Char('^') => {
                let s = current_line_start(&self.text, self.cursor);
                self.cursor = first_non_whitespace_at(&self.text, s);
                return Vec::new();
            }
            KeyCode::Char('w') => {
                self.cursor = vim_word_forward(&self.text, self.cursor);
                return Vec::new();
            }
            KeyCode::Char('b') => {
                self.cursor = backward_word_start(&self.text, self.cursor);
                return Vec::new();
            }
            KeyCode::Char('e') => {
                self.cursor = vim_word_end(&self.text, self.cursor);
                return Vec::new();
            }
            KeyCode::Char(op_key) if matches!(op_key, 'd' | 'c' | 'y' | 'x' | 'X') => {
                let op = match op_key {
                    'c' => Operator::Change,
                    'y' => Operator::Yank,
                    _ => Operator::Delete,
                };
                let range = self.input_visual_range_inclusive();
                self.visual_anchor = None;
                let mut actions = self.apply_op_charwise(op, range);
                if !matches!(op, Operator::Change) {
                    actions.extend(self.enter_mode(Mode::Normal));
                }
                return actions;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Byte range covered by an active input-pane char-visual
    /// selection, expressed inclusively at the right edge so an
    /// operator deletes the char under the cursor too. Returns
    /// `(0, 0)` when no selection is active (callers should check
    /// [`Self::visual_anchor`] first).
    pub(crate) fn input_visual_range_inclusive(&self) -> (usize, usize) {
        let Some(anchor) = self.visual_anchor else {
            return (0, 0);
        };
        let (start, end) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        let end_inclusive = if let Some((_, w)) = char_at(&self.text, end) {
            end + w
        } else {
            end
        };
        (start, end_inclusive)
    }

    /// Public accessor: byte range of the active input-pane visual
    /// selection, inclusive at the right edge. Returns `None` when
    /// the user is not in input-pane Visual mode (either Normal /
    /// Insert, or Buffer-pane visual).
    #[must_use]
    pub fn input_visual_range(&self) -> Option<(usize, usize)> {
        self.visual_anchor?;
        Some(self.input_visual_range_inclusive())
    }

    pub(crate) fn enter_mode(&mut self, mode: Mode) -> Vec<InputAction> {
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

    pub(crate) fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }
}
