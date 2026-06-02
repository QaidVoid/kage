//! Tests for the Anthropic provider and SSE stream.

use super::*;
use kage_core::{Content, Message, Role};

fn user_msg(text: &str) -> Message {
    Message::new(
        Role::User,
        vec![Content::Text {
            text: text.to_owned(),
        }],
        None,
    )
}

#[test]
fn body_sets_model_and_messages() {
    let req = StreamRequest::new("claude-sonnet-4-6", vec![user_msg("hi")]);
    let body = build_request_body(&req, false);
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["stream"], false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    let blocks = messages[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "hi");
}

#[test]
fn body_promotes_system_to_cached_array() {
    let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
    req.system = Some("you are kage".into());
    let body = build_request_body(&req, false);
    let system = body["system"].as_array().expect("system is array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "you are kage");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn body_marks_last_message_block_for_caching() {
    let req = StreamRequest::new("m", vec![user_msg("hello"), user_msg("again")]);
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    let last = &messages[messages.len() - 1];
    let blocks = last["content"].as_array().unwrap();
    let last_block = &blocks[blocks.len() - 1];
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");
}

#[test]
fn body_does_not_mark_earlier_messages() {
    let req = StreamRequest::new("m", vec![user_msg("first"), user_msg("second")]);
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    let first = &messages[0];
    let blocks = first["content"].as_array().unwrap();
    assert!(blocks[0].get("cache_control").is_none());
}

#[test]
fn body_drops_system_role_messages() {
    let mut req = StreamRequest::new(
        "m",
        vec![
            Message::new(
                Role::System,
                vec![Content::Text {
                    text: "ignored".into(),
                }],
                None,
            ),
            user_msg("hi"),
        ],
    );
    req.system = Some("the real system prompt".into());
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn body_includes_tools_when_present() {
    let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
    req.tools = vec![ToolSpec {
        name: "read".into(),
        description: "read a file".into(),
        schema: serde_json::json!({"type":"object"}),
    }];
    let body = build_request_body(&req, false);
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "read");
    assert_eq!(tools[0]["description"], "read a file");
    assert_eq!(
        tools[0]["input_schema"],
        serde_json::json!({"type":"object"})
    );
}

#[test]
fn body_includes_thinking_when_configured() {
    let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
    req.thinking = Some(crate::ThinkingConfig {
        budget_tokens: 12_000,
    });
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 12_000);
}

#[test]
fn body_resolves_thinking_level_to_default_budget() {
    let mut req = StreamRequest::new("unknown-model", vec![user_msg("hi")]);
    req.level = Some(crate::ThinkingLevel::High);
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(
        body["thinking"]["budget_tokens"],
        crate::ThinkingLevel::High.default_budget_tokens().unwrap()
    );
}

#[test]
fn body_omits_thinking_when_level_off() {
    let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
    req.level = Some(crate::ThinkingLevel::Off);
    let body = build_request_body(&req, false);
    assert!(body.get("thinking").is_none());
}

#[test]
fn explicit_thinking_config_wins_over_level() {
    let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
    req.thinking = Some(crate::ThinkingConfig { budget_tokens: 999 });
    req.level = Some(crate::ThinkingLevel::XHigh);
    let body = build_request_body(&req, false);
    assert_eq!(body["thinking"]["budget_tokens"], 999);
}

#[test]
fn body_uses_default_max_tokens_when_unset() {
    let req = StreamRequest::new("m", vec![user_msg("hi")]);
    let body = build_request_body(&req, false);
    assert_eq!(body["max_tokens"], 4_096);
}

#[test]
fn assistant_message_with_tool_call_serializes() {
    let assistant = Message::new(
        Role::Assistant,
        vec![Content::ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read".into(),
            input: serde_json::json!({"path":"/etc/hosts"}),
        }],
        None,
    );
    let req = StreamRequest::new("m", vec![user_msg("read hosts"), assistant]);
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["role"], "assistant");
    let blocks = messages[1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["id"], "call_1");
    assert_eq!(blocks[0]["name"], "read");
    assert_eq!(blocks[0]["input"]["path"], "/etc/hosts");
}

