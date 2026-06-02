//! Tests for the conversation buffer.

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
