//! End-to-end test of the plugin extension seam through a real loop turn.
//!
//! The piecewise plugin tests in `kage-plugin` invoke each registered
//! surface in isolation. These tests close the gap the review flagged:
//! a Lua-registered tool routed into a [`ToolRegistry`] and dispatched by
//! [`kage_loop::run`] when a scripted provider names it, and a
//! Lua-registered provider driving an actual turn. This asserts the
//! registration -> registry -> loop path the production wiring relies on.

use std::sync::Arc;

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage, ToolCallId};
use kage_loop::{AgentContext, LoopConfig, NoopHooks, run};
use kage_plugin::PluginRuntime;
use kage_provider::testing::MockProvider;
use kage_provider::{ProviderEvent, StopReason};
use kage_tools::ToolRegistry;

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
fn model_invokes_a_lua_registered_tool_through_the_loop() {
    let rt = PluginRuntime::new().expect("runtime builds");
    rt.eval_plugin(
        "echo_tool",
        "kage.register_tool({ \
            name = 'plugin_echo', \
            description = 'echoes input.msg', \
            schema = { type = 'object' }, \
            risk = 'read', \
            execute = function(input) return 'echo:' .. (input.msg or '?') end, \
        })",
    )
    .expect("plugin loads");

    let mut tools = ToolRegistry::new();
    for tool in rt.registered_tools() {
        tools.register(tool);
    }
    assert!(tools.get("plugin_echo").is_some());

    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "plugin_echo".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: ToolCallId::new("call_1"),
                partial: "{\"msg\":\"ping\"}".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "msg": "ping" }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "done".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
    ]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("please echo")));
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();

    run(
        &mock,
        &tools,
        &mut cx,
        LoopConfig::default(),
        &mut hooks,
        &cancel,
        |_| {},
    )
    .expect("loop runs");

    let results: Vec<String> = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResultBlock { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        ["echo:ping"],
        "exactly one tool result with exactly the Lua tool output"
    );
}

/// A Lua tool that raises must reach the loop as an error-shaped tool
/// result, not abort the run.
#[test]
fn a_raising_lua_tool_becomes_an_error_tool_result() {
    let rt = PluginRuntime::new().expect("runtime builds");
    rt.eval_plugin(
        "boom_tool",
        "kage.register_tool({ \
            name = 'plugin_boom', \
            description = 'always raises', \
            schema = { type = 'object' }, \
            risk = 'read', \
            execute = function(input) error('kaboom') end, \
        })",
    )
    .expect("plugin loads");

    let mut tools = ToolRegistry::new();
    for tool in rt.registered_tools() {
        tools.register(tool);
    }

    let mock = MockProvider::sequence(vec![
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "plugin_boom".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "recovered".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
    ]);

    let mut cx = AgentContext::new("mock:m", "");
    cx.history.push(Arc::new(user_msg("blow up")));
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();

    run(
        &mock,
        &tools,
        &mut cx,
        LoopConfig::default(),
        &mut hooks,
        &cancel,
        |_| {},
    )
    .expect("the error result does not abort the run");

    let failures: Vec<&Content> = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::ToolResultBlock { is_error: true, .. }))
        .collect();
    let [failure] = &failures[..] else {
        panic!("exactly one error result expected, got {failures:?}");
    };
    let Content::ToolResultBlock { output, .. } = failure else {
        unreachable!()
    };
    assert!(
        output.contains("kaboom"),
        "the error result quotes the Lua error: {output:?}"
    );
}

#[test]
fn loop_streams_from_a_lua_registered_provider() {
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("fake_provider".to_owned(), vec!["provider".to_owned()]);
    let rt = PluginRuntime::builder()
        .capabilities(caps)
        .build()
        .expect("runtime builds");
    rt.eval_plugin(
        "fake_provider",
        "kage.request_capabilities({'provider'}); \
        kage.register_provider({ \
            id = 'fakeprov', \
            stream = function(req) \
                return { \
                    { type = 'message_start' }, \
                    { type = 'text_delta', delta = 'hi from lua' }, \
                    { type = 'message_end', stop_reason = 'end_turn', \
                      usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } }, \
                } \
            end, \
        })",
    )
    .expect("plugin loads");

    let provider = rt
        .registered_providers()
        .pop()
        .expect("a provider was registered");

    let tools = ToolRegistry::new();
    let mut cx = AgentContext::new("fakeprov:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();

    run(
        provider.as_ref(),
        &tools,
        &mut cx,
        LoopConfig::default(),
        &mut hooks,
        &cancel,
        |_| {},
    )
    .expect("loop runs");

    let text: Vec<String> = cx
        .history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text,
        ["hi from lua"],
        "exactly one assistant message with exactly the Lua provider stream"
    );
}

/// A Lua provider whose stream errors must end the turn with the
/// error recorded, not hang or loop.
#[test]
fn an_erroring_lua_provider_stream_ends_the_turn_with_the_error() {
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("broken_provider".to_owned(), vec!["provider".to_owned()]);
    let rt = PluginRuntime::builder()
        .capabilities(caps)
        .build()
        .expect("runtime builds");
    rt.eval_plugin(
        "broken_provider",
        "kage.request_capabilities({'provider'}); \
        kage.register_provider({ \
            id = 'brokenprov', \
            stream = function(req) error('stream exploded') end, \
        })",
    )
    .expect("plugin loads");

    let provider = rt
        .registered_providers()
        .pop()
        .expect("a provider was registered");

    let tools = ToolRegistry::new();
    let mut cx = AgentContext::new("brokenprov:m", "");
    cx.history.push(Arc::new(user_msg("hello")));
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();

    let outcome = run(
        provider.as_ref(),
        &tools,
        &mut cx,
        LoopConfig::default(),
        &mut hooks,
        &cancel,
        |_| {},
    );
    let err = outcome.expect_err("the erroring stream fails the run");
    let rendered = err.to_string();
    assert!(
        rendered.contains("stream exploded") || rendered.contains("provider"),
        "the error names its source: {rendered}"
    );
    assert!(
        cx.history.iter().all(|m| m.role != Role::Assistant),
        "a failed attempt appends no assistant message"
    );
}
