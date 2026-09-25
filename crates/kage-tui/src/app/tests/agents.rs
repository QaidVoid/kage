//! Agent sessions: their buffers, cards, pinned rows, focused views, the
//! agents overlay and resumed agents.

use super::*;

/// The progress text of the tool call `id` in `buffer`.
fn progress_of(buffer: &SharedBuffer, id: &str) -> String {
    let buf = buffer.lock().unwrap();
    buf.blocks()
        .iter()
        .find_map(|b| match b {
            crate::buffer::Block::ToolCall {
                call_id, progress, ..
            } if call_id == id => Some(progress.clone()),
            _ => None,
        })
        .expect("tool call present")
}

#[test]
fn agent_deltas_land_in_the_agent_buffer_only() {
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    let delta = kage_core::LoopEvent::TextDelta {
        id: kage_core::MessageId::new(),
        delta: "child reply".into(),
    };
    send_to(
        &mut app,
        &events,
        child,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            delta.into(),
        ],
    );
    let has_reply = |buffer: &SharedBuffer| {
        buffer.lock().unwrap().blocks().iter().any(
            |b| matches!(b, crate::buffer::Block::Assistant { text, .. } if text == "child reply"),
        )
    };
    assert!(has_reply(&app.agent_buffers[&child]));
    assert!(!has_reply(&app.buffer));
}

#[test]
fn an_agent_card_follows_its_latest_tool_and_asks() {
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    assert_eq!(progress_of(&app.buffer, "a1"), "queued\n0 tools");

    let grep = kage_core::LoopEvent::ToolCallStart {
        id: kage_core::ToolCallId::new("c1"),
        name: "grep".into(),
        input_partial: serde_json::json!({ "pattern": "export ", "path": "src/components" }),
    };
    let usage = kage_core::protocol::Usage {
        total: kage_core::TokenUsage {
            input: 20_000,
            output: 2_000,
            ..kage_core::TokenUsage::default()
        },
        ..kage_core::protocol::Usage::default()
    };
    send_to(
        &mut app,
        &events,
        child,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            grep.into(),
            kage_core::LoopEvent::ToolExecutionStart {
                id: kage_core::ToolCallId::new("c1"),
            }
            .into(),
            kage_core::protocol::HostEvent::UsageUpdated { usage }.into(),
        ],
    );
    assert_eq!(
        progress_of(&app.buffer, "a1"),
        "Searching \"export \" in src/components\n1 tool \u{b7} 22k tok"
    );
    send_to(
        &mut app,
        &events,
        child,
        vec![
            kage_core::LoopEvent::ToolCallEnd {
                id: kage_core::ToolCallId::new("c1"),
                output: kage_core::ToolOutput::default(),
            }
            .into(),
        ],
    );
    assert!(
        progress_of(&app.buffer, "a1").starts_with("Searched \"export \" in src/components\n"),
        "{}",
        progress_of(&app.buffer, "a1")
    );

    let input = serde_json::json!({ "command": "cargo test -p router" });
    let bash = kage_core::LoopEvent::ToolCallStart {
        id: kage_core::ToolCallId::new("c2"),
        name: "bash".into(),
        input_partial: input.clone(),
    };
    let ask = kage_core::protocol::HostEvent::PermissionRequested {
        request_id: kage_core::protocol::RequestId(9),
        tool_call_id: Some(kage_core::ToolCallId::new("c2")),
        tool: "bash".into(),
        subject: "cargo test -p router".into(),
        input,
    };
    send_to(&mut app, &events, child, vec![bash.into(), ask.into()]);
    assert_eq!(
        progress_of(&app.buffer, "a1"),
        "Waiting for approval: $ cargo test -p router\n2 tools \u{b7} 22k tok"
    );
    let agent_buffer = Arc::clone(&app.agent_buffers[&child]);
    let phase = agent_buffer
        .lock()
        .unwrap()
        .blocks()
        .iter()
        .find_map(|b| match b {
            crate::buffer::Block::ToolCall { call_id, phase, .. } if call_id == "c2" => {
                Some(*phase)
            }
            _ => None,
        });
    assert_eq!(phase, Some(crate::view::tool_view::ToolPhase::Waiting));
    assert_eq!(
        tool_phase(&app, "a1"),
        crate::view::tool_view::ToolPhase::Running
    );
}

#[test]
fn an_agent_ask_is_labeled_and_its_feedback_goes_to_the_agent() {
    let (mut app, rx, events) = app_with_events();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    send_to(
        &mut app,
        &events,
        child,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            bash_start("c1"),
            permission_request("c1", 5),
        ],
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("explore: explore task \u{b7} Run this command?")),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("5. No, and tell explore what to do instead")),
        "{rows:#?}"
    );
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
                request_id: kage_core::protocol::RequestId(5),
                decision: PermissionDecision::Deny,
            },
            RunRequest::Submit {
                text: "use ls".to_owned(),
                images: Vec::new(),
                queue: false,
                session: Some(child),
            },
        ]
    );
    assert!(
        app.pending
            .iter()
            .all(|(session, _)| *session == Some(child)),
        "no main pending row for the agent"
    );
}

