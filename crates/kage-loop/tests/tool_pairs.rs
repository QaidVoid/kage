//! Wire-level coverage for tool-result pairing after a provider switch.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kage_core::{CancelFlag, Content, MediaSource, Message, Role, ToolCallId};
use kage_loop::{AgentContext, LoopConfig, NoopHooks, run};
use kage_provider::anthropic::AnthropicProvider;
use kage_tools::ToolRegistry;
use serde_json::{Value, json};

fn message(role: Role, content: Vec<Content>) -> Arc<Message> {
    Arc::new(Message::new(role, content, None))
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

fn call(id: &str, batch: u32) -> Content {
    Content::ToolCall {
        id: ToolCallId::new(id),
        name: "read".into(),
        input: json!({"batch": batch}),
    }
}

fn result(id: &str, output: &str) -> Content {
    Content::ToolResultBlock {
        call_id: ToolCallId::new(id),
        output: output.into(),
        is_error: false,
    }
}

fn capture_request(listener: &TcpListener) -> Value {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "provider did not connect");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept request: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST /v1/messages HTTP/1.1"));
    let mut content_length = None;
    let mut header_bytes = line.len();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        header_bytes += line.len();
        assert!(header_bytes < 16_384);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            assert!(content_length.is_none());
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let content_length = content_length.expect("request Content-Length");
    assert!(content_length < 1_048_576);
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    let request = serde_json::from_slice(&body).unwrap();
    let events = [
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_done", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-6", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 10, "output_tokens": 0}
            }}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "done"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {
            "stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 1}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    let mut response = String::new();
    for (name, data) in events {
        write!(response, "event: {name}\ndata: {data}\n\n").unwrap();
    }
    let stream = reader.get_mut();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
    stream.flush().unwrap();
    request
}

fn paired_result_groups(messages: &[Value]) -> Vec<Vec<&Value>> {
    let mut pending = BTreeSet::new();
    let mut groups: Vec<Vec<&Value>> = Vec::new();
    for message in messages {
        let blocks = message["content"].as_array().unwrap();
        if blocks.iter().any(|block| block["type"] == "tool_result") {
            assert_eq!(message["role"], "user");
            assert!(blocks.iter().all(|block| block["type"] == "tool_result"));
            for block in blocks {
                let id = block["tool_use_id"].as_str().unwrap();
                assert!(pending.remove(id), "unpaired or late result: {block}");
                groups.last_mut().unwrap().push(block);
            }
        } else {
            assert!(
                pending.is_empty(),
                "non-result message interrupts {pending:?}"
            );
            for block in blocks.iter().filter(|block| block["type"] == "tool_use") {
                assert_eq!(message["role"], "assistant");
                assert!(pending.insert(block["id"].as_str().unwrap()));
            }
            if !pending.is_empty() {
                groups.push(Vec::new());
            }
        }
    }
    assert!(pending.is_empty(), "unanswered calls: {pending:?}");
    groups
}

fn switched_history() -> Vec<Arc<Message>> {
    vec![
        message(Role::User, vec![text("initial request")]),
        message(Role::Assistant, vec![call("call_a", 0), call("call_b", 0)]),
        message(
            Role::ToolResult,
            vec![
                result("call_b", "matching image output"),
                Content::Image {
                    source: MediaSource::Base64 {
                        data: "iVBORw0KGgo=".into(),
                    },
                    mime: "image/png".into(),
                },
            ],
        ),
        message(Role::User, vec![text("interrupted before a finished")]),
        message(Role::ToolResult, vec![result("call_a", "late a output")]),
        message(
            Role::ToolResult,
            vec![result("call_orphan", "orphan output")],
        ),
        message(Role::Assistant, vec![call("call_reused", 1)]),
        message(
            Role::ToolResult,
            vec![result("call_reused", "first batch output")],
        ),
        message(Role::Assistant, vec![call("call_reused", 2)]),
        message(Role::User, vec![text("continue with Claude")]),
    ]
}

#[test]
fn switched_anthropic_request_pairs_results_without_rewriting_history() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = AnthropicProvider::with_base_url(
        "test-key",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = thread::spawn(move || capture_request(&listener));
    let mut cx = AgentContext::new("claude-sonnet-4-6", "test");
    cx.history = switched_history();
    let original = cx.history.clone();
    let outcome = run(
        &provider,
        &ToolRegistry::new(),
        &mut cx,
        LoopConfig {
            max_provider_retries: 0,
            compaction_threshold: 0.0,
            ..LoopConfig::default()
        },
        &mut NoopHooks,
        &CancelFlag::new(),
        |_| {},
    );
    let request = server.join().unwrap();
    outcome.unwrap();
    assert_eq!(request["model"], "claude-sonnet-4-6");
    assert_eq!(&cx.history[..original.len()], original.as_slice());
    assert!(
        cx.history
            .iter()
            .zip(&original)
            .all(|(current, previous)| Arc::ptr_eq(current, previous))
    );
    assert_eq!(cx.history.len(), original.len() + 1);
    let groups = paired_result_groups(request["messages"].as_array().unwrap());
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].len(), 2);
    let interrupted = groups[0]
        .iter()
        .find(|block| block["tool_use_id"] == "call_a")
        .unwrap();
    assert_eq!(interrupted["is_error"], true);
    assert!(
        interrupted["content"]
            .as_str()
            .unwrap()
            .starts_with("Interrupted:")
    );
    let image = groups[0]
        .iter()
        .find(|block| block["tool_use_id"] == "call_b")
        .unwrap();
    assert_eq!(image["is_error"], false);
    assert_eq!(
        image["content"],
        json!([
            {"type": "text", "text": "matching image output"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}
        ])
    );
    assert_eq!(groups[1].len(), 1);
    assert_eq!(groups[1][0]["content"], "first batch output");
    assert_eq!(groups[1][0]["is_error"], false);
    assert_eq!(groups[2].len(), 1);
    assert_eq!(groups[2][0]["is_error"], true);
    assert!(
        groups[2][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Interrupted:")
    );
    let wire = request.to_string();
    assert!(!wire.contains("late a output"));
    assert!(!wire.contains("orphan output"));
    assert!(!wire.contains("call_orphan"));
}
