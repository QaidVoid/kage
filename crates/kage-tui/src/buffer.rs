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

pub(crate) use std::mem;
pub(crate) use std::sync::Arc;
pub(crate) use std::time::Instant;

pub(crate) use ratatui::text::Line;

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

mod edit;
mod view;

#[cfg(test)]
mod tests;
