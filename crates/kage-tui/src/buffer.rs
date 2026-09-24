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
pub(crate) use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

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

/// First line of `text`, for single-line summaries.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_owned()
}

/// Trim `label` to at most `label_width` characters, appending an
/// ellipsis when truncated. `None` for empty labels.
fn truncate_label(label: &str, label_width: usize) -> Option<String> {
    if label.is_empty() {
        return None;
    }
    if label.chars().count() <= label_width {
        return Some(label.to_owned());
    }
    let cut: String = label.chars().take(label_width.saturating_sub(3)).collect();
    Some(format!("{cut}..."))
}

/// Call/result pairing for [`Block::ToolCall`] and
/// [`Block::ToolResult`] blocks, derived from the block list and
/// cached between frames. The renderer rebuilds it only when the
/// block count or structural epoch changes; holding it behind an [`Arc`] lets buffer
/// snapshots share it instead of copying a map of every call id per
/// frame.
#[derive(Debug, Default)]
pub struct ToolTopology {
    /// Result block index for each call id; the first result wins.
    pub(crate) result_by_call: HashMap<String, usize>,
    /// Result block indexes already merged into their call block.
    pub(crate) consumed_results: HashSet<usize>,
    /// Result block index to its call block index.
    pub(crate) call_idx_for_result: HashMap<usize, usize>,
}

impl ToolTopology {
    /// Derive the pairing from an append-only block list.
    fn build(blocks: &[Block]) -> Self {
        let mut topo = Self::default();
        for (i, block) in blocks.iter().enumerate() {
            if let Block::ToolResult { call_id, .. } = block {
                topo.result_by_call.entry(call_id.clone()).or_insert(i);
            }
        }
        for (i, block) in blocks.iter().enumerate() {
            if let Block::ToolCall { call_id, .. } = block
                && let Some(&rid) = topo.result_by_call.get(call_id)
            {
                topo.consumed_results.insert(rid);
                topo.call_idx_for_result.insert(rid, i);
            }
        }
        topo
    }
}

/// Append-only conversation history with a viewport anchor in
/// absolute virtual-row space. `scroll == None` means the viewport is
/// pinned to the latest content (auto-follow on streaming);
/// `Some(top)` means the viewport's first row is virtual row `top`,
/// so content arriving below never moves what the user is reading.
/// The model layer does not cap the anchor: only the renderer knows
/// how many visual rows the wrapped blocks occupy, so it clamps
/// against the real total each frame (and re-arms follow when the
/// clamp lands the viewport on the bottom row).
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    blocks: Vec<Block>,
    scroll: Option<usize>,
    /// Index of the user-selected foldable block, if any. `None` means
    /// "no explicit selection"; the renderer falls back to the last
    /// foldable block in the buffer for fold-toggle gestures.
    focus: Option<usize>,
    /// The focus value the renderer last painted. The renderer
    /// compares this to the current effective focus each frame; when
    /// they differ, it invalidates the moved blocks' caches so
    /// emphasis repaints.
    last_drawn_focus: Option<usize>,
    /// The explicit focus the renderer last saw. Auto-scrolling a
    /// moved focus into view keys on this, not the effective focus:
    /// appended blocks change the effective fallback every time one
    /// lands, and a streaming append must never yank a pinned
    /// viewport back to the bottom.
    last_user_focus: Option<usize>,
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
    /// Cached call/result block pairing together with the
    /// `(epoch, block count)` it was built at, shared behind an
    /// [`Arc`]. See [`ToolTopology`]. `None` until the first render;
    /// rebuilt by the renderer whenever either key changed since.
    tool_topology: Option<((u64, usize), Arc<ToolTopology>)>,
    /// Structural generation, bumped by every change that is not a
    /// pure append (`clear`, `take`, compaction). Block indices from
    /// one epoch mean nothing in another, so index-keyed caches built
    /// against a different epoch are discarded. Appends leave it
    /// alone because existing indices stay valid.
    epoch: u64,
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
    /// When the live last block received a delta but its render caches
    /// were deliberately left stale. `Some(t)` means "a re-parse has
    /// been due since `t`"; the cache readers serve the stale lines
    /// until `t` ages past [`STREAM_REPARSE_THROTTLE`], then force a
    /// miss so the renderer rebuilds. Cleared when a rebuild stores
    /// fresh lines and when the stream finishes.
    stream_dirty_since: Option<Instant>,
}

/// Minimum spacing between full markdown re-parses of a streaming
/// block. Deltas inside the window update `text` but keep serving the
/// previous render from cache, bounding re-parse cost to one build
/// per window instead of one per delta (which was quadratic over the
/// stream length).
const STREAM_REPARSE_THROTTLE: Duration = Duration::from_millis(50);

/// Maximum blocks kept in the conversation buffer. Beyond this, the
/// oldest blocks are compacted away at draw time so an all-day
/// session can't grow memory (blocks, text, and render caches)
/// without bound. Chosen to stay far above what a focused work
/// session produces while keeping the per-frame block walk cheap.
const MAX_BLOCKS: usize = 512;

mod edit;
mod view;

#[cfg(test)]
mod tests;
