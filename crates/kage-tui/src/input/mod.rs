//! Modal input state machine: Normal / Insert.
//!
//! Vim-flavoured key handling: `i` enters insert, `Esc` returns to
//! normal, `j`/`k`/`gg`/`G` scroll the buffer, `y` copies the active
//! screen-cell selection, `/` starts a search, `:` starts a command.
//! Each key resolves to zero or more [`InputAction`]s the host applies
//! to its [`crate::Buffer`] / clipboard / command runner.
//!
//! The state machine is intentionally pure: input is a [`KeyEvent`],
//! output is a [`Vec<InputAction>`]. Side effects live in the host.
//!
//! Selection lives outside this state machine: the host paints a
//! cell-based screen overlay driven by mouse drag. `Esc` clears the
//! selection through [`InputAction::ClearSelection`].

pub(crate) use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Editing mode of the input area.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    /// Vim-style normal mode: navigation, folding, mode entry.
    #[default]
    Normal,
    /// Free-text insert mode for the prompt input.
    Insert,
    /// Keyboard-driven cell selection. The host paints a screen
    /// overlay anchored at the position the user pressed `v`; this
    /// mode's keys (`h`/`j`/`k`/`l`, arrows, `y`, `Esc`) move the
    /// cursor end of that selection or finalise it.
    Visual,
}

/// Vim-style window focus: which "pane" Normal-mode keys (and the
/// hardware cursor) belong to. `Pane::Input` is the bordered input
/// card at the bottom of the screen; `Pane::Buffer` is the read-only
/// conversation buffer above it. `Ctrl-w` toggles between them, and
/// mouse clicks inside a region focus that pane.
///
/// The pane is purely a routing flag in Stage B (PR-2): keystrokes
/// behave the same regardless. Stage C (vim ops on the input pane)
/// uses it to decide whether `j`/`k` scrolls the buffer or moves the
/// input cursor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pane {
    /// The bordered input card. Default focus: text and cursor live
    /// here so the user can type immediately on startup.
    #[default]
    Input,
    /// The conversation buffer. Block focus and scroll act on this
    /// pane's content.
    Buffer,
}

impl Pane {
    /// Return the other pane. Stage B's `Ctrl-w` toggles using this.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Input => Self::Buffer,
            Self::Buffer => Self::Input,
        }
    }
}

/// Side effect produced by a key press.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputAction {
    /// Mode transition; host updates its rendering accordingly.
    EnterMode(Mode),
    /// Submit the input buffer as a user prompt; clears the buffer.
    Submit(String),
    /// Scroll the conversation buffer by `delta` lines (negative = up).
    Scroll(i32),
    /// Snap the conversation scroll to the top.
    ScrollToTop,
    /// Snap the conversation scroll to the bottom.
    ScrollToBottom,
    /// Toggle the fold state of the focused block.
    ToggleFold,
    /// Open every foldable block.
    UnfoldAll,
    /// Close every foldable block.
    FoldAll,
    /// Begin a `:` command. Host opens a command line.
    BeginCommand,
    /// Begin a `/` search. Host opens a search overlay.
    BeginSearch,
    /// Yank the active screen-cell selection into the OSC52
    /// clipboard. The host walks its rendered cell grid to extract
    /// the text, so the copy matches what's painted (including
    /// across tool blocks).
    Yank,
    /// Drop the active screen-cell selection without copying.
    /// Triggered by `Esc` when the user wants to abandon a drag.
    ClearSelection,
    /// Cancel the in-flight turn.
    Cancel,
    /// Open the in-TUI model picker overlay.
    OpenModelPicker,
    /// Open the in-TUI session picker overlay so the user can resume
    /// an earlier conversation in place.
    OpenSessionPicker,
    /// Move focus to the previous (older) foldable block.
    FocusPrev,
    /// Move focus to the next (newer) foldable block.
    FocusNext,
    /// Open the slash command palette overlay (leading `/` typed in
    /// Insert mode against an empty prompt).
    OpenCommandPalette,
    /// Jump focus to the next block matching the active search.
    SearchNext,
    /// Jump focus to the previous block matching the active search.
    SearchPrev,
    /// Enter keyboard visual select. The host anchors a selection at
    /// a sensible position (the focused block's first visible row)
    /// and switches mode to [`Mode::Visual`].
    EnterVisual,
    /// Move the visual cursor by one column to the left.
    VisualLeft,
    /// Move the visual cursor by one column to the right.
    VisualRight,
    /// Move the visual cursor up by one row.
    VisualUp,
    /// Move the visual cursor down by one row.
    VisualDown,
    /// Snap the visual cursor to the start of the current row.
    VisualLineStart,
    /// Snap the visual cursor to the end of the current row.
    VisualLineEnd,
    /// Yank the entire content of the focused block (vim-style
    /// `Y`). Selection state is left alone.
    YankFocusedBlock,
    /// Toggle vim-style window focus between the input card and the
    /// conversation buffer (`Ctrl-w` in Normal mode).
    CyclePane,
    /// Force focus to a specific pane (e.g. mouse click in a
    /// particular region).
    FocusPane(Pane),
    /// Cycle the active thinking level one step forward (`Shift+Tab`
    /// in any mode). The host advances its tracked level and forwards
    /// the new value on the next provider request.
    CycleThinkingLevel,
}

