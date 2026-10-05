//! State accessors, undo/redo, and prompt history.

use super::*;

impl InputState {
    /// Construct a state in [`Mode::Insert`] with an empty prompt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force the mode to [`Mode::Normal`] without emitting any
    /// [`InputAction`]. Used by tests that need to start in Normal.
    #[cfg(test)]
    pub(crate) fn force_normal(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Current editing mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Record whether a sent prompt waits in the engine queue. While
    /// one does, `Up` on the draft's top row asks for it back
    /// ([`InputAction::RecallPrompt`]) instead of walking the history.
    pub fn set_recallable(&mut self, on: bool) {
        self.recallable = on;
    }

    /// Switch between vim-modal and non-modal (modeless) editing.
    /// Turning modeless on snaps the editor into the insert-like
    /// state and keeps it there; `Esc` then clears the draft or
    /// interrupts the turn rather than entering Normal. Live-applicable from the settings
    /// dialog.
    pub fn set_modeless(&mut self, on: bool) {
        self.modeless = on;
        if on {
            self.mode = Mode::Insert;
        }
    }

    /// Whether the grammar is waiting for the rest of a command: a
    /// `g` or `z` prefix, an operator, the character after `r`, or
    /// post-operator count digits. The host then passes the next key
    /// straight to [`Self::handle_key`] instead of looking it up as a
    /// keymap.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
            || self.pending_op.is_some()
            || self.pending_replace.is_some()
            || self.pending_op_count.is_some()
    }

    /// Whether the editor is in non-modal mode.
    #[must_use]
    pub fn is_modeless(&self) -> bool {
        self.modeless
    }

    /// Whether shell-escape mode is armed (`!` typed on an empty
    /// prompt). The next submit runs as a shell command.
    #[must_use]
    pub fn shell_armed(&self) -> bool {
        self.content.shell
    }

