//! Permission requests and the approval panel.

use super::*;

#[test]
fn permission_requests_open_a_prompt_and_answer_through_requests() {
    let (mut app, rx, events) = app_with_events();
    let request_id = kage_core::protocol::RequestId(7);
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::PermissionRequested {
                request_id,
                tool_call_id: None,
                tool: "shell".into(),
                subject: "ls".into(),
                input: serde_json::json!({ "command": "ls" }),
            },
        ))
        .unwrap();
    assert!(app.drain_engine_events());
    assert!(app.approval_panel.is_some());

    app.answer_permission(PermissionDecision::AllowOnce);
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce
        })
    );
}

#[test]
fn tool_timing_excludes_the_approval_wait() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![shell_start("c1"), permission_request("c1", 1)],
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Waiting);
    std::thread::sleep(std::time::Duration::from_millis(60));
    app.answer_permission(PermissionDecision::AllowOnce);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Approved);
    feed(
        &mut app,
        &events,
        vec![
            kage_core::LoopEvent::ToolExecutionStart {
                id: kage_core::ToolCallId::new("c1"),
            }
            .into(),
            kage_core::LoopEvent::ToolCallEnd {
                id: kage_core::ToolCallId::new("c1"),
                output: kage_core::ToolOutput {
                    is_error: false,
                    text: "stdout:\na\nexit: 0".into(),
                    structured: None,
                    terminate: false,
                },
            }
            .into(),
        ],
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Done);
    let buf = app.buffer.lock().unwrap();
    let duration = buf.blocks().iter().find_map(|b| match b {
        crate::buffer::Block::ToolResult { duration_ms, .. } => *duration_ms,
        _ => None,
    });
    assert!(duration.is_some_and(|ms| ms < 60), "{duration:?}");
}

#[test]
fn a_denied_call_reads_denied_after_its_result() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![shell_start("c1"), permission_request("c1", 1)],
    );
    app.answer_permission(PermissionDecision::Deny);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Denied);
    app.buffer
        .lock()
        .unwrap()
        .push_tool_result("c1", "denied by the user", true);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Denied);
}

#[test]
fn a_request_resolved_elsewhere_resumes_the_call() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            shell_start("c1"),
            shell_start("c2"),
            permission_request("c1", 1),
            permission_request("c2", 2),
        ],
    );
    assert_eq!(tool_phase(&app, "c2"), ToolPhase::Waiting);
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::PermissionResolved {
                request_id: kage_core::protocol::RequestId(2),
            }
            .into(),
        ],
    );
    assert_eq!(tool_phase(&app, "c2"), ToolPhase::Queued);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Waiting);
}

#[test]
fn the_approval_panel_replaces_the_input_below_the_buffer() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![shell_start("c1"), permission_request("c1", 1)],
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let (top, height) = {
        let buf = app.buffer.lock().unwrap();
        (buf.last_area_y(), buf.last_area_height())
    };
    let bottom = usize::from(top + height);
    let title = rows
        .iter()
        .position(|r| r.contains("Run this command?"))
        .expect("panel title");
    assert!(title >= bottom, "title at {title}, buffer ends at {bottom}");
    assert_eq!(rows[title + 1], "   $ ls");
    assert!(rows[title..].iter().any(|r| r == " > 1. Yes"), "{rows:#?}");
    assert!(
        rows[..bottom]
            .iter()
            .all(|r| !r.contains("1. Yes") && !r.contains("$ ls")),
        "{rows:#?}"
    );
}

#[test]
fn keys_right_after_the_panel_opens_are_dropped() {
    let (mut app, rx, events) = app_with_events();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    app.dispatch_key(key('y'));
    assert!(app.approval_panel.is_some());
    assert!(resolutions(&rx).is_empty());
}

#[test]
fn s_allows_the_tool_for_the_session() {
    let (mut app, rx, events) = app_with_events();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    app.approval_key_at(key('s'), past_guard());
    assert_eq!(
        resolutions(&rx),
        [RunRequest::ResolvePermission {
            request_id: kage_core::protocol::RequestId(1),
            decision: PermissionDecision::AllowSession,
        }]
    );
    assert!(app.approval_panel.is_none());
}

#[test]
fn feedback_denies_then_submits_the_text() {
    let (mut app, rx, events) = app_with_events();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    let now = past_guard();
    app.approval_key_at(key('t'), now);
    for c in "use ls".chars() {
        app.approval_key_at(key(c), now);
    }
    app.approval_key_at(code(KeyCode::Enter), now);
    assert_eq!(
        resolutions(&rx),
        [
            RunRequest::ResolvePermission {
                request_id: kage_core::protocol::RequestId(1),
                decision: PermissionDecision::Deny,
            },
            RunRequest::Submit {
                text: "use ls".to_owned(),
                images: Vec::new(),
                queue: false,
                session: None,
            },
        ]
    );
}

#[test]
fn the_count_shows_the_requests_still_waiting() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            permission_request("c1", 1),
            permission_request("c2", 2),
            permission_request("c3", 3),
        ],
    );
    let title = |app: &mut App| {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.render_into(&mut terminal).unwrap();
        snapshot_rows(&terminal)
            .into_iter()
            .find(|r| r.contains("Run this command?"))
            .expect("panel title")
    };
    assert!(title(&mut app).contains(" 1 of 3 "));
    app.approval_key_at(key('1'), past_guard());
    assert!(title(&mut app).contains(" 1 of 2 "));
    app.approval_key_at(key('y'), Instant::now());
    assert!(
        title(&mut app).contains(" 1 of 2 "),
        "the next panel guards too"
    );
    app.approval_key_at(key('y'), past_guard());
    assert!(!title(&mut app).contains(" of "));
}

#[test]
fn the_draft_survives_an_approval() {
    let (mut app, _rx, events) = app_with_events();
    type_str(&mut app, "half a thought");
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    assert!(app.footer_hint().starts_with("y/s/a/n/t or 1-5"));
    app.approval_key_at(key('t'), past_guard());
    app.approval_key_at(key('x'), past_guard());
    app.approval_key_at(code(KeyCode::Esc), past_guard());
    app.approval_key_at(key('n'), past_guard());
    assert!(app.approval_panel.is_none());
    assert_eq!(app.input.text(), "half a thought");
}

#[test]
fn answering_moves_the_row_from_waiting_to_approved() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![shell_start("c1"), permission_request("c1", 1)],
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Waiting);
    app.approval_key_at(code(KeyCode::Enter), past_guard());
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Approved);
}