#[test]
fn tool_result_message_uses_user_role() {
    let result = Message::new(
        Role::ToolResult,
        vec![Content::ToolResultBlock {
            call_id: ToolCallId::new("call_1"),
            output: "127.0.0.1".into(),
            is_error: false,
        }],
        None,
    );
    let req = StreamRequest::new("m", vec![result]);
    let body = build_request_body(&req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    let blocks = messages[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "tool_result");
    assert_eq!(blocks[0]["tool_use_id"], "call_1");
    assert_eq!(blocks[0]["content"], "127.0.0.1");
    assert_eq!(blocks[0]["is_error"], false);
}

#[test]
fn parse_response_extracts_text_and_usage() {
    let json = serde_json::json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type":"text","text":"hello"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let parsed: AnthropicMessage = serde_json::from_value(json).unwrap();
    let (msg, stop, usage) = parsed.into_internal();
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.content.len(), 1);
    if let Content::Text { text } = &msg.content[0] {
        assert_eq!(text, "hello");
    } else {
        panic!("expected Text content");
    }
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(usage.input, 10);
    assert_eq!(usage.output, 5);
}

/// Round-trip the real Anthropic API. Opt-in: requires `ANTHROPIC_API_KEY`
/// in the environment. Run with:
///
/// ```sh
/// ANTHROPIC_API_KEY=sk-ant-... cargo test -p kage-provider -- --ignored anthropic_live
/// ```
#[test]
#[ignore = "requires ANTHROPIC_API_KEY"]
fn anthropic_live_smoke() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY to run this test");
    let provider = AnthropicProvider::new(key);
    let req = StreamRequest::new(
        "claude-haiku-4-5-20251001",
        vec![Message::new(
            Role::User,
            vec![Content::Text {
                text: "Reply with exactly the word: pong".into(),
            }],
            None,
        )],
    );
    let resp = provider
        .request(&req, &CancelFlag::new())
        .expect("request succeeds");
    let (msg, _stop, usage) = resp.into_internal();
    assert!(!msg.content.is_empty(), "response has at least one block");
    assert!(usage.input > 0, "input tokens reported");
    assert!(usage.output > 0, "output tokens reported");
}

#[test]
fn parse_response_extracts_tool_call_and_cache_tokens() {
    let json = serde_json::json!({
        "id": "msg_02",
        "type": "message",
        "role": "assistant",
        "model": "m",
        "content": [
            {"type":"text","text":"reading"},
            {"type":"tool_use","id":"call_1","name":"read","input":{"path":"/x"}}
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 50,
            "cache_read_input_tokens": 80
        }
    });
    let parsed: AnthropicMessage = serde_json::from_value(json).unwrap();
    let (msg, stop, usage) = parsed.into_internal();
    assert_eq!(msg.content.len(), 2);
    assert_eq!(stop, StopReason::ToolUse);
    assert_eq!(usage.cache_read, 80);
    assert_eq!(usage.cache_write, 50);
}

fn stream_from_bytes(bytes: &'static [u8]) -> AnthropicStream {
    AnthropicStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
}

fn collect_ok(stream: AnthropicStream) -> Vec<ProviderEvent> {
    stream.map(|r| r.expect("stream item is Ok")).collect()
}

#[test]
fn sse_parser_extracts_event_and_data() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
    let first = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(first.name, "message_start");
    assert!(first.data.contains("input_tokens"));
    let second = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(second.name, "message_stop");
    assert!(crate::sse::read_sse_event(&mut reader).unwrap().is_none());
}

