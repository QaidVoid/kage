//! Searching the conversation.

use super::*;

/// Four assistant blocks; "needle" appears in blocks 1 and 3.
fn search_fixture() -> (App, SharedBuffer) {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        for text in ["alpha", "needle one", "gamma", "needle two"] {
            buf.append_assistant_delta(text);
            buf.finish_streaming();
        }
        buf.set_focus(Some(0));
    }
    (app, buffer)
}

#[test]
fn search_jump_walks_matches_and_wraps_at_either_end() {
    let (mut app, buffer) = search_fixture();
    app.search_pattern = Some("needle".into());

    app.jump_to_search_match(true);
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.jump_to_search_match(true);
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));
    app.jump_to_search_match(true);
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.jump_to_search_match(false);
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));
}

#[test]
fn a_reopened_search_line_walks_only_what_it_shows() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    type_str(&mut app, "needle");
    app.handle_key(code(KeyCode::Enter));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.handle_key(ctrl('f'));
    assert_eq!(app.search_line.as_ref().unwrap().text(), "");
    app.handle_key(code(KeyCode::Down));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.search_pattern.as_deref(), Some("needle"));
}

#[test]
fn search_cache_refreshes_when_pattern_changes() {
    let (mut app, _buffer) = search_fixture();
    app.search_pattern = Some("needle".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[1, 3]);
    // Fixture pins focus on block 0, which is not a match.
    assert_eq!(app.compute_search_match_count(), Some((0, 2)));

    // Same buffer version, different pattern: the cache must not go
    // stale (the counter is a binary search over these indices).
    app.search_pattern = Some("gamma".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[2]);
    assert_eq!(app.compute_search_match_count(), Some((0, 1)));

    // No match anywhere: empty list, count of zero.
    app.search_pattern = Some("zzz".into());
    assert_eq!(app.compute_search_match_count(), Some((0, 0)));
}

#[test]
fn search_cache_rebuilds_after_scrollback_compaction() {
    let (mut app, buffer) = search_fixture();
    app.search_pattern = Some("needle".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[1, 3]);

    // Drop block 0: compaction shifts indices and bumps the buffer
    // version, so the cached match list must rebuild to [0, 2].
    // Focus pointed at the dropped block and falls back to the
    // transcript bottom (block 2), which is itself a match.
    assert_eq!(buffer.lock().unwrap().compact_to(3), 1);
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[0, 2]);
    assert_eq!(app.compute_search_match_count(), Some((2, 2)));
}

#[test]
fn search_count_is_none_without_a_pattern() {
    let (mut app, _buffer) = search_fixture();
    assert_eq!(app.compute_search_match_count(), None);
    app.jump_to_search_match(true);
    // Jump with no pattern is a no-op; the fixture pinned block 0.
    assert_eq!(app.buffer.lock().unwrap().focus(), Some(0));
}

#[test]
fn ctrl_f_counts_matches_while_typing() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    assert!(app.search_line.is_some());
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    assert!(app.search_line.is_some(), "still open before Enter");
    assert_eq!(app.compute_search_match_count(), Some((1, 2)));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.handle_key(key('x'));
    assert_eq!(app.compute_search_match_count(), Some((0, 0)));
    assert_eq!(
        buffer.lock().unwrap().focus(),
        Some(0),
        "no match keeps the view"
    );
}

#[test]
fn search_line_down_and_up_walk_matches() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Down));
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));
    assert_eq!(app.search_line.as_ref().unwrap().text(), "needle");
    app.handle_key(code(KeyCode::Up));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.handle_key(code(KeyCode::Enter));
    assert!(app.search_line.is_none());
    assert_eq!(app.search_pattern.as_deref(), Some("needle"));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
}

#[test]
fn search_from_the_bottom_lands_on_the_latest_match() {
    let (mut app, buffer) = search_fixture();
    buffer.lock().unwrap().set_focus(None);
    app.handle_key(ctrl('f'));
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));
}

#[test]
fn esc_restores_the_previous_pattern_and_view() {
    let (mut app, buffer) = search_fixture();
    app.search_pattern = Some("gamma".into());
    buffer.lock().unwrap().set_scroll(2);
    app.handle_key(ctrl('f'));
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Down));
    assert_eq!(app.search_pattern.as_deref(), Some("needle"));
    app.handle_key(code(KeyCode::Esc));
    assert!(app.search_line.is_none());
    assert_eq!(app.search_pattern.as_deref(), Some("gamma"));
    let buf = buffer.lock().unwrap();
    assert_eq!(buf.focus(), Some(0));
    assert_eq!(buf.scroll(), Some(2));
}

#[test]
fn a_block_leaving_the_match_set_loses_its_match_rule() {
    let (mut app, buffer) = search_fixture();
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
    let rule_of_block_1 = |app: &mut App, terminal: &mut Terminal<TestBackend>| {
        app.render_into(terminal).unwrap();
        let buf = buffer.lock().unwrap();
        let (top, _) = buf.screen_rows_of(1).expect("block 1 painted");
        terminal.backend().buffer()[(0, top)].symbol().to_owned()
    };
    app.search_pattern = Some("needle".into());
    assert_eq!(rule_of_block_1(&mut app, &mut terminal), "\u{258c}");
    app.search_pattern = Some("gamma".into());
    assert_eq!(rule_of_block_1(&mut app, &mut terminal), " ");
}

#[test]
fn pasting_into_the_search_line_searches() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    app.handle_paste("gamma");
    assert_eq!(app.compute_search_match_count(), Some((1, 1)));
    assert_eq!(buffer.lock().unwrap().focus(), Some(2));
}

#[test]
fn noh_command_clears_search_highlighting() {
    let (mut app, _buffer) = search_fixture();
    app.search_pattern = Some("needle".into());
    assert_eq!(app.compute_search_match_count(), Some((0, 2)));

    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("noh", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );

    assert!(app.search_pattern.is_none());
    assert_eq!(app.compute_search_match_count(), None);
    // The cached match list empties on the next refresh.
    app.refresh_search_matches();
    assert!(app.search_matches().is_empty());
}

#[test]
fn idle_esc_clears_the_search_and_a_session_change_drops_it() {
    let (mut app, _rx, events) = app_with_events();
    app.set_editor_modeless(true);
    app.search_pattern = Some("needle".into());
    assert!(app.footer_hint().starts_with("esc to clear the search"));
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.search_pattern, None);
    app.search_pattern = Some("needle".into());
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::SessionChanged {
                path: std::path::PathBuf::from("/tmp/s.jsonl"),
                title: None,
                messages: Vec::new(),
                compaction: None,
            }
            .into(),
        ],
    );
    assert_eq!(app.search_pattern, None);
    assert_eq!(app.compute_search_match_count(), None);
}
