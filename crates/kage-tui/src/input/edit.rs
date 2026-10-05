//! Insert-mode editing, deletion, and the kill ring.

use super::*;

impl InputState {
    /// Vim-style `x`: delete the char at the cursor. If the deletion
    /// leaves the cursor past the end of its line, snap it to the
    /// last char on that line (vim convention).
    pub(crate) fn delete_char_at_cursor(&mut self) {
        let Some((c, w)) = char_at(&self.content.text, self.content.cursor) else {
            return;
        };
        if c == '\n' {
            // Vim's `x` does not eat newlines; ignore.
            return;
        }
        self.content
            .text
            .drain(self.content.cursor..self.content.cursor + w);
        let line_end = current_line_end(&self.content.text, self.content.cursor);
        let line_start = current_line_start(&self.content.text, self.content.cursor);
        if self.content.cursor > line_end {
            self.content.cursor = line_end;
        }
        if self.content.cursor == line_end
            && line_end > line_start
            && let Some((_, pw)) = prev_char(&self.content.text, self.content.cursor)
        {
            self.content.cursor -= pw;
        }
    }

    #[expect(clippy::too_many_lines, reason = "one match over the insert-mode keys")]
    pub(crate) fn handle_insert(&mut self, key: KeyEvent) -> Vec<InputAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

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
                KeyCode::Char('w') => {
                    self.reset_history_navigation();
                    let to = unix_word_rubout_start(&self.content.text, self.content.cursor);
                    self.kill_range(to, self.content.cursor);
                    return Vec::new();
                }
                KeyCode::Char('a') => {
                    self.content.cursor =
                        current_line_start(&self.content.text, self.content.cursor);
                    return Vec::new();
                }
                KeyCode::Char('e') => {
                    self.content.cursor = current_line_end(&self.content.text, self.content.cursor);
                    return Vec::new();
                }
                KeyCode::Char('u') => {
                    self.reset_history_navigation();
                    let start = current_line_start(&self.content.text, self.content.cursor);
                    self.kill_range(start, self.content.cursor);
                    return Vec::new();
                }
                KeyCode::Char('k') => {
                    self.reset_history_navigation();
                    let end = current_line_end(&self.content.text, self.content.cursor);
                    self.kill_range(self.content.cursor, end);
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
                    if self.content.pastes.is_empty() {
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
                    let to = backward_word_start(&self.content.text, self.content.cursor);
                    self.kill_range(to, self.content.cursor);
                    return Vec::new();
                }
                KeyCode::Delete | KeyCode::Char('d') => {
                    self.reset_history_navigation();
                    let to = forward_word_end(&self.content.text, self.content.cursor);
                    self.kill_range(self.content.cursor, to);
                    return Vec::new();
                }
                KeyCode::Char('b') | KeyCode::Left => {
                    self.content.cursor =
                        backward_word_start(&self.content.text, self.content.cursor);
                    return Vec::new();
                }
                KeyCode::Char('f') | KeyCode::Right => {
                    self.content.cursor = forward_word_end(&self.content.text, self.content.cursor);
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
                } else if self.content.text.is_empty() {
                    // Nothing to send; an image attached without a
                    // surviving marker is stale - drop it, but say so
                    // rather than vanishing silently.
                    let stale = !self.content.attached.is_empty();
                    self.content.attached.clear();
                    if stale {
                        vec![InputAction::DroppedStaleAttach]
                    } else {
                        Vec::new()
                    }
                } else if self.content.shell {
                    // Shell commands stay out of the prompt history;
                    // they are not prompts.
                    self.content.shell = false;
                    let text = self.take_draft();
                    self.reset_history_navigation();
                    vec![InputAction::RunShell(text)]
                } else {
                    self.take_prompt()
                        .map(InputAction::Submit)
                        .into_iter()
                        .collect()
                }
            }
            KeyCode::Up => {
                // Multi-line input: walk a row up first; only fall
                // through when the cursor is already on the top row of
                // the current draft. A sent prompt still waiting is
                // newer than any history entry, so it comes back first.
                if self.move_cursor_up() {
                    return Vec::new();
                }
                if self.recallable && self.history_cursor.is_none() && !self.content.shell {
                    return vec![InputAction::RecallPrompt];
                }
                self.history_prev();
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
                if self.content.shell && self.content.text.is_empty() {
                    self.content.shell = false;
                    return Vec::new();
                }
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
                self.content.cursor = 0;
                Vec::new()
            }
            KeyCode::End => {
                self.content.cursor = self.content.text.len();
                Vec::new()
            }
            KeyCode::Delete => {
                self.reset_history_navigation();
                self.forward_delete();
                Vec::new()
            }
            KeyCode::Char('!') if self.content.text.is_empty() && self.content.cursor == 0 => {
                self.content.shell = true;
                Vec::new()
            }
            // An unmapped Ctrl chord does not type its letter. Ctrl
            // with Alt stays text, since some terminals report AltGr
            // characters that way.
            KeyCode::Char(_) if ctrl && !alt => Vec::new(),
            KeyCode::Char(c) => {
                self.reset_history_navigation();
                self.insert_char(c);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Take the draft as the text to send and empty it: collapsed
    /// pastes resolve to their full text, images whose chip is gone
    /// are dropped, and the chips are stripped (the images ride as
    /// `Content::Image` blocks instead). Snapshots before emptying so
    /// a submit boundary still allows undoing the pre-submit text.
    fn take_draft(&mut self) -> String {
        self.snapshot_for_undo();
        let raw = std::mem::take(&mut self.content.text);
        let expanded = self.resolve_pastes(&raw);
        self.content.pastes.clear();
        let live = image_marker_ids(&expanded);
        self.content.attached.retain(|(id, _)| live.contains(id));
        self.content.cursor = 0;
        strip_image_markers(&expanded)
    }

    /// Take the draft as a prompt, as Enter submits it, and record it
    /// in the history. `None` when the draft is empty or shell mode is
    /// armed.
    pub(crate) fn take_prompt(&mut self) -> Option<String> {
        if self.content.text.is_empty() || self.content.shell {
            return None;
        }
        let text = self.take_draft();
        self.push_history(&text);
        self.reset_history_navigation();
        Some(text)
    }

    /// Remove `text[start..end]` and clamp the cursor to the deletion
    /// point. Used by the Emacs-style edits in [`Self::handle_insert`].
    pub(crate) fn delete_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.content.text.len() {
            return;
        }
        self.content.text.drain(start..end);
        if self.content.cursor >= end {
            self.content.cursor -= end - start;
        } else if self.content.cursor > start {
            self.content.cursor = start;
        }
    }

