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
        self.blocks.push(block);
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
        if let Some(Block::Assistant { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
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
        if let Some(Block::Thinking { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
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

    /// Show `diff` as the change of the open call `call_id`, such as an
    /// edit waiting for approval whose file lacks the text to replace.
    /// No-op for an unknown id.
    pub fn set_tool_diff(&mut self, call_id: &str, diff: EditDiff) {
        let Some(Block::ToolCall { diff: d, .. }) = self.open_tool_call_mut(call_id) else {
            return;
        };
        *d = Some(Arc::new(diff));
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// Replace the progress text of the call `call_id` with the latest
    /// tool update. No-op for an unknown id.
    pub fn set_tool_progress(&mut self, call_id: &str, text: impl Into<String>) {
        let Some(Block::ToolCall { progress, .. }) = self.open_tool_call_mut(call_id) else {
            return;
        };
        *progress = text.into();
        self.invalidate_pair_height(call_id);
        self.bump_version();
    }

    /// Set how long the call `call_id` took, once its result is in.
    /// No-op while the newest call with that id has no result.
    pub fn set_tool_duration(&mut self, call_id: &str, ms: u64) {
        let newest = self.blocks.iter_mut().rev().find(|b| match b {
            Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. } => {
                cid == call_id
            }
            _ => false,
        });
        let Some(Block::ToolResult { duration_ms, .. }) = newest else {
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
            if let Block::ToolCall { phase, .. } = block
                && matches!(
                    phase,
                    ToolPhase::Streaming
                        | ToolPhase::Queued
                        | ToolPhase::Waiting
                        | ToolPhase::Approved
                        | ToolPhase::Running
                )
            {
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
        for (i, block) in self.blocks.iter_mut().enumerate() {
            let Block::ToolCall {
                name,
                input,
                phase: ToolPhase::Done,
                diff: diff @ None,
                ..
            } = block
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
            *diff = Some(Arc::new(lines.unwrap_or_else(|| edit_diff(input))));
            changed.push(i);
        }
        for i in changed {
            self.invalidate_height(i);
        }
    }

    /// The newest call `call_id` that has no result yet. Providers may
    /// reuse ids across turns, so a call already answered never matches.
    fn open_tool_call_mut(&mut self, call_id: &str) -> Option<&mut Block> {
        let idx = self.blocks.iter().rposition(|b| match b {
            Block::ToolCall { call_id: cid, .. } | Block::ToolResult { call_id: cid, .. } => {
                cid == call_id
            }
            _ => false,
        })?;
        let block = &mut self.blocks[idx];
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
        let duration_ms = match self.open_tool_call_mut(&call_id) {
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
        let output = output.into();
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
        if let Some(Block::Custom {
            kind: k, text: t, ..
        }) = self.blocks.last_mut()
            && k == kind
        {
            *t = text.into();
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
            |b| matches!(b, Block::Custom { kind: k, text: t, .. } if k == kind && pick(t)),
        );
        match found.map(|idx| (idx, &mut self.blocks[idx])) {
            Some((idx, Block::Custom { text: t, .. })) => {
                *t = text.into();
                self.invalidate_height(idx);
            }
            _ => self.push_custom(kind, text, false),
        }
    }

    /// Mark the most recent live (assistant or thinking) block as
    /// finished. No-op if there is no streaming block.
    pub fn finish_streaming(&mut self) {
        if let Some(last) = self.blocks.last_mut() {
            last.finish();
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
        block.toggle_fold();
        self.invalidate_height(index);
        if let Block::ToolCall {
            call_id, folded, ..
        }
        | Block::ToolResult {
            call_id, folded, ..
        } = &self.blocks[index]
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
            match block {
                Block::ToolCall {
                    call_id: cid,
                    folded: f,
                    ..
                }
                | Block::ToolResult {
                    call_id: cid,
                    folded: f,
                    ..
                } if cid == call_id => *f = folded,
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
        self.clear_block_caches();
        self.scroll = None;
        self.focus = None;
        self.last_drawn_focus = None;
        self.last_user_focus = None;
        self.stream_dirty_since = None;
    }

    /// Take ownership of the blocks, leaving the buffer empty. Focus
    /// and scroll reset for the same reason as [`Self::clear`].
    pub fn take(&mut self) -> Vec<Block> {
        self.scroll = None;
        self.focus = None;
        self.last_drawn_focus = None;
        self.last_user_focus = None;
        self.stream_dirty_since = None;
        self.clear_block_caches();
        mem::take(&mut self.blocks)
    }

    /// Enforce [`MAX_BLOCKS`]. Returns the number of blocks dropped
    /// (zero when under the cap). UI-thread only: it shifts every
    /// block index, so it must run before a draw snapshots the
    /// buffer; the version bump it performs makes index-bearing
    /// caches elsewhere (the search-match list) rebuild themselves.
    ///
    /// Not to be confused with the agent loop's context compaction:
    /// that compacts the *session history* against the token budget;
    /// this only trims *rendered scrollback*.
    pub(crate) fn trim_scrollback(&mut self) -> usize {
        if self.blocks.len() <= MAX_BLOCKS {
            return 0;
        }
        self.compact_to(MAX_BLOCKS)
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
        loop {
            let dropped_calls: std::collections::HashSet<&str> = self.blocks[..k]
                .iter()
                .filter_map(|b| match b {
                    Block::ToolCall { call_id, .. } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect();
            let Some(next) = self.blocks[k..].iter().position(|b| match b {
                Block::ToolResult { call_id, .. } => dropped_calls.contains(call_id.as_str()),
                _ => false,
            }) else {
                break;
            };
            k += next + 1;
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
        self.focus = self.focus.and_then(|f| f.checked_sub(k));
        self.last_drawn_focus = self.last_drawn_focus.and_then(|f| f.checked_sub(k));
        self.last_user_focus = self.last_user_focus.and_then(|f| f.checked_sub(k));
        renumber_after_compact(&mut self.last_block_screen_rows, k);
        renumber_after_compact(&mut self.last_block_virtual_rows, k);
        self.bump_epoch();
        self.bump_version();
        k
    }

    /// Sum of the virtual rows (height + separator) the `k` blocks
    /// dropped by [`Self::compact_to`] occupied, best effort from the
    /// renderer's height cache. Must run before the cache drain.
    fn dropped_block_rows(&self, k: usize) -> usize {
        self.block_heights[..k]
            .iter()
            .map(|slot| slot.map_or(1, |(_, h)| usize::from(h) + 1))
            .sum()
    }

    pub(crate) fn last_is_live_assistant(&self) -> bool {
        matches!(
            self.blocks.last(),
            Some(Block::Assistant { live: true, .. })
        )
    }

    pub(crate) fn last_is_live_thinking(&self) -> bool {
        matches!(self.blocks.last(), Some(Block::Thinking { live: true, .. }))
    }
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
