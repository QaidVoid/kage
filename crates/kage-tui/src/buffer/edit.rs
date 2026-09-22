//! Buffer write-side: block construction, streaming, and fold ops.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl Buffer {
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
        self.mark_stream_dirty();
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
        self.mark_stream_dirty();
    }

    /// Record that the live last block grew without dropping its
    /// render caches: the stale lines keep serving until
    /// [`STREAM_REPARSE_THROTTLE`] elapses, then the cache readers
    /// force one rebuild. Version still bumps so the render loop
    /// wakes and repaints from the (possibly stale) cache.
    pub(crate) fn mark_stream_dirty(&mut self) {
        self.stream_dirty_since.get_or_insert_with(Instant::now);
        self.bump_version();
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
        self.stream_dirty_since = None;
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

    /// Drain the buffer's blocks, resetting scroll and focus to zero.
    /// Useful for `kage resume` and tests. Focus must go too: a stale
    /// index past the (now empty) block list would panic the next
    /// render, and `set_focus`'s range check never sees this path.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.clear_block_caches();
        self.scroll = 0;
        self.focus = None;
        self.last_drawn_focus = None;
        self.stream_dirty_since = None;
    }

    /// Take ownership of the blocks, leaving the buffer empty. Focus
    /// and scroll reset for the same reason as [`Self::clear`].
    pub fn take(&mut self) -> Vec<Block> {
        self.scroll = 0;
        self.focus = None;
        self.last_drawn_focus = None;
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
    /// Not to be confused with `kage_loop::compact::maybe_compact`:
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
    /// can leave fewer than `cap` blocks. Scroll survives untouched:
    /// it counts rows from the bottom, which compaction doesn't
    /// move.
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

        self.blocks.drain(0..k);
        self.block_heights.drain(0..k);
        self.block_render_lines.drain(0..k);
        self.focus = self.focus.and_then(|f| f.checked_sub(k));
        self.last_drawn_focus = self.last_drawn_focus.and_then(|f| f.checked_sub(k));
        renumber_after_compact(&mut self.last_block_screen_rows, k);
        renumber_after_compact(&mut self.last_block_virtual_rows, k);
        self.bump_version();
        k
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

/// Reindex a block-keyed row cache after `k` leading blocks were
/// dropped: entries for dropped blocks go, surviving indices shift.
fn renumber_after_compact<T, U>(rows: &mut Vec<(usize, T, U)>, k: usize) {
    rows.retain(|(i, ..)| *i >= k);
    for (i, ..) in rows.iter_mut() {
        *i -= k;
    }
}
