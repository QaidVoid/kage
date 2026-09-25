//! Tests for the conversation buffer.

use serde_json::json;

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
fn append_keeps_growing_block_cache_until_window_expires() {
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
        Some(2),
        "inside the throttle window the stale height is served"
    );

    // Once the window lapses the same query misses and the renderer
    // rebuilds just the growing block.
    buf.stream_dirty_since = Some(Instant::now().checked_sub(STREAM_REPARSE_THROTTLE).unwrap());
    assert_eq!(
        buf.cached_height(0, 80),
        Some(1),
        "only the growing block is forced to rebuild"
    );
    assert_eq!(
        buf.cached_height(1, 80),
        None,
        "the assistant block that just grew must rebuild after the window"
    );
}

#[test]
fn push_tool_result_invalidates_paired_call_height() {
    let mut buf = Buffer::new();
    buf.push_tool_call("c1", "read", json!({"path": "summary"}));
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
    buf.push_tool_call("c1", "read", json!({"path": "summary"}));
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
    buf.push_tool_call("c1", "bash", json!({"command": "ls"}));
    assert_eq!(buf.total_lines(), 1, "folded contributes header line only");
    assert!(buf.toggle_fold(0));
    assert!(buf.total_lines() > 1, "unfolded shows body lines");
}

#[test]
fn upsert_tool_call_refreshes_in_place_without_duplicates() {
    let mut buf = Buffer::new();
    buf.upsert_tool_call("c1", "write", json!({"path": "a"}));
    buf.upsert_tool_call("c1", "write", json!({"path": "a.rs", "content": "x"}));
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
            input,
            ..
        } => {
            assert_eq!(name, "write");
            assert_eq!(input_summary, "a.rs");
            assert_eq!(**input, json!({"path": "a.rs", "content": "x"}));
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn upsert_tool_call_appends_distinct_ids() {
    let mut buf = Buffer::new();
    buf.upsert_tool_call("c1", "bash", json!({"command": "ls"}));
    buf.upsert_tool_call("c2", "read", json!({}));
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
    buf.push_tool_call("c1", "bash", json!({"command": "ls"}));
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
    buf.push_tool_call("c1", "bash", json!({}));
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
    assert_eq!(buf.scroll(), Some(99));
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
    buf.push_tool_call("c1", "read", json!({"path": "a.rs"})); // 1
    buf.push_tool_result("c1", "out", false); // 2: paired with 1, skipped
    buf.append_assistant_delta("ok"); // 3: not foldable
    buf.finish_streaming();
    buf.push_tool_call("c2", "read", json!({"path": "b.rs"})); // 4
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
    buf.push_tool_call("c1", "read", json!({"path": "a.rs"})); // 1
    buf.push_tool_result("c1", "out", false); // 2: skipped
    buf.append_assistant_delta("ok"); // 3
    buf.finish_streaming();
    buf.push_tool_call("c2", "read", json!({"path": "b.rs"})); // 4
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
    buf.push_tool_call("c1", "ls", json!({"path": "."}));
    buf.set_focus(Some(0));
    assert_eq!(buf.focus(), Some(0));
    buf.set_focus(Some(1));
    assert_eq!(buf.focus(), Some(1));
    buf.set_focus(Some(99));
    assert_eq!(buf.focus(), None);
}

#[test]
fn clear_and_take_reset_focus_so_render_cannot_index_stale() {
    let mut buf = Buffer::new();
    buf.push_user("hi");
    buf.push_tool_call("c1", "ls", json!({"path": "."}));
    buf.set_focus(Some(1));
    buf.clear();
    assert_eq!(buf.focus(), None);
    assert_eq!(buf.effective_focus(), None);

    buf.push_user("back");
    buf.set_focus(Some(0));
    let blocks = buf.take();
    assert_eq!(blocks.len(), 1);
    assert_eq!(buf.focus(), None);
    assert_eq!(buf.effective_focus(), None);
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
    assert_eq!(buf.scroll(), None);
}

#[test]
fn append_does_not_disturb_user_scroll_position() {
    let mut buf = Buffer::new();
    buf.push_user("aa\nbb\ncc");
    buf.set_scroll(2);
    assert!(!buf.is_following());
    buf.append_assistant_delta("hi\nthere\nyou");
    assert_eq!(buf.scroll(), Some(2));
    assert!(!buf.is_following());
}

#[test]
fn follow_re_arms_and_pin_at_top_is_not_following() {
    let mut buf = Buffer::new();
    buf.push_user("aa\nbb\ncc");
    buf.set_scroll(2);
    buf.follow();
    assert!(buf.is_following());
    // Pinning at row 0 is a genuine top pin on a buffer taller than
    // the viewport; only the renderer's clamp turns a bottom pin
    // back into follow.
    buf.set_scroll(0);
    assert_eq!(buf.scroll(), Some(0));
    assert!(!buf.is_following());
}

#[test]
fn take_returns_blocks_and_resets_scroll() {
    let mut buf = Buffer::new();
    buf.push_user("a");
    buf.set_scroll(1);
    let taken = buf.take();
    assert_eq!(taken.len(), 1);
    assert_eq!(buf.scroll(), None);
    assert!(buf.is_following());
    assert!(buf.blocks().is_empty());
}

#[test]
fn merge_render_state_copies_renderer_state_and_keeps_appends() {
    let mut live = Buffer::new();
    live.push_user("hello");
    let mut snap = live.clone();
    snap.set_scroll(7);
    snap.set_cached_height(0, 80, 3);
    snap.set_cached_render_lines(0, 80, Arc::new(Vec::new()));
    snap.set_last_drawn_focus(Some(0));
    snap.set_last_block_screen_rows(vec![(0, 1, 4)]);
    live.push_user("world");
    live.merge_render_state(&snap);
    assert_eq!(live.scroll(), Some(7));
    assert_eq!(live.cached_height(0, 80), Some(3));
    assert_eq!(live.cached_height(1, 80), None);
    assert_eq!(live.last_drawn_focus(), Some(0));
    assert_eq!(live.block_at_screen_row(2), Some(0));
    assert_eq!(live.blocks().len(), 2);
}

#[test]
fn merge_render_state_skips_when_live_has_fewer_blocks() {
    let mut live = Buffer::new();
    live.push_user("a");
    let mut snap = Buffer::new();
    snap.push_user("a");
    snap.push_user("b");
    snap.set_scroll(4);
    live.merge_render_state(&snap);
    assert_eq!(live.scroll(), None);
    assert_eq!(live.blocks().len(), 1);
}

#[test]
fn merge_render_state_skips_across_an_epoch_change() {
    let mut live = Buffer::new();
    live.push_user("a");
    live.push_user("b");
    let mut snap = live.clone();
    snap.set_cached_height(0, 80, 9);
    snap.set_scroll(4);

    live.clear();
    live.push_user("c");
    live.push_user("d");
    live.set_cached_height(0, 80, 2);
    live.merge_render_state(&snap);
    assert_eq!(live.cached_height(0, 80), Some(2));
    assert_eq!(live.scroll(), None);
}

/// Push `n` tool call/result pairs with ids `c{start}` onward.
fn push_tool_pairs(buf: &mut Buffer, start: usize, n: usize) {
    for i in start..start + n {
        let id = format!("c{i}");
        buf.push_tool_call(&id, "bash", json!({"command": "ls"}));
        buf.push_tool_result_with_duration(&id, "out", false, None);
    }
}

#[test]
fn tool_topology_tracks_new_pairs_at_the_block_cap() {
    let mut buf = Buffer::new();
    push_tool_pairs(&mut buf, 0, MAX_BLOCKS / 2);
    assert_eq!(buf.tool_topology().result_of_call.get(&0), Some(&1));

    push_tool_pairs(&mut buf, MAX_BLOCKS / 2, 1);
    assert_eq!(buf.trim_scrollback(), 2);
    assert_eq!(buf.blocks().len(), MAX_BLOCKS);

    let topo = buf.tool_topology();
    assert_eq!(
        topo.result_of_call.get(&(MAX_BLOCKS - 2)),
        Some(&(MAX_BLOCKS - 1))
    );
    assert_eq!(
        topo.call_idx_for_result.get(&(MAX_BLOCKS - 1)),
        Some(&(MAX_BLOCKS - 2))
    );
    assert_eq!(topo.result_of_call.len(), MAX_BLOCKS / 2);
}

#[test]
fn tool_topology_rebuilds_after_clear_to_the_same_length() {
    let mut buf = Buffer::new();
    push_tool_pairs(&mut buf, 0, 2);
    assert_eq!(buf.tool_topology().result_of_call.get(&0), Some(&1));

    buf.clear();
    buf.push_user("next");
    push_tool_pairs(&mut buf, 10, 1);
    buf.push_user("again");
    let topo = buf.tool_topology();
    assert_eq!(topo.result_of_call.get(&1), Some(&2));
    assert!(!topo.result_of_call.contains_key(&0));
}

/// Live assistant block with renderer caches primed for the stale
/// reuse tests.
fn throttled_stream_fixture() -> Buffer {
    let mut buf = Buffer::new();
    buf.append_assistant_delta("hello world");
    buf.set_cached_height(0, 80, 3);
    buf.set_cached_render_lines(0, 80, Arc::new(vec![Line::from("hello world")]));
    buf
}

#[test]
fn streaming_delta_keeps_caches_inside_throttle_window() {
    let mut buf = throttled_stream_fixture();
    let v0 = buf.version();
    buf.append_assistant_delta(" and more");

    // Stale lines are served instead of forcing a re-parse, and the
    // version still moves so the render loop wakes up.
    assert_eq!(buf.cached_height(0, 80), Some(3));
    assert!(buf.cached_render_lines(0, 80).is_some());
    assert!(buf.stream_edits_pending());
    assert_ne!(buf.version(), v0);

    // A block pushed after the stream finishes it and drops the stale
    // render, since only the last block's throttled cache is refreshed.
    buf.push_user("done");
    assert_eq!(buf.cached_height(0, 80), None);
    assert!(buf.cached_render_lines(0, 80).is_none());
    assert!(matches!(
        buf.blocks()[0],
        Block::Assistant { live: false, .. }
    ));
}

#[test]
fn a_tool_call_after_streamed_text_finishes_the_text() {
    let mut buf = throttled_stream_fixture();
    buf.append_assistant_delta(" and the rest");
    buf.push_tool_call("c1", "read", json!({"path": "a.rs"}));
    buf.push_tool_call("c2", "read", json!({"path": "b.rs"}));
    buf.finish_streaming();
    assert!(matches!(
        &buf.blocks()[0],
        Block::Assistant { text, live: false } if text == "hello world and the rest"
    ));
    assert!(buf.cached_render_lines(0, 80).is_none());
    assert!(!buf.stream_edits_pending());
}

#[test]
fn streaming_reparse_forces_rebuild_after_window_expires() {
    let mut buf = throttled_stream_fixture();
    buf.append_assistant_delta(" and more");
    buf.stream_dirty_since = Some(Instant::now().checked_sub(STREAM_REPARSE_THROTTLE).unwrap());

    assert_eq!(buf.cached_height(0, 80), None);
    assert!(buf.cached_render_lines(0, 80).is_none());

    // Storing a fresh rebuild consumes the pending flag.
    buf.set_cached_render_lines(0, 80, Arc::new(vec![Line::from("rebuilt")]));
    assert!(!buf.stream_edits_pending());
    assert!(buf.cached_render_lines(0, 80).is_some());
}

#[test]
fn finish_streaming_invalidates_and_clears_throttle() {
    let mut buf = throttled_stream_fixture();
    buf.append_assistant_delta(" tail");
    buf.finish_streaming();

    assert!(!buf.stream_edits_pending());
    assert_eq!(buf.cached_height(0, 80), None);

    // After finish, a new delta still throttles (a fresh live block).
    buf.append_assistant_delta("reopened");
    assert!(buf.stream_edits_pending());
    assert_eq!(buf.cached_height(0, 80), None);
}

#[test]
fn merge_render_state_carries_throttle_flag() {
    let mut live = Buffer::new();
    live.append_assistant_delta("hello");
    let mut snap = live.clone();
    snap.set_cached_height(0, 80, 2);
    snap.set_cached_render_lines(0, 80, Arc::new(vec![Line::from("hello")]));
    assert!(!snap.stream_edits_pending());

    live.append_assistant_delta(" world");
    assert!(live.stream_edits_pending());
    live.merge_render_state(&snap);
    assert!(!live.stream_edits_pending());
}

#[test]
fn compact_drops_oldest_and_keeps_pairs_together() {
    let mut buf = Buffer::new();
    buf.push_user("1");
    buf.push_user("2");
    buf.push_user("3");
    buf.push_tool_call("A", "bash", json!({"command": "ls"}));
    buf.push_tool_result("A", "out", false);
    buf.begin_thinking();
    buf.append_thinking_delta("hmm");
    buf.push_tool_call("B", "bash", json!({"command": "ls"}));
    buf.push_tool_result("B", "out", false);
    buf.push_user("4");
    buf.push_user("5");
    assert_eq!(buf.blocks().len(), 10);

    assert_eq!(buf.compact_to(4), 6);
    assert_eq!(buf.blocks().len(), 4);
    assert!(
        matches!(&buf.blocks()[0], Block::ToolCall { call_id, .. } if call_id == "B"),
        "the frontier lands on the surviving call, not its oldest filler"
    );
    assert!(matches!(&buf.blocks()[1], Block::ToolResult { call_id, .. } if call_id == "B"));
}

#[test]
fn compact_frontier_extends_past_orphaned_results() {
    let mut buf = Buffer::new();
    buf.push_user("0");
    buf.push_tool_call("A", "bash", json!({"command": "ls"}));
    buf.begin_thinking();
    buf.append_thinking_delta("thinking");
    buf.push_tool_result("A", "out", false);
    for i in 0..4 {
        buf.push_user(format!("filler{i}"));
    }

    // The naive frontier is 3, which would leave result A rendering
    // as a composite without its call half (stuck "running"). The
    // frontier slides past the orphaned result so the pair drops
    // whole, even though that lands under the cap.
    assert_eq!(buf.compact_to(5), 4);
    assert_eq!(buf.blocks().len(), 4);
    assert!(
        matches!(&buf.blocks()[0], Block::User { text } if text == "filler0"),
        "the orphaned call/result pair is gone entirely"
    );
}

#[test]
fn compact_shifts_focus_and_drops_stale_focus() {
    let mut buf = Buffer::new();
    for i in 0..5 {
        buf.push_user(format!("m{i}"));
    }
    buf.set_focus(Some(3));
    assert_eq!(buf.compact_to(3), 2);
    assert_eq!(
        buf.focus(),
        Some(1),
        "surviving focus shifts by the drop count"
    );

    buf.set_focus(Some(0));
    assert_eq!(buf.compact_to(2), 1);
    assert_eq!(
        buf.focus(),
        None,
        "focus pointing at a dropped block clears"
    );
}

#[test]
fn compact_renumbers_renderer_row_caches() {
    let mut buf = Buffer::new();
    for i in 0..4 {
        buf.push_user(format!("m{i}"));
    }
    buf.set_last_block_screen_rows(vec![(0, 5, 8), (1, 8, 12), (2, 12, 20), (3, 20, 24)]);
    buf.set_last_block_virtual_rows(vec![(0, 0, 4), (1, 4, 9), (2, 9, 15), (3, 15, 21)]);

    assert_eq!(buf.compact_to(2), 2);
    assert_eq!(buf.block_at_screen_row(12), Some(0));
    assert_eq!(buf.screen_top_of(1), Some(20));
    assert_eq!(buf.block_at_screen_row(11), None, "dropped rows are gone");
    assert_eq!(buf.block_virtual_rows(0), Some((9, 15)));
    assert_eq!(buf.block_virtual_rows(1), Some((15, 21)));
    assert_eq!(buf.block_virtual_rows(2), None);
}

#[test]
fn compact_shifts_a_pinned_scroll_anchor() {
    let mut buf = Buffer::new();
    for i in 0..4 {
        buf.push_user(format!("m{i}"));
    }
    for idx in 0..4 {
        buf.set_cached_height(idx, 80, 1);
    }
    // Pin at m2's top: m0 and m1 occupy two virtual rows each (one
    // wrapped row + one separator), so m2 starts at row 4.
    buf.set_scroll(4);

    assert_eq!(buf.compact_to(2), 2);
    assert_eq!(
        buf.scroll(),
        Some(0),
        "the anchor tracks m2 as it slides to the top"
    );

    // A following viewport is untouched by compaction.
    let mut buf = Buffer::new();
    for i in 0..4 {
        buf.push_user(format!("m{i}"));
    }
    assert_eq!(buf.compact_to(2), 2);
    assert!(buf.is_following());
}

#[test]
fn trim_scrollback_is_a_noop_under_the_cap() {
    let mut buf = Buffer::new();
    buf.push_user("a");
    buf.push_user("b");
    let v = buf.version();
    assert_eq!(buf.trim_scrollback(), 0);
    assert_eq!(
        buf.version(),
        v,
        "a no-op compaction must not bump the version"
    );
}

#[test]
fn trim_scrollback_enforces_the_block_cap() {
    let mut buf = Buffer::new();
    let extra = 8;
    for i in 0..MAX_BLOCKS + extra {
        buf.push_user(format!("m{i}"));
    }
    assert_eq!(buf.trim_scrollback(), extra);
    assert_eq!(buf.blocks().len(), MAX_BLOCKS);
    assert!(
        matches!(&buf.blocks()[0], Block::User { text } if text == "m8"),
        "oldest blocks are the ones dropped"
    );
    assert!(
        matches!(&buf.blocks()[MAX_BLOCKS - 1], Block::User { text } if *text == format!("m{}", MAX_BLOCKS + extra - 1)),
        "newest block survives"
    );
}

#[test]
fn jump_targets_lists_messages_with_labels_not_thinking() {
    let mut buf = Buffer::new();
    buf.push_user("hello there");
    buf.begin_thinking();
    buf.append_thinking_delta("secret thoughts");
    buf.begin_assistant();
    buf.append_assistant_delta("the answer\nsecond line");
    buf.push_tool_call("c1", "bash", json!({"command": "ls"}));
    buf.push_tool_result_with_duration("c1", "ok", false, None);

    let targets = buf.jump_targets(80);
    let labels: Vec<&str> = targets.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(
        labels,
        vec!["you: hello there", "the answer", "Ran ls"],
        "thinking and consumed results are skipped"
    );
    assert_eq!(targets[0].0, 0);
    assert_eq!(targets[1].0, 2);
    assert_eq!(targets[2].0, 3);
}

#[test]
fn jump_targets_drop_markdown_and_welcome_notices() {
    let mut buf = Buffer::new();
    buf.push_custom("kage:help", "welcome to kage", false);
    buf.push_custom("kage:notify", "Interrupted", false);
    buf.append_assistant_delta("**Done**: see `main.rs`");
    buf.finish_streaming();
    let labels: Vec<String> = buf.jump_targets(80).into_iter().map(|(_, l)| l).collect();
    assert_eq!(labels, ["Done: see main.rs"]);
}

#[test]
fn jump_targets_truncate_and_skip_empty() {
    let mut buf = Buffer::new();
    buf.push_user(format!("x{}", "y".repeat(200)));
    buf.begin_assistant();
    buf.append_assistant_delta("");

    let targets = buf.jump_targets(20);
    assert_eq!(targets.len(), 1, "empty labels are skipped");
    assert_eq!(targets[0].1.chars().count(), 20);
    assert!(targets[0].1.ends_with("..."));
}

#[test]
fn focus_moves_bump_the_version() {
    let mut buf = Buffer::new();
    buf.push_user("q");
    buf.push_thinking("t");
    buf.push_tool_call("c1", "bash", json!({"command": "ls"}));
    buf.push_tool_result_with_duration("c1", "out", false, None);
    buf.append_assistant_delta("a");
    buf.finish_streaming();

    let moves: [fn(&mut Buffer) -> bool; 5] = [
        Buffer::focus_prev_any,
        Buffer::focus_next_any,
        Buffer::focus_prev,
        Buffer::focus_prev,
        Buffer::focus_next,
    ];
    for step in moves {
        let v = buf.version();
        assert!(step(&mut buf));
        assert_ne!(buf.version(), v, "a focus move must repaint");
    }
}

#[test]
fn the_first_fold_acts_on_the_last_foldable_block() {
    let mut buf = Buffer::new();
    buf.push_user("q");
    buf.push_tool_call("c1", "bash", json!({"command": "ls"}));
    buf.push_tool_result_with_duration("c1", "out", false, None);
    buf.append_assistant_delta("done");
    buf.finish_streaming();
    assert_eq!(buf.effective_focus(), Some(3));
    assert_eq!(buf.fold_target(), Some(1));
    buf.set_focus(Some(0));
    assert_eq!(buf.fold_target(), Some(0));
}
