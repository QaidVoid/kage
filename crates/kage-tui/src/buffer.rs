//! Conversation buffer model.
//!
//! [`Buffer`] is an append-only list of [`Block`]s plus a scroll offset.
//! It is the source of truth the TUI's render loop walks each frame: the
//! buffer maps a streamed [`kage_core::LoopEvent`] timeline (assembled
//! by the host's `Hooks` impl) into discrete blocks the renderer can
//! lay out.
//!
//! Folding state lives on each block so the user can collapse thinking
//! blocks and tool calls without losing their content.

use std::mem;
use std::sync::Arc;
use std::time::Instant;

use ratatui::text::Line;

/// One renderable region of the conversation.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// User prompt.
    User {
        /// Raw user text. May contain newlines.
        text: String,
    },
    /// Assistant text response. The host appends streamed deltas with
    /// [`Buffer::append_assistant_delta`] until the turn ends.
    Assistant {
        /// Reassembled assistant text.
        text: String,
        /// Whether this block is still receiving deltas.
        live: bool,
    },
    /// Hidden chain-of-thought emitted by the model.
    Thinking {
        /// Reassembled thinking text.
        text: String,
        /// Whether the user has collapsed this block.
        folded: bool,
        /// Whether this block is still receiving deltas.
        live: bool,
    },
    /// One tool invocation by the assistant.
    ToolCall {
        /// Stable id from the provider; matches the corresponding
        /// [`Block::ToolResult`].
        call_id: String,
        /// Tool name as the model invoked it.
        name: String,
        /// One-line summary of the tool input shown in the folded
        /// header (e.g. `bash("ls -la")`).
        input_summary: String,
        /// Pretty-printed full input shown when expanded.
        input_pretty: String,
        /// Whether the user has collapsed the body.
        folded: bool,
        /// Wall-clock instant when the call was registered. Used by
        /// the renderer to compute and show duration once the matching
        /// [`Block::ToolResult`] arrives.
        started_at: Instant,
    },
    /// Output of a previously-issued tool call.
    ToolResult {
        /// Correlation id matching the prior [`Block::ToolCall`].
        call_id: String,
        /// Tool name, copied for header rendering.
        name: String,
        /// Stringified output.
        output: String,
        /// Whether the tool reported failure.
        is_error: bool,
        /// Whether the user has collapsed the body.
        folded: bool,
        /// Milliseconds elapsed between the matching call's
        /// `started_at` and when this result was pushed. `None` when
        /// the call was missing (orphan result).
        duration_ms: Option<u64>,
    },
    /// Plugin-defined block the core does not interpret.
    Custom {
        /// Plugin-defined kind tag, namespaced like `plugin:tps`.
        kind: String,
        /// Human-readable text the renderer shows verbatim.
        text: String,
        /// Whether the user has collapsed the body.
        folded: bool,
    },
}

impl Block {
    /// Count of logical (newline-separated) lines this block contributes
    /// when rendered. Folded blocks always contribute 1 (the header).
    /// Width-aware wrapping happens in the renderer.
    #[must_use]
    pub fn line_count(&self) -> usize {
        match self {
            Self::User { text } | Self::Assistant { text, .. } => count_lines(text),
            Self::Thinking { text, folded, .. } => {
                if *folded {
                    1
                } else {
                    1 + count_lines(text)
                }
            }
            Self::ToolCall {
                input_pretty,
                folded,
                ..
            } => {
                if *folded {
                    1
                } else {
                    1 + count_lines(input_pretty)
                }
            }
            Self::ToolResult { output, folded, .. } => {
                if *folded {
                    1
                } else {
                    1 + count_lines(output)
                }
            }
            Self::Custom { text, folded, .. } => {
                if *folded {
                    1
                } else {
                    count_lines(text)
                }
            }
        }
    }

    /// True if the block is collapsible (has a folded/unfolded toggle).
    #[must_use]
    pub fn is_foldable(&self) -> bool {
        matches!(
            self,
            Self::Thinking { .. }
                | Self::ToolCall { .. }
                | Self::ToolResult { .. }
                | Self::Custom { .. }
        )
    }

    /// Toggle the fold state. No-op for non-foldable blocks.
    pub fn toggle_fold(&mut self) {
        match self {
            Self::Thinking { folded, .. }
            | Self::ToolCall { folded, .. }
            | Self::ToolResult { folded, .. }
            | Self::Custom { folded, .. } => *folded = !*folded,
            _ => {}
        }
    }

    /// Mark a streaming block as no longer accepting deltas.
    pub fn finish(&mut self) {
        match self {
            Self::Assistant { live, .. } | Self::Thinking { live, .. } => *live = false,
            _ => {}
        }
    }
}

/// True if `haystack` contains `needle` ignoring ASCII case (a/A,
/// b/B, ...). Non-ASCII bytes are compared exactly. Allocates
/// nothing. Returns `false` for empty needles.
fn ascii_icontains(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    let limit = h.len() - n.len();
    'outer: for i in 0..=limit {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.split('\n').count()
}

