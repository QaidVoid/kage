//! Usage state preserves the pricing quality carried by session updates.

use kage_client::{Client, Frame, Usage, wire::CostStatus};
use serde_json::{Value, json};

fn update(value: &Value) -> Frame {
    Frame::Notification {
        method: "session/update".into(),
        params: json!({"sessionId": "s1", "update": value}),
    }
}

fn usage_update(status: Option<&str>) -> Value {
    let mut value = json!({
        "sessionUpdate": "usage_update",
        "used": 3,
        "size": 100,
        "cost": {"amount": 0.0, "currency": "USD"},
        "_meta": {"kage": {"input": 3, "output": 0, "cacheRead": 0, "cacheWrite": 0}}
    });
    if let Some(status) = status {
        value["_meta"]["kage"]["costStatus"] = json!(status);
    }
    value
}

#[test]
fn usage_queries_default_to_recorded_without_metadata() {
    assert_eq!(Usage::default().cost_status(), CostStatus::Recorded);
    let mut client = Client::new();
    client.handle(update(&json!({
        "sessionUpdate": "usage_update",
        "used": 4,
        "size": 100,
        "cost": {"amount": 1.5, "currency": "EUR"}
    })));
    let usage = &client.state().session("s1").unwrap().usage;
    assert_eq!(usage.cost_status(), CostStatus::Recorded);
    assert!(usage.tokens.is_none());
    assert_eq!(usage.cost.as_ref().unwrap().currency, "EUR");
}

#[test]
fn updates_preserve_pricing_quality_and_distinguish_zero_amounts() {
    let mut client = Client::new();
    for (status, expected) in [
        (None, CostStatus::Recorded),
        (Some("estimated"), CostStatus::Estimated),
        (Some("incomplete"), CostStatus::Incomplete),
        (Some("recorded"), CostStatus::Recorded),
    ] {
        client.handle(update(&usage_update(status)));
        let usage = &client.state().session("s1").unwrap().usage;
        assert_eq!(
            usage.cost.as_ref().unwrap().amount.to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(usage.cost_status(), expected);
        assert_eq!(usage.tokens.unwrap().cost_status, expected);
    }
}

#[test]
fn session_replay_restores_pricing_quality() {
    for (status, expected) in [
        ("estimated", CostStatus::Estimated),
        ("incomplete", CostStatus::Incomplete),
    ] {
        let mut client = Client::new();
        client.handle(update(&usage_update(Some(status))));
        let (id, _) = client.load_session("s1", "/srv/proj", &[], None);
        client.handle(update(&usage_update(Some(status))));
        client.handle(Frame::Success {
            id: kage_client::RequestId::Number(id),
            result: json!({}),
        });
        assert_eq!(
            client.state().session("s1").unwrap().usage.cost_status(),
            expected
        );
        let mut reconnected = Client::new();
        let (id, _) = reconnected.load_session("s1", "/srv/proj", &[], None);
        reconnected.handle(update(&usage_update(Some(status))));
        reconnected.handle(Frame::Success {
            id: kage_client::RequestId::Number(id),
            result: json!({}),
        });
        assert_eq!(
            reconnected
                .state()
                .session("s1")
                .unwrap()
                .usage
                .cost_status(),
            expected
        );
    }
}

#[test]
fn child_updates_preserve_their_own_cost_status() {
    let mut client = Client::new();
    client.handle(update(&json!({
        "sessionUpdate": "subagent_update",
        "subagentSessionId": "child",
        "usage": {"input": 3, "output": 2, "cost": 0.0, "costStatus": "incomplete"}
    })));
    let session = client.state().session("s1").unwrap();
    assert_eq!(
        session.agents["child"].usage.as_ref().unwrap().cost_status,
        CostStatus::Incomplete
    );
    assert_eq!(session.usage.cost_status(), CostStatus::Recorded);
}
