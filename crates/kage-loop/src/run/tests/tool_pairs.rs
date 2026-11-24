use super::*;

fn calls(ids: &[&str]) -> Message {
    assistant_msg(
        ids.iter()
            .map(|id| Content::ToolCall {
                id: ToolCallId::new(*id),
                name: "read".into(),
                input: serde_json::json!({}),
            })
            .collect(),
    )
}

fn results(ids: &[&str]) -> Message {
    Message::new(
        Role::ToolResult,
        ids.iter()
            .map(|id| Content::ToolResultBlock {
                call_id: ToolCallId::new(*id),
                output: (*id).into(),
                is_error: false,
            })
            .collect(),
        None,
    )
}

fn interrupted(id: &str) -> Message {
    Message::new(
        Role::ToolResult,
        vec![Content::ToolResultBlock {
            call_id: ToolCallId::new(id),
            output: INTERRUPTED.into(),
            is_error: true,
        }],
        None,
    )
}

fn request(history: Vec<Message>) -> StreamRequest {
    let mut cx = AgentContext::new("mock:m", "");
    cx.history = history.into_iter().map(Arc::new).collect();
    let original = cx.history.clone();
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    assert_eq!(cx.history, original);
    assert!(
        cx.history
            .iter()
            .zip(&original)
            .all(|(actual, original)| Arc::ptr_eq(actual, original))
    );
    assert_eq!(
        repair_tool_pairs(req.messages.clone()),
        req.messages,
        "repair must be idempotent"
    );
    req
}

#[test]
fn build_request_pairs_tool_results_with_only_the_current_batch() {
    let cases = [
        (
            "misplaced result",
            vec![calls(&["a"]), user_msg("between"), results(&["a"])],
            vec![calls(&["a"]), interrupted("a"), user_msg("between")],
        ),
        (
            "orphan results",
            vec![results(&["orphan"]), user_msg("go"), results(&["other"])],
            vec![user_msg("go")],
        ),
        (
            "earlier result cannot answer a later reused id",
            vec![
                calls(&["a"]),
                results(&["a"]),
                user_msg("next"),
                calls(&["a"]),
            ],
            vec![
                calls(&["a"]),
                results(&["a"]),
                user_msg("next"),
                calls(&["a"]),
                interrupted("a"),
            ],
        ),
        (
            "later result cannot answer an earlier reused id",
            vec![
                calls(&["a"]),
                user_msg("next"),
                calls(&["a"]),
                results(&["a"]),
            ],
            vec![
                calls(&["a"]),
                interrupted("a"),
                user_msg("next"),
                calls(&["a"]),
                results(&["a"]),
            ],
        ),
        (
            "duplicate results across messages",
            vec![calls(&["a"]), results(&["a"]), results(&["a"])],
            vec![calls(&["a"]), results(&["a"])],
        ),
        (
            "mixed orphan and duplicate results in a combined message",
            vec![
                calls(&["a", "b"]),
                results(&["orphan", "a", "a", "b", "other"]),
            ],
            vec![calls(&["a", "b"]), results(&["a", "b"])],
        ),
        (
            "out of order results in a valid batch",
            vec![calls(&["a", "b"]), results(&["b"]), results(&["a"])],
            vec![calls(&["a", "b"]), results(&["b"]), results(&["a"])],
        ),
        (
            "late result after a partial batch",
            vec![
                calls(&["a", "b"]),
                results(&["a"]),
                user_msg("next"),
                results(&["b"]),
            ],
            vec![
                calls(&["a", "b"]),
                results(&["a"]),
                interrupted("b"),
                user_msg("next"),
            ],
        ),
        (
            "consecutive assistant batches",
            vec![calls(&["a"]), calls(&["b"]), results(&["b"])],
            vec![
                calls(&["a"]),
                interrupted("a"),
                calls(&["b"]),
                results(&["b"]),
            ],
        ),
        (
            "empty result messages do not close a batch",
            vec![calls(&["a"]), results(&[]), results(&["a"])],
            vec![calls(&["a"]), results(&["a"])],
        ),
    ];
    for (name, history, expected) in cases {
        let req = request(history);
        let actual: Vec<_> = req.messages.iter().map(|m| (m.role, &m.content)).collect();
        let expected: Vec<_> = expected.iter().map(|m| (m.role, &m.content)).collect();
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn build_request_keeps_valid_result_messages_and_metadata() {
    let first_call = calls(&["a", "b"]);
    let first_result = results(&["b", "a"]);
    let second_call = calls(&["a"]);
    let second_result = results(&["a"]);
    let mut cx = AgentContext::new("mock:m", "");
    cx.history = [
        first_call,
        first_result,
        user_msg("next"),
        second_call,
        second_result,
    ]
    .into_iter()
    .map(Arc::new)
    .collect();
    let req = build_request(&cx, &ToolRegistry::new(), &MockProvider::sequence(vec![]));
    assert_eq!(req.messages, cx.history);
    assert!(
        req.messages
            .iter()
            .zip(&cx.history)
            .all(|(sent, stored)| Arc::ptr_eq(sent, stored))
    );
}

#[test]
fn build_request_drops_invalid_result_media_without_reattaching_it() {
    let image = |name: &str| Content::Image {
        source: kage_core::MediaSource::Url {
            url: format!("https://example.com/{name}.png"),
        },
        mime: "image/png".into(),
    };
    let audio = |name: &str| Content::Audio {
        source: kage_core::MediaSource::Url {
            url: format!("https://example.com/{name}.wav"),
        },
        mime: "audio/wav".into(),
    };
    let result = |id: &str| results(&[id]).content.remove(0);
    let valid = vec![result("a"), image("a"), audio("a"), result("b"), audio("b")];
    let mixed = Message::new(
        Role::ToolResult,
        vec![
            image("unbound"),
            result("orphan"),
            image("orphan"),
            result("a"),
            image("a"),
            audio("a"),
            result("a"),
            image("duplicate"),
            result("b"),
            audio("b"),
            result("other"),
            audio("other"),
        ],
        None,
    );
    let original = mixed.clone();
    let req = request(vec![calls(&["a", "b"]), mixed]);
    assert_eq!(req.messages.len(), 2);
    assert_eq!(
        *req.messages[1],
        Message {
            content: valid,
            ..original
        }
    );
    let parts = kage_core::tool_results(&req.messages[1].content);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].media.len(), 1);
    assert_eq!(parts[0].audio.len(), 1);
    assert_eq!(parts[1].media.len(), 0);
    assert_eq!(parts[1].audio.len(), 1);
}

#[test]
fn build_request_parents_interrupted_results_to_the_issuing_assistant() {
    let call = calls(&["a", "b"]);
    let parent = call.id;
    let req = request(vec![call, user_msg("next")]);
    assert_eq!(req.messages.len(), 4);
    assert_eq!(req.messages[1].parent, Some(parent));
    assert_eq!(req.messages[2].parent, Some(parent));
}
