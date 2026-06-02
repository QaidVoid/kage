//! Paste collapsing, image chips, and cursor movement.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl InputState {
    /// Insert pasted text at the cursor when in [`Mode::Insert`]. No-op
    /// in other modes so a stray paste in normal mode does not mutate
    /// the prompt. The paste is preserved verbatim, including newlines,
    /// so a multi-line paste does not auto-submit.
    pub fn paste(&mut self, text: &str) {
        if self.mode != Mode::Insert {
            return;
        }
        let lines = text.split('\n').count();
        if lines >= PASTE_COLLAPSE_LINES {
            let id = self.next_paste_id;
            self.next_paste_id = self.next_paste_id.wrapping_add(1);
            let blob = PasteBlob {
                id,
                text: text.to_owned(),
                lines,
            };
            let token = blob.placeholder();
            self.pastes.push(blob);
            self.text.insert_str(self.cursor, &token);
            self.cursor += token.len();
            return;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Replace every collapsed-paste placeholder in `s` with its full
    /// text. A placeholder a user has edited no longer matches and is
    /// left as-is (the literal token is what gets sent), which is
    /// visible rather than silent.
    pub(crate) fn resolve_pastes(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for blob in &self.pastes {
            out = out.replace(&blob.placeholder(), &blob.text);
        }
        out
    }

    /// Expand all collapsed pastes inline (Ctrl+O): the draft becomes
    /// the full text and the registry is cleared. The cursor lands at
    /// the end so the user can keep typing after the expanded block.
    pub(crate) fn expand_pastes(&mut self) {
        if self.pastes.is_empty() {
            return;
        }
        self.snapshot_for_undo();
        self.text = self.resolve_pastes(&self.text);
        self.cursor = self.text.len();
        self.pastes.clear();
    }

    /// Number of collapsed pastes currently held. Test/inspection aid.
    #[cfg(test)]
    pub(crate) fn collapsed_paste_count(&self) -> usize {
        self.pastes.len()
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if self.backspace_image_marker() {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    /// The whole `[image #N ...]` chip a Backspace would remove right
    /// now: any cursor position from just inside the opening `[`
    /// through the optional trailing space (`open < cursor <= end`).
    pub(crate) fn backspace_chip(&self) -> Option<(usize, usize, u32)> {
        let cur = self.cursor;
        image_marker_spans(&self.text)
            .into_iter()
            .find(|&(open, end, _)| open < cur && cur <= end)
    }

    /// The whole chip a forward Delete would remove: the mirror of
    /// [`Self::backspace_chip`], looking ahead of the cursor instead
    /// (`open <= cursor < end`), so Delete in front of or inside a
    /// chip takes the block, not one character.
    pub(crate) fn forward_delete_chip(&self) -> Option<(usize, usize, u32)> {
        let cur = self.cursor;
        image_marker_spans(&self.text)
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
        let cur = self.cursor;
        image_marker_spans(&self.text)
            .into_iter()
            .find(|&(open, end, _)| open <= cur && cur <= end)
            .map(|(open, end, _)| (open, end))
    }

    /// Remove a resolved chip span: drop its image, cut the marker
    /// (and trailing space) from the text, and park the cursor where
    /// it stood.
    pub(crate) fn remove_chip(&mut self, chip: (usize, usize, u32)) {
        let (open, end, id) = chip;
        self.attached.retain(|(i, _)| *i != id);
        self.text.drain(open..end);
        self.cursor = open;
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
        if let Some((_, w)) = char_at(&self.text, self.cursor) {
            self.text.drain(self.cursor..self.cursor + w);
        }
    }

    pub(crate) fn move_cursor(&mut self, delta: i32) {
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
    pub(crate) fn move_cursor_up(&mut self) -> bool {
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
    pub(crate) fn move_cursor_down(&mut self) -> bool {
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