#[test]
fn run_ends_drop_only_their_own_session_approvals() {
    let (mut app, _rx, events) = app_with_events();
    let first = spawn_agent(&mut app, &events, "a1", "explore");
    let second = spawn_agent(&mut app, &events, "a2", "general");
    send_to(
        &mut app,
        &events,
        first,
        vec![bash_start("c1"), permission_request("c1", 1)],
    );
    send_to(
        &mut app,
        &events,
        second,
        vec![bash_start("c1"), permission_request("c1", 2)],
    );
    let shown = |app: &App| app.pending_permission.as_ref().map(|a| a.session);
    assert_eq!(shown(&app), Some(first));
    feed(
        &mut app,
        &events,
        vec![run_ended(kage_core::protocol::RunOutcome::Cancelled)],
    );
    assert_eq!(
        shown(&app),
        Some(first),
        "the main run end keeps agent asks"
    );
    assert_eq!(app.permission_queue.len(), 1);
    send_to(
        &mut app,
        &events,
        first,
        vec![run_ended(kage_core::protocol::RunOutcome::Cancelled)],
    );
    assert_eq!(shown(&app), Some(second));
    assert!(app.permission_queue.is_empty());
    send_to(
        &mut app,
        &events,
        second,
        vec![run_ended(kage_core::protocol::RunOutcome::Completed)],
    );
    assert!(app.approval_panel.is_none());
    assert!(app.pending_permission.is_none());
}

#[test]
fn agent_prompts_leave_the_main_pending_rows_alone() {
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    lock(app.session_usage.as_ref().unwrap()).working = true;
    app.handle_submit("later".into(), true);
    assert_eq!(app.pending.len(), 1);
    send_to(&mut app, &events, child, vec![user_message("the task")]);
    assert_eq!(app.pending.len(), 1);
}

#[test]
fn agent_state_and_usage_leave_the_footer_alone() {
    let (mut app, _rx, events) = app_with_events();
    let status = Arc::new(Mutex::new("main:m".to_owned()));
    app.set_status_model(Arc::clone(&status));
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    let state = kage_core::protocol::SessionState {
        model: "agent:m".into(),
        thinking: Some(kage_core::ThinkingLevel::Off),
        ..kage_core::protocol::SessionState::default()
    };
    let usage = kage_core::protocol::Usage {
        context_used: 99,
        ..kage_core::protocol::Usage::default()
    };
    send_to(
        &mut app,
        &events,
        child,
        vec![
            kage_core::protocol::HostEvent::StateChanged { state }.into(),
            kage_core::protocol::HostEvent::UsageUpdated { usage }.into(),
        ],
    );
    assert_eq!(*status.lock().unwrap(), "main:m");
    let usage = app.session_usage_snapshot().unwrap();
    assert_eq!(usage.model, "");
    assert_eq!(usage.current_context, 0);
    assert_eq!(app.agents.get(child).unwrap().model, "agent:m");
}

#[test]
fn the_working_row_counts_live_agents() {
    let (mut app, _rx, events) = app_with_events();
    app.set_editor_modeless(true);
    app.run_started = Instant::now().checked_sub(Duration::from_secs(41));
    spawn_agent(&mut app, &events, "a1", "explore");
    let second = spawn_agent(&mut app, &events, "a2", "explore");
    let label = |app: &App| app.activity_label(&lock(&app.buffer), 80).unwrap();
    assert_eq!(label(&app), "Waiting for 2 agents (41s, esc to interrupt)");
    send_to(
        &mut app,
        &events,
        second,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            run_ended(kage_core::protocol::RunOutcome::Completed),
        ],
    );
    assert!(
        label(&app).starts_with("Waiting for 1 agent ("),
        "{}",
        label(&app)
    );
}

#[test]
fn the_main_session_change_forgets_its_agents() {
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    assert!(app.agents.get(child).is_some());
    assert!(app.agent_buffers.contains_key(&child));
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
    assert!(app.agents.get(child).is_none());
    assert!(app.agent_buffers.is_empty());
}

#[test]
fn the_pinned_list_shows_queued_running_and_waiting_agents_only() {
    use crate::view::AgentRowState;
    let (mut app, _rx, events) = app_with_events();
    let queued = spawn_agent(&mut app, &events, "a1", "queued");
    let running = spawn_agent(&mut app, &events, "a2", "running");
    let asking = spawn_agent(&mut app, &events, "a3", "asking");
    let done = spawn_agent(&mut app, &events, "a4", "done");
    let grep = kage_core::LoopEvent::ToolCallStart {
        id: kage_core::ToolCallId::new("c1"),
        name: "grep".into(),
        input_partial: serde_json::json!({ "pattern": "export ", "path": "src" }),
    };
    send_to(
        &mut app,
        &events,
        running,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            grep.into(),
            kage_core::LoopEvent::ToolExecutionStart {
                id: kage_core::ToolCallId::new("c1"),
            }
            .into(),
        ],
    );
    app.picker = Some(OverlayPicker::new("busy", vec![PickItem::simple("x")]));
    send_to(
        &mut app,
        &events,
        asking,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            bash_start("c1"),
            permission_request("c1", 3),
        ],
    );
    send_to(
        &mut app,
        &events,
        done,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            run_ended(kage_core::protocol::RunOutcome::Completed),
        ],
    );
    assert!(app.approval_panel.is_none());
    assert_eq!(
        pinned(&app),
        [
            (1, "queued".to_owned(), AgentRowState::Queued),
            (1, "running".to_owned(), AgentRowState::Running),
            (1, "asking".to_owned(), AgentRowState::Waiting),
        ]
    );
    let rows = app.agent_rows();
    assert_eq!(rows[0].session, queued);
    assert_eq!(rows[0].description, "queued task");
    assert_eq!(rows[0].elapsed_ms, None);
    assert_eq!(rows[1].activity, "Searching \"export \" in src");
    assert!(rows[1].elapsed_ms.is_some());
}

