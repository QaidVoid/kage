//! Buffer write-side: block construction, streaming, and fold ops.

use super::*;

use kage_core::event::TOOL_CANCELLED_TEXT;

impl Buffer {
    /// Append `block`, first finishing a live assistant or thinking
    /// block it follows: a stream ends when anything else begins, and
    /// only the last block's throttled render cache is ever refreshed.
    fn push_block(&mut self, block: Block) {
        if self.last_is_live_assistant() || self.last_is_live_thinking() {
            self.finish_streaming();
        }
        self.total_text_bytes += block.text_bytes();
        self.blocks.push(Arc::new(block));
        self.push_block_caches();
    }

    /// Push a fully-formed user prompt.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push_block(Block::User { text: text.into() });
    }

    /// Begin a streaming assistant block. Subsequent deltas append to it
    /// via [`Self::append_assistant_delta`].
    pub fn begin_assistant(&mut self) {
        self.push_block(Block::Assistant {
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
        if let Some(Block::Assistant { text, .. }) = self.blocks.last_mut().map(Arc::make_mut) {
            text.push_str(delta);
        }
        self.total_text_bytes += delta.len();
        self.mark_stream_dirty();
    }

    /// Begin a streaming thinking block. It starts folded, showing only
    /// its last lines while it streams.
    pub fn begin_thinking(&mut self) {
        self.push_block(Block::Thinking {
            text: String::new(),
            folded: true,
            live: true,
            started_at: Instant::now(),
            duration_ms: None,
            pinned: false,
        });
    }

    /// Append text to the most recent thinking block.
    pub fn append_thinking_delta(&mut self, delta: &str) {
        if !self.last_is_live_thinking() {
            self.begin_thinking();
        }
        if let Some(Block::Thinking { text, .. }) = self.blocks.last_mut().map(Arc::make_mut) {
            text.push_str(delta);
        }
        self.total_text_bytes += delta.len();
        self.mark_stream_dirty();
    }

    /// Push a finished, folded thinking block as replayed from history,
    /// with the duration the session recorded for it, if any.
    pub fn push_thinking(&mut self, text: impl Into<String>, duration_ms: Option<u64>) {
        self.push_block(Block::Thinking {
            text: text.into(),
            folded: true,
            live: false,
            started_at: Instant::now(),
            duration_ms,
            pinned: false,
        });
    }

    /// Record that the live last block grew without dropping its
    /// render caches: the stale lines keep serving until
    /// [`STREAM_REPARSE_THROTTLE`] elapses, then the cache readers
    /// force one rebuild. Version still bumps so the render loop
    /// wakes and repaints from the (possibly stale) cache.
    pub(crate) fn mark_stream_dirty(&mut self) {
        self.stream_dirty_since.get_or_insert_with(Instant::now);
        self.output_serial += 1;
        self.bump_version();
    }

    /// Add a tool-call block in [`ToolPhase::Streaming`]. The header
    /// summary and the pretty-printed input are derived from `input`.
    pub fn push_tool_call(
        &mut self,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) {
        let name = name.into();
        self.push_block(Block::ToolCall {
            call_id: call_id.into(),
            input_summary: crate::events::summarize_input(&name, &input),
            input_pretty: pretty_input(&input),
            name,
            input: Arc::new(input),
            folded: true,
            phase: ToolPhase::Streaming,
            progress: String::new(),
            started_at: Instant::now(),
            diff: None,
        });
    }

    /// Insert a tool-call block, or refresh the input of the open one
    /// (no result yet) with the same `call_id` in place. Used for progressive
    /// argument streaming: the placeholder created from the first
    /// [`kage_core::LoopEvent::ToolCallArgsDelta`] is updated as more
    /// arguments arrive and finalized by the authoritative
    /// [`kage_core::LoopEvent::ToolCallStart`]. The fold state, phase
    /// and start time of an existing block are preserved.
    pub fn upsert_tool_call(
        &mut self,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) {
        let call_id = call_id.into();
        let name = name.into();
        let before = self.tool_call_text_bytes(&call_id);
        let Some(Block::ToolCall {
            name: n,
            input_summary,
            input_pretty,
            input: i,
            ..
        }) = self.open_tool_call_mut(&call_id)
        else {
            self.push_tool_call(call_id, name, input);
            return;
        };
        *input_summary = crate::events::summarize_input(&name, &input);
        *input_pretty = pretty_input(&input);
        *n = name;
        *i = Arc::new(input);
        self.reaccount(&call_id, before);
        self.invalidate_pair_height(&call_id);
        self.bump_version();
    }

    /// Move the call `call_id` to `phase`. Entering
    /// [`ToolPhase::Running`] restarts its timer. No-op for an unknown
    /// id.
    pub fn set_tool_phase(&mut self, call_id: &str, phase: ToolPhase) {
        let Some(Block::ToolCall {
            phase: p,
            started_at,
            ..
        }) = self.open_tool_call_mut(call_id)
        else {
            return;
        };
        *p = phase;
        if phase == ToolPhase::Running {
            *started_at = Instant::now();
        }
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// The phase of the open tool call `call_id`, if it is still
    /// shown.
    #[must_use]
    pub fn tool_phase(&self, call_id: &str) -> Option<ToolPhase> {
        match self.open_tool_call(call_id) {
            Some(Block::ToolCall { phase, .. }) => Some(*phase),
            _ => None,
        }
    }

    /// Show `diff` as the change of the open call `call_id`, such as an
    /// edit waiting for approval whose file lacks the text to replace.
    /// No-op for an unknown id.
    pub fn set_tool_diff(&mut self, call_id: &str, diff: EditDiff) {
        let before = self.tool_call_text_bytes(call_id);
        let Some(Block::ToolCall { diff: d, .. }) = self.open_tool_call_mut(call_id) else {
            return;
        };
        *d = Some(Arc::new(diff));
        self.reaccount(call_id, before);
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// Replace the progress text of the call `call_id` with the latest
    /// tool update. No-op for an unknown id.
    pub fn set_tool_progress(&mut self, call_id: &str, text: impl Into<String>) {
        let before = self.tool_call_text_bytes(call_id);
        let Some(Block::ToolCall { progress, .. }) = self.open_tool_call_mut(call_id) else {
            return;
        };
        *progress = text.into();
        self.reaccount(call_id, before);
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// Set how long the call `call_id` took, once its result is in.
    /// No-op while the newest call with that id has no result.
    pub fn set_tool_duration(&mut self, call_id: &str, ms: u64) {
        let newest = self.blocks.iter_mut().rev().find(|b| match b.as_ref() {
            Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. } => {
                cid == call_id
            }
            _ => false,
        });
        let Some(Block::ToolResult { duration_ms, .. }) = newest.map(Arc::make_mut) else {
            return;
        };
        if *duration_ms == Some(ms) {
            return;
        }
        *duration_ms = Some(ms);
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// Mark every call that has not finished as
    /// [`ToolPhase::Interrupted`], so nothing keeps animating after a
    /// run ends.
    pub fn interrupt_running_tools(&mut self) {
        let mut changed = Vec::new();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if !matches!(
                block.as_ref(),
                Block::ToolCall {
                    phase: ToolPhase::Streaming
                        | ToolPhase::Queued
                        | ToolPhase::Waiting
                        | ToolPhase::Approved
                        | ToolPhase::Running,
                    ..
                }
            ) {
                continue;
            }
            if let Block::ToolCall { phase, .. } = Arc::make_mut(block) {
                *phase = ToolPhase::Interrupted;
                changed.push(i);
            }
        }
        for i in changed {
            self.invalidate_height(i);
        }
    }

    /// Give every finished `edit` call without a line diff one, from
    /// its file as `read` returns it by path. Each file is read once.
    /// A call whose file does not hold its new text shows the change
    /// its input describes.
    pub fn annotate_edits(&mut self, read: impl Fn(&str) -> Option<String>) {
        use crate::view::tool_view::{EditSide, edit_diff, file_edit_diff};
        let mut files: HashMap<String, Option<String>> = HashMap::new();
        let mut changed = Vec::new();
        let mut added_bytes = 0usize;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            let Block::ToolCall {
                name,
                input,
                phase: ToolPhase::Done,
                diff: None,
                ..
            } = &**block
            else {
                continue;
            };
            if name != "edit" {
                continue;
            }
            let path = input.get("path").and_then(serde_json::Value::as_str);
            let content = path.and_then(|path| {
                files
                    .entry(path.to_owned())
                    .or_insert_with(|| read(path))
                    .as_deref()
            });
            let lines = content.and_then(|c| file_edit_diff(input, c, EditSide::After));
            let diff = lines.unwrap_or_else(|| edit_diff(input));
            added_bytes += diff.lines.iter().map(|l| l.text.len()).sum::<usize>();
            let Block::ToolCall { diff: slot, .. } = Arc::make_mut(block) else {
                continue;
            };
            *slot = Some(Arc::new(diff));
            changed.push(i);
        }
        self.total_text_bytes += added_bytes;
        for i in changed {
            self.invalidate_height(i);
        }
    }

    /// The newest call `call_id` that has no result yet. Providers may
    /// reuse ids across turns, so a call already answered never matches.
    /// The returned block is uniquely owned by this buffer, so writes
    /// never disturb a snapshot's copy.
    fn open_tool_call_mut(&mut self, call_id: &str) -> Option<&mut Block> {
        let idx = self.blocks.iter().rposition(|b| match b.as_ref() {
            Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. } => {
                cid == call_id
            }
            _ => false,
        })?;
        if !matches!(self.blocks[idx].as_ref(), Block::ToolCall { .. }) {
            return None;
        }
        Some(Arc::make_mut(&mut self.blocks[idx]))
    }

    /// The open tool call `call_id`, if it is still shown.
    fn open_tool_call(&self, call_id: &str) -> Option<&Block> {
        let idx = self.blocks.iter().rposition(|b| match b.as_ref() {
            Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. } => {
                cid == call_id
            }
            _ => false,
        })?;
        let block = self.blocks[idx].as_ref();
        matches!(block, Block::ToolCall { .. }).then_some(block)
    }

    /// Add a tool-result block. Looks up the open tool call with the
    /// same id to copy its name, record how long it ran, and move it to
    /// its final phase: done, failed, denied or interrupted. A call
    /// that never started running records no duration.
    pub fn push_tool_result(
        &mut self,
        call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) {
        let call_id = call_id.into();
        let duration_ms = match self.open_tool_call(&call_id) {
            Some(Block::ToolCall {
                phase: ToolPhase::Running,
                started_at,
                ..
            }) => Some(elapsed_ms(*started_at)),
            _ => None,
        };
        self.push_tool_result_with_duration(call_id, output, is_error, duration_ms);
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
        let output = truncate_tool_output(output.into());
        let name = match self.open_tool_call_mut(&call_id) {
            Some(Block::ToolCall { name, phase, .. }) => {
                *phase = result_phase(*phase, &output, is_error);
                name.clone()
            }
            _ => String::new(),
        };
        self.push_block(Block::ToolResult {
            call_id: call_id.clone(),
            name,
            output,
            is_error,
            folded: true,
            duration_ms,
        });
        // The matching ToolCall now renders as a merged composite, so
        // its previously-cached unmerged height is wrong.
        self.invalidate_pair_height(&call_id);
    }

    /// Add a plugin-defined custom block.
    pub fn push_custom(&mut self, kind: impl Into<String>, text: impl Into<String>, folded: bool) {
        self.push_block(Block::Custom {
            kind: kind.into(),
            text: text.into(),
            folded,
        });
    }

    /// Replace the text of the last block when it is an unfolded
    /// custom block of `kind`, otherwise push a new one. Keeps a burst
    /// of status notices, such as provider retries, to one line.
    pub fn replace_or_push_custom(&mut self, kind: &str, text: impl Into<String>) {
        if let Some(last) = self.blocks.last_mut()
            && matches!(
                last.as_ref(),
                Block::Custom {
                    kind: k,
                    ..
                } if k == kind
            )
        {
            let Block::Custom { text: t, .. } = Arc::make_mut(last) else {
                unreachable!("just matched a custom block");
            };
            let text = text.into();
            self.total_text_bytes += text.len();
            self.total_text_bytes -= t.len();
            *t = text;
            self.invalidate_last_block_caches();
        } else {
            self.push_custom(kind, text, false);
        }
    }

    /// Replace the text of the newest custom block of `kind` that `pick`
    /// accepts, otherwise push a new one. Keeps a block that is still
    /// live, such as a running shell command, in one place.
    pub fn replace_custom_where(
        &mut self,
        kind: &str,
        pick: impl Fn(&str) -> bool,
        text: impl Into<String>,
    ) {
        let found = self.blocks.iter().rposition(
            |b| matches!(b.as_ref(), Block::Custom { kind: k, text: t, .. } if k == kind && pick(t)),
        );
        if let Some(idx) = found {
            let Block::Custom { text: t, .. } = Arc::make_mut(&mut self.blocks[idx]) else {
                unreachable!("matched a custom block above");
            };
            let text = text.into();
            self.total_text_bytes += text.len();
            self.total_text_bytes -= t.len();
            *t = text;
            self.invalidate_height(idx);
        } else {
            self.push_custom(kind, text, false);
        }
    }

    /// Mark the most recent live (assistant or thinking) block as
    /// finished. No-op if there is no streaming block.
    pub fn finish_streaming(&mut self) {
        if let Some(last) = self.blocks.last_mut() {
            Arc::make_mut(last).finish();
        }
        // Finishing folds a thinking block and drops the live markdown
        // renderer, so the cached lines are stale.
        self.invalidate_last_block_caches();
        self.stream_dirty_since = None;
    }

    /// Toggle the fold state of the block at `index`. Returns whether
    /// the toggle had any effect (false if `index` is out of range or
    /// the block is not foldable).
    ///
    /// When the toggled block is one half of a tool-call/result pair,
    /// the matching half is set to the same fold state, so one `zo`
    /// collapses or expands the visible composite. A call grouped into
    /// an `Explored` row toggles the row's first call: unfolding it
    /// shows the group's calls individually, folding it groups them
    /// again.
    pub fn toggle_fold(&mut self, index: usize) -> bool {
        let index = self
            .tool_topology()
            .head_of_member
            .get(&index)
            .copied()
            .unwrap_or(index);
        let Some(block) = self.blocks.get_mut(index) else {
            return false;
        };
        if !block.is_foldable() {
            return false;
        }
        Arc::make_mut(block).toggle_fold();
        self.invalidate_height(index);
        if let Block::ToolCall {
            call_id, folded, ..
        }
        | Block::ToolResult {
            call_id, folded, ..
        } = self.blocks[index].as_ref()
        {
            let (call_id, folded) = (call_id.clone(), *folded);
            self.set_call_folded(&call_id, folded);
        }
        self.bump_fold_generation();
        true
    }

    /// Fold or unfold both halves of the tool call `call_id`.
    fn set_call_folded(&mut self, call_id: &str, folded: bool) {
        for block in &mut self.blocks {
            let hit = matches!(
                block.as_ref(),
                Block::ToolCall {
                    call_id: cid,
                    ..
                }
                | Block::ToolResult {
                    call_id: cid,
                    ..
                } if cid == call_id
            );
            if !hit {
                continue;
            }
            match Arc::make_mut(block) {
                Block::ToolCall { folded: f, .. } | Block::ToolResult { folded: f, .. } => {
                    *f = folded;
                }
                _ => {}
            }
        }
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// Set the fold state on every foldable block.
    pub fn set_all_folded(&mut self, folded: bool) {
        let mut invalidated: Vec<usize> = Vec::new();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if !matches!(
                block.as_ref(),
                Block::Thinking { .. }
                    | Block::ToolCall { .. }
                    | Block::ToolResult { .. }
                    | Block::Custom { .. }
            ) {
                continue;
            }
            match Arc::make_mut(block) {
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
        self.bump_fold_generation();
    }

    fn bump_fold_generation(&mut self) {
        self.fold_generation = self.fold_generation.wrapping_add(1);
    }

    /// Drain the buffer's blocks, resetting scroll and focus to zero.
    /// Useful for `kage resume` and tests. Focus must go too: a stale
    /// index past the (now empty) block list would panic the next
    /// render, and `set_focus`'s range check never sees this path.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.total_text_bytes = 0;
        self.clear_block_caches();
        self.scroll = None;
        self.focus = None;
        self.last_drawn_focus = None;
        self.last_user_focus = None;
        self.stream_dirty_since = None;
    }

    /// Take ownership of the blocks, leaving the buffer empty. Focus
    /// and scroll reset for the same reason as [`Self::clear`].
    pub fn take(&mut self) -> Vec<Arc<Block>> {
        self.scroll = None;
        self.focus = None;
        self.last_drawn_focus = None;
        self.last_user_focus = None;
        self.stream_dirty_since = None;
        self.total_text_bytes = 0;
        self.clear_block_caches();
        mem::take(&mut self.blocks)
    }

    /// Enforce [`MAX_BLOCKS`] and [`MAX_BYTES`]. Returns the number of
    /// blocks dropped (zero when under both caps). The count cap compacts
    /// first; the byte cap then keeps compacting the oldest blocks while
    /// the kept text still exceeds [`MAX_BYTES`], never below the newest
    /// block, so one oversized block is retained rather than erased. UI-
    /// thread only: it shifts every block index, so it must run before a
    /// draw snapshots the buffer; the version bump it performs makes
    /// index-bearing caches elsewhere (the search-match list) rebuild
    /// themselves.
    ///
    /// Not to be confused with the agent loop's context compaction:
    /// that compacts the *session history* against the token budget;
    /// this only trims *rendered scrollback*.
    pub(crate) fn trim_scrollback(&mut self) -> usize {
        let mut dropped = 0;
        if self.blocks.len() > MAX_BLOCKS {
            dropped += self.compact_to(MAX_BLOCKS);
        }
        while self.total_text_bytes > MAX_BYTES && self.blocks.len() > 1 {
            dropped += self.compact_to(self.blocks.len() - 1);
        }
        dropped
    }

    /// The kept text bytes of the newest open call `call_id`, captured
    /// before an in-place text replacement.
    fn tool_call_text_bytes(&self, call_id: &str) -> usize {
        self.open_tool_call(call_id).map_or(0, Block::text_bytes)
    }

    /// Re-account the open call `call_id` after an in-place text
    /// change; `before` is what [`Self::tool_call_text_bytes`] read
    /// beforehand.
    fn reaccount(&mut self, call_id: &str, before: usize) {
        self.total_text_bytes += self.tool_call_text_bytes(call_id);
        self.total_text_bytes -= before;
    }

    /// Drop the oldest blocks so at most `cap` remain, keeping every
    /// [`Block::ToolResult`] with its call. A result always sits
    /// after its call (calls are pushed before their results), so
    /// the only pair a frontier can split is a dropped call whose
    /// result is still kept; the frontier extends past such results
    /// until none remain, preventing a merged composite from losing
    /// its call half (which would render as running forever). This
    /// can leave fewer than `cap` blocks. A pinned scroll anchor
    /// shifts up by the virtual rows the dropped blocks occupied so
    /// the viewport keeps showing the same content; uncached heights
    /// count one row (never measured). Following state is untouched.
    pub(crate) fn compact_to(&mut self, cap: usize) -> usize {
        let len = self.blocks.len();
        if len <= cap {
            return 0;
        }
        let mut k = len - cap;
        let mut dropped_bytes: usize = self.blocks[..k].iter().map(|b| b.text_bytes()).sum();
        loop {
            let dropped_calls: std::collections::HashSet<&str> = self.blocks[..k]
                .iter()
                .filter_map(|b| match b.as_ref() {
                    Block::ToolCall { call_id, .. } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect();
            let Some(next) = self.blocks[k..].iter().position(|b| match b.as_ref() {
                Block::ToolResult { call_id, .. } => dropped_calls.contains(call_id.as_str()),
                _ => false,
            }) else {
                break;
            };
            k += next + 1;
            dropped_bytes += self.blocks[k - 1].text_bytes();
        }

        // Shift a pinned viewport anchor up by the virtual rows the
        // dropped blocks occupied so it keeps pointing at the same
        // content. Reads the height cache, so must precede the drain.
        if let Some(top) = self.scroll {
            let dropped_rows = self.dropped_block_rows(k);
            self.scroll = Some(top.saturating_sub(dropped_rows));
        }

        self.blocks.drain(0..k);
        self.block_heights.drain(0..k);
        self.block_render_lines.drain(0..k);
        self.total_text_bytes -= dropped_bytes;
        self.focus = self.focus.and_then(|f| f.checked_sub(k));
        self.last_drawn_focus = self.last_drawn_focus.and_then(|f| f.checked_sub(k));
        self.last_user_focus = self.last_user_focus.and_then(|f| f.checked_sub(k));
        renumber_after_compact(&mut self.last_block_screen_rows, k);
        renumber_after_compact(&mut self.last_block_virtual_rows, k);
        self.bump_epoch();
        self.bump_version();
        k
    }

    /// Virtual rows the `k` blocks dropped by [`Self::compact_to`]
    /// occupied, gaps included, best effort from the renderer's height
    /// cache. Must run before the cache drain.
    fn dropped_block_rows(&mut self, k: usize) -> usize {
        let topology = self.tool_topology();
        let mut rows = 0usize;
        let mut above: Option<usize> = None;
        for idx in (0..self.blocks.len()).filter(|&i| !topology.is_hidden(i)) {
            if let Some(prev) = above {
                rows += gap_between(&self.blocks[prev], &self.blocks[idx]);
            }
            if idx >= k {
                break;
            }
            rows += self.block_heights[idx].map_or(1, |(_, h)| usize::from(h));
            above = Some(idx);
        }
        rows
    }

    pub(crate) fn last_is_live_assistant(&self) -> bool {
        matches!(
            self.blocks.last().map(Arc::as_ref),
            Some(Block::Assistant { live: true, .. })
        )
    }

    pub(crate) fn last_is_live_thinking(&self) -> bool {
        matches!(
            self.blocks.last().map(Arc::as_ref),
            Some(Block::Thinking { live: true, .. })
        )
    }
}

/// Oldest bytes of a tool result kept verbatim when the result is
/// pushed.
const TOOL_OUTPUT_HEAD_BYTES: usize = 64 * 1024;
/// Newest bytes of a tool result kept verbatim when the result is
/// pushed.
const TOOL_OUTPUT_TAIL_BYTES: usize = 16 * 1024;

/// Bound a tool result's stored output: past the first
/// [`TOOL_OUTPUT_HEAD_BYTES`] plus the last [`TOOL_OUTPUT_TAIL_BYTES`]
/// bytes, one elision marker line replaces the middle. Rendering caps
/// a body at 500 lines and 256 KB anyway, so only extremely long tails
/// change what is shown. Cut points move back and forward to UTF-8
/// character boundaries, so multi-byte text never splits.
fn truncate_tool_output(output: String) -> String {
    if output.len() <= TOOL_OUTPUT_HEAD_BYTES + TOOL_OUTPUT_TAIL_BYTES {
        return output;
    }
    let head_end = floor_boundary(&output, TOOL_OUTPUT_HEAD_BYTES);
    let tail_start = ceil_boundary(&output, output.len() - TOOL_OUTPUT_TAIL_BYTES);
    format!(
        "{}\n[{} bytes elided]\n{}",
        &output[..head_end],
        tail_start - head_end,
        &output[tail_start..]
    )
}

/// The largest character boundary of `s` at or before `at`.
fn floor_boundary(s: &str, mut at: usize) -> usize {
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest character boundary of `s` at or after `at`.
fn ceil_boundary(s: &str, mut at: usize) -> usize {
    while !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The phase a call in `phase` moves to when its result arrives. A
/// denied call stays denied. A call the loop cancelled, or whose
/// approval ended without an answer, reads as interrupted.
fn result_phase(phase: ToolPhase, output: &str, is_error: bool) -> ToolPhase {
    match phase {
        ToolPhase::Denied => ToolPhase::Denied,
        _ if is_error && output == TOOL_CANCELLED_TEXT => ToolPhase::Interrupted,
        ToolPhase::Waiting if is_error => ToolPhase::Interrupted,
        _ if is_error => ToolPhase::Failed,
        _ => ToolPhase::Done,
    }
}

/// Pretty-printed JSON for a tool input.
fn pretty_input(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// Reindex a block-keyed row cache after `k` leading blocks were
/// dropped: entries for dropped blocks go, surviving indices shift.
fn renumber_after_compact<T, U>(rows: &mut Vec<(usize, T, U)>, k: usize) {
    rows.retain(|(i, ..)| *i >= k);
    for (i, ..) in rows.iter_mut() {
        *i -= k;
    }
}