    /// Delete `start..end` like [`Self::delete_range`], but first
    /// snapshot for undo and push the removed text onto the kill ring
    /// so Ctrl+Y can yank it back. Used by the Emacs line/word kills
    /// (Ctrl+W / Ctrl+U / Ctrl+K, Alt+Backspace, Alt+d). Empty or
    /// invalid ranges are a no-op and do not touch the ring.
    pub(crate) fn kill_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.content.text.len() {
            return;
        }
        self.snapshot_for_undo();
        let killed = self.content.text[start..end].to_owned();
        self.content.kill_ring.push(killed);
        if self.content.kill_ring.len() > KILL_RING_MAX {
            self.content.kill_ring.remove(0);
        }
        self.delete_range(start, end);
    }

    /// Insert the most recent kill-ring entry at the cursor (Emacs
    /// Ctrl+Y). A no-op when the ring is empty. Snapshots for undo and
    /// leaves the cursor just past the inserted text.
    pub(crate) fn yank_kill(&mut self) {
        let Some(text) = self.content.kill_ring.last().cloned() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.snapshot_for_undo();
        self.content.text.insert_str(self.content.cursor, &text);
        self.content.cursor += text.len();
    }

    /// Read-only view of the kill ring, oldest first. Test/inspection
    /// aid; the most recent entry is what Ctrl+Y yanks.
    #[cfg(test)]
    pub(crate) fn kill_ring(&self) -> &[String] {
        &self.content.kill_ring
    }
}