#[test]
fn a_nested_agent_is_pinned_under_its_parent() {
    let (mut app, _rx, events) = app_with_events();
    let parent = spawn_agent(&mut app, &events, "a1", "general");
    let sibling = spawn_agent(&mut app, &events, "a2", "explore");
    let nested = kage_core::SessionId::new();
    let spawned = kage_core::protocol::HostEvent::AgentSpawned {
        parent,
        tool_call_id: kage_core::ToolCallId::new("n1"),
        agent: "test".into(),
        description: "run the provider tests".into(),
    };
    send_to(&mut app, &events, nested, vec![spawned.into()]);
    for session in [parent, sibling, nested] {
        send_to(
            &mut app,
            &events,
            session,
            vec![kage_core::protocol::HostEvent::RunStarted.into()],
        );
    }
    let depths: Vec<(usize, String)> = pinned(&app)
        .into_iter()
        .map(|(depth, agent, _)| (depth, agent))
        .collect();
    assert_eq!(
        depths,
        [
            (1, "general".to_owned()),
            (2, "test".to_owned()),
            (1, "explore".to_owned()),
        ]
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let general = rows
        .iter()
        .position(|r| r.contains(" general  general task"))
        .unwrap();
    assert!(rows[general + 1].starts_with("    "), "{rows:#?}");
    assert!(rows[general + 1].contains(" test "), "{rows:#?}");
    assert!(rows[general + 3].starts_with('\u{2500}'), "{rows:#?}");
}

#[test]
fn the_pinned_list_hides_while_the_approval_panel_is_open() {
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    send_to(
        &mut app,
        &events,
        child,
        vec![kage_core::protocol::HostEvent::RunStarted.into()],
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let pinned_row = |app: &mut App, terminal: &mut Terminal<TestBackend>| {
        app.render_into(terminal).unwrap();
        snapshot_rows(terminal)
            .into_iter()
            .any(|r| r.contains("explore  explore task"))
    };
    assert!(pinned_row(&mut app, &mut terminal));
    send_to(
        &mut app,
        &events,
        child,
        vec![bash_start("c1"), permission_request("c1", 4)],
    );
    assert!(app.approval_panel.is_some());
    assert!(app.agent_rows().is_empty());
    assert!(!pinned_row(&mut app, &mut terminal));
    app.answer_permission(PermissionDecision::AllowOnce);
    assert!(pinned_row(&mut app, &mut terminal));
}

/// A modeless App with one running `explore` agent on screen.
fn focused_app() -> (
    App,
    mpsc::Receiver<RunRequest>,
    mpsc::Sender<kage_core::protocol::Envelope>,
    kage_core::SessionId,
) {
    let (mut app, rx, events) = app_with_events();
    app.set_editor_modeless(true);
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    send_to(
        &mut app,
        &events,
        child,
        vec![kage_core::protocol::HostEvent::RunStarted.into()],
    );
    app.focus_agent(child);
    assert_eq!(app.focus, Some(child));
    (app, rx, events, child)
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(key(c));
    }
}

fn text_delta(text: &str) -> kage_core::protocol::Event {
    kage_core::LoopEvent::TextDelta {
        id: kage_core::MessageId::new(),
        delta: text.into(),
    }
    .into()
}

#[test]
fn focusing_shows_the_agent_and_going_back_restores_the_main_scroll() {
    let (mut app, _rx, events, child) = focused_app();
    app.set_focus(None);
    {
        let mut buf = lock(&app.root_buffer);
        for n in 0..40 {
            buf.push_user(format!("main line {n}"));
        }
    }
    rendered(&mut app, 60, 16);
    app.scroll_by(-10);
    let scroll = lock(&app.buffer).scroll();
    assert!(scroll.is_some());
    send_to(&mut app, &events, child, vec![text_delta("child reply")]);

    app.focus_agent(child);
    assert!(Arc::ptr_eq(&app.buffer, &app.agent_buffers[&child]));
    let rows = rendered(&mut app, 60, 16);
    assert!(rows.iter().any(|r| r.contains("child reply")), "{rows:#?}");
    assert!(rows.iter().all(|r| !r.contains("main line")), "{rows:#?}");

    app.leave_agent();
    assert_eq!(app.focus, None);
    assert!(Arc::ptr_eq(&app.buffer, &app.root_buffer));
    assert_eq!(lock(&app.buffer).scroll(), scroll);
}

#[test]
fn agent_deltas_repaint_while_focused() {
    let (mut app, _rx, events, child) = focused_app();
    rendered(&mut app, 60, 16);
    events
        .send(envelope(child, 9, text_delta("fresh")))
        .unwrap();
    assert!(app.drain_engine_events());
    let rows = rendered(&mut app, 60, 16);
    assert!(rows.iter().any(|r| r.contains("fresh")), "{rows:#?}");
}

#[test]
fn enter_steers_the_focused_agent_and_tab_queues() {
    let (mut app, rx, _events, child) = focused_app();
    type_text(&mut app, "also defaults");
    app.handle_key(code(KeyCode::Enter));
    type_text(&mut app, "then routes");
    app.handle_key(code(KeyCode::Tab));
    assert_eq!(
        resolutions(&rx),
        [
            RunRequest::Submit {
                text: "also defaults".into(),
                images: Vec::new(),
                queue: false,
                session: Some(child),
            },
            RunRequest::Submit {
                text: "then routes".into(),
                images: Vec::new(),
                queue: true,
                session: Some(child),
            },
        ]
    );
}

#[test]
fn esc_in_an_agent_view_clears_a_draft_then_goes_back_without_interrupting() {
    let (mut app, rx, _events, _child) = focused_app();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    type_text(&mut app, "draft");
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.input().text(), "");
    assert!(app.focus.is_some(), "the draft goes first");
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.focus, None);
    assert!(rx.try_recv().is_err(), "nothing was interrupted");
}