/// Cap on retained history entries. The host's persistence layer is
/// expected to truncate to the same bound when it serializes.
pub const HISTORY_MAX: usize = 1000;

/// Cap on retained undo / redo entries. Each entry stores a complete
/// snapshot of the input text plus cursor position; capping keeps
/// memory bounded for pathological inputs.
const UNDO_MAX: usize = 100;

/// Maximum entries kept in the Emacs kill ring. Older kills fall off
/// the bottom; Ctrl+Y always yanks the most recent.
const KILL_RING_MAX: usize = 60;

/// A bracketed paste of at least this many lines is collapsed to a
/// `[paste #N: M lines]` placeholder in the draft until the user
/// expands it (Ctrl+O) or submits (submission always sends the full
/// text). Keeps a multi-hundred-line paste from flooding the input.
const PASTE_COLLAPSE_LINES: usize = 10;

/// One step on the undo or redo stack. We snapshot full text +
/// cursor rather than diff-encode because input bodies are small
/// (capped to a handful of KB by the host) and snapshot semantics
/// make undo deterministic regardless of which mutation produced
/// the change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditSnapshot {
    text: String,
    cursor: usize,
}

/// A large bracketed paste held out of the visible draft. The draft
/// shows `[paste #id: lines lines]`; the real text is restored when
/// the user expands (Ctrl+O) or submits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PasteBlob {
    id: u32,
    text: String,
    lines: usize,
}

impl PasteBlob {
    /// The exact ASCII placeholder token that stands in for this blob
    /// in the draft. Resolution matches it verbatim, so editing into
    /// it simply drops the substitution (the literal token is sent).
    fn placeholder(&self) -> String {
        format!("[paste #{}: {} lines]", self.id, self.lines)
    }
}

/// Vim-style operator pending after `d`, `c`, or `y`. Combines with
/// a motion or a doubled key (`dd`, `cc`, `yy`) to act on a range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operator {
    /// `d` - delete the range, save to register.
    Delete,
    /// `c` - delete the range, save to register, enter Insert.
    Change,
    /// `y` - copy the range to register, leave cursor and text.
    Yank,
}

impl Operator {
    fn from_key(c: char) -> Option<Self> {
        match c {
            'd' => Some(Self::Delete),
            'c' => Some(Self::Change),
            'y' => Some(Self::Yank),
            _ => None,
        }
    }

    fn double_key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Change => 'c',
            Self::Yank => 'y',
        }
    }
}

