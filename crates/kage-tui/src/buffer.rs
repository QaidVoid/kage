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

    /// Replace the explicit focus. `None` clears it (renderer falls
    /// back to the last selectable block). Out-of-range indices are
    /// silently dropped.
    pub fn set_focus(&mut self, idx: Option<usize>) {
        self.focus = idx.filter(|i| self.blocks.get(*i).is_some());
    }

    /// Move focus to the previous (older) selectable block. Returns
    /// `true` if focus changed.
    pub fn focus_prev(&mut self) -> bool {
        let current = self.effective_focus();
        let next = match current {
            None => return false,
            Some(idx) => self.selectable_index_before(idx),
        };
        match next {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                true
            }
            _ => false,
        }
    }

    /// Move focus to the next (newer) selectable block. Returns
    /// `true` if focus changed.
    pub fn focus_next(&mut self) -> bool {
        let current = self.effective_focus();
        let next = match current {
            None => return false,
            Some(idx) => self.selectable_index_after(idx),
        };
        match next {
            Some(n) if Some(n) != current => {
                self.focus = Some(n);
                true
            }
            _ => false,
        }
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
    }

    /// Begin a streaming assistant block. Subsequent deltas append to it
    /// via [`Self::append_assistant_delta`].
    pub fn begin_assistant(&mut self) {
        self.blocks.push(Block::Assistant {
            text: String::new(),
            live: true,
        });
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
    }

    /// Begin a streaming thinking block.
    pub fn begin_thinking(&mut self) {
        self.blocks.push(Block::Thinking {
            text: String::new(),
            folded: false,
            live: true,
        });
    }

    /// Append text to the most recent thinking block.
    pub fn append_thinking_delta(&mut self, delta: &str) {
        if !self.last_is_live_thinking() {
            self.begin_thinking();
        }
        if let Some(Block::Thinking { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
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
            call_id,
            name,
            output: output.into(),
            is_error,
            folded: true,
            duration_ms,
        });
    }

    /// Add a plugin-defined custom block.
    pub fn push_custom(&mut self, kind: impl Into<String>, text: impl Into<String>, folded: bool) {
        self.blocks.push(Block::Custom {
            kind: kind.into(),
            text: text.into(),
            folded,
        });
    }

    /// Mark the most recent live (assistant or thinking) block as
    /// finished. No-op if there is no streaming block.
    pub fn finish_streaming(&mut self) {
        if let Some(last) = self.blocks.last_mut() {
            last.finish();
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
        }
        true
    }

    /// Set the fold state on every foldable block.
    pub fn set_all_folded(&mut self, folded: bool) {
        for block in &mut self.blocks {
            match block {
                Block::Thinking { folded: f, .. }
                | Block::ToolCall { folded: f, .. }
                | Block::ToolResult { folded: f, .. }
                | Block::Custom { folded: f, .. } => *f = folded,
                _ => {}
            }
        }
    }

    /// Drain the buffer's blocks, resetting scroll to zero. Useful for
    /// `kage resume` and tests.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.scroll = 0;
    }

    /// Take ownership of the blocks, leaving the buffer empty.
    pub fn take(&mut self) -> Vec<Block> {
        self.scroll = 0;
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
    fn focus_walks_all_blocks_skipping_merged_results() {
        let mut buf = Buffer::new();
        buf.push_user("hi"); // 0: User (selectable)
        buf.push_tool_call("c1", "read", "a.rs", "{}"); // 1: Call
        buf.push_tool_result("c1", "out", false); // 2: Result paired with 1, NOT selectable
        buf.append_assistant_delta("ok"); // 3: Assistant
        buf.finish_streaming();
        buf.push_tool_call("c2", "read", "b.rs", "{}"); // 4
        // No explicit focus -> last selectable.
        assert_eq!(buf.effective_focus(), Some(4));
        // Walk back: 4 -> 3 -> 1 -> 0 (skipping 2).
        assert!(buf.focus_prev());
        assert_eq!(buf.focus(), Some(3));
        assert!(buf.focus_prev());
        assert_eq!(buf.focus(), Some(1));
        assert!(buf.focus_prev());
        assert_eq!(buf.focus(), Some(0));
        // No more before 0.
        assert!(!buf.focus_prev());
        // Walk forward, also skipping 2.
        assert!(buf.focus_next());
        assert_eq!(buf.focus(), Some(1));
        assert!(buf.focus_next());
        assert_eq!(buf.focus(), Some(3));
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
