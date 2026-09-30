//! Golden transcript replays and the steering gate.
//!
//! Each fixture is one connection's incoming frames in order, taken
//! from the flows `kage rpc` produces: a fix with tool calls, a
//! permission round trip, a cancel while asked, and a run with
//! subagents. The tests drive a [`Client`] the way a host would,
//! sending the commands and feeding every fixture line, then assert
//! the state the replay left behind.

use std::fs;
use std::path::{Path, PathBuf};

use kage_acp_wire::{
    ClientCapabilities, ContentBlock, FsEntry, FsKind, FsListResult, FsOp, FsResult,
    RequestPermissionRequest, SessionNotification, StopReason, ToolCallStatus,
};
use kage_client::{Change, Client, Frame, PermissionDecision, PromptOutcome, SteerError, TranscriptItem};

const PARENT: &str = "01KA5C0D1NG000000000000000";
const CHILD: &str = "01KA5C0D1NG000000000000001";

/// The fixtures directory of this crate.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// The parsed frames of one fixture, in file order.
fn fixture(name: &str) -> Vec<Frame> {
    let text = fs::read_to_string(fixtures().join(name)).unwrap();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            Frame::parse(&value).unwrap_or_else(|| panic!("unparseable line in {name}: {line}"))
        })
        .collect()
}

/// Feeds every frame and collects the changes in order.
fn drive(client: &mut Client, frames: &[Frame]) -> Vec<Change> {
    frames
        .iter()
        .flat_map(|frame| client.handle(frame.clone()))
        .collect()
}

fn text(message: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(message)]
}

/// An `initialize` answer the gate tests build to measure: with or
/// without the steering capability.
fn init_result(steer: bool) -> Frame {
    Frame::Success {
        id: 1,
        result: serde_json::json!({
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": true, "steer": steer},
        }),
    }
}

fn new_result(id: u64, session: &str) -> Frame {
    Frame::Success {
        id,
        result: serde_json::json!({"sessionId": session}),
    }
}

fn stop(id: u64, reason: &str) -> Frame {
    Frame::Success {
        id,
        result: serde_json::json!({"stopReason": reason}),
    }
}

/// A client through the handshake with one open session "s1", as the
/// gate tests start.
fn connected(steer: bool) -> Client {
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    let _ = client.take_outgoing();
    client.handle(init_result(steer));
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    client.handle(new_result(2, "s1"));
    client
}

#[test]
fn every_fixture_line_parses_against_the_wire_types() {
    for name in [
        "fix-tools.jsonl",
        "approval.jsonl",
        "cancel.jsonl",
        "subagents.jsonl",
    ] {
        for frame in fixture(name) {
            match frame {
                Frame::Notification { method, params } => match method.as_str() {
                    "session/update" => {
                        let note: SessionNotification = serde_json::from_value(params).unwrap();
                        assert!(!note.session_id.is_empty(), "{name}");
                    }
                    "$/cancel_request" => {
                        assert!(
                            params
                                .get("requestId")
                                .and_then(serde_json::Value::as_u64)
                                .is_some(),
                            "{name}"
                        );
                    }
                    other => panic!("{name} carries an unexpected notification: {other}"),
                },
                Frame::Request { method, params, .. } => {
                    assert_eq!(method, "session/request_permission", "{name}");
                    let ask: RequestPermissionRequest = serde_json::from_value(params).unwrap();
                    assert_eq!(ask.options.len(), 3, "{name}");
                }
                Frame::Success { result, .. } => {
                    let object = result.as_object().unwrap();
                    assert!(
                        object.contains_key("sessionId")
                            || object.contains_key("stopReason")
                            || object.contains_key("protocolVersion"),
                        "{name}"
                    );
                }
                Frame::Failure { .. } => panic!("{name} carries no failures"),
            }
        }
    }
}

