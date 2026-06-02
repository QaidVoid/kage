//! Buffer read-side: scroll, focus, cache geometry, search, folding queries.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

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

    pub(crate) fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub(crate) fn push_block_caches(&mut self) {
        self.block_heights.push(None);
        self.block_render_lines.push(None);
        self.bump_version();
    }

    pub(crate) fn invalidate_last_block_caches(&mut self) {
        if let Some(slot) = self.block_heights.last_mut() {
            *slot = None;
        }
        if let Some(slot) = self.block_render_lines.last_mut() {
            *slot = None;
        }
        self.bump_version();
    }

    pub(crate) fn clear_block_caches(&mut self) {
        self.block_heights.clear();
        self.block_render_lines.clear();
        self.bump_version();
    }

    pub(crate) fn invalidate_height(&mut self, idx: usize) {
        if let Some(slot) = self.block_heights.get_mut(idx) {
            *slot = None;
        }
        if let Some(slot) = self.block_render_lines.get_mut(idx) {
            *slot = None;
        }
        self.bump_version();
    }

    pub(crate) fn invalidate_pair_height(&mut self, call_id: &str) {
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

    pub(crate) fn foldable_index_before(&self, idx: usize) -> Option<usize> {
        (0..idx)
            .rev()
            .find(|i| self.is_selectable(*i) && self.blocks[*i].is_foldable())
    }

    pub(crate) fn foldable_index_after(&self, idx: usize) -> Option<usize> {
        (idx + 1..self.blocks.len())
            .find(|i| self.is_selectable(*i) && self.blocks[*i].is_foldable())
    }

    /// Whether `idx` is something `[` / `]` should land on. Every
    /// block kind is selectable except a `ToolResult` whose matching
    /// `ToolCall` exists earlier in the buffer (the renderer merges
    /// the pair into one composite, so landing on the result would
    /// look like a no-op visual).
    pub(crate) fn is_selectable(&self, idx: usize) -> bool {
        match self.blocks.get(idx) {
            Some(Block::ToolResult { call_id, .. }) => !self.blocks[..idx]
                .iter()
                .any(|b| matches!(b, Block::ToolCall { call_id: cid, .. } if cid == call_id)),
            Some(_) => true,
            None => false,
        }
    }

    pub(crate) fn last_selectable_index(&self) -> Option<usize> {
        (0..self.blocks.len())
            .rev()
            .find(|i| self.is_selectable(*i))
    }

    pub(crate) fn selectable_index_before(&self, idx: usize) -> Option<usize> {
        (0..idx).rev().find(|i| self.is_selectable(*i))
    }

    pub(crate) fn selectable_index_after(&self, idx: usize) -> Option<usize> {
        (idx + 1..self.blocks.len()).find(|i| self.is_selectable(*i))
    }
}