#[test]
fn ctrl_c_in_an_agent_view_stops_it_while_running_and_goes_back_when_idle() {
    let (mut app, rx, events, child) = focused_app();
    assert_eq!(app.handle_key(ctrl('c')), None);
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::Cancel {
            session: Some(child)
        })
    );
    assert_eq!(app.focus, Some(child));
    send_to(
        &mut app,
        &events,
        child,
        vec![run_ended(kage_core::protocol::RunOutcome::Cancelled)],
    );
    assert_eq!(app.handle_key(ctrl('c')), None);
    assert_eq!(app.focus, None);
    assert!(rx.try_recv().is_err());
    assert_eq!(app.handle_key(ctrl('c')), None, "the main view arms quit");
    assert_eq!(app.footer_hint(), "ctrl+c again to quit");
    assert_eq!(app.handle_key(ctrl('c')), Some(AppExit::Quit));
}

#[test]
fn vim_normal_esc_goes_back_from_an_agent_view() {
    let (mut app, rx, _events, _child) = focused_app();
    app.set_editor_modeless(false);
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.input().mode(), Mode::Normal);
    assert!(app.focus.is_some(), "insert Esc only leaves insert mode");
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.focus, None);
    assert!(rx.try_recv().is_err());
}

#[test]
fn pending_rows_show_in_the_view_of_their_session() {
    let (mut app, _rx, events, child) = focused_app();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    type_text(&mut app, "to explore");
    app.handle_key(code(KeyCode::Enter));
    app.set_focus(None);
    type_text(&mut app, "to main");
    app.handle_key(code(KeyCode::Tab));
    let main_rows = pending_rows(&mut app);
    assert_eq!(main_rows.len(), 1);
    assert!(main_rows[0].starts_with("  > to main"), "{main_rows:#?}");

    app.focus_agent(child);
    let agent_rows = pending_rows(&mut app);
    assert_eq!(agent_rows.len(), 1);
    assert!(
        agent_rows[0].starts_with("  > to explore"),
        "{agent_rows:#?}"
    );
    send_to(&mut app, &events, child, vec![user_message("to explore")]);
    assert!(pending_rows(&mut app).is_empty());
    app.set_focus(None);
    assert_eq!(pending_rows(&mut app), main_rows);
}

#[test]
fn the_breadcrumb_names_the_agent_and_the_main_view_has_no_header() {
    let (mut app, _rx, _events, _child) = focused_app();
    let rows = rendered(&mut app, 100, 20);
    assert!(
        rows[0].starts_with(" kage > explore: explore task  running \u{b7} 0s \u{b7} 0 tools"),
        "{rows:#?}"
    );
    app.set_focus(None);
    let rows = rendered(&mut app, 100, 20);
    assert!(rows.iter().all(|r| !r.contains("kage >")), "{rows:#?}");
}

#[test]
fn the_placeholder_steers_a_running_agent_and_messages_a_finished_one() {
    let (mut app, rx, events, child) = focused_app();
    let rows = rendered(&mut app, 80, 16);
    assert!(rows.iter().any(|r| r == " > Steer explore"), "{rows:#?}");
    assert_eq!(
        app.footer_hint(),
        "enter to steer \u{b7} esc to go back \u{b7} ctrl+c to stop"
    );
    send_to(
        &mut app,
        &events,
        child,
        vec![run_ended(kage_core::protocol::RunOutcome::Completed)],
    );
    let rows = rendered(&mut app, 80, 16);
    assert!(
        rows.iter()
            .any(|r| r == " > Message explore (the reply stays in this agent)"),
        "{rows:#?}"
    );
    assert_eq!(app.footer_hint(), "enter to send \u{b7} esc to go back");
    type_text(&mut app, "one more");
    app.handle_key(code(KeyCode::Enter));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::Submit {
            text: "one more".into(),
            images: Vec::new(),
            queue: false,
            session: Some(child),
        })
    );
    assert!(app.pending.is_empty(), "an idle agent starts a run at once");
}

