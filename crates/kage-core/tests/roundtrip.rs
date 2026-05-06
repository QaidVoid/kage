//! Integration tests: every public type with serde derives must round-trip
//! losslessly through JSON.

use kage_core::{
    Config, Content, ImageSource, LoopError, LoopEvent, Message, MessageId, Risk, Role,
    SandboxBackend, TokenUsage, ToolCallId, ToolOutput,
};

fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let s = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(value, &back);
}

#[test]
fn message_roundtrips() {
    let msg = Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None);
    roundtrip(&msg);
}

#[test]
fn all_content_variants_roundtrip() {
    let blocks = [
        Content::Text { text: "x".into() },
        Content::Thinking { text: "y".into() },
        Content::Image {
            source: ImageSource::Url {
                url: "https://example.com/x.png".into(),
            },
            mime: "image/png".into(),
        },
        Content::Image {
            source: ImageSource::Base64 {
                data: "AAAA".into(),
            },
            mime: "image/jpeg".into(),
        },
        Content::ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read".into(),
            input: serde_json::json!({"path": "/etc/hosts"}),
        },
        Content::ToolResultBlock {
            call_id: ToolCallId::new("call_1"),
            output: "127.0.0.1".into(),
            is_error: false,
        },
        Content::Custom {
            kind: "plugin:tps".into(),
            data: serde_json::json!({"tps": 42}),
        },
    ];
    for c in blocks {
        roundtrip(&c);
    }
}

#[test]
fn all_roles_roundtrip() {
    for r in [Role::User, Role::Assistant, Role::ToolResult, Role::System] {
        roundtrip(&r);
    }
}

#[test]
fn all_loop_events_roundtrip() {
    let mid = MessageId::new();
    let cid = ToolCallId::new("c");
    let events = [
        LoopEvent::MessageStart { id: mid },
        LoopEvent::TextDelta {
            id: mid,
            delta: "x".into(),
        },
        LoopEvent::ThinkingDelta {
            id: mid,
            delta: "y".into(),
        },
        LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "n".into(),
            input_partial: serde_json::Value::Null,
        },
        LoopEvent::ToolCallEnd {
            id: cid.clone(),
            output: ToolOutput {
                is_error: false,
                text: "ok".into(),
                structured: None,
            },
        },
        LoopEvent::MessageEnd {
            id: mid,
            usage: TokenUsage::default(),
        },
        LoopEvent::Compaction {
            kept: 5,
            summarized: 30,
        },
        LoopEvent::Error {
            kind: LoopError::Cancelled,
        },
    ];
    for ev in events {
        roundtrip(&ev);
    }
}

#[test]
fn all_loop_errors_roundtrip() {
    let errors = [
        LoopError::Provider {
            message: "x".into(),
        },
        LoopError::Tool {
            name: "bash".into(),
            message: "y".into(),
        },
        LoopError::Cancelled,
        LoopError::ContextOverflow,
        LoopError::Other {
            message: "z".into(),
        },
    ];
    for err in errors {
        roundtrip(&err);
    }
}

#[test]
fn all_sandbox_backends_roundtrip() {
    for b in [
        SandboxBackend::Local,
        SandboxBackend::Bubblewrap,
        SandboxBackend::SandboxExec,
    ] {
        roundtrip(&b);
    }
}

#[test]
fn all_risks_roundtrip() {
    for r in [Risk::Read, Risk::Write, Risk::Exec, Risk::Network] {
        roundtrip(&r);
    }
}

#[test]
fn config_default_roundtrips() {
    roundtrip(&Config::default());
}

#[test]
fn token_usage_with_cache_roundtrips() {
    roundtrip(&TokenUsage {
        input: 1_000,
        output: 500,
        cache_read: 800,
        cache_write: 200,
    });
}

#[test]
fn tool_output_with_structured_payload_roundtrips() {
    roundtrip(&ToolOutput {
        is_error: true,
        text: "out of bounds".into(),
        structured: Some(serde_json::json!({"line": 42, "kind": "syntax"})),
    });
}

#[test]
fn message_tolerates_unknown_fields() {
    let real = Message::new(Role::User, vec![], None);
    let mut json = serde_json::to_value(&real).expect("encode");
    let obj = json.as_object_mut().expect("object");
    obj.insert("future_field".into(), serde_json::json!("ignored"));
    obj.insert("another_extra".into(), serde_json::json!(42));
    let parsed: Message = serde_json::from_value(json).expect("lenient parse");
    assert_eq!(parsed.role, Role::User);
    assert_eq!(parsed.id, real.id);
}

#[test]
fn message_id_serializes_as_ulid_string() {
    let mid = MessageId::new();
    let s = serde_json::to_string(&mid).expect("encode");
    assert!(s.starts_with('"') && s.ends_with('"'));
    assert_eq!(s.len(), 28);
}
