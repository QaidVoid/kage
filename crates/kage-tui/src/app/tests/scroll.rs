//! Scrolling the conversation and following new content.

use super::*;

#[test]
fn scrolling_up_freezes_viewport_when_more_content_arrives() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        for i in 0..20 {
            buf.push_user(format!("line{i}"));
        }
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    // Prime the renderer so `last_virtual_top` reflects a real frame;
    // the scroll anchor derives from it while following.
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let vt0 = buffer.lock().unwrap().last_virtual_top();
    // Default mode is Insert; switch to Normal for scrolling keys.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // k/j/G are pane-aware: switch to the buffer pane so scrolling
    // keys hit the buffer instead of moving the input cursor.
    app.input.set_focused_pane(Pane::Buffer);
    // User scrolls up by 5 rows: an absolute anchor below the bottom.
    for _ in 0..5 {
        app.handle_key(key('k'));
    }
    let pinned = vt0.saturating_sub(5);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(pinned));
    // Streaming delta arrives and the frame repaints.
    if let Ok(mut buf) = buffer.lock() {
        buf.append_assistant_delta("new\nstreaming\ncontent");
    }
    app.render_into(&mut terminal).unwrap();
    // The viewport stayed exactly where the user was reading.
    assert_eq!(buffer.lock().unwrap().scroll(), Some(pinned));
    assert_eq!(buffer.lock().unwrap().last_virtual_top(), pinned);
    // Pressing G snaps back to bottom (auto-follow re-armed).
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    assert_eq!(buffer.lock().unwrap().scroll(), None);
    assert!(buffer.lock().unwrap().is_following());
}

#[test]
fn focusing_an_offscreen_block_scrolls_it_into_view() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        for i in 0..20 {
            buf.push_user(format!("line{i}"));
        }
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    // The bottom of the buffer is on screen; block 0 is not.
    assert!(buffer.lock().unwrap().screen_rows_of(0).is_none());

    // Focusing block 0 brings it into view on the next frame.
    buffer.lock().unwrap().set_focus(Some(0));
    app.render_into(&mut terminal).unwrap();
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));
    assert_eq!(buffer.lock().unwrap().last_virtual_top(), 0);
    assert!(buffer.lock().unwrap().screen_rows_of(0).is_some());

    // Focusing the newest block from the top clamps to the bottom and
    // re-arms follow.
    buffer.lock().unwrap().set_focus(Some(19));
    app.render_into(&mut terminal).unwrap();
    assert!(buffer.lock().unwrap().is_following());
    assert!(buffer.lock().unwrap().screen_rows_of(19).is_some());
}

#[test]
fn sending_a_prompt_while_scrolled_up_follows_the_conversation() {
    let (mut app, _rx, _events) = app_with_events();
    app.set_editor_modeless(true);
    {
        let mut buf = lock(&app.buffer);
        for _ in 0..40 {
            buf.push_user("earlier");
        }
        buf.set_scroll(0);
        assert!(!buf.is_following());
    }
    type_str(&mut app, "hi");
    app.handle_key(code(KeyCode::Enter));
    assert!(lock(&app.buffer).is_following());
}
