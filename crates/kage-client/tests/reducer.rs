//! Reducer behavior on handcrafted frames: what the client records
//! itself, what extension facts it keeps, and what survives a reload.

use kage_acp_wire::{ClientCapabilities, ContentBlock, SubagentState};
use kage_client::{Client, Frame, PromptOutcome, TranscriptItem};
use serde_json::{Value, json};

/// A client that initialized against a steering kage agent running in
/// `/srv/proj`, and opened session `s1` with an empty cwd.
fn opened() -> Client {
    let mut client = Client::new();
    client.initialize(ClientCapabilities::default(), None);
    client.handle(Frame::Success {
        id: 1,
        result: json!({
            "protocolVersion": 1,
            "agentCapabilities": {"steer": true},
            "agentInfo": {"name": "kage", "version": "0.1.0"},
            "_meta": {"kage": {"cwd": "/srv/proj"}},
        }),
    });
    client.new_session("", &[]);
    client.handle(Frame::Success {
        id: 2,
        result: json!({"sessionId": "s1"}),
    });
    let _ = client.take_outgoing();
    client
}

fn update(session: &str, update: &Value) -> Frame {
    Frame::Notification {
        method: "session/update".into(),
        params: json!({"sessionId": session, "update": update}),
    }
}

fn texts(content: &[ContentBlock]) -> Vec<&str> {
    content.iter().filter_map(ContentBlock::as_text).collect()
}

#[test]
fn the_agent_directory_names_a_session_opened_without_one() {
    let client = opened();
    assert_eq!(client.state().agent_cwd.as_deref(), Some("/srv/proj"));
    let session = client.state().session("s1").unwrap();
    assert_eq!(session.cwd.as_deref(), Some("/srv/proj"));
}

#[test]
fn sent_and_steered_prompts_are_recorded_as_user_rows() {
    let mut client = opened();
    let sent = client.prompt("s1", vec![ContentBlock::text("fix it")]);
    assert!(matches!(sent, PromptOutcome::Sent { .. }));
    client
        .steer("s1", vec![ContentBlock::text("hurry")])
        .unwrap();
    let queued = client.prompt("s1", vec![ContentBlock::text("later")]);
    assert_eq!(queued, PromptOutcome::Queued);

    let items = &client.state().session("s1").unwrap().items;
    assert_eq!(
        items.len(),
        2,
        "a queued prompt waits for its send: {items:#?}"
    );
    match &items[0] {
        TranscriptItem::User { content, steered } => {
            assert_eq!(texts(content), ["fix it"]);
            assert!(!steered);
        }
        other => panic!("expected the prompt, got {other:?}"),
    }
    assert!(matches!(
        &items[1],
        TranscriptItem::User { steered: true, .. }
    ));
}

#[test]
fn an_echoed_prompt_keeps_its_images_and_each_text_starts_a_prompt() {
    let mut client = opened();
    let chunk = |content: Value| {
        update(
            "s1",
            &json!({"sessionUpdate": "user_message_chunk", "content": content}),
        )
    };
    client.handle(chunk(json!({"type": "text", "text": "look"})));
    client.handle(chunk(
        json!({"type": "image", "data": "AAAA", "mimeType": "image/png"}),
    ));
    client.handle(chunk(
        json!({"type": "text", "text": "next, with no reply between"}),
    ));
    let items = &client.state().session("s1").unwrap().items;
    assert_eq!(items.len(), 2, "{items:#?}");
    match &items[0] {
        TranscriptItem::User { content, .. } => {
            assert_eq!(content.len(), 2, "the image rides its prompt");
            assert_eq!(texts(content), ["look"]);
        }
        other => panic!("expected the echo, got {other:?}"),
    }
}

#[test]
fn a_plan_review_keeps_its_document() {
    let mut client = opened();
    client.handle(Frame::Request {
        id: 70,
        method: "session/request_permission".into(),
        params: json!({
            "sessionId": "s1",
            "toolCall": {"toolCallId": "call_plan", "title": "exit_plan"},
            "options": [{"optionId": "approve", "name": "Approve", "kind": "allow_once"}],
            "_meta": {"kage": {"planReview": {"plan": "# Plan\n\n1. Do it"}}},
        }),
    });
    let ask = &client.state().session("s1").unwrap().permissions[0];
    assert_eq!(ask.plan.as_deref(), Some("# Plan\n\n1. Do it"));
}