#[test]
fn the_fix_fixture_replays_to_the_asserted_state() {
    let frames = fixture("fix-tools.jsonl");
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    let mut changes = drive(&mut client, &frames[..2]);

    let outcome = client.prompt(PARENT, text("fix the null check"));
    assert_eq!(outcome, PromptOutcome::Sent { request_id: 3 });
    let _ = client.take_outgoing();
    assert!(client.state().session(PARENT).unwrap().running);
    changes.extend(drive(&mut client, &frames[2..]));

    let transcript = changes
        .iter()
        .filter(|change| **change == Change::Transcript { id: PARENT.into() })
        .count();

    let state = client.state();
    assert_eq!(state.protocol_version, Some(1));
    let agent = state.agent.as_ref().unwrap();
    assert_eq!(agent.name, "kage");
    assert!(state.steer_available());

    let session = state.session(PARENT).unwrap();
    assert!(session.opened);
    assert_eq!(session.cwd.as_deref(), Some("/w"));
    assert_eq!(
        session.title.as_deref(),
        Some("Fix the null dereference in main")
    );
    assert_eq!(session.last_stop, Some(StopReason::EndTurn));
    assert!(!session.running);
    assert!(!session.in_turn);
    assert_eq!(session.config_options.len(), 3);
    assert_eq!(
        session.config_options[0].current_value,
        "zai-coding-plan/glm-4.7"
    );
    assert_eq!((session.usage.used, session.usage.size), (12480, 200_000));

    let items = &session.items;
    assert_eq!(items.len(), 10, "{items:#?}");
    let read = match &items[0] {
        TranscriptItem::ToolCall(call) => call,
        other => panic!("expected the read, got {other:?}"),
    };
    assert_eq!(read.title, "read");
    assert_eq!(read.status, ToolCallStatus::Completed);
    assert_eq!(read.input, Some(serde_json::json!({"path": "src/main.rs"})));
    assert!(read.text().starts_with("fn main()"));
    assert!(matches!(
        &items[1],
        TranscriptItem::TurnEnd {
            reason: Some(kage_acp_wire::TurnReason::ToolCalls)
        }
    ));
    assert_eq!(
        match &items[2] {
            TranscriptItem::Thinking { text } => text.as_str(),
            other => panic!("expected thinking, got {other:?}"),
        },
        "The read confirms main dereferences an Option, so the fix is a match."
    );
    assert_eq!(
        match &items[3] {
            TranscriptItem::Assistant { text } => text.as_str(),
            other => panic!("expected an assistant message, got {other:?}"),
        },
        "Adding a match on the Option before printing."
    );
    let edit = match &items[4] {
        TranscriptItem::ToolCall(call) => call,
        other => panic!("expected the edit, got {other:?}"),
    };
    assert_eq!(edit.status, ToolCallStatus::Completed);
    assert_eq!(edit.text(), "patched src/main.rs");
    assert!(matches!(&items[5], TranscriptItem::ToolCall(_)));
    let plan = match &items[6] {
        TranscriptItem::Plan { entries } => entries,
        other => panic!("expected the plan, got {other:?}"),
    };
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[1]["status"], "in_progress");
    assert_eq!(session.plan.as_ref().unwrap().len(), 3);
    assert!(matches!(&items[7], TranscriptItem::TurnEnd { .. }));
    assert_eq!(
        match &items[8] {
            TranscriptItem::Assistant { text } => text.as_str(),
            other => panic!("expected an assistant message, got {other:?}"),
        },
        "The null check is in; cargo test passes."
    );
    assert!(matches!(
        &items[9],
        TranscriptItem::TurnEnd {
            reason: Some(kage_acp_wire::TurnReason::NoToolCalls)
        }
    ));
    assert!(client.take_outgoing().is_empty());
    assert!(
        transcript >= 6,
        "every transcript move was reported: {transcript}"
    );
}

