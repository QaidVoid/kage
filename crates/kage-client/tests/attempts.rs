//! Provisional provider output is isolated from committed conversation items.

use kage_client::{Change, Client, Frame, TranscriptItem};
use serde_json::{Value, json};

fn update(client: &mut Client, payload: Value) -> Vec<Change> {
    let mut params = json!({"sessionId": "s"});
    params["update"] = payload;
    client.handle(Frame::Notification {
        method: "session/update".into(),
        params,
    })
}

fn message(client: &mut Client, id: &str, phase: &str) {
    update(
        client,
        json!({"sessionUpdate": "_kage/message", "messageId": id, "phase": phase}),
    );
}

fn chunk(client: &mut Client, kind: &str, text: &str) {
    update(
        client,
        json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
    );
}

fn retry(client: &mut Client, id: Option<&str>) -> Vec<Change> {
    update(
        client,
        json!({
            "sessionUpdate": "_kage/provider_retry", "messageId": id,
            "attempt": 1, "maxAttempts": 3, "waitSecs": 0, "waitMs": 250,
            "error": "busy"
        }),
    )
}

fn tool(client: &mut Client, id: &str, status: &str) {
    update(
        client,
        json!({"sessionUpdate": "tool_call", "toolCallId": id,
        "title": "read", "kind": "read", "status": status}),
    );
}

fn items(client: &Client) -> &[TranscriptItem] {
    &client.state().session("s").unwrap().items
}

#[test]
fn retry_discards_only_owned_output_and_keeps_intervening_items() {
    let mut client = Client::new();
    chunk(&mut client, "agent_message_chunk", "committed");
    message(&mut client, "failed", "start");
    chunk(&mut client, "agent_message_chunk", "partial");
    chunk(&mut client, "agent_thought_chunk", "thinking");
    update(
        &mut client,
        json!({"sessionUpdate": "agent_message_chunk",
        "content": {"type": "image", "data": "AAAA", "mimeType": "image/png"}}),
    );
    tool(&mut client, "reused", "pending");
    update(
        &mut client,
        json!({"sessionUpdate": "_kage/notice", "tone": "info", "text": "keep notice"}),
    );
    chunk(&mut client, "user_message_chunk", "keep user");
    chunk(&mut client, "agent_message_chunk", "more partial");
    chunk(&mut client, "agent_thought_chunk", "more thinking");
    tool(&mut client, "executed", "completed");
    let changes = retry(&mut client, Some("failed"));
    assert_eq!(
        changes,
        vec![
            Change::AttemptDiscarded {
                id: "s".into(),
                removed: vec![1, 2, 3, 4, 7, 8]
            },
            Change::Transcript { id: "s".into() }
        ]
    );
    assert_eq!(changes[0].session_id(), Some("s"));
    assert_eq!(items(&client).len(), 5);
    assert!(
        matches!(&items(&client)[0], TranscriptItem::Assistant { text } if text == "committed")
    );
    assert!(
        matches!(&items(&client)[1], TranscriptItem::Notice { text, .. } if text == "keep notice")
    );
    assert!(matches!(&items(&client)[2], TranscriptItem::User { .. }));
    assert!(
        matches!(&items(&client)[3], TranscriptItem::ToolCall(call) if call.tool_call_id == "executed")
    );
    assert!(
        matches!(&items(&client)[4], TranscriptItem::Notice { text, .. } if text == "provider error (busy); retrying 1/3 in 250ms")
    );
    message(&mut client, "success", "start");
    tool(&mut client, "reused", "pending");
    update(
        &mut client,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "reused", "status": "completed"}),
    );
    chunk(&mut client, "agent_message_chunk", "done");
    message(&mut client, "success", "end");
    assert_eq!(
        retry(&mut client, Some("success")),
        vec![Change::Transcript { id: "s".into() }]
    );
    assert!(items(&client).iter().any(|item| matches!(item, TranscriptItem::ToolCall(call) if call.tool_call_id == "reused" && call.status == kage_client::wire::ToolCallStatus::Completed)));
}

#[test]
fn stale_boundaries_and_unmatched_retries_do_not_own_current_output() {
    let mut client = Client::new();
    message(&mut client, "old", "start");
    chunk(&mut client, "agent_message_chunk", "old");
    message(&mut client, "current", "start");
    chunk(&mut client, "agent_message_chunk", "current");
    message(&mut client, "old", "end");
    assert_eq!(
        retry(&mut client, Some("old")),
        vec![Change::Transcript { id: "s".into() }]
    );
    assert_eq!(items(&client).len(), 3);
    assert_eq!(
        retry(&mut client, None),
        vec![Change::Transcript { id: "s".into() }]
    );
    assert_eq!(
        retry(&mut client, Some("current"))[0],
        Change::AttemptDiscarded {
            id: "s".into(),
            removed: vec![1]
        }
    );
    assert!(matches!(&items(&client)[0], TranscriptItem::Assistant { text } if text == "old"));
    assert_eq!(items(&client).len(), 4);
}

