//! Counts, operators, and motion-operator ranges.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl InputState {
    /// Walk the count digit `c` into [`Self::pending_count`]. Caps
    /// the accumulator at `usize::MAX / 10` so absurd input can't
    /// overflow.
    pub(crate) fn accumulate_count(&mut self, c: char) {
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
    pub(crate) fn replace_char_at_cursor(&mut self, c: char) {
        let Some((_, w)) = char_at(&self.text, self.cursor) else {
            return;
        };
        self.snapshot_for_undo();
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.replace_range(self.cursor..self.cursor + w, s);
    }

    /// Cursor target after moving `delta` chars, clamped to text
    /// bounds. Negative `delta` walks left.
    pub(crate) fn cursor_after_char_move(&self, delta: i32) -> usize {
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
    pub(crate) fn motion_operator_range(
        &self,
        motion_key: char,
        count: usize,
    ) -> Option<(usize, usize)> {
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
    pub(crate) fn apply_op_charwise(
        &mut self,
        op: Operator,
        range: (usize, usize),
    ) -> Vec<InputAction> {
        let (s, e) = range;
        if s >= e || e > self.text.len() {
            return Vec::new();
        }
        self.register = self.text[s..e].to_string();
        self.register_linewise = false;
        match op {
            Operator::Yank => Vec::new(),
            Operator::Delete => {
                self.snapshot_for_undo();
                self.text.drain(s..e);
                self.cursor = s;
                Vec::new()
            }
            Operator::Change => {
                self.snapshot_for_undo();
                self.text.drain(s..e);
                self.cursor = s;
                self.enter_mode(Mode::Insert)
            }
        }
    }

    /// Paste the contents of [`Self::register`] after the cursor
    /// (vim's `p`). Linewise registers paste below the current line;
    /// charwise registers paste inline after the char under the
    /// cursor. Cursor lands on the last char of the pasted text.
    pub(crate) fn paste_after(&mut self, count: usize) {
        if self.register.is_empty() {
            return;
        }
        let count = count.max(1);
        self.snapshot_for_undo();
        let payload = self.register.repeat(count);
        if self.register_linewise {
            // Insert as a new line *below* the current line. If the
            // current line is the last (no trailing newline), prepend
            // a newline so the pasted block lands on its own row.
            let line_end = current_line_end(&self.text, self.cursor);
            let insert_pos = if line_end < self.text.len() {
                line_end + 1
            } else {
                // At end of last line: insert newline first.
                self.text.push('\n');
                self.text.len()
            };
            self.text.insert_str(insert_pos, &payload);
            self.cursor = insert_pos;
        } else {
            let insert_pos = if let Some((_, w)) = char_at(&self.text, self.cursor) {
                self.cursor + w
            } else {
                self.cursor
            };
            self.text.insert_str(insert_pos, &payload);
            self.cursor = last_char_offset(&self.text, insert_pos + payload.len());
        }
    }

    /// Paste the contents of [`Self::register`] before the cursor
    /// (vim's `P`). Linewise registers paste above the current line;
    /// charwise registers paste at the cursor.
    pub(crate) fn paste_before(&mut self, count: usize) {
        if self.register.is_empty() {
            return;
        }
        let count = count.max(1);
        self.snapshot_for_undo();
        let payload = self.register.repeat(count);
        if self.register_linewise {
            let line_start = current_line_start(&self.text, self.cursor);
            self.text.insert_str(line_start, &payload);
            self.cursor = line_start;
        } else {
            let insert_pos = self.cursor;
            self.text.insert_str(insert_pos, &payload);
            self.cursor = last_char_offset(&self.text, insert_pos + payload.len());
        }
    }

    /// Apply a linewise operator covering `count` lines starting from
    /// the cursor's line. `dd` removes the line and its trailing
    /// newline, `yy` only copies, `cc` removes the line content but
    /// preserves the surrounding newline structure and enters Insert.
    pub(crate) fn apply_op_linewise(&mut self, op: Operator, count: usize) -> Vec<InputAction> {
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
                self.snapshot_for_undo();
                self.text.drain(line_start..end);
                self.cursor = line_start.min(self.text.len());
                Vec::new()
            }
            Operator::Change => {
                self.snapshot_for_undo();
                self.text.drain(line_start..end);
                self.cursor = line_start;
                self.enter_mode(Mode::Insert)
            }
        }
    }

    pub(crate) fn handle_pending(&mut self, prev: char, key: KeyEvent) -> Vec<InputAction> {
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
}