#[test]
fn the_approval_fixture_round_trips_a_permission_ask() {
    let frames = fixture("approval.jsonl");
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    drive(&mut client, &frames[..2]);
    let outcome = client.prompt(PARENT, text("run the tests"));
    assert_eq!(outcome, PromptOutcome::Sent { request_id: 3 });

    let mut changes = drive(&mut client, &frames[2..5]);
    assert!(
        changes.contains(&Change::Permission { id: PARENT.into() }),
        "{changes:?}"
    );
    let session = client.state().session(PARENT).unwrap();
    assert_eq!(session.permissions.len(), 1);
    let ask = &session.permissions[0];
    assert_eq!(ask.request_id, 101);
    assert_eq!(ask.tool_call.title.as_deref(), Some("shell"));
    assert_eq!(
        ask.option_of(kage_acp_wire::PermissionOptionKind::AllowOnce),
        Some("allow")
    );

    assert!(client.reply_permission(PARENT, 101, &PermissionDecision::Allow));
    let outgoing = client.take_outgoing();
    assert_eq!(
        outgoing.last(),
        Some(&Frame::Success {
            id: 101,
            result: serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
        })
    );
    assert!(
        client
            .state()
            .session(PARENT)
            .unwrap()
            .permissions
            .is_empty()
    );

    changes.extend(drive(&mut client, &frames[5..]));
    let session = client.state().session(PARENT).unwrap();
    assert_eq!(session.title.as_deref(), Some("cargo test green"));
    assert_eq!(session.last_stop, Some(StopReason::EndTurn));
    let call = match &session.items[0] {
        TranscriptItem::ToolCall(call) => call,
        other => panic!("expected the shell call, got {other:?}"),
    };
    assert_eq!(call.status, ToolCallStatus::Completed);
    assert!(
        call.text()
            .ends_with("test result: ok. 12 passed; 0 failed")
    );
    assert_eq!(call.raw_output, Some(serde_json::json!({"exit_code": 0})));
    assert!(matches!(&session.items[1], TranscriptItem::TurnEnd { .. }));
}

#[test]
fn the_cancel_fixture_withdraws_the_ask_and_ends_cancelled() {
    let frames = fixture("cancel.jsonl");
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    drive(&mut client, &frames[..2]);
    let outcome = client.prompt(PARENT, text("build it"));
    assert_eq!(outcome, PromptOutcome::Sent { request_id: 3 });

    let changes = drive(&mut client, &frames[2..6]);
    assert!(changes.contains(&Change::Permission { id: PARENT.into() }));
    let session = client.state().session(PARENT).unwrap();
    assert!(
        session.permissions.is_empty(),
        "the withdraw emptied the ask queue"
    );
    assert_eq!(
        match &session.items[0] {
            TranscriptItem::ToolCall(call) => call.status,
            other => panic!("expected the shell call, got {other:?}"),
        },
        ToolCallStatus::Pending,
        "the call it was asking about stays honest"
    );
    assert!(session.running, "the response has not landed yet");

    client.handle(frames[6].clone());
    let session = client.state().session(PARENT).unwrap();
    assert_eq!(session.last_stop, Some(StopReason::Cancelled));
    assert!(!session.running);
    assert!(
        session.in_turn,
        "the turn never closed on the wire, so it stays open"
    );
}

#[test]
fn the_subagent_fixture_builds_the_agent_tree_and_the_child_transcript() {
    let frames = fixture("subagents.jsonl");
    let mut client = Client::new();
    let caps = ClientCapabilities {
        subagents: Some(serde_json::json!({})),
        ..ClientCapabilities::default()
    };
    client.initialize(caps, None);
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    let mut changes = drive(&mut client, &frames[..2]);

    let outcome = client.prompt(PARENT, text("list the files"));
    assert_eq!(outcome, PromptOutcome::Sent { request_id: 3 });
    let _ = client.take_outgoing();
    changes.extend(drive(&mut client, &frames[2..8]));

    let parent = client.state().session(PARENT).unwrap();
    let child_agent = parent.agents.get(CHILD).unwrap();
    assert_eq!(child_agent.name.as_deref(), Some("general"));
    assert_eq!(child_agent.task.as_deref(), Some("list files"));
    assert!(child_agent.capabilities.unwrap().cancel);
    assert_eq!(child_agent.state, None, "still running");

    let child = client.state().session(CHILD).unwrap();
    assert!(!child.opened, "the child is heard, not opened");
    assert_eq!(child.permissions.len(), 1, "the child's tool call asks");
    assert_eq!(child.permissions[0].request_id, 101);

    assert!(client.reply_permission(CHILD, 101, &PermissionDecision::Allow));
    assert!(
        client
            .take_outgoing()
            .last()
            .is_some_and(|frame| { matches!(frame, Frame::Success { id: 101, .. }) })
    );
    changes.extend(drive(&mut client, &frames[8..]));

    let parent = client.state().session(PARENT).unwrap();
    let child_agent = parent.agents.get(CHILD).unwrap();
    assert_eq!(
        child_agent.state,
        Some(kage_acp_wire::SubagentState::Completed)
    );
    assert_eq!(parent.title.as_deref(), Some("List the project files"));
    assert_eq!(parent.last_stop, Some(StopReason::EndTurn));
    let call = match &parent.items[0] {
        TranscriptItem::ToolCall(call) => call,
        other => panic!("expected the agent call, got {other:?}"),
    };
    assert_eq!(call.tool_call_id, "call-agent");
    assert_eq!(
        call.status,
        ToolCallStatus::Pending,
        "the parent call stays honest"
    );
    assert_eq!(parent.items.len(), 4, "{:#?}", parent.items);

    let child = client.state().session(CHILD).unwrap();
    assert!(!child.running);
    assert!(child.permissions.is_empty());
    assert_eq!(child.items.len(), 4, "{:#?}", child.items);
    let child_call = match &child.items[0] {
        TranscriptItem::ToolCall(call) => call,
        other => panic!("expected the ls call, got {other:?}"),
    };
    assert_eq!(child_call.title, "ls");
    assert_eq!(child_call.status, ToolCallStatus::Completed);
    assert_eq!(child_call.text(), "main.rs\nlib.rs");
    assert_eq!(
        match &child.items[2] {
            TranscriptItem::Assistant { text } => text.as_str(),
            other => panic!("expected the child reply, got {other:?}"),
        },
        "Listed main.rs and lib.rs."
    );
    assert!(
        changes
            .iter()
            .any(|change| { *change == Change::Agents { id: PARENT.into() } })
    );
}

