//! Paste collapsing, image chips, and cursor movement.

use super::*;

impl InputState {
    /// Insert pasted text at the cursor when in [`Mode::Insert`]. No-op
    /// in other modes so a stray paste in normal mode does not mutate
    /// the prompt. Line breaks are kept, so a multi-line paste does not
    /// auto-submit. Terminals send them as CR, CRLF or LF; all become
    /// LF. A paste of many lines or characters collapses to a
    /// placeholder.
    pub fn paste(&mut self, text: &str) {
        if self.mode != Mode::Insert {
            return;
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = text.lines().count();
        let chars = text.chars().count();
        let size = if lines >= PASTE_COLLAPSE_LINES {
            format!("{lines} lines")
        } else if chars > PASTE_COLLAPSE_CHARS {
            format!("{chars} chars")
        } else {
            self.content.text.insert_str(self.content.cursor, &text);
            self.content.cursor += text.len();
            return;
        };
        let id = self.content.next_paste_id;
        self.content.next_paste_id = self.content.next_paste_id.wrapping_add(1);
        let blob = PasteBlob { id, text, size };
        let token = blob.placeholder();
        self.content.pastes.push(blob);
        self.content.text.insert_str(self.content.cursor, &token);
        self.content.cursor += token.len();
        self.content.undo_dirty = true;
    }

    /// Replace every collapsed-paste placeholder in `s` with its full
    /// text in one pass: each position takes the earliest matching
    /// placeholder and the appended blob text is never rescanned. A
    /// blob whose body itself contains another paste's placeholder is
    /// therefore sent verbatim, as is a placeholder the user edited
    /// until it no longer matches.
    pub(crate) fn resolve_pastes(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while !rest.is_empty() {
            let mut earliest: Option<(usize, &PasteBlob)> = None;
            for blob in &self.content.pastes {
                let needle = blob.placeholder();
                if let Some(pos) = rest.find(needle.as_str())
                    && earliest.as_ref().is_none_or(|(best, _)| pos < *best)
                {
                    earliest = Some((pos, blob));
                }
            }
            let Some((pos, blob)) = earliest else {
                break;
            };
            out.push_str(&rest[..pos]);
            out.push_str(&blob.text);
            rest = &rest[pos + blob.placeholder().len()..];
        }
        out.push_str(rest);
        out
    }

    /// Expand all collapsed pastes inline (Ctrl+O): the draft becomes
    /// the full text and the registry is cleared. The cursor lands at
    /// the end so the user can keep typing after the expanded block.
    pub(crate) fn expand_pastes(&mut self) {
        if self.content.pastes.is_empty() {
            return;
        }
        self.snapshot_for_undo();
        self.content.text = self.resolve_pastes(&self.content.text);
        self.content.cursor = self.content.text.len();
        self.content.pastes.clear();
    }

    /// Number of collapsed pastes currently held. Test/inspection aid.
    #[cfg(test)]
    pub(crate) fn collapsed_paste_count(&self) -> usize {
        self.content.pastes.len()
    }

    pub(crate) fn backspace(&mut self) {
        if self.content.cursor == 0 {
            return;
        }
        if self.backspace_image_marker() {
            return;
        }
        let prev = self.content.text[..self.content.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx);
        self.content.text.drain(prev..self.content.cursor);
        self.content.cursor = prev;
    }

    /// The whole `[image #N ...]` chip a Backspace would remove right
    /// now: any cursor position from just inside the opening `[`
    /// through the optional trailing space (`open < cursor <= end`).
    pub(crate) fn backspace_chip(&self) -> Option<(usize, usize, u32)> {
        let cur = self.content.cursor;
        image_marker_spans(&self.content.text)
            .into_iter()
            .find(|&(open, end, _)| open < cur && cur <= end)
    }

    /// The whole chip a forward Delete would remove: the mirror of
    /// [`Self::backspace_chip`], looking ahead of the cursor instead
    /// (`open <= cursor < end`), so Delete in front of or inside a
    /// chip takes the block, not one character.
    pub(crate) fn forward_delete_chip(&self) -> Option<(usize, usize, u32)> {
        let cur = self.content.cursor;
        image_marker_spans(&self.content.text)
            .into_iter()
            .find(|&(open, end, _)| open <= cur && cur < end)
    }

    /// Byte range of the chip the cursor is touching, for the
    /// renderer to highlight it as one solid block. This is the
    /// union of what a Backspace or a forward Delete here would
    /// remove (`open <= cursor <= end`), so the chip reads as atomic
    /// whenever the caret is adjacent to or within it.
    #[must_use]
    pub fn armed_image_range(&self) -> Option<(usize, usize)> {
        let cur = self.content.cursor;
        image_marker_spans(&self.content.text)
            .into_iter()
            .find(|&(open, end, _)| open <= cur && cur <= end)
            .map(|(open, end, _)| (open, end))
    }

    /// Remove a resolved chip span: drop its image, cut the marker
    /// (and trailing space) from the text, and park the cursor where
    /// it stood.
    pub(crate) fn remove_chip(&mut self, chip: (usize, usize, u32)) {
        let (open, end, id) = chip;
        self.content.attached.retain(|(i, _)| *i != id);
        self.content.text.drain(open..end);
        self.content.cursor = open;
        self.content.undo_dirty = true;
    }

    /// One Backspace deletes a whole image chip (and drops image `N`)
    /// rather than nibbling it character by character. Returns
    /// whether it handled the keystroke.
    pub(crate) fn backspace_image_marker(&mut self) -> bool {
        let Some(chip) = self.backspace_chip() else {
            return false;
        };
        self.remove_chip(chip);
        true
    }

    /// Forward Delete counterpart of [`Self::backspace_image_marker`].
    pub(crate) fn forward_delete_image_marker(&mut self) -> bool {
        let Some(chip) = self.forward_delete_chip() else {
            return false;
        };
        self.remove_chip(chip);
        true
    }

    /// Delete the chip ahead of the cursor whole, else the single
    /// character at the cursor (the standard forward-Delete edit).
    pub(crate) fn forward_delete(&mut self) {
        if self.forward_delete_image_marker() {
            return;
        }
        if let Some((_, w)) = char_at(&self.content.text, self.content.cursor) {
            self.content
                .text
                .drain(self.content.cursor..self.content.cursor + w);
        }
    }

    pub(crate) fn move_cursor(&mut self, delta: i32) {
        self.content.cursor = self.cursor_after_char_move(delta);
    }

    /// Move the cursor up one row inside the current text. Returns true
    /// if a move happened; false when the cursor is already on the top
    /// row (caller falls through to history navigation).
    pub(crate) fn move_cursor_up(&mut self) -> bool {
        let prefix = &self.content.text[..self.content.cursor];
        let Some(curr_line_start) = prefix.rfind('\n').map(|i| i + 1) else {
            return false;
        };
        let col_chars = self.content.text[curr_line_start..self.content.cursor]
            .chars()
            .count();
        let prev_line_end = curr_line_start - 1;
        let prev_line_start = self.content.text[..prev_line_end]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        self.content.cursor = byte_offset_at_column(
            &self.content.text[prev_line_start..prev_line_end],
            col_chars,
        ) + prev_line_start;
        true
    }

    /// Move the cursor down one row inside the current text. Returns
    /// true if a move happened; false when the cursor is on the last
    /// row (caller falls through to history navigation).
    pub(crate) fn move_cursor_down(&mut self) -> bool {
        let curr_line_start = self.content.text[..self.content.cursor]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let col_chars = self.content.text[curr_line_start..self.content.cursor]
            .chars()
            .count();
        let curr_line_end = self.content.text[self.content.cursor..]
            .find('\n')
            .map(|i| self.content.cursor + i);
        let Some(end) = curr_line_end else {
            return false;
        };
        let next_line_start = end + 1;
        let next_line_end = self.content.text[next_line_start..]
            .find('\n')
            .map_or(self.content.text.len(), |i| next_line_start + i);
        self.content.cursor = byte_offset_at_column(
            &self.content.text[next_line_start..next_line_end],
            col_chars,
        ) + next_line_start;
        true
    }
}