#[test]
fn sse_parser_ignores_comments_and_blank_lines() {
    let bytes: &[u8] = b": this is a comment\n\nevent: ping\ndata: {}\n\nevent: ping\ndata: {}\n\n";
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
    let first = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(first.name, "ping");
    let second = crate::sse::read_sse_event(&mut reader).unwrap().unwrap();
    assert_eq!(second.name, "ping");
}

#[test]
fn stream_emits_text_deltas() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));
    assert!(matches!(events[0], ProviderEvent::MessageStart));
    assert!(
        matches!(&events[1], ProviderEvent::TextDelta { delta } if delta == "hello"),
        "got {:?}",
        events[1]
    );
    assert!(
        matches!(&events[2], ProviderEvent::TextDelta { delta } if delta == " world"),
        "got {:?}",
        events[2]
    );
    if let ProviderEvent::MessageEnd { stop_reason, usage } = events.last().unwrap() {
        assert_eq!(*stop_reason, StopReason::EndTurn);
        assert_eq!(usage.input, 5);
        assert_eq!(usage.output, 2);
    } else {
        panic!("expected MessageEnd at end, got {:?}", events.last());
    }
}

#[test]
fn stream_emits_thinking_delta() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning...\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));
    assert!(
        events.iter().any(
            |e| matches!(e, ProviderEvent::ThinkingDelta { delta } if delta == "reasoning...")
        )
    );
}

#[test]
fn stream_assembles_tool_call() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let events = collect_ok(stream_from_bytes(bytes));

    let start_idx = events
        .iter()
        .position(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
        .expect("ToolCallStart present");
    if let ProviderEvent::ToolCallStart { id, name } = &events[start_idx] {
        assert_eq!(id.0, "call_1");
        assert_eq!(name, "read");
    }

    let args_count = events
        .iter()
        .filter(|e| matches!(e, ProviderEvent::ToolCallArgsDelta { .. }))
        .count();
    assert_eq!(args_count, 2);

    let end = events
        .iter()
        .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
        .expect("ToolCallEnd present");
    if let ProviderEvent::ToolCallEnd { id, input } = end {
        assert_eq!(id.0, "call_1");
        assert_eq!(input["path"], "/tmp");
    }

    if let Some(ProviderEvent::MessageEnd { stop_reason, .. }) = events.last() {
        assert_eq!(*stop_reason, StopReason::ToolUse);
    } else {
        panic!("expected MessageEnd");
    }
}

#[test]
fn stream_emits_decode_error_when_tool_input_partial_json_is_malformed() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"write\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not json\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let s = stream_from_bytes(bytes);
    let events: Vec<_> = s.collect();
    let decode_err = events
        .iter()
        .find(|r| matches!(r, Err(ProviderError::Decode(_))))
        .expect("expected a Decode error event for malformed tool input");
    if let Err(ProviderError::Decode(msg)) = decode_err {
        assert!(msg.contains("call_1"), "error should name the tool call id");
        assert!(
            msg.contains("{not json"),
            "error should include the raw partial input"
        );
    }
    assert!(
        !events
            .iter()
            .any(|r| matches!(r, Ok(ProviderEvent::ToolCallEnd { .. }))),
        "no ToolCallEnd should fire when input fails to parse"
    );
}

#[test]
fn stream_yields_cancelled_when_flag_set() {
    let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let cancel = CancelFlag::new();
    cancel.cancel();
    let stream = AnthropicStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
    let mut events = stream;
    let first = events.next();
    assert!(matches!(first, Some(Err(ProviderError::Cancelled))));
    assert!(events.next().is_none());
}

#[test]
fn stream_propagates_error_event() {
    let bytes: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers are overloaded\"}}\n\n";
    let mut events = stream_from_bytes(bytes);
    let first = events.next().unwrap();
    match first {
        Err(ProviderError::Decode(msg)) => {
            assert!(msg.contains("overloaded"), "got {msg}");
        }
        other => panic!("expected Decode error, got {other:?}"),
    }
}