#[test]
fn a_prompt_queues_while_a_run_is_in_flight_even_with_the_capability() {
    let mut client = connected(true);
    assert_eq!(
        client.prompt("s1", text("go")),
        PromptOutcome::Sent { request_id: 3 }
    );
    let _ = client.take_outgoing();
    assert_eq!(
        client.prompt("s1", text("later")),
        PromptOutcome::Queued,
        "plain prompts queue while a run is in flight"
    );
    assert!(client.take_outgoing().is_empty(), "a queued prompt sends nothing");
    let session = client.state().session("s1").unwrap();
    assert_eq!(session.queue.len(), 1);
    assert_eq!(session.queue[0].prompt, text("later"));
}

#[test]
fn a_prompt_queues_without_the_steer_capability() {
    let mut client = connected(false);
    assert!(!client.state().steer_available());

    assert_eq!(
        client.prompt("s1", text("go")),
        PromptOutcome::Sent { request_id: 3 }
    );
    let _ = client.take_outgoing();
    assert_eq!(
        client.prompt("s1", text("later")),
        PromptOutcome::Queued,
        "a run is in flight and the agent cannot steer"
    );
    assert!(
        client.take_outgoing().is_empty(),
        "a queued prompt sends nothing"
    );
    let session = client.state().session("s1").unwrap();
    assert_eq!(session.queue.len(), 1);

    client.handle(stop(3, "end_turn"));
    let session = client.state().session("s1").unwrap();
    assert!(session.running, "the queued prompt became the run");
    assert!(
        session.queue.is_empty(),
        "the run ended, so the queue flushed"
    );

    let outgoing = client.take_outgoing();
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    match &outgoing[0] {
        Frame::Request {
            id: 4,
            method,
            params,
        } => {
            assert_eq!(method, "session/prompt");
            assert!(params.get("delivery").is_none(), "queued prompts go plain");
            assert_eq!(params["prompt"][0]["text"], "later");
        }
        other => panic!("expected the flushed prompt, got {other:?}"),
    }
    assert!(client.state().session("s1").unwrap().running);
}

#[test]
fn steer_joins_a_run_only_when_advertised_and_in_flight() {
    let mut client = connected(true);
    assert_eq!(
        client.prompt("s1", text("go")),
        PromptOutcome::Sent { request_id: 3 }
    );
    let _ = client.take_outgoing();

    assert_eq!(
        client.steer("s1", text("hurry")),
        Ok(4),
        "the advertised capability steers the run in flight"
    );
    let session = client.state().session("s1").unwrap();
    assert!(session.queue.is_empty(), "steered prompts never queue");
    let outgoing = client.take_outgoing();
    match &outgoing[0] {
        Frame::Request { id: 4, params, .. } => {
            assert_eq!(params["delivery"], "steer");
            assert_eq!(params["prompt"][0]["text"], "hurry");
        }
        other => panic!("expected the steered prompt, got {other:?}"),
    }

    client.handle(stop(4, "end_turn"));
    assert!(
        client.state().session("s1").unwrap().running,
        "the steered prompt does not end the run it joined"
    );
    client.handle(stop(3, "end_turn"));
    assert!(!client.state().session("s1").unwrap().running);

    assert_eq!(
        client.steer("s1", text("late")),
        Err(SteerError::NotRunning),
        "an idle session has no run to steer"
    );
    assert!(client.take_outgoing().is_empty());
}