/// Append-only conversation history with a scroll offset measured as
/// "rows scrolled up from the bottom". `scroll == 0` means the viewport
/// is pinned to the latest content (auto-follow on streaming); larger
/// values walk backwards through history. New content arriving while
/// the user is scrolled back leaves their position alone, so the
/// "follow while idle, freeze while reading" behavior emerges from the
/// scroll model rather than a separate flag.
#[derive(Debug, Default)]
pub struct Buffer {
    blocks: Vec<Block>,
    scroll: usize,
    /// Index of the user-selected foldable block, if any. `None` means
    /// "no explicit selection"; the renderer falls back to the last
    /// foldable block in the buffer for fold-toggle gestures.
    focus: Option<usize>,
    /// The focus value the renderer last painted. The renderer
    /// compares this to the current effective focus each frame; when
    /// they differ, it scrolls so the newly focused block is in view.
    last_drawn_focus: Option<usize>,
    /// Per-block rendered-height cache, indexed parallel to
    /// [`Self::blocks`]. Each entry stores `(width, height_in_rows)`
    /// captured by the renderer's last successful layout pass for
    /// that block. The renderer reuses cached entries whose `width`
    /// matches the current viewport width and otherwise rebuilds.
    /// Mutators push or invalidate entries in lockstep with `blocks`
    /// to avoid stale data; this is what lets virtualized rendering
    /// skip building [`ratatui::text::Line`]s for off-screen blocks.
    block_heights: Vec<Option<(u16, u16)>>,
    /// Per-block rendered-line cache, indexed parallel to
    /// [`Self::blocks`]. Each entry stores
    /// `(width, Arc<Vec<Line<'static>>>)` captured at the same time
    /// as [`Self::block_heights`]. Renderers reuse the lines when
    /// the block is unfocused (no emphasis-driven rebuild), turning
    /// the per-frame cost of a re-render into a `Vec<Line>` clone.
    /// Stored behind `Arc` so the mutex isn't holding a clone of a
    /// possibly-huge vector while the renderer is still using it.
    block_render_lines: Vec<Option<(u16, Arc<Vec<Line<'static>>>)>>,
    /// Monotonically increasing counter bumped by every mutation
    /// (push, append, fold, focus, scroll). The render loop reads
    /// this to decide whether to repaint: an unchanged version means
    /// nothing user-visible has shifted, so the previous frame is
    /// still correct and we can sleep instead of redrawing at the
    /// full 30 Hz target. Wraps at `u64::MAX`, which won't happen in
    /// any realistic session lifetime.
    version: u64,
    /// Map of "what block currently sits under each screen row in
    /// the buffer area": `(block_idx, screen_top, screen_bottom)` in
    /// absolute terminal coordinates. The renderer rewrites this
    /// each frame; mouse handlers read it to translate a click row
    /// into a block. Cleared whenever the buffer is empty.
    last_block_screen_rows: Vec<(usize, u16, u16)>,
    /// `(idx, virtual_top, virtual_bottom)` per painted block in the
    /// last frame, in the unclamped 0..total virtual-row space (the
    /// same space mouse-selection rows live in). Unlike
    /// [`Self::last_block_screen_rows`] this is *not* clamped to the
    /// viewport, so a block scrolled past its own top still reports
    /// its true first row - yank uses this to map a selected row to
    /// the right source line regardless of scroll.
    last_block_virtual_rows: Vec<(usize, usize, usize)>,
    /// Width and X-origin of the buffer area in the last painted
    /// frame. Mouse handlers use this to translate a click column
    /// into a block-relative char column.
    last_area_x: u16,
    last_area_width: u16,
    /// First virtual row (0-indexed across the whole rendered buffer)
    /// that was visible in the last painted frame. Mouse handlers
    /// add `screen_row - area_y` to this to get a stable virtual-row
    /// coordinate that survives subsequent scrolls; the renderer uses
    /// it the other way to translate a virtual row back to a screen
    /// row when painting selection overlay.
    last_virtual_top: usize,
    last_area_y: u16,
    last_area_height: u16,
}

impl Buffer {
    /// Construct an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only view of the blocks.
    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Total logical lines summed across all blocks.
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.blocks.iter().map(Block::line_count).sum()
    }

    /// Rows scrolled up from the bottom. Zero means "follow newest".
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// True when the viewport is pinned to the bottom; auto-follows
    /// streaming content.
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    /// Set the scroll offset (rows up from the bottom). No model-layer
    /// cap is applied here because the model doesn't know about line
    /// wrapping: a logical line may render as multiple visual rows
    /// once Paragraph wraps it. The renderer holds the authoritative
    /// max each frame and clamps there.
    pub fn set_scroll(&mut self, scroll: usize) {
        if self.scroll != scroll {
            self.scroll = scroll;
            self.bump_version();
        }
    }

    /// Currently focused foldable block index, if the user has
    /// explicitly selected one. Renderers that highlight a focused
    /// block should fall back to [`Self::effective_focus`] for
    /// "no selection but show something".
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Effective focus: the explicit user selection if any, otherwise
    /// the index of the last selectable block in the buffer. `None`
    /// when there are no selectable blocks at all.
    #[must_use]
    pub fn effective_focus(&self) -> Option<usize> {
        self.focus.or_else(|| self.last_selectable_index())
    }

    /// What focus value the renderer painted last frame. The renderer
    /// uses this to detect focus changes and auto-scroll the newly
    /// focused block into view.
    #[must_use]
    pub fn last_drawn_focus(&self) -> Option<usize> {
        self.last_drawn_focus
    }

    /// Renderer hook: stash the focus value used while painting this
    /// frame so the next frame can compare and react.
    pub fn set_last_drawn_focus(&mut self, value: Option<usize>) {
        self.last_drawn_focus = value;
    }