#[test]
fn clicking_a_pinned_row_focuses_its_agent() {
    let (mut app, _rx, _events, child) = focused_app();
    app.set_focus(None);
    let rows = rendered(&mut app, 80, 24);
    let row = rows
        .iter()
        .position(|r| r.contains("explore  explore task"))
        .unwrap();
    app.mouse_down(u16::try_from(row).unwrap(), 10);
    assert_eq!(app.focus, Some(child));
}

#[test]
fn the_main_session_change_while_focused_returns_to_the_main_view() {
    let (mut app, _rx, events, _child) = focused_app();
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
    assert_eq!(app.focus, None);
    assert!(Arc::ptr_eq(&app.buffer, &app.root_buffer));
    assert!(app.agent_buffers.is_empty());
}

#[test]
fn search_matches_reset_on_a_focus_change() {
    let (mut app, _rx, _events, child) = focused_app();
    app.set_focus(None);
    lock(&app.root_buffer).push_user("needle here");
    app.search_pattern = Some("needle".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches().len(), 1);
    app.focus_agent(child);
    assert_eq!(app.search_pattern, None);
    assert!(app.search_matches().is_empty());
}

#[test]
fn another_agents_approval_still_opens_while_focused() {
    let (mut app, _rx, events, _child) = focused_app();
    let other = spawn_agent(&mut app, &events, "a2", "general");
    send_to(
        &mut app,
        &events,
        other,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            bash_start("c1"),
            permission_request("c1", 8),
        ],
    );
    assert!(app.approval_panel.is_some());
    let rows = rendered(&mut app, 80, 24);
    assert!(
        rows.iter()
            .any(|r| r.contains("general: general task \u{b7} Run this command?")),
        "{rows:#?}"
    );
}

/// An App with a running `general` agent that started a `test` agent,
/// and a finished `explore` agent. Returns the three sessions in that
/// order.
fn agents_app() -> (
    App,
    mpsc::Receiver<RunRequest>,
    mpsc::Sender<kage_core::protocol::Envelope>,
    [kage_core::SessionId; 3],
) {
    let (mut app, rx, events) = app_with_events();
    app.set_editor_modeless(true);
    let general = spawn_agent(&mut app, &events, "a1", "general");
    let explore = spawn_agent(&mut app, &events, "a2", "explore");
    let test = kage_core::SessionId::new();
    let spawned = kage_core::protocol::HostEvent::AgentSpawned {
        parent: general,
        tool_call_id: kage_core::ToolCallId::new("n1"),
        agent: "test".into(),
        description: "run the provider tests".into(),
    };
    send_to(&mut app, &events, test, vec![spawned.into()]);
    for session in [general, test] {
        send_to(
            &mut app,
            &events,
            session,
            vec![kage_core::protocol::HostEvent::RunStarted.into()],
        );
    }
    send_to(
        &mut app,
        &events,
        explore,
        vec![
            kage_core::protocol::HostEvent::RunStarted.into(),
            run_ended(kage_core::protocol::RunOutcome::Completed),
        ],
    );
    (app, rx, events, [general, test, explore])
}

#[test]
fn agents_overlay_rows_list_the_main_session_then_the_tree() {
    use crate::overlay::AgentsRowState;
    let (app, _rx, _events, [general, test, explore]) = agents_app();
    let rows: Vec<_> = app
        .agents_overlay_rows()
        .into_iter()
        .map(|r| (r.session, r.depth, r.name, r.state))
        .collect();
    assert_eq!(
        rows,
        [
            (None, 0, "kage".to_owned(), AgentsRowState::Idle),
            (
                Some(general),
                1,
                "general".to_owned(),
                AgentsRowState::Running
            ),
            (Some(test), 2, "test".to_owned(), AgentsRowState::Running),
            (Some(explore), 1, "explore".to_owned(), AgentsRowState::Done),
        ]
    );
}

#[test]
fn enter_in_the_agents_overlay_opens_an_agent_or_the_main_view() {
    let (mut app, _rx, _events, [_, _, explore]) = agents_app();
    app.handle_key(ctrl('t'));
    assert!(app.agents_overlay.is_some());
    for _ in 0..3 {
        app.handle_key(code(KeyCode::Down));
    }
    app.handle_key(code(KeyCode::Enter));
    assert!(app.agents_overlay.is_none());
    assert_eq!(app.focus, Some(explore), "a finished agent opens too");

    app.handle_key(ctrl('t'));
    let overlay = app.agents_overlay.as_ref().unwrap();
    assert_eq!(overlay.selected(), Some(explore));
    app.handle_key(code(KeyCode::Home));
    app.handle_key(code(KeyCode::Enter));
    assert_eq!(app.focus, None);
    assert!(Arc::ptr_eq(&app.buffer, &app.root_buffer));
}

#[test]
fn x_in_the_agents_overlay_stops_the_selected_agent_and_esc_closes() {
    let (mut app, rx, _events, [general, _, _]) = agents_app();
    app.handle_key(ctrl('t'));
    app.handle_key(key('j'));
    app.handle_key(key('x'));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::Cancel {
            session: Some(general)
        })
    );
    assert!(app.agents_overlay.is_some(), "stopping keeps the list open");
    app.handle_key(code(KeyCode::Esc));
    assert!(app.agents_overlay.is_none());
    assert_eq!(app.focus, None);
    assert!(rx.try_recv().is_err());
}