#[test]
fn steer_refuses_when_the_capability_was_not_advertised() {
    let mut client = connected(false);
    assert_eq!(
        client.prompt("s1", text("go")),
        PromptOutcome::Sent { request_id: 3 }
    );
    let _ = client.take_outgoing();
    assert_eq!(
        client.steer("s1", text("hurry")),
        Err(SteerError::NotAdvertised)
    );
    assert!(client.take_outgoing().is_empty(), "a refused steer sends nothing");
    assert!(client.state().session("s1").unwrap().queue.is_empty());
}

#[test]
fn queued_prompts_withdraw_and_promote_to_steer() {
    let mut client = connected(true);
    assert_eq!(
        client.prompt("s1", text("go")),
        PromptOutcome::Sent { request_id: 3 }
    );
    let _ = client.take_outgoing();
    assert_eq!(client.prompt("s1", text("first")), PromptOutcome::Queued);
    assert_eq!(client.prompt("s1", text("second")), PromptOutcome::Queued);
    assert_eq!(client.state().session("s1").unwrap().queue.len(), 2);

    assert!(client.withdraw_queued("s1", 0), "the held prompt is dropped");
    assert_eq!(
        client.state().session("s1").unwrap().queue[0].prompt,
        text("second")
    );
    assert!(!client.withdraw_queued("s1", 5), "nothing at the index");
    assert_eq!(client.state().session("s1").unwrap().queue.len(), 1);
    assert!(client.take_outgoing().is_empty(), "withdrawal sends nothing");

    assert_eq!(client.steer_queued("s1", 0), Ok(4));
    let outgoing = client.take_outgoing();
    match &outgoing[0] {
        Frame::Request { id: 4, params, .. } => {
            assert_eq!(params["delivery"], "steer");
            assert_eq!(params["prompt"][0]["text"], "second");
        }
        other => panic!("expected the promoted prompt, got {other:?}"),
    }
    assert!(client.state().session("s1").unwrap().queue.is_empty());
}

#[test]
fn drafts_round_trip_per_session() {
    let mut client = connected(false);
    assert_eq!(client.state().draft("s1"), None);
    assert!(client.set_draft("s1", "fix the loop"));
    assert_eq!(client.state().draft("s1"), Some("fix the loop"));
    assert!(
        !client.set_draft("ghost", "lost"),
        "a session the state has not heard of holds no draft"
    );
    assert!(client.set_draft("s1", ""));
    assert_eq!(client.state().draft("s1"), Some(""));
}

#[test]
fn one_shot_answers_arrive_as_changes() {
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    client.handle(new_result(2, "s1"));

    client.config_get();
    let changes = drive(
        &mut client,
        &[Frame::Success {
            id: 3,
            result: serde_json::json!({"providers": {}, "permissions": {}}),
        }],
    );
    assert_eq!(
        changes,
        vec![Change::Config {
            config: serde_json::json!({"providers": {}, "permissions": {}}),
        }]
    );

    client.fs("s1", FsOp::List, "");
    let changes = drive(
        &mut client,
        &[Frame::Success {
            id: 4,
            result: serde_json::to_value(FsResult::List(FsListResult {
                entries: vec![FsEntry {
                    path: "src".into(),
                    kind: FsKind::Directory,
                    size: 0,
                }],
                truncated: false,
            }))
            .unwrap(),
        }],
    );
    assert_eq!(
        changes,
        vec![Change::Fs {
            session_id: "s1".into(),
            result: FsResult::List(FsListResult {
                entries: vec![FsEntry {
                    path: "src".into(),
                    kind: FsKind::Directory,
                    size: 0,
                }],
                truncated: false,
            }),
        }]
    );
}

