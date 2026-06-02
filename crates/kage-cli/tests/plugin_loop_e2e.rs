//! End-to-end test of the plugin extension seam through a real loop turn.
//!
//! The piecewise plugin tests in `kage-plugin` invoke each registered
//! surface in isolation. These tests close the gap the review flagged:
//! a Lua-registered tool routed into a [`ToolRegistry`] and dispatched by
//! [`kage_loop::run`] when a scripted provider names it, and a
//! Lua-registered provider driving an actual turn. This asserts the
//! registration -> registry -> loop path the production wiring relies on.

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
    cx.history.push(user_msg("please echo"));
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

    let result = cx
        .history
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResultBlock { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a tool result was appended");
    assert!(
        result.contains("echo:ping"),
        "tool result should carry the Lua tool output, got {result:?}"
    );
}

#[test]
fn loop_streams_from_a_lua_registered_provider() {
    let rt = PluginRuntime::new().expect("runtime builds");
    rt.eval_plugin(
        "fake_provider",
        "kage.register_provider({ \
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
    cx.history.push(user_msg("hello"));
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

    let text = cx
        .history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .expect("assistant text was appended");
    assert!(
        text.contains("hi from lua"),
        "assistant message should carry the Lua provider stream, got {text:?}"
    );
}
