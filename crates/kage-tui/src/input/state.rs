//! State accessors, undo/redo, and prompt history.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
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

    /// Switch between vim-modal and non-modal (modeless) editing.
    /// Turning modeless on snaps the editor into the insert-like
    /// state and keeps it there; `Esc` then cancels the turn rather
    /// than entering Normal. Live-applicable from the settings
    /// dialog.
    pub fn set_modeless(&mut self, on: bool) {
        self.modeless = on;
        if on {
            self.mode = Mode::Insert;
        }
    }

    /// Whether the editor is in non-modal mode.
    #[must_use]
    pub fn is_modeless(&self) -> bool {
        self.modeless
    }

    /// Current prompt-input text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Queue an image and drop an editable `[image #N ...]` marker
    /// into the prompt at the cursor. The marker is ordinary text:
    /// deleting it (backspace / edit) removes the image, and the
    /// reconcile on submit drops any image whose marker is gone.
    pub fn attach_image(&mut self, image: crate::image::AttachedImage) {
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1);
        let marker = format!("[image #{id} {}] ", image.summary());
        self.text.insert_str(self.cursor, &marker);
        self.cursor += marker.len();
        self.attached.push((id, image));
    }

    /// Queued images still referenced by a marker in the prompt.
    #[must_use]
    pub fn attached(&self) -> &[(u32, crate::image::AttachedImage)] {
        &self.attached
    }

    /// Reconcile against `text`: drop any image whose `[image #N ...]`
    /// marker the user deleted, take the survivors, and clear the
    /// queue. Returns the images to send.
    pub fn take_attached(&mut self) -> Vec<crate::image::AttachedImage> {
        std::mem::take(&mut self.attached)
            .into_iter()
            .map(|(_, img)| img)
            .collect()
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

    /// Push the current `(text, cursor)` onto the undo stack and
    /// drop the redo stack (any forward history is invalidated by
    /// taking a new branch). Call this *before* a mutation so undo
    /// can return to the pre-mutation state.
    pub(crate) fn snapshot_for_undo(&mut self) {
        self.undo_stack.push(EditSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        if self.undo_stack.len() > UNDO_MAX {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// Pop one undo snapshot, push the current state to redo, and
    /// restore the popped state. Skips snapshots that match the
    /// current state (which can happen when an Insert session
    /// produced no actual mutations).
    pub fn undo(&mut self) {
        while let Some(snap) = self.undo_stack.pop() {
            if snap.text == self.text && snap.cursor == self.cursor {
                continue;
            }
            self.redo_stack.push(EditSnapshot {
                text: std::mem::take(&mut self.text),
                cursor: self.cursor,
            });
            self.text = snap.text;
            self.cursor = snap.cursor;
            return;
        }
    }

    /// Pop one redo snapshot, push the current state to undo, and
    /// restore the popped state.
    pub fn redo(&mut self) {
        if let Some(snap) = self.redo_stack.pop() {
            self.undo_stack.push(EditSnapshot {
                text: std::mem::take(&mut self.text),
                cursor: self.cursor,
            });
            self.text = snap.text;
            self.cursor = snap.cursor;
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
        if start > end || end > self.text.len() {
            return false;
        }
        if !self.text.is_char_boundary(start) || !self.text.is_char_boundary(end) {
            return false;
        }
        self.snapshot_for_undo();
        self.text.replace_range(start..end, replacement);
        self.cursor = start + replacement.len();
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

    pub(crate) fn history_next(&mut self) {
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

    pub(crate) fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_stash = None;
    }
}