#[test]
fn slash_agents_opens_the_overlay_and_help_lists_its_key() {
    let (mut app, _rx, _events, _) = agents_app();
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("agents", &registry);
    assert!(matches!(result, CommandResult::Done(None)), "{result:?}");
    assert!(app.agents_overlay.is_some());
    app.agents_overlay = None;

    app.open_help();
    let rows = app.help_overlay.as_ref().unwrap().mapped_rows();
    assert!(rows.contains(&("ctrl+t", "agents")), "{rows:?}");
}

#[test]
fn a_session_without_agents_opens_no_overlay() {
    let (mut app, _rx, events) = app_with_events();
    feed(&mut app, &events, vec![text_delta("hi")]);
    app.handle_key(ctrl('t'));
    assert!(app.agents_overlay.is_none());
}

#[test]
fn the_agents_overlay_fits_80_by_24_and_stays_live() {
    let (mut app, _rx, events, [general, _, _]) = agents_app();
    app.handle_key(ctrl('t'));
    let rows = rendered(&mut app, 80, 24);
    let top = rows.iter().position(|r| r.contains(" agents ")).unwrap();
    assert!(rows[top].contains("2 running \u{b7} 1 done"), "{rows:#?}");
    assert!(rows[top + 1].contains("> kage"), "{rows:#?}");
    assert!(rows[top + 1].contains("idle"), "{rows:#?}");
    assert!(rows[top + 2].contains("general task"), "{rows:#?}");
    assert!(rows[top + 3].contains("test "), "{rows:#?}");
    assert!(rows[top + 4].contains("\u{2022} explore"), "{rows:#?}");
    assert!(rows[top + 4].contains("done"), "{rows:#?}");
    assert!(rows[top + 5].contains("enter to open"), "{rows:#?}");

    send_to(
        &mut app,
        &events,
        general,
        vec![bash_start("c1"), permission_request("c1", 5)],
    );
    let rows = rendered(&mut app, 80, 24);
    assert!(
        rows.iter().any(|r| r.contains("waiting for approval")),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("general: general task \u{b7} Run this command?")),
        "the approval panel shows under the overlay: {rows:#?}"
    );
}

#[test]
fn the_more_row_and_the_working_hint_name_the_agents_key() {
    let check = |mut app: App, events: &mpsc::Sender<kage_core::protocol::Envelope>, key: &str| {
        app.set_editor_modeless(true);
        app.set_session_usage(crate::usage::shared_session_usage());
        lock(app.session_usage.as_ref().unwrap()).working = true;
        for n in 1..=6 {
            let child = spawn_agent(&mut app, events, &format!("a{n}"), "explore");
            send_to(
                &mut app,
                events,
                child,
                vec![kage_core::protocol::HostEvent::RunStarted.into()],
            );
        }
        let rows = rendered(&mut app, 80, 24);
        let more = format!("  +2 more \u{b7} {key} for agents");
        assert!(rows.contains(&more), "{rows:#?}");
        assert_eq!(
            app.footer_hint(),
            format!("{key} for agents \u{b7} esc to interrupt")
        );
    };
    let (app, _rx, events) = app_with_events();
    check(app, &events, "ctrl+t");

    let (mut app, _rx, _) = app_with_config(
        "kage.keymap.del('g', '<C-t>')
         kage.keymap.set('g', '<M-a>', kage.action.OpenAgents)",
        &[],
    );
    let (events, events_rx) = mpsc::channel();
    app.set_engine_events(events_rx);
    check(app, &events, "alt+a");
}

#[test]
fn pinned_agents_follow_their_cards_and_finished_ones_leave_the_queue_hint() {
    let (mut app, _rx, events) = app_with_events();
    app.set_editor_modeless(true);
    lock(app.session_usage.as_ref().unwrap()).working = true;
    let input = |agent: &str| serde_json::json!({ "agent": agent, "description": format!("{agent} task"), "prompt": "go" });
    feed(
        &mut app,
        &events,
        ["c1", "c2"]
            .into_iter()
            .zip(["first", "second"])
            .map(|(id, agent)| {
                kage_core::LoopEvent::ToolCallStart {
                    id: kage_core::ToolCallId::new(id),
                    name: "agent".into(),
                    input_partial: input(agent),
                }
                .into()
            })
            .collect(),
    );
    let mut children = Vec::new();
    for (id, agent) in [("c2", "second"), ("c1", "first")] {
        let child = kage_core::SessionId::new();
        let spawned = kage_core::protocol::HostEvent::AgentSpawned {
            parent: app.active_session.unwrap(),
            tool_call_id: kage_core::ToolCallId::new(id),
            agent: agent.into(),
            description: format!("{agent} task"),
        };
        send_to(&mut app, &events, child, vec![spawned.into()]);
        children.push(child);
    }
    let names: Vec<String> = app.agent_rows().into_iter().map(|r| r.agent).collect();
    assert_eq!(names, ["first", "second"]);
    assert!(app.footer_hint().starts_with("ctrl+t for agents"));
    for child in children {
        send_to(
            &mut app,
            &events,
            child,
            vec![
                kage_core::protocol::HostEvent::RunStarted.into(),
                run_ended(kage_core::protocol::RunOutcome::Completed),
            ],
        );
    }
    assert_eq!(app.footer_hint(), "tab to queue \u{b7} esc to interrupt");
}