    /// Current prompt-input text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.content.text
    }

    /// Queue an image and drop an editable `[{IMAGE_MARK_SENTINEL}image
    /// #N ...]` chip into the prompt at the cursor. The chip is text
    /// branded with a zero-width sentinel, so typed `[image #N]`
    /// lookalikes are never mistaken for it. Deleting the chip
    /// (backspace / edit) removes the image, and the reconcile on
    /// submit drops any image whose chip is gone.
    pub fn attach_image(&mut self, image: crate::image::AttachedImage) {
        let id = self.content.next_image_id;
        self.content.next_image_id = self.content.next_image_id.wrapping_add(1);
        let marker = format!("[{IMAGE_MARK_SENTINEL}image #{id} {}] ", image.summary());
        self.content.text.insert_str(self.content.cursor, &marker);
        self.content.cursor += marker.len();
        self.content.attached.push((id, image));
        self.content.undo_dirty = true;
    }

    /// Queued images still referenced by a chip in the prompt.
    #[must_use]
    pub fn attached(&self) -> &[(u32, crate::image::AttachedImage)] {
        &self.content.attached
    }

    /// Take every queued image and clear the queue. This does NOT
    /// reconcile against the prompt text: dropping the images whose
    /// chips the user deleted is [`Self::take_draft`]'s job and must
    /// run first (the host calls it on submit).
    pub fn take_attached(&mut self) -> Vec<crate::image::AttachedImage> {
        std::mem::take(&mut self.content.attached)
            .into_iter()
            .map(|(_, img)| img)
            .collect()
    }

    /// Put `draft` in the editor and return the draft it held. A
    /// pending command, an input selection and history browsing start
    /// over. The whole per-view [`Draft`] swaps as one unit, so no
    /// field can be forgotten on a new addition.
    pub(crate) fn swap_draft(&mut self, draft: Draft) -> Draft {
        self.pending = None;
        self.pending_op = None;
        self.pending_count = None;
        self.pending_op_count = None;
        self.pending_replace = None;
        self.visual_anchor = None;
        self.reset_history_navigation();
        std::mem::replace(&mut self.content, draft)
    }

    /// Byte offset of the cursor in the prompt text.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.content.cursor
    }

    /// Move the cursor to byte offset `at`. Ignored when `at` is past
    /// the end or not on a `char` boundary.
    pub(crate) fn set_cursor(&mut self, at: usize) {
        if self.content.text.is_char_boundary(at) {
            self.content.cursor = at;
        }
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

    /// Push the current `(text, cursor)` onto the undo stack and
    /// drop the redo stack (any forward history is invalidated by
    /// taking a new branch). Call this *before* a mutation so undo
    /// can return to the pre-mutation state. Marks the live state as
    /// diverged from the stack top so the next typed char opens a
    /// fresh undo unit.
    pub(crate) fn snapshot_for_undo(&mut self) {
        self.content.undo_stack.push(EditSnapshot {
            text: self.content.text.clone(),
            cursor: self.content.cursor,
        });
        if self.content.undo_stack.len() > UNDO_MAX {
            self.content.undo_stack.remove(0);
        }
        self.content.redo_stack.clear();
        self.content.undo_dirty = true;
    }

    /// Baseline a typing run: snapshot unless the stack top already
    /// matches the live state, so an insert session records one
    /// pre-typing unit instead of one per keystroke. A snapshot a
    /// previous insert-entry (`i`/`a`/...) just took matches and is
    /// reused rather than duplicated.
    pub(crate) fn snapshot_for_typing(&mut self) {
        if self.content.undo_dirty
            && self.content.undo_stack.last().is_none_or(|snap| {
                snap.text != self.content.text || snap.cursor != self.content.cursor
            })
        {
            self.snapshot_for_undo();
        }
        self.content.undo_dirty = false;
    }

    /// Pop one undo snapshot, push the current state to redo, and
    /// restore the popped state. Skips snapshots that match the
    /// current state (which can happen when an Insert session
    /// produced no actual mutations).
    pub fn undo(&mut self) {
        while let Some(snap) = self.content.undo_stack.pop() {
            if snap.text == self.content.text && snap.cursor == self.content.cursor {
                continue;
            }
            self.content.redo_stack.push(EditSnapshot {
                text: std::mem::take(&mut self.content.text),
                cursor: self.content.cursor,
            });
            self.content.text = snap.text;
            self.content.cursor = snap.cursor;
            self.content.undo_dirty = true;
            return;
        }
    }

    /// Pop one redo snapshot, push the current state to undo, and
    /// restore the popped state.
    pub fn redo(&mut self) {
        if let Some(snap) = self.content.redo_stack.pop() {
            self.content.undo_stack.push(EditSnapshot {
                text: std::mem::take(&mut self.content.text),
                cursor: self.content.cursor,
            });
            self.content.text = snap.text;
            self.content.cursor = snap.cursor;
            self.content.undo_dirty = true;
        }
    }

    /// Replace the byte range `start..end` of the prompt with
    /// `replacement` and move the cursor to just past the inserted
    /// text. Used by the autocomplete popup to accept a candidate.
    ///
    /// Returns `false` without mutating when the range is out of
    /// bounds, inverted, or not on `char` boundaries, so a bad range
    /// from a plugin provider degrades to a no-op rather than a panic.
    /// A successful splice records one undo snapshot.
    pub fn splice(&mut self, start: usize, end: usize, replacement: &str) -> bool {
        if start > end || end > self.content.text.len() {
            return false;
        }
        if !self.content.text.is_char_boundary(start) || !self.content.text.is_char_boundary(end) {
            return false;
        }
        self.snapshot_for_undo();
        self.content.text.replace_range(start..end, replacement);
        self.content.cursor = start + replacement.len();
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

    pub(crate) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.history_stash = Some(self.content.text.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(idx) => idx - 1,
        };
        self.history_cursor = Some(next);
        self.content.text.clone_from(&self.history[next]);
        self.content.cursor = self.content.text.len();
        self.content.undo_dirty = true;
    }

    pub(crate) fn history_next(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.history.len() {
            let next = idx + 1;
            self.history_cursor = Some(next);
            self.content.text.clone_from(&self.history[next]);
            self.content.cursor = self.content.text.len();
        } else {
            self.history_cursor = None;
            self.content.text = self.history_stash.take().unwrap_or_default();
            self.content.cursor = self.content.text.len();
        }
        self.content.undo_dirty = true;
    }

    pub(crate) fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_stash = None;
    }

    /// Whether Up/Down is currently walking the prompt history
    /// ([`Self::history_prev`]) rather than editing the draft. While
    /// this holds, the arrow keys belong to history, so the host keeps
    /// the completion popup out of the way.
    pub(crate) fn history_browsing(&self) -> bool {
        self.history_cursor.is_some()
    }
}