#[test]
fn retries_without_output_only_append_notices() {
    let mut client = Client::new();
    chunk(&mut client, "agent_message_chunk", "legacy partial");
    assert_eq!(
        retry(&mut client, None),
        vec![Change::Transcript { id: "s".into() }]
    );
    message(&mut client, "empty", "start");
    assert_eq!(
        retry(&mut client, Some("empty")),
        vec![Change::Transcript { id: "s".into() }]
    );
    assert_eq!(items(&client).len(), 3);
}

#[test]
fn transcript_resets_clear_attempt_ownership() {
    for reset in ["drop", "load", "replace"] {
        let mut client = Client::new();
        message(&mut client, "old", "start");
        chunk(&mut client, "agent_message_chunk", "partial");
        match reset {
            "drop" => client.drop_items("s"),
            "load" => {
                client.load_session("s", "", &[], None);
            }
            _ => {
                update(
                    &mut client,
                    json!({"sessionUpdate": "_kage/session_replaced"}),
                );
            }
        }
        chunk(&mut client, "agent_message_chunk", "replayed");
        assert_eq!(
            retry(&mut client, Some("old")),
            vec![Change::Transcript { id: "s".into() }],
            "{reset}"
        );
        assert!(
            matches!(&items(&client)[0], TranscriptItem::Assistant { text } if text == "replayed"),
            "{reset}"
        );
    }
}

#[test]
fn tools_that_started_execution_are_not_discarded() {
    let mut client = Client::new();
    tool(&mut client, "prior", "pending");
    message(&mut client, "m", "start");
    tool(&mut client, "live", "pending");
    update(
        &mut client,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "live", "status": "in_progress"}),
    );
    assert_eq!(
        retry(&mut client, Some("m")),
        vec![Change::Transcript { id: "s".into() }]
    );
    assert_eq!(items(&client).len(), 3);
}

#[test]
fn generic_initialize_leaves_attempt_output_opt_in_to_the_host() {
    let mut client = Client::new();
    client.initialize(kage_client::wire::ClientCapabilities::default(), None);
    let frames = client.take_outgoing();
    let Frame::Request { params, .. } = &frames[0] else {
        panic!("initialize request")
    };
    assert!(params["clientCapabilities"]["_meta"]["kage"]["attemptOutput"].is_null());
}

#[test]
fn retry_notices_use_seconds_for_long_waits() {
    let mut client = Client::new();
    update(
        &mut client,
        json!({
            "sessionUpdate": "_kage/provider_retry", "attempt": 2, "maxAttempts": 3,
            "waitSecs": 5, "waitMs": 5000, "requestedSecs": 300, "error": "limited"
        }),
    );
    assert!(
        matches!(&items(&client)[0], TranscriptItem::Notice { text, .. }
        if text == "provider error (limited); retrying 2/3 in 5s")
    );
}

#[test]
fn turn_end_commits_partial_output_for_clients_without_a_prompt_request() {
    for reason in ["no_tool_calls", "tool_calls"] {
        let mut client = Client::new();
        message(&mut client, "terminal", "start");
        chunk(&mut client, "agent_message_chunk", "terminal output");
        let ended = update(
            &mut client,
            json!({"sessionUpdate": "_kage/turn", "phase": "end", "reason": reason}),
        );
        assert_eq!(
            ended,
            vec![
                Change::Session { id: "s".into() },
                Change::Transcript { id: "s".into() }
            ],
        );
        assert_eq!(
            retry(&mut client, Some("terminal")),
            vec![Change::Transcript { id: "s".into() }]
        );
        message(&mut client, "next", "start");
        chunk(&mut client, "agent_message_chunk", "next partial");
        assert_eq!(
            retry(&mut client, Some("terminal")),
            vec![Change::Transcript { id: "s".into() }]
        );
        assert_eq!(
            retry(&mut client, Some("next")),
            vec![
                Change::AttemptDiscarded {
                    id: "s".into(),
                    removed: vec![3]
                },
                Change::Transcript { id: "s".into() }
            ]
        );
        assert!(
            matches!(&items(&client)[0], TranscriptItem::Assistant { text } if text == "terminal output")
        );
        assert!(matches!(&items(&client)[1], TranscriptItem::TurnEnd { .. }));
        assert_eq!(items(&client).len(), 5);
    }
}
