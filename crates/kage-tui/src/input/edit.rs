//! Insert-mode editing, deletion, and the kill ring.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl InputState {
    /// Vim-style `x`: delete the char at the cursor. If the deletion
    /// leaves the cursor past the end of its line, snap it to the
    /// last char on that line (vim convention).
    pub(crate) fn delete_char_at_cursor(&mut self) {
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
    pub(crate) fn handle_insert(&mut self, key: KeyEvent) -> Vec<InputAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl {
            match key.code {
                KeyCode::Up => return vec![InputAction::Scroll(-1)],
                KeyCode::Down => return vec![InputAction::Scroll(1)],
                KeyCode::Home => return vec![InputAction::ScrollToTop],
                KeyCode::End => return vec![InputAction::ScrollToBottom],
                KeyCode::Char('p') => return vec![InputAction::FocusPrev],
                KeyCode::Char('n') => return vec![InputAction::FocusNext],
                _ => {}
            }
        }

        // Readline / Emacs-style word and line edits. Match shells
        // (bash, zsh, fish): Ctrl+W deletes back to whitespace
        // ("unix-word-rubout"), Alt+Backspace deletes back to the
        // previous alphanumeric boundary ("backward-kill-word"),
        // Alt+d deletes forward, Alt+b/Alt+f move by word, and
        // Ctrl+a/e/u/k operate on the current visual line. The kills
        // (Ctrl+W/U/K, Alt+Backspace, Alt+d) feed a kill ring; Ctrl+Y
        // yanks the most recent entry, Ctrl+/ (or Ctrl+_) undoes, and
        // Ctrl+O toggles the fold on the focused buffer block (or, if
        // a large paste is collapsed, expands it inline).
        if ctrl && !alt {
            match key.code {
                KeyCode::Char('s') => {
                    return vec![InputAction::OpenSessionPicker];
                }
                KeyCode::Char('w') => {
                    self.reset_history_navigation();
                    let to = unix_word_rubout_start(&self.text, self.cursor);
                    self.kill_range(to, self.cursor);
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
                    self.kill_range(start, self.cursor);
                    return Vec::new();
                }
                KeyCode::Char('k') => {
                    self.reset_history_navigation();
                    let end = current_line_end(&self.text, self.cursor);
                    self.kill_range(self.cursor, end);
                    return Vec::new();
                }
                KeyCode::Char('y') => {
                    self.reset_history_navigation();
                    self.yank_kill();
                    return Vec::new();
                }
                KeyCode::Char('/' | '_') => {
                    self.reset_history_navigation();
                    self.undo();
                    return Vec::new();
                }
                KeyCode::Char('o') => {
                    if self.pastes.is_empty() {
                        return vec![InputAction::ToggleFold];
                    }
                    self.expand_pastes();
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
                    self.kill_range(to, self.cursor);
                    return Vec::new();
                }
                KeyCode::Delete | KeyCode::Char('d') => {
                    self.reset_history_navigation();
                    let to = forward_word_end(&self.text, self.cursor);
                    self.kill_range(self.cursor, to);
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
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.insert_char('\n');
                    Vec::new()
                } else if self.text.is_empty() {
                    // Nothing to send; an image attached without a
                    // surviving marker is stale - drop it.
                    self.attached.clear();
                    Vec::new()
                } else {
                    let raw = std::mem::take(&mut self.text);
                    let expanded = self.resolve_pastes(&raw);
                    self.pastes.clear();
                    // Keep only images whose `[image #N ...]` marker
                    // still exists; strip the markers from the text
                    // the model receives (the image rides as a
                    // `Content::Image` block instead).
                    let live = image_marker_ids(&expanded);
                    self.attached.retain(|(id, _)| live.contains(id));
                    let text = strip_image_markers(&expanded);
                    self.cursor = 0;
                    self.push_history(&text);
                    self.reset_history_navigation();
                    vec![InputAction::Submit(text)]
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
            KeyCode::Delete => {
                self.reset_history_navigation();
                self.forward_delete();
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
    pub(crate) fn delete_range(&mut self, start: usize, end: usize) {
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

    /// Delete `start..end` like [`Self::delete_range`], but first
    /// snapshot for undo and push the removed text onto the kill ring
    /// so Ctrl+Y can yank it back. Used by the Emacs line/word kills
    /// (Ctrl+W / Ctrl+U / Ctrl+K, Alt+Backspace, Alt+d). Empty or
    /// invalid ranges are a no-op and do not touch the ring.
    pub(crate) fn kill_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.text.len() {
            return;
        }
        self.snapshot_for_undo();
        let killed = self.text[start..end].to_owned();
        self.kill_ring.push(killed);
        if self.kill_ring.len() > KILL_RING_MAX {
            self.kill_ring.remove(0);
        }
        self.delete_range(start, end);
    }

    /// Insert the most recent kill-ring entry at the cursor (Emacs
    /// Ctrl+Y). A no-op when the ring is empty. Snapshots for undo and
    /// leaves the cursor just past the inserted text.
    pub(crate) fn yank_kill(&mut self) {
        let Some(text) = self.kill_ring.last().cloned() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.snapshot_for_undo();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    /// Read-only view of the kill ring, oldest first. Test/inspection
    /// aid; the most recent entry is what Ctrl+Y yanks.
    #[cfg(test)]
    pub(crate) fn kill_ring(&self) -> &[String] {
        &self.kill_ring
    }
}
