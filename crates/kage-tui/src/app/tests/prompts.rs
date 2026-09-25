//! Prompts and engine events: submitting, delivery, pending rows and
//! session changes.

use super::*;

#[test]
fn submitting_a_prompt_sends_it_without_painting() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    // Default mode is Insert; type "hi" and press Enter.
    app.handle_key(key('h'));
    app.handle_key(key('i'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let req = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert_eq!(
        req,
        RunRequest::Submit {
            text: "hi".into(),
            images: Vec::new(),
            queue: false,
            session: None,
        }
    );
    assert!(
        !buffer
            .lock()
            .unwrap()
            .blocks()
            .iter()
            .any(|b| matches!(b, crate::buffer::Block::User { .. })),
        "the user block appears when the engine delivers the prompt"
    );
    assert_eq!(app.input().mode(), Mode::Insert);
}

#[test]
fn submit_while_a_run_is_in_flight_is_still_sent() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let usage = crate::usage::shared_session_usage();
    usage.lock().unwrap().working = true;
    app.set_session_usage(usage);

    app.handle_submit("later".into(), false);

    assert_eq!(
        rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        RunRequest::Submit {
            text: "later".into(),
            images: Vec::new(),
            queue: false,
            session: None,
        }
    );
}

#[test]
fn submit_carries_attached_images() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    app.input.attach_image(crate::image::AttachedImage {
        source: kage_core::ImageSource::Base64 {
            data: "AAAA".into(),
        },
        mime: "image/png".into(),
        label: "shot.png".into(),
        bytes: 3,
    });

    app.handle_submit("look".into(), false);

    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::Submit { text, images, .. } => {
            assert_eq!(text, "look");
            assert_eq!(images.len(), 1);
        }
        other => panic!("expected Submit with images, got {other:?}"),
    }
}

#[test]
fn delivered_prompts_paint_user_blocks() {
    let (mut app, _rx, events) = app_with_events();
    let session = kage_core::SessionId::new();
    let message = kage_core::Message::new(
        kage_core::Role::User,
        vec![kage_core::Content::Text { text: "hi".into() }],
        None,
    );
    events
        .send(envelope(
            session,
            1,
            kage_core::LoopEvent::MessageAppended { message },
        ))
        .unwrap();
    assert!(app.drain_engine_events());
    assert!(matches!(
        app.buffer.lock().unwrap().blocks().last(),
        Some(crate::buffer::Block::User { text }) if text == "hi"
    ));
}

#[test]
fn events_from_other_sessions_are_ignored() {
    let (mut app, _rx, events) = app_with_events();
    let (mine, other) = (kage_core::SessionId::new(), kage_core::SessionId::new());
    let notice = |text: &str| kage_core::protocol::HostEvent::Notice {
        level: kage_core::protocol::NoticeLevel::Error,
        text: text.into(),
        transient: false,
    };
    events.send(envelope(mine, 1, notice("mine"))).unwrap();
    events.send(envelope(other, 1, notice("other"))).unwrap();
    app.drain_engine_events();
    let count = app.buffer.lock().unwrap().blocks().len();
    assert_eq!(count, 1);
}

#[test]
fn run_ended_stops_every_running_tool() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(&mut app, &events, vec![bash_start("c1")]);
    assert!(app.has_running_tool_call());
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::RunEnded {
                outcome: kage_core::protocol::RunOutcome::Cancelled,
            }
            .into(),
        ],
    );
    assert!(!app.has_running_tool_call());
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Interrupted);
}

#[test]
fn session_changed_rebuilds_the_transcript() {
    let (mut app, _rx, events) = app_with_events();
    app.buffer.lock().unwrap().push_user("stale");
    let message = kage_core::Message::new(
        kage_core::Role::User,
        vec![kage_core::Content::Text {
            text: "restored".into(),
        }],
        None,
    );
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::SessionChanged {
                path: "/tmp/s.jsonl".into(),
                title: None,
                messages: vec![message],
            },
        ))
        .unwrap();
    app.drain_engine_events();
    let buf = app.buffer.lock().unwrap();
    assert_eq!(buf.blocks().len(), 1);
    assert!(matches!(
        buf.blocks().first(),
        Some(crate::buffer::Block::User { text }) if text == "restored"
    ));
}

#[test]
fn state_changes_update_the_modeline() {
    let (mut app, _rx, events) = app_with_events();
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::StateChanged {
                state: kage_core::protocol::SessionState {
                    model: "mock:m".into(),
                    working: true,
                    ..Default::default()
                },
            },
        ))
        .unwrap();
    app.drain_engine_events();
    let usage = app.session_usage_snapshot().unwrap();
    assert_eq!(usage.model, "mock:m");
    assert!(usage.working);
    assert!(app.is_run_in_flight());
}

#[test]
fn pending_rows_show_until_delivered_steers_first() {
    let (mut app, _rx, events) = app_with_events();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    for c in "later".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Tab));
    for c in "now".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Enter));
    let steer = format!("  > now{}after the current tool call", " ".repeat(25));
    let queue = format!("  > later{}when this run ends", " ".repeat(32));
    assert_eq!(pending_rows(&mut app), [steer, queue.clone()]);
    feed(&mut app, &events, vec![user_message("now")]);
    assert_eq!(pending_rows(&mut app), [queue]);
    feed(
        &mut app,
        &events,
        vec![user_message("rewritten by a plugin")],
    );
    assert!(pending_rows(&mut app).is_empty());
}

#[test]
fn pending_rows_fold_past_three_and_clear_on_session_change() {
    let (mut app, _rx, events) = app_with_events();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    for text in ["one", "two", "three", "four", "five"] {
        app.handle_submit(text.into(), true);
    }
    let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let first = rows.iter().position(|r| r.starts_with("  > one")).unwrap();
    assert!(rows[first + 2].starts_with("  > three"), "{rows:#?}");
    assert_eq!(rows[first + 3], "  +2 more");
    assert!(rows[first + 4].starts_with('\u{2500}'), "{rows:#?}");
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::SessionChanged {
                path: std::path::PathBuf::from("/tmp/s.jsonl"),
                title: None,
                messages: Vec::new(),
            }
            .into(),
        ],
    );
    assert!(app.pending.is_empty());
}

#[test]
fn pasting_an_image_path_attaches_instead_of_inserting_text() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let dir = std::env::temp_dir().join(format!("kage-paste-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1]).unwrap();

    app.handle_paste(&png.to_string_lossy());
    assert_eq!(app.input.attached().len(), 1, "image path attached");
    assert!(
        app.input.text().contains("[image #1 shot.png"),
        "an editable marker is inserted, not the raw path: {:?}",
        app.input.text()
    );
    assert!(
        !app.input.text().contains(&*dir.to_string_lossy()),
        "the path itself was not pasted as text: {:?}",
        app.input.text()
    );

    app.handle_paste("just some text");
    assert_eq!(app.input.attached().len(), 1, "no new attachment");
    assert!(
        app.input.text().contains("just some text"),
        "non-image paste inserted verbatim"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn shell_submit_sends_run_shell_without_a_user_block() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.input.set_modeless(true);
    for c in "!ls".chars() {
        app.dispatch_key(key(c));
    }
    app.dispatch_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::RunShell(cmd) => assert_eq!(cmd, "ls"),
        other => panic!("expected RunShell, got {other:?}"),
    }
    assert!(
        !buffer
            .lock()
            .unwrap()
            .blocks()
            .iter()
            .any(|b| matches!(b, crate::buffer::Block::User { .. })),
        "shell submit must not paint a user block"
    );
}
