//! Steering autonomous children follows their parent's lifecycle updates.

use kage_acp_wire::{ClientCapabilities, ContentBlock};
use kage_client::{Client, Frame, PromptOutcome, RequestId, RpcError, SteerError, TranscriptItem};
use serde_json::{Value, json};

fn connected(steer: bool) -> Client {
    let mut client = Client::new();
    let id = client.initialize(ClientCapabilities::default(), None);
    client.handle(Frame::Success {
        id: RequestId::Number(id),
        result: json!({
            "protocolVersion": 1,
            "agentCapabilities": {"steer": steer},
        }),
    });
    client.take_outgoing();
    client
}

fn update(client: &mut Client, session: &str, update: &Value) {
    client.handle(Frame::Notification {
        method: "session/update".into(),
        params: json!({"sessionId": session, "update": update}),
    });
}

fn announce(client: &mut Client, state: Option<&str>) {
    let mut announcement = json!({
        "sessionUpdate": "subagent_update",
        "subagentSessionId": "child",
        "name": "explore",
    });
    if let Some(state) = state {
        announcement["state"] = json!(state);
    }
    update(client, "parent", &announcement);
}

fn text(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(text)]
}

fn answer(client: &mut Client, id: u64, failed: bool) {
    client.handle(if failed {
        Frame::Failure {
            id: RequestId::Number(id),
            error: RpcError {
                code: -32000,
                message: "steer refused".into(),
                data: None,
            },
        }
    } else {
        Frame::Success {
            id: RequestId::Number(id),
            result: json!({"stopReason": "end_turn"}),
        }
    });
}

#[test]
fn an_autonomous_child_accepts_explicit_steering_between_provider_turns() {
    let mut client = connected(true);
    announce(&mut client, Some("running"));
    update(
        &mut client,
        "child",
        &json!({"sessionUpdate": "_kage/turn", "phase": "start"}),
    );
    update(
        &mut client,
        "child",
        &json!({"sessionUpdate": "_kage/turn", "phase": "end"}),
    );
    update(
        &mut client,
        "child",
        &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "search",
            "title": "search",
            "status": "in_progress",
        }),
    );
    let session = client.state().session("child").unwrap();
    assert!(!session.running);
    assert!(!session.in_turn, "tools run between provider turns");
    assert_eq!(session.parent.as_deref(), Some("parent"));

    let id = client.steer("child", text("check the tests too")).unwrap();
    let outgoing = client.take_outgoing();
    assert_eq!(outgoing.len(), 1);
    let Frame::Request {
        id: sent_id,
        method,
        params,
    } = &outgoing[0]
    else {
        panic!("expected a steer request");
    };
    assert_eq!(*sent_id, RequestId::Number(id));
    assert_eq!(method, "session/prompt");
    assert_eq!(params["sessionId"], "child");
    assert_eq!(params["delivery"], "steer");
    assert_eq!(params["prompt"][0]["text"], "check the tests too");

    let session = client.state().session("child").unwrap();
    assert!(!session.running, "a steer does not own the autonomous run");
    assert!(session.queue.is_empty());
    let Some(TranscriptItem::User { content, steered }) = session.items.last() else {
        panic!("expected the client's own user transcript row");
    };
    assert_eq!(content, &text("check the tests too"));
    assert!(*steered);

    answer(&mut client, id, false);
    assert!(!client.state().session("child").unwrap().running);
    assert!(client.steer("child", text("keep going")).is_ok());
}

#[test]
fn paused_and_legacy_announcements_still_describe_live_children() {
    for state in [Some("paused"), None] {
        let mut client = connected(true);
        announce(&mut client, state);
        assert!(client.steer("child", text("focus here")).is_ok());
        assert!(!client.state().session("child").unwrap().running);
    }
}

#[test]
fn final_child_lifecycles_reject_steering_even_with_a_stale_turn() {
    for state in ["completed", "failed", "cancelled", "future_state"] {
        let mut client = connected(true);
        announce(&mut client, Some("running"));
        update(
            &mut client,
            "child",
            &json!({"sessionUpdate": "_kage/turn", "phase": "start"}),
        );
        announce(&mut client, Some(state));
        announce(&mut client, None);
        assert!(client.state().session("child").unwrap().in_turn);
        assert_eq!(
            client.steer("child", text("too late")),
            Err(SteerError::NotRunning),
            "terminal lifecycle {state} must survive an update without a state"
        );
        assert!(client.take_outgoing().is_empty());
        assert!(client.state().session("child").unwrap().items.is_empty());
    }
}

#[test]
fn child_steer_answers_do_not_leave_an_owned_run_after_completion_or_failure() {
    for failed in [false, true] {
        for terminal_first in [false, true] {
            let mut client = connected(true);
            announce(&mut client, Some("running"));
            let id = client.steer("child", text("focus here")).unwrap();
            client.take_outgoing();
            assert!(!client.state().session("child").unwrap().running);
            let state = if failed { "failed" } else { "completed" };
            if terminal_first {
                announce(&mut client, Some(state));
            }
            answer(&mut client, id, failed);
            if !terminal_first {
                announce(&mut client, Some(state));
            }
            assert!(!client.state().session("child").unwrap().running);
            assert_eq!(
                client.steer("child", text("too late")),
                Err(SteerError::NotRunning)
            );
            assert!(client.take_outgoing().is_empty());
        }
    }
}

#[test]
fn a_live_child_still_requires_the_advertised_steering_capability() {
    let mut client = connected(false);
    announce(&mut client, Some("running"));
    assert_eq!(
        client.steer("child", text("focus here")),
        Err(SteerError::NotAdvertised)
    );
    let session = client.state().session("child").unwrap();
    assert!(!session.running);
    assert!(session.items.is_empty());
    assert!(client.take_outgoing().is_empty());
}

#[test]
fn root_steering_requires_an_owned_run_not_just_an_active_turn() {
    let mut client = connected(true);
    assert_eq!(
        client.steer("root", text("too early")),
        Err(SteerError::NotRunning)
    );
    update(
        &mut client,
        "root",
        &json!({"sessionUpdate": "_kage/turn", "phase": "start"}),
    );
    assert_eq!(
        client.steer("root", text("still too early")),
        Err(SteerError::NotRunning)
    );
    assert!(client.take_outgoing().is_empty());
}

#[test]
fn a_root_steer_success_or_failure_never_ends_the_owned_run() {
    for failed in [false, true] {
        let mut client = connected(true);
        let PromptOutcome::Sent { request_id } = client.prompt("root", text("go")) else {
            panic!("expected an owned prompt");
        };
        let steer_id = client.steer("root", text("focus here")).unwrap();
        client.take_outgoing();
        answer(&mut client, steer_id, failed);
        assert!(client.state().session("root").unwrap().running);
        answer(&mut client, request_id, false);
        assert!(!client.state().session("root").unwrap().running);
        assert_eq!(
            client.steer("root", text("too late")),
            Err(SteerError::NotRunning)
        );
    }
}