    /// Cached rendered height (in wrapped rows) for the block at
    /// `idx`, but only if the cache entry was captured at the given
    /// `width`. Width-mismatched entries return `None` so the caller
    /// recomputes and stores a fresh value. Out-of-range indices and
    /// uncached blocks also return `None`.
    #[must_use]
    pub fn cached_height(&self, idx: usize, width: u16) -> Option<u16> {
        self.block_heights
            .get(idx)
            .copied()
            .flatten()
            .and_then(|(w, h)| (w == width).then_some(h))
    }

    /// Renderer hook: store the wrapped-row height it just measured
    /// for the block at `idx` at the given `width`. Subsequent frames
    /// reuse this without rebuilding the block's [`Line`]s.
    pub fn set_cached_height(&mut self, idx: usize, width: u16, height: u16) {
        if let Some(slot) = self.block_heights.get_mut(idx) {
            *slot = Some((width, height));
        }
    }

    /// Drop every cached height. Called by the renderer when it sees
    /// a width change, since wrap counts depend on width.
    pub fn invalidate_all_heights(&mut self) {
        for slot in &mut self.block_heights {
            *slot = None;
        }
        for slot in &mut self.block_render_lines {
            *slot = None;
        }
    }

    /// Cached rendered lines for the block at `idx`, but only if the
    /// cache entry was captured at the given `width`. The lines were
    /// rendered with `Emphasis::None`; callers that need a focused or
    /// selection-emphasised render must rebuild.
    #[must_use]
    pub fn cached_render_lines(&self, idx: usize, width: u16) -> Option<Arc<Vec<Line<'static>>>> {
        self.block_render_lines
            .get(idx)
            .and_then(Clone::clone)
            .and_then(|(w, lines)| (w == width).then_some(lines))
    }

    /// Renderer hook: store the rendered lines it just built for the
    /// block at `idx`, paired with the width used. Held behind `Arc`
    /// so the renderer's emit pass can clone the handle without
    /// duplicating the line vector.
    pub fn set_cached_render_lines(
        &mut self,
        idx: usize,
        width: u16,
        lines: Arc<Vec<Line<'static>>>,
    ) {
        if let Some(slot) = self.block_render_lines.get_mut(idx) {
            *slot = Some((width, lines));
        }
    }

    /// Renderer hook: replace the absolute screen-row layout it just
    /// painted. The vec is sorted by `screen_top`; entries don't
    /// overlap. Used by mouse handlers to translate a click into a
    /// block index.
    pub fn set_last_block_screen_rows(&mut self, rows: Vec<(usize, u16, u16)>) {
        self.last_block_screen_rows = rows;
    }

    /// Renderer hook: stash each painted block's unclamped
    /// `(idx, virtual_top, virtual_bottom)` for this frame. See
    /// [`Self::last_block_virtual_rows`].
    pub fn set_last_block_virtual_rows(&mut self, rows: Vec<(usize, usize, usize)>) {
        self.last_block_virtual_rows = rows;
    }

    /// Unclamped `(virtual_top, virtual_bottom)` of the block at
    /// `idx` from the last frame, `bottom` exclusive. `None` when the
    /// block was not painted. Used by yank to map a selected row to
    /// the block's source line correctly under any scroll.
    #[must_use]
    pub fn block_virtual_rows(&self, idx: usize) -> Option<(usize, usize)> {
        self.last_block_virtual_rows
            .iter()
            .find_map(|(i, top, bot)| (*i == idx).then_some((*top, *bot)))
    }

    /// Renderer hook: stash the buffer area's bounding box and the
    /// virtual-row index of its first visible row. Mouse handlers
    /// add `screen_row - area_y` to `last_virtual_top` to recover a
    /// stable virtual-row coordinate that survives subsequent
    /// scrolls; the renderer reverses that to project a virtual row
    /// back to a screen row when painting selection overlay.
    pub fn set_last_area_geometry(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        virtual_top: usize,
    ) {
        self.last_area_x = x;
        self.last_area_y = y;
        self.last_area_width = width;
        self.last_area_height = height;
        self.last_virtual_top = virtual_top;
    }

    /// Width of the last-painted buffer area, in cells.
    #[must_use]
    pub fn last_area_width(&self) -> u16 {
        self.last_area_width
    }

    /// X-origin of the last-painted buffer area.
    #[must_use]
    pub fn last_area_x(&self) -> u16 {
        self.last_area_x
    }

    /// Y-origin of the last-painted buffer area.
    #[must_use]
    pub fn last_area_y(&self) -> u16 {
        self.last_area_y
    }

    /// Height of the last-painted buffer area, in cells.
    #[must_use]
    pub fn last_area_height(&self) -> u16 {
        self.last_area_height
    }

    /// Virtual-row index of the first visible row in the last
    /// painted frame.
    #[must_use]
    pub fn last_virtual_top(&self) -> usize {
        self.last_virtual_top
    }

    /// Find the block painted under absolute terminal row `y` from
    /// the most recent frame. Returns `None` when the row is outside
    /// any block (separator gap, empty buffer, off-screen) or when
    /// the renderer hasn't painted a frame yet.
    #[must_use]
    pub fn block_at_screen_row(&self, y: u16) -> Option<usize> {
        self.last_block_screen_rows
            .iter()
            .find_map(|(idx, top, bot)| (y >= *top && y < *bot).then_some(*idx))
    }

    /// Top screen row of the block at `idx` from the most recent
    /// frame. Used by mouse handlers to detect "click on header row"
    /// (whose row matches this top).
    #[must_use]
    pub fn screen_top_of(&self, idx: usize) -> Option<u16> {
        self.last_block_screen_rows
            .iter()
            .find_map(|(i, top, _)| (*i == idx).then_some(*top))
    }

    /// `(top, bottom)` screen-row range of the block at `idx` from
    /// the most recent frame, with `bottom` exclusive. `None` when
    /// the block is currently off-screen.
    #[must_use]
    pub fn screen_rows_of(&self, idx: usize) -> Option<(u16, u16)> {
        self.last_block_screen_rows
            .iter()
            .find_map(|(i, top, bot)| (*i == idx).then_some((*top, *bot)))
    }

    /// Current mutation counter. Render loops compare consecutive
    /// reads to decide if anything has changed and a repaint is
    /// warranted.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    fn push_block_caches(&mut self) {
        self.block_heights.push(None);
        self.block_render_lines.push(None);
        self.bump_version();
    }

    fn invalidate_last_block_caches(&mut self) {
        if let Some(slot) = self.block_heights.last_mut() {
            *slot = None;
        }
        if let Some(slot) = self.block_render_lines.last_mut() {
            *slot = None;
        }
        self.bump_version();
    }

    fn clear_block_caches(&mut self) {
        self.block_heights.clear();
        self.block_render_lines.clear();
        self.bump_version();
    }

    fn invalidate_height(&mut self, idx: usize) {
        if let Some(slot) = self.block_heights.get_mut(idx) {
            *slot = None;
        }
        if let Some(slot) = self.block_render_lines.get_mut(idx) {
            *slot = None;
        }
        self.bump_version();
    }

    fn invalidate_pair_height(&mut self, call_id: &str) {
        for (i, b) in self.blocks.iter().enumerate() {
            match b {
                Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. }
                    if cid == call_id =>
                {
                    if let Some(slot) = self.block_heights.get_mut(i) {
                        *slot = None;
                    }
                    if let Some(slot) = self.block_render_lines.get_mut(i) {
                        *slot = None;
                    }
                }
                _ => {}
            }
        }
    }

    /// True if block `idx` contains `needle` (ASCII case-insensitive,
    /// fallback to case-sensitive for non-ASCII). Empty needles never
    /// match.
    ///
    /// Uses byte-level matching with no allocation so the renderer
    /// can call this for every block on every frame without
    /// `to_lowercase()` blowing up on multi-MB tool outputs.
    #[must_use]
    pub fn block_contains(&self, idx: usize, needle: &str) -> bool {
        let needle = needle.trim();
        if needle.is_empty() {
            return false;
        }
        let Some(block) = self.blocks.get(idx) else {
            return false;
        };
        match block {
            Block::User { text } | Block::Assistant { text, .. } | Block::Thinking { text, .. } => {
                ascii_icontains(text, needle)
            }
            Block::ToolCall {
                name,
                input_summary,
                input_pretty,
                ..
            } => {
                ascii_icontains(name, needle)
                    || ascii_icontains(input_summary, needle)
                    || ascii_icontains(input_pretty, needle)
            }
            Block::ToolResult { name, output, .. } => {
                ascii_icontains(name, needle) || ascii_icontains(output, needle)
            }
            Block::Custom { kind, text, .. } => {
                ascii_icontains(kind, needle) || ascii_icontains(text, needle)
            }
        }
    }

    /// Verbatim source text of block `idx`, for clipboard yank: the
    /// raw assistant / user / thinking / custom text (the markdown
    /// *source*, not the syntect-rendered screen cells), a tool
    /// call's pretty-printed input, or a tool result's output.
    /// `None` for an out-of-range index.
    #[must_use]
    pub fn block_text(&self, idx: usize) -> Option<String> {
        let block = self.blocks.get(idx)?;
        Some(match block {
            Block::User { text }
            | Block::Assistant { text, .. }
            | Block::Thinking { text, .. }
            | Block::Custom { text, .. } => text.clone(),
            Block::ToolCall { input_pretty, .. } => input_pretty.clone(),
            Block::ToolResult { output, .. } => output.clone(),
        })
    }

    /// All block indices whose content contains `needle`, in buffer
    /// order. Skips merged tool-result halves.
    #[must_use]
    pub fn match_indices(&self, needle: &str) -> Vec<usize> {
        (0..self.blocks.len())
            .filter(|i| self.is_selectable(*i) && self.block_contains(*i, needle))
            .collect()
    }

    /// Index of the next block after `from` (exclusive) whose content
    /// contains `needle`. Skips merged tool-result halves.
    #[must_use]
    pub fn next_match(&self, from: usize, needle: &str) -> Option<usize> {
        (from + 1..self.blocks.len())
            .find(|i| self.is_selectable(*i) && self.block_contains(*i, needle))
    }

    /// Index of the previous block before `from` (exclusive) whose
    /// content contains `needle`.
    #[must_use]
    pub fn prev_match(&self, from: usize, needle: &str) -> Option<usize> {
        (0..from)
            .rev()
            .find(|i| self.is_selectable(*i) && self.block_contains(*i, needle))
    }

    /// Replace the explicit focus. `None` clears it (renderer falls
    /// back to the last selectable block). Out-of-range indices are
    /// silently dropped.
    pub fn set_focus(&mut self, idx: Option<usize>) {
        let new = idx.filter(|i| self.blocks.get(*i).is_some());
        if self.focus != new {
            self.focus = new;
            self.bump_version();
        }
    }

    /// Move focus to the previous (older) foldable block, skipping
    /// non-foldable kinds (User/Assistant). Returns `true` if focus
    /// changed.
    pub fn focus_prev(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.foldable_index_before(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                true
            }
            _ => false,
        }
    }

    /// Move focus to the next (newer) foldable block. Returns
    /// `true` if focus changed.
    pub fn focus_next(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.foldable_index_after(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                true
            }
            _ => false,
        }
    }

    /// Move focus to the previous selectable block, walking *every*
    /// kind (used by visual-mode head extension). Returns `true` if
    /// focus changed.
    pub fn focus_prev_any(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.selectable_index_before(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                true
            }
            _ => false,
        }
    }

    /// Move focus to the next selectable block, walking *every*
    /// kind. Returns `true` if focus changed.
    pub fn focus_next_any(&mut self) -> bool {
        let current = self.effective_focus();
        let Some(idx) = current else { return false };
        match self.selectable_index_after(idx) {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                true
            }
            _ => false,
        }
    }

    fn foldable_index_before(&self, idx: usize) -> Option<usize> {
        (0..idx)
            .rev()
            .find(|i| self.is_selectable(*i) && self.blocks[*i].is_foldable())
    }

    fn foldable_index_after(&self, idx: usize) -> Option<usize> {
        (idx + 1..self.blocks.len())
            .find(|i| self.is_selectable(*i) && self.blocks[*i].is_foldable())
    }

    /// Whether `idx` is something `[` / `]` should land on. Every
    /// block kind is selectable except a `ToolResult` whose matching
    /// `ToolCall` exists earlier in the buffer (the renderer merges
    /// the pair into one composite, so landing on the result would
    /// look like a no-op visual).
    fn is_selectable(&self, idx: usize) -> bool {
        match self.blocks.get(idx) {
            Some(Block::ToolResult { call_id, .. }) => !self.blocks[..idx]
                .iter()
                .any(|b| matches!(b, Block::ToolCall { call_id: cid, .. } if cid == call_id)),
            Some(_) => true,
            None => false,
        }
    }

    fn last_selectable_index(&self) -> Option<usize> {
        (0..self.blocks.len())
            .rev()
            .find(|i| self.is_selectable(*i))
    }

    fn selectable_index_before(&self, idx: usize) -> Option<usize> {
        (0..idx).rev().find(|i| self.is_selectable(*i))
    }

    fn selectable_index_after(&self, idx: usize) -> Option<usize> {
        (idx + 1..self.blocks.len()).find(|i| self.is_selectable(*i))
    }

    /// Push a fully-formed user prompt.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::User { text: text.into() });
        self.push_block_caches();
    }

    /// Begin a streaming assistant block. Subsequent deltas append to it
    /// via [`Self::append_assistant_delta`].
    pub fn begin_assistant(&mut self) {
        self.blocks.push(Block::Assistant {
            text: String::new(),
            live: true,
        });
        self.push_block_caches();
    }

    /// Append text to the most recent assistant block. If no live
    /// assistant block exists, a fresh one is started.
    pub fn append_assistant_delta(&mut self, delta: &str) {
        if !self.last_is_live_assistant() {
            self.begin_assistant();
        }
        if let Some(Block::Assistant { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
        self.invalidate_last_block_caches();
    }

    /// Begin a streaming thinking block.
    pub fn begin_thinking(&mut self) {
        self.blocks.push(Block::Thinking {
            text: String::new(),
            folded: false,
            live: true,
        });
        self.push_block_caches();
    }

    /// Append text to the most recent thinking block.
    pub fn append_thinking_delta(&mut self, delta: &str) {
        if !self.last_is_live_thinking() {
            self.begin_thinking();
        }
        if let Some(Block::Thinking { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
        self.invalidate_last_block_caches();
    }

    /// Add a tool-call block to the buffer.
    pub fn push_tool_call(
        &mut self,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input_summary: impl Into<String>,
        input_pretty: impl Into<String>,
    ) {
        self.blocks.push(Block::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input_summary: input_summary.into(),
            input_pretty: input_pretty.into(),
            folded: true,
            started_at: Instant::now(),
        });
        self.push_block_caches();
    }

    /// Insert a tool-call block, or refresh the existing one with the
    /// same `call_id` in place. Used for progressive argument
    /// streaming: the placeholder created from the first
    /// [`kage_core::LoopEvent::ToolCallArgsDelta`] is updated as more
    /// arguments arrive and finalized by the authoritative
    /// [`kage_core::LoopEvent::ToolCallStart`]. The fold state and
    /// start time of an existing block are preserved so the timer and
    /// the user's expand/collapse choice survive each refresh.
    pub fn upsert_tool_call(
        &mut self,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input_summary: impl Into<String>,
        input_pretty: impl Into<String>,
    ) {
        let call_id = call_id.into();
        let name = name.into();
        let input_summary = input_summary.into();
        let input_pretty = input_pretty.into();
        let mut found = false;
        for block in &mut self.blocks {
            if let Block::ToolCall {
                call_id: cid,
                name: n,
                input_summary: s,
                input_pretty: p,
                ..
            } = block
                && *cid == call_id
            {
                n.clone_from(&name);
                s.clone_from(&input_summary);
                p.clone_from(&input_pretty);
                found = true;
                break;
            }
        }
        if found {
            self.invalidate_pair_height(&call_id);
        } else {
            self.push_tool_call(call_id, name, input_summary, input_pretty);
        }
    }

    /// Add a tool-result block. Looks up the matching tool call (by id)
    /// and copies its name into the result so the renderer can display
    /// the output under the right header. Records the elapsed time
    /// since the call was issued so the renderer can show `Took 12ms`.
    pub fn push_tool_result(
        &mut self,
        call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) {
        let call_id_owned = call_id.into();
        let mut duration_ms = None;
        for block in self.blocks.iter().rev() {
            if let Block::ToolCall {
                call_id: cid,
                started_at,
                ..
            } = block
                && cid == &call_id_owned
            {
                duration_ms =
                    Some(u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX));
                break;
            }
        }
        self.push_tool_result_with_duration(call_id_owned, output, is_error, duration_ms);
    }

    /// Add a tool-result block with an explicit duration (or `None` if
    /// timing was not recorded, e.g. during session replay where the
    /// original timing is not preserved on disk).
    pub fn push_tool_result_with_duration(
        &mut self,
        call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
        duration_ms: Option<u64>,
    ) {
        let call_id = call_id.into();
        let name = self
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                Block::ToolCall {
                    call_id: cid, name, ..
                } if cid == &call_id => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.blocks.push(Block::ToolResult {
            call_id: call_id.clone(),
            name,
            output: output.into(),
            is_error,
            folded: true,
            duration_ms,
        });
        self.push_block_caches();
        // The matching ToolCall now renders as a merged composite, so
        // its previously-cached unmerged height is wrong; invalidate
        // both halves so the next layout pass remeasures.
        self.invalidate_pair_height(&call_id);
    }

    /// Add a plugin-defined custom block.
    pub fn push_custom(&mut self, kind: impl Into<String>, text: impl Into<String>, folded: bool) {
        self.blocks.push(Block::Custom {
            kind: kind.into(),
            text: text.into(),
            folded,
        });
        self.push_block_caches();
    }

    /// Mark the most recent live (assistant or thinking) block as
    /// finished. No-op if there is no streaming block.
    pub fn finish_streaming(&mut self) {
        if let Some(last) = self.blocks.last_mut() {
            last.finish();
        }
        // The `live` flag doesn't currently change rendered height,
        // but invalidate anyway so a future renderer change that
        // styles "stream done" differently picks up cleanly.
        self.invalidate_last_block_caches();
    }

    /// Toggle the fold state of the block at `index`. Returns whether
    /// the toggle had any effect (false if `index` is out of range or
    /// the block is not foldable).
    ///
    /// When the toggled block is one half of a tool-call/result pair,
    /// the matching half is set to the same fold state. This keeps the
    /// merged renderer's view consistent with the user gesture: one
    /// `zo` collapses or expands the visible composite, not just one
    /// of its two source blocks.
    pub fn toggle_fold(&mut self, index: usize) -> bool {
        let Some(block) = self.blocks.get_mut(index) else {
            return false;
        };
        if !block.is_foldable() {
            return false;
        }
        block.toggle_fold();
        self.invalidate_height(index);
        let pair_id = match &self.blocks[index] {
            Block::ToolCall { call_id, .. } | Block::ToolResult { call_id, .. } => {
                Some(call_id.clone())
            }
            _ => None,
        };
        let new_state = matches!(
            &self.blocks[index],
            Block::ToolCall { folded: true, .. } | Block::ToolResult { folded: true, .. }
        );
        if let Some(pid) = pair_id {
            for (i, b) in self.blocks.iter_mut().enumerate() {
                if i == index {
                    continue;
                }
                match b {
                    Block::ToolCall {
                        call_id, folded, ..
                    } if *call_id == pid => *folded = new_state,
                    Block::ToolResult {
                        call_id, folded, ..
                    } if *call_id == pid => *folded = new_state,
                    _ => {}
                }
            }
            self.invalidate_pair_height(&pid);
        }
        true
    }

    /// Set the fold state on every foldable block.
    pub fn set_all_folded(&mut self, folded: bool) {
        let mut invalidated: Vec<usize> = Vec::new();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            match block {
                Block::Thinking { folded: f, .. }
                | Block::ToolCall { folded: f, .. }
                | Block::ToolResult { folded: f, .. }
                | Block::Custom { folded: f, .. } => {
                    *f = folded;
                    invalidated.push(i);
                }
                _ => {}
            }
        }
        for i in invalidated {
            self.invalidate_height(i);
        }
    }

    /// Drain the buffer's blocks, resetting scroll to zero. Useful for
    /// `kage resume` and tests.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.clear_block_caches();
        self.scroll = 0;
    }

    /// Take ownership of the blocks, leaving the buffer empty.
    pub fn take(&mut self) -> Vec<Block> {
        self.scroll = 0;
        self.clear_block_caches();
        mem::take(&mut self.blocks)
    }

    fn last_is_live_assistant(&self) -> bool {
        matches!(
            self.blocks.last(),
            Some(Block::Assistant { live: true, .. })
        )
    }

    fn last_is_live_thinking(&self) -> bool {
        matches!(self.blocks.last(), Some(Block::Thinking { live: true, .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_at_screen_row_returns_block_under_click() {
        let mut buf = Buffer::new();
        buf.push_user("hi");
        buf.push_user("hello");
        buf.set_last_block_screen_rows(vec![(0, 5, 8), (1, 8, 12)]);
        assert_eq!(buf.block_at_screen_row(5), Some(0));
        assert_eq!(buf.block_at_screen_row(7), Some(0));
        assert_eq!(buf.block_at_screen_row(8), Some(1));
        assert_eq!(buf.block_at_screen_row(11), Some(1));
        assert_eq!(buf.block_at_screen_row(12), None);
        assert_eq!(buf.block_at_screen_row(4), None);
    }

    #[test]
    fn screen_top_of_returns_first_row_for_idx() {
        let mut buf = Buffer::new();
        buf.push_user("first");
        buf.set_last_block_screen_rows(vec![(0, 10, 14)]);
        assert_eq!(buf.screen_top_of(0), Some(10));
        assert_eq!(buf.screen_top_of(1), None);
    }

    #[test]
    fn cached_height_misses_when_width_differs() {
        let mut buf = Buffer::new();
        buf.push_user("hello");
        buf.set_cached_height(0, 80, 3);
        assert_eq!(buf.cached_height(0, 80), Some(3));
        assert_eq!(buf.cached_height(0, 100), None);
    }

    #[test]
    fn append_invalidates_only_the_growing_block() {
        let mut buf = Buffer::new();
        buf.push_user("first");
        buf.begin_assistant();
        buf.set_cached_height(0, 80, 1);
        buf.set_cached_height(1, 80, 2);
        buf.append_assistant_delta("more");
        assert_eq!(
            buf.cached_height(0, 80),
            Some(1),
            "user block height must survive an unrelated assistant delta"
        );
        assert_eq!(
            buf.cached_height(1, 80),
            None,
            "the assistant block that just grew must invalidate its cached height"
        );
    }

    #[test]
    fn push_tool_result_invalidates_paired_call_height() {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "read", "summary", "{}");
        buf.set_cached_height(0, 80, 4);
        assert_eq!(buf.cached_height(0, 80), Some(4));
        buf.push_tool_result("c1", "ok", false);
        assert_eq!(
            buf.cached_height(0, 80),
            None,
            "the call's pre-merge height is wrong once a result arrives"
        );
    }

    #[test]
    fn toggle_fold_invalidates_both_halves_of_pair() {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "read", "summary", "{}");
        buf.push_tool_result("c1", "body", false);
        // After push_tool_result, the call's height was already
        // invalidated; reseat a value to verify toggle invalidates.
        buf.set_cached_height(0, 80, 5);
        buf.set_cached_height(1, 80, 7);
        buf.toggle_fold(0);
        assert_eq!(buf.cached_height(0, 80), None);
        assert_eq!(buf.cached_height(1, 80), None);
    }

    #[test]
    fn clear_drops_height_cache() {
        let mut buf = Buffer::new();
        buf.push_user("hi");
        buf.set_cached_height(0, 80, 1);
        buf.clear();
        assert_eq!(buf.cached_height(0, 80), None);
    }

    #[test]
    fn user_block_line_count_matches_text() {
        let mut buf = Buffer::new();
        buf.push_user("hello\nworld");
        assert_eq!(buf.total_lines(), 2);
    }

    #[test]
    fn streaming_assistant_reassembles_deltas() {
        let mut buf = Buffer::new();
        buf.append_assistant_delta("hello ");
        buf.append_assistant_delta("world");
        assert_eq!(buf.blocks().len(), 1);
        match &buf.blocks()[0] {
            Block::Assistant { text, live } => {
                assert_eq!(text, "hello world");
                assert!(*live);
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn finish_streaming_marks_last_block_inert() {
        let mut buf = Buffer::new();
        buf.append_assistant_delta("done");
        buf.finish_streaming();
        match &buf.blocks()[0] {
            Block::Assistant { live, .. } => assert!(!*live),
            _ => panic!(),
        }
        // A subsequent delta after finish should start a fresh block.
        buf.append_assistant_delta("next turn");
        assert_eq!(buf.blocks().len(), 2);
    }

    #[test]
    fn tool_call_starts_folded_then_toggles() {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "bash", "ls", "{\n  cmd: 'ls'\n}");
        assert_eq!(buf.total_lines(), 1, "folded contributes header line only");
        assert!(buf.toggle_fold(0));
        assert!(buf.total_lines() > 1, "unfolded shows body lines");
    }

    #[test]
    fn upsert_tool_call_refreshes_in_place_without_duplicates() {
        let mut buf = Buffer::new();
        buf.upsert_tool_call("c1", "write", "write(a)", "{\"a\":1}");
        buf.upsert_tool_call("c1", "write", "write(a,b)", "{\"a\":1,\"b\":2}");
        let calls: Vec<&Block> = buf
            .blocks()
            .iter()
            .filter(|b| matches!(b, Block::ToolCall { .. }))
            .collect();
        assert_eq!(calls.len(), 1, "same call_id must not duplicate the block");
        match calls[0] {
            Block::ToolCall {
                name,
                input_summary,
                input_pretty,
                ..
            } => {
                assert_eq!(name, "write");
                assert_eq!(input_summary, "write(a,b)");
                assert_eq!(input_pretty, "{\"a\":1,\"b\":2}");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn upsert_tool_call_appends_distinct_ids() {
        let mut buf = Buffer::new();
        buf.upsert_tool_call("c1", "bash", "ls", "{}");
        buf.upsert_tool_call("c2", "read", "read(x)", "{}");
        let calls = buf
            .blocks()
            .iter()
            .filter(|b| matches!(b, Block::ToolCall { .. }))
            .count();
        assert_eq!(calls, 2);
    }

    #[test]
    fn tool_result_inherits_name_from_matching_call() {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "bash", "ls", "{}");
        buf.push_tool_result("c1", "file1\nfile2\n", false);
        match &buf.blocks()[1] {
            Block::ToolResult { name, .. } => assert_eq!(name, "bash"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_without_matching_call_has_empty_name() {
        let mut buf = Buffer::new();
        buf.push_tool_result("orphan", "x", false);
        match &buf.blocks()[0] {
            Block::ToolResult { name, .. } => assert_eq!(name, ""),
            _ => panic!(),
        }
    }

    #[test]
    fn fold_on_user_block_is_a_no_op() {
        let mut buf = Buffer::new();
        buf.push_user("hi");
        assert!(!buf.toggle_fold(0));
    }

    #[test]
    fn set_all_folded_only_touches_foldable_blocks() {
        let mut buf = Buffer::new();
        buf.push_user("hi");
        buf.append_assistant_delta("ok");
        buf.push_tool_call("c1", "bash", "ls", "{}");
        buf.set_all_folded(false);
        assert_eq!(buf.total_lines(), 1 + 1 + 1 + 1);
    }

    #[test]
    fn set_scroll_does_not_cap_at_logical_total_lines() {
        let mut buf = Buffer::new();
        buf.push_user("a\nb\nc");
        // The model does not clamp; the renderer will, since only it
        // knows how many visual rows the wrapped paragraph occupies.
        buf.set_scroll(99);
        assert_eq!(buf.scroll(), 99);
    }

    #[test]
    fn thinking_streams_separately_from_assistant() {
        let mut buf = Buffer::new();
        buf.append_thinking_delta("let me think");
        buf.append_assistant_delta("ok");
        buf.append_thinking_delta(" more");
        assert_eq!(buf.blocks().len(), 3);
        if let Block::Thinking { text, .. } = &buf.blocks()[2] {
            assert_eq!(text, " more");
        } else {
            panic!("expected fresh thinking after assistant");
        }
    }

    #[test]
    fn focus_prev_next_walks_only_foldable_blocks() {
        let mut buf = Buffer::new();
        buf.push_user("hi"); // 0: not foldable
        buf.push_tool_call("c1", "read", "a.rs", "{}"); // 1
        buf.push_tool_result("c1", "out", false); // 2: paired with 1, skipped
        buf.append_assistant_delta("ok"); // 3: not foldable
        buf.finish_streaming();
        buf.push_tool_call("c2", "read", "b.rs", "{}"); // 4
        assert_eq!(buf.effective_focus(), Some(4));
        // Foldable-only walk: 4 -> 1 -> stop.
        assert!(buf.focus_prev());
        assert_eq!(buf.focus(), Some(1));
        assert!(!buf.focus_prev());
        assert!(buf.focus_next());
        assert_eq!(buf.focus(), Some(4));
    }

    #[test]
    fn focus_any_walks_every_block_skipping_merged_results() {
        let mut buf = Buffer::new();
        buf.push_user("hi"); // 0
        buf.push_tool_call("c1", "read", "a.rs", "{}"); // 1
        buf.push_tool_result("c1", "out", false); // 2: skipped
        buf.append_assistant_delta("ok"); // 3
        buf.finish_streaming();
        buf.push_tool_call("c2", "read", "b.rs", "{}"); // 4
        assert_eq!(buf.effective_focus(), Some(4));
        // 4 -> 3 -> 1 -> 0 (2 always skipped because merged with 1).
        assert!(buf.focus_prev_any());
        assert_eq!(buf.focus(), Some(3));
        assert!(buf.focus_prev_any());
        assert_eq!(buf.focus(), Some(1));
        assert!(buf.focus_prev_any());
        assert_eq!(buf.focus(), Some(0));
        assert!(!buf.focus_prev_any());
    }

    #[test]
    fn set_focus_only_rejects_out_of_range() {
        let mut buf = Buffer::new();
        buf.push_user("hi");
        buf.push_tool_call("c1", "ls", ".", "{}");
        buf.set_focus(Some(0));
        assert_eq!(buf.focus(), Some(0));
        buf.set_focus(Some(1));
        assert_eq!(buf.focus(), Some(1));
        buf.set_focus(Some(99));
        assert_eq!(buf.focus(), None);
    }

    #[test]
    fn block_text_returns_raw_markdown_source_not_render() {
        let mut buf = Buffer::new();
        buf.push_user("hi");
        buf.append_assistant_delta("# Title\n\n```rust\nfn x() {}\n```");
        assert_eq!(buf.block_text(0).as_deref(), Some("hi"));
        assert_eq!(
            buf.block_text(1).as_deref(),
            Some("# Title\n\n```rust\nfn x() {}\n```"),
            "yank gets the verbatim markdown, fences and all"
        );
        assert_eq!(buf.block_text(99), None);
    }

    #[test]
    fn fresh_buffer_is_following() {
        let buf = Buffer::new();
        assert!(buf.is_following());
        assert_eq!(buf.scroll(), 0);
    }

    #[test]
    fn append_does_not_disturb_user_scroll_position() {
        let mut buf = Buffer::new();
        buf.push_user("aa\nbb\ncc");
        buf.set_scroll(2);
        assert!(!buf.is_following());
        buf.append_assistant_delta("hi\nthere\nyou");
        assert_eq!(buf.scroll(), 2);
        assert!(!buf.is_following());
    }

    #[test]
    fn returning_to_zero_scroll_re_enables_follow() {
        let mut buf = Buffer::new();
        buf.push_user("aa\nbb\ncc");
        buf.set_scroll(2);
        buf.set_scroll(0);
        assert!(buf.is_following());
    }

    #[test]
    fn take_returns_blocks_and_resets_scroll() {
        let mut buf = Buffer::new();
        buf.push_user("a");
        buf.set_scroll(1);
        let taken = buf.take();
        assert_eq!(taken.len(), 1);
        assert_eq!(buf.scroll(), 0);
        assert!(buf.blocks().is_empty());
    }
}