#[test]
fn each_view_keeps_its_own_draft() {
    let (mut app, _rx, _events, child) = focused_app();
    app.set_focus(None);
    type_text(&mut app, "main words");
    app.focus_agent(child);
    assert_eq!(app.input.text(), "", "the main draft stays behind");
    type_text(&mut app, "agent words");
    app.set_focus(None);
    assert_eq!(app.input.text(), "main words");
    app.escalate(keys::Trigger::Esc);
    assert_eq!(app.input.text(), "");
    app.focus_agent(child);
    assert_eq!(
        app.input.text(),
        "agent words",
        "esc cleared only the main draft"
    );
}

/// The duration on the finished row of the tool call `id` in `buffer`.
fn result_duration(buffer: &SharedBuffer, id: &str) -> Option<u64> {
    let buf = buffer.lock().unwrap();
    buf.blocks()
        .iter()
        .rev()
        .find_map(|b| match b {
            crate::buffer::Block::ToolResult {
                call_id,
                duration_ms,
                ..
            } if call_id == id => Some(*duration_ms),
            _ => None,
        })
        .expect("tool result present")
}

fn agent_call_end(id: &str) -> kage_core::protocol::Event {
    kage_core::LoopEvent::ToolCallEnd {
        id: kage_core::ToolCallId::new(id),
        output: kage_core::ToolOutput {
            text: "<agent name=\"explore\" session=\"x\" state=\"completed\">\nok\n</agent>".into(),
            ..kage_core::ToolOutput::default()
        },
    }
    .into()
}

#[test]
fn a_queued_agent_card_has_no_timer_and_times_its_run_once_started() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    assert_eq!(tool_phase(&app, "a1"), ToolPhase::Queued);
    let rows = rendered(&mut app, 100, 24);
    let card = rows.iter().find(|r| r.contains("Agent explore")).unwrap();
    assert!(card.trim_end().ends_with("explore task"), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("queued")), "{rows:#?}");

    send_to(
        &mut app,
        &events,
        child,
        vec![kage_core::protocol::HostEvent::RunStarted.into()],
    );
    assert_eq!(tool_phase(&app, "a1"), ToolPhase::Running);
    send_to(
        &mut app,
        &events,
        child,
        vec![run_ended(kage_core::protocol::RunOutcome::Completed)],
    );
    feed(&mut app, &events, vec![agent_call_end("a1")]);
    let took = app.agents.get(child).unwrap().took.unwrap();
    assert_eq!(
        result_duration(&app.buffer, "a1"),
        Some(u64::try_from(took.as_millis()).unwrap()),
        "the card shows the agent's own time"
    );
}

#[test]
fn an_agent_stopped_while_queued_shows_no_time_anywhere() {
    let (mut app, _rx, events) = app_with_events();
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    feed(&mut app, &events, vec![agent_call_end("a1")]);
    send_to(
        &mut app,
        &events,
        child,
        vec![run_ended(kage_core::protocol::RunOutcome::Cancelled)],
    );
    assert_eq!(result_duration(&app.buffer, "a1"), None);
    let row = app
        .agents_overlay_rows()
        .into_iter()
        .find(|r| r.session == Some(child))
        .unwrap();
    assert_eq!(row.elapsed_ms, None);
}

#[test]
fn allowing_a_tool_for_the_session_answers_its_queued_asks() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, rx, events) = app_with_events();
    let read = |id: &str, request: u64| -> kage_core::protocol::Event {
        kage_core::protocol::HostEvent::PermissionRequested {
            request_id: kage_core::protocol::RequestId(request),
            tool_call_id: Some(kage_core::ToolCallId::new(id)),
            tool: "grep".into(),
            subject: "x".into(),
            input: serde_json::json!({ "pattern": "x" }),
        }
        .into()
    };
    feed(
        &mut app,
        &events,
        vec![
            bash_start("c1"),
            bash_start("c2"),
            bash_start("c3"),
            read("c1", 1),
            permission_request("c2", 2),
            read("c3", 3),
        ],
    );
    app.approval_key_at(key('s'), past_guard());
    let resolve = |request: u64, decision| RunRequest::ResolvePermission {
        request_id: kage_core::protocol::RequestId(request),
        decision,
    };
    assert_eq!(
        resolutions(&rx),
        [
            resolve(1, PermissionDecision::AllowSession),
            resolve(3, PermissionDecision::AllowOnce),
        ]
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Approved);
    assert_eq!(tool_phase(&app, "c3"), ToolPhase::Approved);
    assert_eq!(tool_phase(&app, "c2"), ToolPhase::Waiting);
    assert_eq!(
        app.pending_permission.as_ref().map(|a| a.tool.as_str()),
        Some("bash")
    );
    assert!(app.permission_queue.is_empty());
    let rows = rendered(&mut app, 100, 30);
    assert!(rows.iter().any(|r| r.ends_with("approved")), "{rows:#?}");
}