#[test]
fn list_pages_merge_and_config_and_close_land_on_the_session() {
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    client.new_session("/w", &[]);
    let _ = client.take_outgoing();
    client.handle(new_result(2, "s1"));

    client.list_sessions(Some("/w"), None);
    let changes = drive(
        &mut client,
        &[Frame::Success {
            id: 3,
            result: serde_json::json!({"sessions": [
                {"sessionId": "rec-1", "cwd": "/w", "title": "Fix the build"}
            ]}),
        }],
    );
    assert_eq!(changes, vec![Change::Directory]);
    assert_eq!(client.state().directory.len(), 1);
    assert_eq!(
        client.state().directory[0].title.as_deref(),
        Some("Fix the build")
    );

    client.list_sessions(Some("/w"), Some("50"));
    drive(
        &mut client,
        &[Frame::Success {
            id: 4,
            result: serde_json::json!({"sessions": [
                {"sessionId": "rec-1", "cwd": "/w", "title": "Renamed"},
                {"sessionId": "rec-2", "cwd": "/w"}
            ]}),
        }],
    );
    assert_eq!(client.state().directory.len(), 2, "pages merge by id");
    assert_eq!(
        client.state().directory[0].title.as_deref(),
        Some("Renamed")
    );

    client.set_config_option("s1", "mode", "ask");
    let changes = drive(
        &mut client,
        &[Frame::Success {
            id: 5,
            result: serde_json::json!({"configOptions": [{
                "id": "mode", "name": "Mode", "category": "mode",
                "type": "select", "currentValue": "ask", "options": []
            }]}),
        }],
    );
    assert_eq!(changes, vec![Change::Session { id: "s1".into() }]);
    assert_eq!(
        client.state().session("s1").unwrap().config_options[0].current_value,
        "ask"
    );

    client.close_session("s1");
    let changes = drive(
        &mut client,
        &[Frame::Success {
            id: 6,
            result: serde_json::json!({}),
        }],
    );
    assert_eq!(changes, vec![Change::Session { id: "s1".into() }]);
    assert!(
        client.state().session("s1").is_none(),
        "the release removed it"
    );
}

#[test]
fn unheard_of_answers_and_frames_change_nothing() {
    let mut client = Client::new();
    assert_eq!(
        drive(
            &mut client,
            &[Frame::Success {
                id: 99,
                result: serde_json::json!({})
            }]
        ),
        Vec::<Change>::new()
    );
    assert_eq!(
        drive(
            &mut client,
            &[Frame::Notification {
                method: "other/notification".into(),
                params: serde_json::json!({}),
            }],
        ),
        Vec::<Change>::new()
    );
    assert_eq!(
        drive(
            &mut client,
            &[Frame::Notification {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": "s1", "update": {"sessionUpdate": "from the future"}}),
            }],
        ),
        Vec::<Change>::new(),
        "an unknown update kind changes nothing"
    );
}

#[test]
fn server_requests_outside_the_permission_ask_are_refused() {
    let mut client = Client::new();
    client.handle(Frame::Request {
        id: 5,
        method: "fs/read_text_file".into(),
        params: serde_json::json!({"sessionId": "s1", "path": "a"}),
    });
    let outgoing = client.take_outgoing();
    assert_eq!(
        outgoing,
        vec![Frame::Failure {
            id: 5,
            error: kage_client::RpcError::method_not_found("fs/read_text_file"),
        }]
    );

    client.handle(Frame::Request {
        id: 6,
        method: "session/request_permission".into(),
        params: serde_json::json!({"toolCall": {}}),
    });
    let outgoing = client.take_outgoing();
    assert_eq!(
        outgoing.len(),
        1,
        "a malformed ask is refused, not swallowed"
    );
    assert!(matches!(
        &outgoing[0],
        Frame::Failure { id: 6, error } if error.code == -32602
    ));
}

#[test]
fn a_failed_prompt_answer_releases_the_run_and_flushes_the_queue() {
    let mut client = connected(false);
    assert_eq!(
        client.prompt("s1", text("go")),
        PromptOutcome::Sent { request_id: 3 }
    );
    let _ = client.take_outgoing();
    assert_eq!(client.prompt("s1", text("later")), PromptOutcome::Queued);
    client.handle(Frame::Failure {
        id: 3,
        error: kage_client::RpcError::new(-32603, "session is busy"),
    });
    assert!(
        client.state().session("s1").unwrap().running,
        "the failed run released, so the queued prompt took over"
    );
    let outgoing = client.take_outgoing();
    assert!(matches!(&outgoing[0], Frame::Request { id: 4, .. }));
}
