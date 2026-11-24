//! Withdrawal requests preserve host queue outcomes and original attachments.

use kage_client::wire::PromptDelivery;
use kage_client::{Change, Client, Frame, RequestId};
use serde_json::json;

#[test]
fn host_withdrawal_returns_the_original_prompt_to_its_session() {
    let mut client = Client::new();
    let request = client.withdraw_prompt("session", PromptDelivery::Steer);
    assert_eq!(
        client.take_outgoing(),
        [Frame::Request {
            id: RequestId::Number(request),
            method: "_kage/prompt/withdraw".into(),
            params: json!({"sessionId": "session", "delivery": "steer"}),
        }]
    );
    let content = json!([
        {"type": "text", "text": "edit this"},
        {"type": "image", "mimeType": "image/png", "data": "AAAA"}
    ]);
    let changes = client.handle(Frame::Success {
        id: RequestId::Number(request),
        result: json!({"delivery": "steer", "content": content}),
    });
    assert!(
        matches!(&changes[..], [Change::PromptWithdrawn { session_id, delivery: PromptDelivery::Steer, content, .. }] if session_id == "session" && content.len() == 2)
    );
    assert_eq!(changes[0].session_id(), Some("session"));
}

#[test]
fn an_empty_withdrawal_result_preserves_the_actual_host_outcome() {
    let mut client = Client::new();
    let request = client.withdraw_prompt("session", PromptDelivery::Steer);
    let changes = client.handle(Frame::Success {
        id: RequestId::Number(request),
        result: json!({"delivery": "steer"}),
    });
    assert!(
        matches!(&changes[..], [Change::PromptWithdrawn { content, .. }] if content.is_empty())
    );
}

#[test]
fn malformed_withdrawal_answers_report_failure_without_returning_a_prompt() {
    let mut client = Client::new();
    let request = client.withdraw_prompt("session", PromptDelivery::Steer);
    let changes = client.handle(Frame::Success {
        id: RequestId::Number(request),
        result: json!({"delivery": "invalid"}),
    });
    assert!(matches!(&changes[..], [Change::Failed { request: id, .. }] if *id == request));
}