#[test]
fn the_working_row_counts_only_the_agents_directly_under_the_view() {
    let (mut app, _rx, _events, [general, _, _]) = agents_app();
    app.run_started = Instant::now().checked_sub(Duration::from_secs(5));
    let label = |app: &App| app.activity_label(&lock(&app.buffer), 100).unwrap();
    assert!(
        label(&app).starts_with("Waiting for 1 agent ("),
        "{}",
        label(&app)
    );
    app.focus_agent(general);
    assert!(
        label(&app).starts_with("Waiting for 1 agent ("),
        "{}",
        label(&app)
    );
}

#[test]
fn ctrl_t_opens_the_agents_overlay_over_an_approval_that_keeps_its_keys() {
    let (mut app, rx, events, [general, _, _]) = agents_app();
    send_to(
        &mut app,
        &events,
        general,
        vec![bash_start("c1"), permission_request("c1", 5)],
    );
    assert!(app.approval_panel.is_some());
    app.handle_key(ctrl('t'));
    assert!(app.agents_overlay.is_some());
    app.handle_key(code(KeyCode::Down));
    assert_eq!(
        app.agents_overlay.as_ref().unwrap().selected(),
        Some(general)
    );
    std::thread::sleep(crate::overlay::approval::TYPE_AHEAD_GUARD);
    app.handle_key(key('y'));
    assert_eq!(
        resolutions(&rx),
        [RunRequest::ResolvePermission {
            request_id: kage_core::protocol::RequestId(5),
            decision: PermissionDecision::AllowOnce,
        }]
    );
    assert!(app.agents_overlay.is_some(), "the overlay stays open");
}

#[test]
fn the_main_row_names_the_first_prompt_until_a_title_arrives() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![user_message("fix the router\nplease")],
    );
    spawn_agent(&mut app, &events, "a1", "explore");
    assert_eq!(app.agents_overlay_rows()[0].title, "fix the router");
}

/// A resumed main session whose history holds one finished `explore`
/// agent, with a loader that serves its transcript. Returns the agent's
/// session.
fn resumed_app() -> (
    App,
    mpsc::Receiver<RunRequest>,
    mpsc::Sender<kage_core::protocol::Envelope>,
    kage_core::SessionId,
) {
    let (mut app, rx, events) = app_with_events();
    app.set_editor_modeless(true);
    let child = kage_core::SessionId::new();
    let call = kage_core::Message::new(
        kage_core::Role::Assistant,
        vec![kage_core::Content::ToolCall {
            id: kage_core::ToolCallId::new("a1"),
            name: "agent".into(),
            input: serde_json::json!({ "agent": "explore", "description": "map src" }),
        }],
        None,
    );
    let mut result = kage_core::Message::new(
        kage_core::Role::ToolResult,
        vec![kage_core::Content::ToolResultBlock {
            call_id: kage_core::ToolCallId::new("a1"),
            output: format!(
                "<agent name=\"explore\" session=\"{child}\" state=\"completed\">\nall mapped\n</agent>"
            ),
            is_error: false,
        }],
        None,
    );
    result.ts = call.ts + chrono::Duration::milliseconds(8_800);
    app.set_agent_loader(Box::new(move |session| {
        (session == child).then(|| {
            vec![kage_core::Message::new(
                kage_core::Role::User,
                vec![kage_core::Content::Text {
                    text: "map everything under src".into(),
                }],
                None,
            )]
        })
    }));
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::SessionChanged {
                path: std::path::PathBuf::from("/tmp/s.jsonl"),
                title: None,
                messages: vec![call, result],
                compaction: None,
            }
            .into(),
        ],
    );
    (app, rx, events, child)
}

#[test]
fn a_resumed_session_lists_its_agents_and_opens_them_read_only() {
    use crate::overlay::AgentsRowState;
    let (mut app, rx, _events, child) = resumed_app();
    let rows = app.agents_overlay_rows();
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert_eq!(rows[1].session, Some(child));
    assert_eq!(rows[1].state, AgentsRowState::Done);
    assert_eq!(rows[1].title, "map src");
    assert_eq!(rows[1].elapsed_ms, Some(8_800));
    assert_eq!(result_duration(&app.buffer, "a1"), Some(8_800));

    app.handle_key(ctrl('t'));
    app.handle_key(code(KeyCode::Down));
    app.handle_key(code(KeyCode::Enter));
    assert_eq!(app.focus, Some(child));
    assert!(lock(&app.buffer).blocks().iter().any(
        |b| matches!(b, crate::buffer::Block::User { text } if text == "map everything under src")
    ));
    assert_eq!(
        app.agent_placeholder().as_deref(),
        Some("explore cannot be messaged after a resume")
    );
    assert_eq!(app.footer_hint(), "esc to go back");
    assert_eq!(app.breadcrumb().unwrap().tool_calls, 0);
    type_text(&mut app, "more please");
    app.handle_key(code(KeyCode::Enter));
    assert!(resolutions(&rx).is_empty(), "nothing reaches the engine");
    assert_eq!(app.input.text(), "more please");
}
