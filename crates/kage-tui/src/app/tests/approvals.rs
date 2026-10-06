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
    let t0 = Instant::now();
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
    let duration = buf.blocks().iter().find_map(|b| match b.as_ref() {
        crate::buffer::Block::ToolResult { duration_ms, .. } => *duration_ms,
        _ => None,
    });
    let duration = duration.expect("the result times the call");
    assert!(
        u128::from(duration) + 10 < t0.elapsed().as_millis(),
        "the approval wait leaked into the duration: {duration:?}"
    );
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
fn approval_keys_exactly_at_the_guard_boundary_flip() {
    let (mut app, rx, events) = app_with_events();
    let t0 = Instant::now();
    feed(
        &mut app,
        &events,
        vec![
            permission_request("c1", 1),
            permission_request("c2", 2),
            permission_request("c3", 3),
        ],
    );
    let inside = t0 + crate::overlay::approval::TYPE_AHEAD_GUARD - Duration::from_millis(1);
    app.approval_key_at(key('1'), inside);
    app.approval_key_at(code(KeyCode::Esc), inside);
    assert!(app.approval_panel.is_some(), "guarded keys are dropped");
    assert!(rx_idle(&rx), "guarded keys answer nothing");

    let decide = |request: u64, decision| RunRequest::ResolvePermission {
        request_id: kage_core::protocol::RequestId(request),
        decision,
    };
    let past = Instant::now() + crate::overlay::approval::TYPE_AHEAD_GUARD;
    app.approval_key_at(key('1'), past);
    assert_eq!(
        rx.try_recv().unwrap(),
        decide(1, PermissionDecision::AllowOnce)
    );
    let past = Instant::now() + crate::overlay::approval::TYPE_AHEAD_GUARD;
    app.approval_key_at(code(KeyCode::Esc), past);
    assert_eq!(rx.try_recv().unwrap(), decide(2, PermissionDecision::Deny));
    let past = Instant::now() + crate::overlay::approval::TYPE_AHEAD_GUARD;
    app.approval_key_at(key('y'), past);
    assert_eq!(
        rx.try_recv().unwrap(),
        decide(3, PermissionDecision::AllowOnce)
    );
    assert!(app.approval_panel.is_none());
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

#[test]
fn an_unparseable_question_request_falls_back_and_toasts_a_warning() {
    let (mut app, _rx, events) = app_with_events();
    app.set_toasts(crate::toast::shared_toasts());
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::PermissionRequested {
                request_id: kage_core::protocol::RequestId(5),
                tool_call_id: None,
                tool: kage_core::protocol::ASK_USER_QUESTION_TOOL.into(),
                subject: "questions".into(),
                input: serde_json::json!({"questions": [{"header": 1}]}),
            }
            .into(),
        ],
    );
    let panel = app.approval_panel.as_ref().expect("panel opens");
    assert!(panel.parse_notice().is_some());
    assert!(panel.hint().starts_with("y/s/a/n/t or 1-5"), "plain panel");
    let toasts = app.live_toasts();
    assert!(
        toasts
            .iter()
            .any(|t| t.kind == ToastKind::Warning && t.text.contains("did not parse")),
        "{toasts:?}"
    );
}

#[test]
fn a_question_opens_the_panel_and_its_answers_go_back() {
    use kage_core::protocol::{HostEvent, Question, QuestionOption, RequestId};
    let (mut app, rx, events) = app_with_events();
    let choices = |labels: &[&str]| {
        labels
            .iter()
            .map(|label| QuestionOption {
                label: (*label).into(),
                description: String::new(),
            })
            .collect()
    };
    let questions = vec![
        Question {
            header: "Store".into(),
            question: "Where should sessions live?".into(),
            options: choices(&["Disk", "Memory"]),
            multi_select: false,
        },
        Question {
            header: "Format".into(),
            question: "Which formats?".into(),
            options: choices(&["JSON", "TOML"]),
            multi_select: true,
        },
    ];
    let request_id = RequestId(9);
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            HostEvent::QuestionAsked {
                request_id,
                tool_call_id: None,
                questions,
            },
        ))
        .unwrap();
    assert!(app.drain_engine_events());
    assert!(app.approval_panel.is_some());
    let rows = rendered(&mut app, 80, 24).join("\n");
    assert!(rows.contains("Where should sessions live?"), "{rows}");
    assert!(rows.contains("3. Answer in my own words"), "{rows}");

    let now = past_guard();
    app.approval_key_at(key('2'), now);
    app.approval_key_at(key('1'), now);
    app.approval_key_at(key('2'), now);
    app.approval_key_at(key('4'), now);
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::AnswerQuestion {
            request_id,
            answers: Some(vec![
                vec!["Memory".into()],
                vec!["JSON".into(), "TOML".into()]
            ]),
        })
    );
    assert!(app.approval_panel.is_none());
}