/// Tracking the editing mode, the prompt text, the prompt history, and
/// any pending leader key (e.g. `g` waiting for the second `g` of `gg`).
#[derive(Debug)]
pub struct InputState {
    mode: Mode,
    text: String,
    cursor: usize,
    pending: Option<char>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_stash: Option<String>,
    focused_pane: Pane,
    /// Vim operator awaiting a motion or doubled key. When set, the
    /// next keystroke either resolves the operator (motion / linewise
    /// `dd`-style / Esc cancel) or extends the count.
    pending_op: Option<Operator>,
    /// Count accumulated from leading digits before an operator or
    /// motion. `Some(3)` after pressing `3`, `Some(15)` after `15`.
    /// Multiplies whatever follows; reset after the action runs.
    pending_count: Option<usize>,
    /// `true` after `r` was pressed; the next character literally
    /// replaces the char at the cursor.
    awaiting_replace: bool,
    /// Last yanked / cut text. Inserted by `p` / `P` (Stage C.4).
    register: String,
    /// `true` when `register` was filled by a linewise op (`dd`, `yy`,
    /// etc.), so `p` pastes on a new line below the cursor instead of
    /// inserting inline.
    register_linewise: bool,
    /// Anchor byte offset of an active input-pane char-visual
    /// selection. `None` outside Visual mode and during buffer-cell
    /// visual; `Some(n)` while the user is dragging a vim-style range
    /// across the input text. Stage C.5 uses this to disambiguate
    /// "v in input pane" (inline selection) from "v in buffer pane"
    /// (today's cell-overlay selection).
    visual_anchor: Option<usize>,
    /// Undo stack: snapshots taken before each mutating op. Vim
    /// groups one Insert session as a single undo unit, so the
    /// snapshot is taken once at insert-entry time, not per
    /// keystroke.
    undo_stack: Vec<EditSnapshot>,
    /// Redo stack: filled by [`Self::undo`], cleared by any new
    /// mutation. Vim's `<C-r>` pops from here.
    redo_stack: Vec<EditSnapshot>,
    /// When true the editor is non-modal (`[ui] editor = "modeless"`):
    /// it never leaves an insert-like state, `Esc` cancels the turn,
    /// and `PageUp` / `PageDown` scroll the buffer. Set by the host
    /// from config / the settings dialog.
    modeless: bool,
    /// Emacs kill ring. Ctrl+W / Ctrl+U / Ctrl+K and the Alt word
    /// kills push here; Ctrl+Y yanks the most recent entry. Capped at
    /// [`KILL_RING_MAX`]; empty kills are not recorded.
    kill_ring: Vec<String>,
    /// Collapsed large pastes, keyed by the placeholder embedded in
    /// the draft. Resolved back to full text on submit (or inline via
    /// Ctrl+O). Empty in the common case.
    pastes: Vec<PasteBlob>,
    /// Monotonic id for the next collapsed paste, so placeholders stay
    /// unique within a draft even after edits.
    next_paste_id: u32,
    /// Images queued for the next prompt (file/path/clipboard), each
    /// paired with the id embedded in its `[image #N ...]` prompt
    /// marker. Reconciled against the marker on submit so editing the
    /// marker out removes the image; survivors become `Content::Image`.
    attached: Vec<(u32, crate::image::AttachedImage)>,
    /// Monotonic id for the next image marker, unique within a draft.
    next_image_id: u32,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            mode: Mode::Insert,
            text: String::new(),
            cursor: 0,
            pending: None,
            history: Vec::new(),
            history_cursor: None,
            history_stash: None,
            focused_pane: Pane::default(),
            pending_op: None,
            pending_count: None,
            awaiting_replace: false,
            register: String::new(),
            register_linewise: false,
            visual_anchor: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            modeless: false,
            kill_ring: Vec::new(),
            pastes: Vec::new(),
            next_paste_id: 1,
            attached: Vec::new(),
            next_image_id: 1,
        }
    }
}

mod edit;
mod keys;
mod motion;
mod paste;
mod state;
mod visual;

/// Byte offset on `line` where the visual column `col` falls. Clamps to
/// the line's length when the column is past the end.
fn byte_offset_at_column(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map_or(line.len(), |(idx, _)| idx)
}

/// True for "word constituent" chars in the readline / Emacs sense:
/// alphanumerics plus underscore. Word boundary motions (`Alt+b`,
/// `Alt+f`) and `Alt+Backspace` / `Alt+d` use this.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Char immediately before byte position `byte`, plus its UTF-8 width.
/// `None` when `byte == 0`.
fn prev_char(text: &str, byte: usize) -> Option<(char, usize)> {
    text[..byte].chars().next_back().map(|c| (c, c.len_utf8()))
}

/// Char at byte position `byte`, plus its UTF-8 width. `None` at
/// end-of-text.
fn char_at(text: &str, byte: usize) -> Option<(char, usize)> {
    text[byte..].chars().next().map(|c| (c, c.len_utf8()))
}

/// Byte offset of the last char before `end_exclusive`. Used by the
/// paste path to land the cursor on the final char of the pasted
/// region. Returns `end_exclusive` when there is no preceding char
/// (empty range, multi-byte boundary at the start of text).
fn last_char_offset(text: &str, end_exclusive: usize) -> usize {
    if end_exclusive == 0 {
        return 0;
    }
    text[..end_exclusive]
        .char_indices()
        .next_back()
        .map_or(end_exclusive, |(idx, _)| idx)
}

/// Position of the start of the word containing or immediately
/// preceding `cursor`. Walks left over non-word chars first, then
/// over word chars, mirroring readline's `backward-word`.
pub(crate) fn backward_word_start(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if is_word_char(c) {
            break;
        }
        i -= w;
    }
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if !is_word_char(c) {
            break;
        }
        i -= w;
    }
    i
}

/// Position one past the end of the word containing or immediately
/// following `cursor`. Walks right over non-word chars first, then
/// over word chars, mirroring readline's `forward-word`.
pub(crate) fn forward_word_end(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while let Some((c, w)) = char_at(text, i) {
        if is_word_char(c) {
            break;
        }
        i += w;
    }
    while let Some((c, w)) = char_at(text, i) {
        if !is_word_char(c) {
            break;
        }
        i += w;
    }
    i
}

