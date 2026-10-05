//! Counts, operators, and motion-operator ranges.

use super::*;

impl InputState {
    /// Walk the count digit `c` into [`Self::pending_count`]. Caps
    /// the accumulator at [`MAX_COUNT`] so an absurd digit run can't
    /// build a count that hangs motion loops or explodes pastes.
    pub(crate) fn accumulate_count(&mut self, c: char) {
        let digit = (c as u32).saturating_sub('0' as u32) as usize;
        let next = self
            .pending_count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit)
            .min(MAX_COUNT);
        self.pending_count = Some(next);
    }

    /// Walk the digit `c` into [`Self::pending_op_count`], the count
    /// typed after an operator. Same cap as [`Self::accumulate_count`].
    pub(crate) fn accumulate_op_count(&mut self, c: char) {
        let digit = (c as u32).saturating_sub('0' as u32) as usize;
        let next = self
            .pending_op_count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit)
            .min(MAX_COUNT);
        self.pending_op_count = Some(next);
    }

    /// Replace up to `count` chars starting at the cursor with `c`
    /// (vim's `3rX`), stopping at the end of the line: `r` never
    /// extends it. Cursor stays at the start of the replaced run.
    pub(crate) fn replace_chars_at_cursor(&mut self, c: char, count: usize) {
        if char_at(&self.content.text, self.content.cursor).is_none() {
            return;
        }
        self.snapshot_for_undo();
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        for _ in 0..count {
            let Some((_, w)) = char_at(&self.content.text, self.content.cursor) else {
                break;
            };
            let end = self.content.cursor + w;
            self.content.text.replace_range(self.content.cursor..end, s);
            self.content.cursor = end;
        }
    }

    /// Cursor target after moving `delta` chars, clamped to text
    /// bounds. Negative `delta` walks left.
    pub(crate) fn cursor_after_char_move(&self, delta: i32) -> usize {
        let mut pos = self.content.cursor;
        if delta >= 0 {
            for _ in 0..delta {
                let Some((_, w)) = char_at(&self.content.text, pos) else {
                    break;
                };
                pos += w;
            }
        } else {
            for _ in 0..(-delta) {
                let Some((_, w)) = prev_char(&self.content.text, pos) else {
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
    pub(crate) fn motion_operator_range(
        &self,
        motion_key: char,
        count: usize,
    ) -> Option<(usize, usize)> {
        let count = count.max(1);
        let target: usize = match motion_key {
            'h' => self.cursor_after_char_move(-i32::try_from(count).unwrap_or(i32::MAX)),
            'l' => self.cursor_after_char_move(i32::try_from(count).unwrap_or(i32::MAX)),
            '0' => current_line_start(&self.content.text, self.content.cursor),
            '$' => current_line_end(&self.content.text, self.content.cursor),
            '^' => {
                let s = current_line_start(&self.content.text, self.content.cursor);
                first_non_whitespace_at(&self.content.text, s)
            }
            'w' => {
                let mut p = self.content.cursor;
                for _ in 0..count {
                    p = vim_word_forward(&self.content.text, p);
                }
                p
            }
            'b' => {
                let mut p = self.content.cursor;
                for _ in 0..count {
                    p = backward_word_start(&self.content.text, p);
                }
                p
            }
            'e' => {
                let mut p = self.content.cursor;
                for _ in 0..count {
                    p = vim_word_end(&self.content.text, p);
                }
                if let Some((_, w)) = char_at(&self.content.text, p) {
                    p + w
                } else {
                    p
                }
            }
            'G' => self.content.text.len(),
            _ => return None,
        };
        let range = if self.content.cursor <= target {
            (self.content.cursor, target)
        } else {
            (target, self.content.cursor)
        };
        Some(range)
    }

    /// Apply a charwise operator on `range`. Saves the consumed text
    /// to the register and updates cursor / text per op semantics.
    pub(crate) fn apply_op_charwise(
        &mut self,
        op: Operator,
        range: (usize, usize),
    ) -> Vec<InputAction> {
        let (s, e) = range;
        if s >= e || e > self.content.text.len() {
            return Vec::new();
        }
        self.content.register = self.content.text[s..e].to_string();
        self.content.register_linewise = false;
        match op {
            Operator::Yank => Vec::new(),
            Operator::Delete => {
                self.snapshot_for_undo();
                self.content.text.drain(s..e);
                self.content.cursor = s;
                Vec::new()
            }
            Operator::Change => {
                self.snapshot_for_undo();
                self.content.text.drain(s..e);
                self.content.cursor = s;
                self.enter_mode(Mode::Insert)
            }
        }
    }

    /// Paste the contents of [`Self::register`] after the cursor
    /// (vim's `p`). Linewise registers paste below the current line;
    /// charwise registers paste inline after the char under the
    /// cursor. Cursor lands on the last char of the pasted text.
    pub(crate) fn paste_after(&mut self, count: usize) {
        if self.content.register.is_empty() {
            return;
        }
        let count = count.max(1);
        if !paste_fits(&self.content.register, count) {
            return;
        }
        self.snapshot_for_undo();
        let payload = self.content.register.repeat(count);
        if self.content.register_linewise {
            // Insert as a new line *below* the current line. If the
            // current line is the last (no trailing newline), prepend
            // a newline so the pasted block lands on its own row.
            let line_end = current_line_end(&self.content.text, self.content.cursor);
            let insert_pos = if line_end < self.content.text.len() {
                line_end + 1
            } else {
                // At end of last line: insert newline first.
                self.content.text.push('\n');
                self.content.text.len()
            };
            self.content.text.insert_str(insert_pos, &payload);
            self.content.cursor = insert_pos;
        } else {
            let insert_pos = if let Some((_, w)) = char_at(&self.content.text, self.content.cursor)
            {
                self.content.cursor + w
            } else {
                self.content.cursor
            };
            self.content.text.insert_str(insert_pos, &payload);
            self.content.cursor = last_char_offset(&self.content.text, insert_pos + payload.len());
        }
    }

    /// Paste the contents of [`Self::register`] before the cursor
    /// (vim's `P`). Linewise registers paste above the current line;
    /// charwise registers paste at the cursor.
    pub(crate) fn paste_before(&mut self, count: usize) {
        if self.content.register.is_empty() {
            return;
        }
        let count = count.max(1);
        if !paste_fits(&self.content.register, count) {
            return;
        }
        self.snapshot_for_undo();
        let payload = self.content.register.repeat(count);
        if self.content.register_linewise {
            let line_start = current_line_start(&self.content.text, self.content.cursor);
            self.content.text.insert_str(line_start, &payload);
            self.content.cursor = line_start;
        } else {
            let insert_pos = self.content.cursor;
            self.content.text.insert_str(insert_pos, &payload);
            self.content.cursor = last_char_offset(&self.content.text, insert_pos + payload.len());
        }
    }

    /// Apply a linewise operator covering `count` lines starting from
    /// the cursor's line. `dd` removes the line and its trailing
    /// newline, `yy` only copies, `cc` removes the line content but
    /// preserves the surrounding newline structure and enters Insert.
    pub(crate) fn apply_op_linewise(&mut self, op: Operator, count: usize) -> Vec<InputAction> {
        let count = count.max(1);
        let line_start = current_line_start(&self.content.text, self.content.cursor);
        let mut end = line_start;
        for i in 0..count {
            let content_end = current_line_end(&self.content.text, end);
            end = if matches!(op, Operator::Change) && i == count - 1 {
                content_end
            } else if content_end < self.content.text.len() {
                content_end + 1
            } else {
                content_end
            };
        }
        self.content.register = self.content.text[line_start..end].to_string();
        self.content.register_linewise = true;
        match op {
            Operator::Yank => Vec::new(),
            Operator::Delete => {
                self.snapshot_for_undo();
                self.content.text.drain(line_start..end);
                self.content.cursor = line_start.min(self.content.text.len());
                Vec::new()
            }
            Operator::Change => {
                self.snapshot_for_undo();
                self.content.text.drain(line_start..end);
                self.content.cursor = line_start;
                self.enter_mode(Mode::Insert)
            }
        }
    }

    /// Resolve the key after a `g` or `z` prefix. Only `gg` in the
    /// input pane is grammar; any other pair re-processes the second
    /// key as a fresh keystroke, so `gd` enters operator pending and
    /// `g Esc` clears the selection instead of both being swallowed.
    /// The host keymap resolves `gw` and the `z` fold keys before the
    /// grammar sees them.
    pub(crate) fn handle_pending(&mut self, prev: char, key: KeyEvent) -> Vec<InputAction> {
        if prev == 'g' && key.code == KeyCode::Char('g') && self.focused_pane == Pane::Input {
            self.content.cursor = 0;
            return Vec::new();
        }
        self.handle_normal(key)
    }
}

/// Guard for [`InputState::paste_after`] / [`InputState::paste_before`]:
/// a payload of register * count beyond [`MAX_PASTE_BYTES`] is
/// refused rather than allocated, which would otherwise hit the
/// allocation-size-abort handler.
fn paste_fits(register: &str, count: usize) -> bool {
    register.len().saturating_mul(count) <= MAX_PASTE_BYTES
}