#[test]
fn swarm_calls_and_members_keep_their_batch_facts() {
    let mut client = opened();
    client.handle(update("s1", &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_swarm",
            "title": "swarm",
            "kind": "other",
            "status": "pending",
            "_meta": {"kage": {"swarm": {"members": ["a.rs", "b.rs"], "template": "Review {{item}}"}}},
        }),
    ));
    client.handle(update(
        "s1",
        &json!({
            "sessionUpdate": "subagent_update",
            "subagentSessionId": "c1",
            "name": "explore",
            "state": "paused",
            "reason": "rate limited, retrying in 3s",
            "swarm": {"id": "b1", "item": "a.rs", "index": 0, "total": 2},
        }),
    ));
    let state = client.state();
    let session = state.session("s1").unwrap();
    let TranscriptItem::ToolCall(call) = &session.items[0] else {
        panic!("expected the swarm call");
    };
    let swarm = call.swarm.as_ref().unwrap();
    assert_eq!(swarm.members, ["a.rs", "b.rs"]);
    assert_eq!(swarm.template.as_deref(), Some("Review {{item}}"));
    let member = &session.agents["c1"];
    assert_eq!(member.swarm.as_ref().unwrap().index, 0);
    assert_eq!(
        member.reason.as_deref(),
        Some("rate limited, retrying in 3s")
    );
    assert_eq!(
        state.session("c1").unwrap().parent.as_deref(),
        Some("s1"),
        "a child knows its parent"
    );

    client.handle(update("s1", &json!({"sessionUpdate": "subagent_update", "subagentSessionId": "c1", "state": "completed"}),
    ));
    let member = &client.state().session("s1").unwrap().agents["c1"];
    assert_eq!(member.state, Some(SubagentState::Completed));
    assert_eq!(member.reason, None, "the reason went with the pause");
}

#[test]
fn the_directory_follows_every_page() {
    let mut client = opened();
    let first = client.list_sessions(None, None);
    let _ = client.take_outgoing();
    client.handle(Frame::Success {
        id: first,
        result: json!({
            "sessions": [{"sessionId": "r1", "cwd": "/srv/proj"}],
            "nextCursor": "page2",
        }),
    });
    let outgoing = client.take_outgoing();
    let Frame::Request { id, method, params } = &outgoing[0] else {
        panic!("expected the next page request");
    };
    assert_eq!(method, "session/list");
    assert_eq!(params["cursor"], "page2");
    client.handle(Frame::Success {
        id: *id,
        result: json!({"sessions": [{"sessionId": "r2", "cwd": "/srv/proj"}]}),
    });
    assert!(client.take_outgoing().is_empty(), "the last page ends it");
    let ids: Vec<&str> = client
        .state()
        .directory
        .iter()
        .map(|info| info.session_id.as_str())
        .collect();
    assert_eq!(ids, ["r1", "r2"]);
}

#[test]
fn a_reload_replays_history_but_keeps_what_only_the_client_holds() {
    let mut client = opened();
    client.prompt("s1", vec![ContentBlock::text("first")]);
    client.prompt("s1", vec![ContentBlock::text("held")]);
    client.set_draft("s1", "typing");
    client.load_session("s1", "/srv/proj", &[]);
    let session = client.state().session("s1").unwrap();
    assert!(
        session.items.is_empty(),
        "the replay brings the history back"
    );
    assert!(!session.running, "the old run answers on a gone connection");
    assert_eq!(session.queue.len(), 1, "the held prompt stays");
    assert_eq!(session.draft.as_deref(), Some("typing"));
    assert_eq!(session.cwd.as_deref(), Some("/srv/proj"));
}

#[test]
fn an_option_change_shows_at_once_and_a_refusal_restores_it() {
    let mut client = opened();
    client.handle(update(
        "s1",
        &json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [{
                "id": "mode", "name": "Mode", "type": "select", "currentValue": "default",
                "options": [{"value": "default", "name": "Default"}, {"value": "plan", "name": "Plan"}],
            }],
        }),
    ));
    let mode = |client: &Client| {
        let session = client.state().session("s1").unwrap();
        (
            session.config_options[0].current_value.clone(),
            session.mode.clone(),
        )
    };
    let id = client.set_config_option("s1", "mode", "plan");
    assert_eq!(mode(&client), ("plan".into(), Some("plan".into())));
    client.handle(Frame::Failure {
        id,
        error: kage_client::RpcError {
            code: -32602,
            message: "no".into(),
            data: None,
        },
    });
    assert_eq!(
        mode(&client),
        ("default".into(), Some("default".into())),
        "the refused value is taken back"
    );
}