/// Position of the start of the run of non-whitespace immediately
/// before `cursor`, mirroring readline's `unix-word-rubout` (the one
/// `Ctrl+W` uses in shells: it splits on whitespace only, so
/// `foo-bar` is one word).
pub(crate) fn unix_word_rubout_start(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if !c.is_whitespace() {
            break;
        }
        i -= w;
    }
    while i > 0 {
        let Some((c, w)) = prev_char(text, i) else {
            break;
        };
        if c.is_whitespace() {
            break;
        }
        i -= w;
    }
    i
}

/// Byte offset of the start of the visual line (between newlines)
/// containing `cursor`.
pub(crate) fn current_line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte offset of the end of the visual line (between newlines)
/// containing `cursor`. The newline itself is not included.
pub(crate) fn current_line_end(text: &str, cursor: usize) -> usize {
    text[cursor..].find('\n').map_or(text.len(), |i| cursor + i)
}

/// Byte offset of the first non-whitespace char at or after `start`,
/// staying on the same logical line. Used by vim's `^` motion.
pub(crate) fn first_non_whitespace_at(text: &str, start: usize) -> usize {
    let mut i = start;
    while let Some((c, w)) = char_at(text, i) {
        if c == '\n' || !c.is_whitespace() {
            return i;
        }
        i += w;
    }
    i
}

/// Vim-style `w`: forward to the start of the next word. From the
/// cursor's current word, skip remaining word chars, then skip
/// non-word chars (whitespace and punctuation), and land on the
/// first char of the next word.
pub(crate) fn vim_word_forward(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while let Some((c, w)) = char_at(text, i) {
        if !is_word_char(c) {
            break;
        }
        i += w;
    }
    while let Some((c, w)) = char_at(text, i) {
        if is_word_char(c) {
            break;
        }
        i += w;
    }
    i
}

/// Vim-style `e`: forward to the *end* of the current word. Vim
/// places the cursor on the last char of the word (not after it),
/// so the byte offset returned is the last word-char's start, not
/// the position past it.
pub(crate) fn vim_word_end(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return cursor;
    }
    let mut i = cursor;
    if let Some((_, w)) = char_at(text, i) {
        i += w;
    }
    while let Some((c, w)) = char_at(text, i) {
        if is_word_char(c) {
            break;
        }
        i += w;
    }
    let mut last_word_pos = i;
    while let Some((c, w)) = char_at(text, i) {
        if !is_word_char(c) {
            break;
        }
        last_word_pos = i;
        i += w;
    }
    last_word_pos
}

/// Scan `s` for `[image #<digits> ...]` markers and collect the ids.
/// A marker runs from `[image #` to the next `]`; content between the
/// digits and `]` is ignored (it is just the human label/size).
fn image_marker_ids(s: &str) -> std::collections::HashSet<u32> {
    let mut ids = std::collections::HashSet::new();
    let mut rest = s;
    while let Some(start) = rest.find("[image #") {
        let after = &rest[start + "[image #".len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Some(end) = after.find(']')
            && let Ok(id) = digits.parse::<u32>()
        {
            ids.insert(id);
            rest = &after[end + 1..];
        } else {
            rest = after;
        }
    }
    ids
}

/// Absolute byte spans of every `[image #<digits> ...]` marker in
/// `s` as `(start, end, id)`, where `end` is just past the closing
/// `]` plus one trailing space if present - i.e. exactly the slice
/// [`strip_image_markers`] would remove. Used to treat a chip as one
/// atomic block for cursor-aware delete and highlight.
fn image_marker_spans(s: &str) -> Vec<(usize, usize, u32)> {
    let mut spans = Vec::new();
    let mut base = 0usize;
    while let Some(rel) = s[base..].find("[image #") {
        let open = base + rel;
        let after_at = open + "[image #".len();
        let after = &s[after_at..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        match (after.find(']'), digits.parse::<u32>()) {
            (Some(close_rel), Ok(id)) => {
                let mut end = after_at + close_rel + 1;
                if s[end..].starts_with(' ') {
                    end += 1;
                }
                spans.push((open, end, id));
                base = end;
            }
            _ => base = after_at,
        }
    }
    spans
}

/// Remove every `[image #<digits> ...]` marker (and one trailing
/// space if present) from `s`, leaving the user's prose for the
/// model. Non-marker `[...]` text is left untouched.
fn strip_image_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(start) = rest.find("[image #") else {
            out.push_str(rest);
            break;
        };
        let after = &rest[start + "[image #".len()..];
        let has_digit = after.starts_with(|c: char| c.is_ascii_digit());
        match after.find(']') {
            Some(end) if has_digit => {
                out.push_str(&rest[..start]);
                let mut tail = &after[end + 1..];
                tail = tail.strip_prefix(' ').unwrap_or(tail);
                rest = tail;
            }
            _ => {
                // Not a real marker; keep the literal `[image #`.
                out.push_str(&rest[..start + "[image #".len()]);
                rest = after;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
