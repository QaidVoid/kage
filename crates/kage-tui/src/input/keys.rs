//! Normal-mode key dispatch.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl InputState {
    /// Drive the state machine forward by one key.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<InputAction> {
        // Shift+Tab cycles the thinking level regardless of mode or
        // focused pane. Different terminals report it as either
        // `BackTab` (xterm/wezterm/kitty in default mode) or
        // `Tab + SHIFT` (some emulators with the kitty keyboard
        // protocol enabled); accept both.
        if matches!(key.code, KeyCode::BackTab)
            || (matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            return vec![InputAction::CycleThinkingLevel];
        }
        if self.modeless {
            return self.handle_modeless(key);
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Visual => self.handle_visual(key),
        }
    }

    /// Non-modal dispatch. The editor is always insert-like: `Esc`
    /// cancels the in-flight turn (never enters Normal), `PageUp` /
    /// `PageDown` scroll the conversation buffer (there is no buffer
    /// pane to focus), and every other key goes through the insert
    /// handler. The insert handler's only mode transition is its own
    /// `Esc` arm, which is intercepted here, so the editor can never
    /// leave the insert state.
    pub(crate) fn handle_modeless(&mut self, key: KeyEvent) -> Vec<InputAction> {
        match key.code {
            KeyCode::Esc => {
                self.reset_history_navigation();
                vec![InputAction::Cancel]
            }
            KeyCode::PageUp => vec![InputAction::Scroll(-10)],
            KeyCode::PageDown => vec![InputAction::Scroll(10)],
            _ => self.handle_insert(key),
        }
    }

    pub(crate) fn handle_normal(&mut self, key: KeyEvent) -> Vec<InputAction> {
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
    pub(crate) fn handle_op_pending(&mut self, key: KeyEvent) -> Vec<InputAction> {
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
    pub(crate) fn handle_normal_buffer(&mut self, key: KeyEvent) -> Vec<InputAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return vec![InputAction::OpenSessionPicker];
        }
        match key.code {
            KeyCode::Char('i' | 'a') => {
                self.focused_pane = Pane::Input;
                self.snapshot_for_undo();
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('v') => vec![InputAction::EnterVisual],
            // Both yank raw block source (not the rendered cells):
            // `y` copies the active selection's blocks, or the
            // focused block when there is no selection; `Y` always
            // copies the focused block, vim's "yank line" adapted to
            // our block-stream layout.
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
    /// (`3dw`, `5j`), undo / redo (`u`, `<C-r>`), and the insert-
    /// entry variants (`i`/`a`/`I`/`A`/`o`/`O`). Cursor movement and
    /// edits mutate state in place; mode transitions return an
    /// [`InputAction`] for the host to react to.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn handle_normal_input(&mut self, key: KeyEvent) -> Vec<InputAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('r')) {
            self.redo();
            return Vec::new();
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return vec![InputAction::OpenSessionPicker];
        }

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
            KeyCode::Char('i') => {
                self.snapshot_for_undo();
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('a') => {
                self.snapshot_for_undo();
                if let Some((_, w)) = char_at(&self.text, self.cursor) {
                    self.cursor += w;
                }
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('I') => {
                self.snapshot_for_undo();
                let start = current_line_start(&self.text, self.cursor);
                self.cursor = first_non_whitespace_at(&self.text, start);
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('A') => {
                self.snapshot_for_undo();
                self.cursor = current_line_end(&self.text, self.cursor);
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('o') => {
                self.snapshot_for_undo();
                let end = current_line_end(&self.text, self.cursor);
                self.text.insert(end, '\n');
                self.cursor = end + 1;
                self.enter_mode(Mode::Insert)
            }
            KeyCode::Char('O') => {
                self.snapshot_for_undo();
                let start = current_line_start(&self.text, self.cursor);
                self.text.insert(start, '\n');
                self.cursor = start;
                self.enter_mode(Mode::Insert)
            }
            // Single-char edits. Snapshot once per `x`/`X` press so a
            // count repeats inside a single undo unit.
            KeyCode::Char('x') => {
                self.snapshot_for_undo();
                for _ in 0..count {
                    self.delete_char_at_cursor();
                }
                Vec::new()
            }
            KeyCode::Char('X') => {
                self.snapshot_for_undo();
                for _ in 0..count {
                    self.backspace();
                }
                Vec::new()
            }
            KeyCode::Char('u') => {
                self.undo();
                Vec::new()
            }
            KeyCode::Char('p') => {
                self.paste_after(count);
                Vec::new()
            }
            KeyCode::Char('P') => {
                self.paste_before(count);
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
            KeyCode::Char('v') => {
                // Input-pane char-visual: anchor at current cursor
                // and switch to Visual mode. The host detects this
                // via `input_visual_range()` and renders an inline
                // highlight; buffer-cell selection is suppressed.
                self.visual_anchor = Some(self.cursor);
                self.enter_mode(Mode::Visual)
            }
            KeyCode::PageDown => vec![InputAction::Scroll(10)],
            KeyCode::PageUp => vec![InputAction::Scroll(-10)],
            _ => Vec::new(),
        }
    }
}
