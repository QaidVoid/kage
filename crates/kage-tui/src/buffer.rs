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
use std::time::Instant;

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
    /// Visual-mode selection anchor. Set when the user pressed `v`;
    /// cleared on `Esc` or `y`. Selection range is `[min, max]` of
    /// `(visual_anchor, effective_focus)`.
    visual_anchor: Option<usize>,
    /// Per-block rendered-height cache, indexed parallel to
    /// [`Self::blocks`]. Each entry stores `(width, height_in_rows)`
    /// captured by the renderer's last successful layout pass for
    /// that block. The renderer reuses cached entries whose `width`
    /// matches the current viewport width and otherwise rebuilds.
    /// Mutators push or invalidate entries in lockstep with `blocks`
    /// to avoid stale data; this is what lets virtualized rendering
    /// skip building [`ratatui::text::Line`]s for off-screen blocks.
    block_heights: Vec<Option<(u16, u16)>>,
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
        self.scroll = scroll;
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
    }

    fn invalidate_height(&mut self, idx: usize) {
        if let Some(slot) = self.block_heights.get_mut(idx) {
            *slot = None;
        }
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
                }
                _ => {}
            }
        }
    }

    /// Set the visual-selection anchor. `None` clears (exits visual).
    /// Out-of-range indices are silently dropped.
    pub fn set_visual_anchor(&mut self, idx: Option<usize>) {
        self.visual_anchor = idx.filter(|i| self.blocks.get(*i).is_some());
    }

    /// Currently set visual anchor.
    #[must_use]
    pub fn visual_anchor(&self) -> Option<usize> {
        self.visual_anchor
    }

    /// `(min, max)` block-index range when visual selection is active,
    /// derived from the anchor and the current focus head. `None` when
    /// the user isn't selecting.
    #[must_use]
    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        let head = self.effective_focus()?;
        Some((anchor.min(head), anchor.max(head)))
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

    /// Concatenate the plain-text content of blocks in the inclusive
    /// range `[start, end]` for clipboard yank. Skips renderer-only
    /// decoration (rule glyphs, padding, status pills). Tool calls
    /// merge with their matching result; thinking blocks are omitted
    /// (they're hidden chain-of-thought, not user-meaningful prose).
    #[must_use]
    pub fn selection_text(&self, start: usize, end: usize) -> String {
        let lo = start.min(end);
        let hi = start.max(end).min(self.blocks.len().saturating_sub(1));
        let mut out = String::new();
        let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let push_separator = |s: &mut String| {
            if !s.is_empty() {
                s.push_str("\n\n");
            }
        };
        for i in lo..=hi {
            if consumed.contains(&i) {
                continue;
            }
            match &self.blocks[i] {
                Block::User { text } => {
                    push_separator(&mut out);
                    out.push_str("> ");
                    out.push_str(text);
                }
                Block::Assistant { text, .. } | Block::Custom { text, .. } => {
                    push_separator(&mut out);
                    out.push_str(text);
                }
                Block::Thinking { .. } => {}
                Block::ToolCall {
                    call_id,
                    name,
                    input_summary,
                    ..
                } => {
                    push_separator(&mut out);
                    out.push_str("$ ");
                    out.push_str(name);
                    if !input_summary.is_empty() {
                        out.push(' ');
                        out.push_str(input_summary);
                    }
                    if let Some((result_idx, output)) =
                        self.blocks.iter().enumerate().find_map(|(j, b)| match b {
                            Block::ToolResult {
                                call_id: cid,
                                output,
                                ..
                            } if cid == call_id => Some((j, output.as_str())),
                            _ => None,
                        })
                        && !output.is_empty()
                    {
                        if result_idx <= hi {
                            consumed.insert(result_idx);
                        }
                        out.push('\n');
                        out.push_str(output);
                    }
                }
                Block::ToolResult { output, .. } => {
                    push_separator(&mut out);
                    out.push_str(output);
                }
            }
        }
        out
    }

    /// Replace the explicit focus. `None` clears it (renderer falls
    /// back to the last selectable block). Out-of-range indices are
    /// silently dropped.
    pub fn set_focus(&mut self, idx: Option<usize>) {
        self.focus = idx.filter(|i| self.blocks.get(*i).is_some());
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
        self.block_heights.push(None);
    }

    /// Begin a streaming assistant block. Subsequent deltas append to it
    /// via [`Self::append_assistant_delta`].
    pub fn begin_assistant(&mut self) {
        self.blocks.push(Block::Assistant {
            text: String::new(),
            live: true,
        });
        self.block_heights.push(None);
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
        if let Some(slot) = self.block_heights.last_mut() {
            *slot = None;
        }
    }

    /// Begin a streaming thinking block.
    pub fn begin_thinking(&mut self) {
        self.blocks.push(Block::Thinking {
            text: String::new(),
            folded: false,
            live: true,
        });
        self.block_heights.push(None);
    }

    /// Append text to the most recent thinking block.
    pub fn append_thinking_delta(&mut self, delta: &str) {
        if !self.last_is_live_thinking() {
            self.begin_thinking();
        }
        if let Some(Block::Thinking { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
        if let Some(slot) = self.block_heights.last_mut() {
            *slot = None;
        }
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
        self.block_heights.push(None);
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
        self.block_heights.push(None);
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
        self.block_heights.push(None);
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
        if let Some(slot) = self.block_heights.last_mut() {
            *slot = None;
        }
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
        for (i, block) in self.blocks.iter_mut().enumerate() {
            match block {
                Block::Thinking { folded: f, .. }
                | Block::ToolCall { folded: f, .. }
                | Block::ToolResult { folded: f, .. }
                | Block::Custom { folded: f, .. } => {
                    *f = folded;
                    if let Some(slot) = self.block_heights.get_mut(i) {
                        *slot = None;
                    }
                }
                _ => {}
            }
        }
    }

    /// Drain the buffer's blocks, resetting scroll to zero. Useful for
    /// `kage resume` and tests.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.block_heights.clear();
        self.scroll = 0;
    }

    /// Take ownership of the blocks, leaving the buffer empty.
    pub fn take(&mut self) -> Vec<Block> {
        self.scroll = 0;
        self.block_heights.clear();
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
