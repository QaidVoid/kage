//! Loop entry point.
//!
//! Top-level control flow:
//!
//! ```text
//! outer loop {                  // follow-ups
//!   inner loop {                // tool-call rounds
//!     stream from provider
//!     translate events -> LoopEvent
//!     if message had tool calls -> dispatch, append results, continue
//!     else                       -> break inner
//!   }
//!   if hooks.get_followup() -> push as user message, continue
//!   else                    -> break outer
//! }
//! ```
//!
//! T4.2 wires only the shell. Real provider-event translation lands in T4.3,
//! tool dispatch in T4.4.

use kage_core::{CancelFlag, LoopError, LoopEvent};
use kage_provider::{Provider, StreamRequest};
use kage_tools::ToolRegistry;

use crate::dispatch::dispatch_tool_calls;
use crate::stream::collect_turn;
use crate::{AgentContext, Hooks, LoopConfig};

/// Drive one agent run to completion.
///
/// `cx` carries the conversation forward: the caller is expected to push the
/// initiating user message into `cx.history` before calling this. On return,
/// `cx.history` reflects every message produced during the run, and
/// `cx.budget` is updated from provider-reported usage.
///
/// Streaming events are delivered to `emit` in order. The same events also
/// flow through `hooks.on_event`, which fires first.
///
/// Cancellation is cooperative: the loop polls `cancel` between turns and
/// after each provider event. On cancel, the run terminates with
/// [`LoopError::Cancelled`].
///
/// # Errors
///
/// Returns the same [`LoopError`] variant that was emitted as the terminal
/// [`LoopEvent::Error`], so callers can react programmatically without
/// re-parsing events.
pub fn run<F>(
    provider: &dyn Provider,
    tools: &ToolRegistry,
    cx: &mut AgentContext,
    config: &LoopConfig,
    hooks: &mut dyn Hooks,
    cancel: &CancelFlag,
    mut emit: F,
) -> Result<(), LoopError>
where
    F: FnMut(LoopEvent),
{
    let mut iterations: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            return finish_cancelled(hooks, &mut emit);
        }

        loop {
            iterations = iterations.saturating_add(1);
            if iterations > config.max_iterations {
                let kind = LoopError::Other {
                    message: format!("max_iterations ({}) exceeded", config.max_iterations),
                };
                emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            if cancel.is_cancelled() {
                return finish_cancelled(hooks, &mut emit);
            }

            if let Some(steering) = hooks.get_steering() {
                cx.history.push(kage_core::Message::new(
                    kage_core::Role::User,
                    vec![kage_core::Content::Text { text: steering }],
                    cx.history.last().map(|m| m.id),
                ));
            }

            let req = build_request(cx, tools);
            let stream = match provider.stream(req, cancel) {
                Ok(s) => s,
                Err(e) => {
                    let kind = LoopError::Provider {
                        message: e.to_string(),
                    };
                    emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                    return Err(kind);
                }
            };

            let parent = cx.history.last().map(|m| m.id);
            let turn = match collect_turn(parent, stream, cancel, hooks, &mut emit) {
                Ok(t) => t,
                Err(kind) => {
                    emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                    return Err(kind);
                }
            };

            cx.budget.add(turn.usage);
            let assistant_id = turn.message.id;
            let pending = turn.tool_calls;
            cx.history.push(turn.message);

            if pending.is_empty() {
                break;
            }

            let workdir = cx.workdir.clone();
            let results = match dispatch_tool_calls(
                pending,
                tools,
                &workdir,
                cancel,
                assistant_id,
                hooks,
                &mut emit,
            ) {
                Ok(r) => r,
                Err(kind) => {
                    emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                    return Err(kind);
                }
            };
            cx.history.extend(results);
        }

        let Some(text) = hooks.get_followup() else {
            return Ok(());
        };
        cx.history.push(kage_core::Message::new(
            kage_core::Role::User,
            vec![kage_core::Content::Text { text }],
            cx.history.last().map(|m| m.id),
        ));
    }
}

/// Emit one event to both the host's `Hooks::on_event` and the user emit
/// callback, in that order.
pub(crate) fn emit_one<F: FnMut(LoopEvent)>(hooks: &mut dyn Hooks, emit: &mut F, event: LoopEvent) {
    hooks.on_event(&event);
    emit(event);
}

/// Construct the next [`StreamRequest`] from the current agent context.
fn build_request(cx: &AgentContext, tools: &ToolRegistry) -> StreamRequest {
    let mut req = StreamRequest::new(&cx.model, cx.history.clone());
    if !cx.system_prompt.is_empty() {
        req.system = Some(cx.system_prompt.clone());
    }
    req.tools = tools.list_for_provider();
    req
}

fn finish_cancelled<F: FnMut(LoopEvent)>(
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<(), LoopError> {
    emit_one(
        hooks,
        emit,
        LoopEvent::Error {
            kind: LoopError::Cancelled,
        },
    );
    Err(LoopError::Cancelled)
}

#[cfg(test)]
mod tests {
    use kage_core::{CancelFlag, Content, Message, Role, TokenUsage};
    use kage_provider::{ProviderEvent, StopReason, testing::MockProvider};
    use kage_tools::ToolRegistry;

    use super::*;
    use crate::NoopHooks;

    fn user_msg(text: &str) -> Message {
        Message::new(
            Role::User,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        )
    }

    struct OneFollowup(bool);
    impl Hooks for OneFollowup {
        fn get_followup(&mut self) -> Option<String> {
            if self.0 {
                self.0 = false;
                Some("anything else?".into())
            } else {
                None
            }
        }
    }

    struct OneShotFollowup {
        text: Option<String>,
    }
    impl Hooks for OneShotFollowup {
        fn get_followup(&mut self) -> Option<String> {
            self.text.take()
        }
    }

    struct Steering(bool);
    impl Hooks for Steering {
        fn get_steering(&mut self) -> Option<String> {
            if self.0 {
                self.0 = false;
                Some("be terse".into())
            } else {
                None
            }
        }
    }

    #[derive(Default)]
    struct CountingSteering {
        polls: u32,
    }
    impl Hooks for CountingSteering {
        fn get_steering(&mut self) -> Option<String> {
            self.polls = self.polls.saturating_add(1);
            None
        }
    }

    struct ForeverFollowup;
    impl Hooks for ForeverFollowup {
        fn get_followup(&mut self) -> Option<String> {
            Some("again".into())
        }
    }

    #[test]
    fn shell_returns_after_provider_emits_text_only_turn() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hello"));
        let cfg = LoopConfig::default();
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let mut events = Vec::new();
        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |ev| {
            events.push(ev);
        });
        assert!(res.is_ok());
        assert_eq!(mock.call_count(), 1);
    }

    #[test]
    fn shell_re_enters_inner_loop_on_followup() {
        let mock = MockProvider::sequence(vec![
            vec![Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })],
            vec![Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })],
        ]);

        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = OneFollowup(true);
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
        assert!(res.is_ok());
        assert_eq!(mock.call_count(), 2, "follow-up should trigger second turn");
    }

    #[test]
    fn shell_emits_steering_message_before_first_turn() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);

        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = Steering(true);
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let _ = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
        let req = mock.last_request().unwrap();
        let last = req.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(matches!(
            &last.content[0],
            Content::Text { text } if text == "be terse"
        ));
    }

    #[test]
    fn shell_returns_cancelled_when_cancel_flagged_up_front() {
        let mock = MockProvider::replaying(vec![]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        cancel.cancel();
        let registry = ToolRegistry::new();

        let mut errors = Vec::new();
        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |ev| {
            if let LoopEvent::Error { kind } = ev {
                errors.push(kind);
            }
        });
        assert!(matches!(res, Err(LoopError::Cancelled)));
        assert_eq!(errors, vec![LoopError::Cancelled]);
    }

    #[test]
    fn shell_propagates_max_iterations() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);

        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig {
            max_iterations: 3,
            ..LoopConfig::default()
        };
        let mut hooks = ForeverFollowup;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
        assert!(matches!(res, Err(LoopError::Other { .. })));
    }

    #[derive(Debug)]
    struct StaticTool;

    impl kage_tools::Tool for StaticTool {
        fn name(&self) -> &'static str {
            "static"
        }
        fn description(&self) -> &'static str {
            "returns a fixed string"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk(&self) -> kage_core::Risk {
            kage_core::Risk::Read
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _cx: &kage_tools::ToolContext<'_>,
        ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
            Ok(kage_core::ToolOutput {
                is_error: false,
                text: "static-result".into(),
                structured: None,
            })
        }
    }

    #[test]
    fn followup_text_appears_in_next_provider_request() {
        let mock = MockProvider::sequence(vec![
            vec![Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })],
            vec![Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })],
        ]);

        let mut cx = AgentContext::new("mock:m", "").with_workdir("/tmp");
        cx.history.push(user_msg("first ask"));
        let cfg = LoopConfig::default();
        let mut hooks = OneShotFollowup {
            text: Some("now also do this".into()),
        };
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
        assert!(res.is_ok());
        assert_eq!(mock.call_count(), 2);

        // The second request must include the followup as the most recent user message.
        let req2 = mock.requests().into_iter().nth(1).unwrap();
        let last = req2.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(matches!(
            &last.content[0],
            Content::Text { text } if text == "now also do this"
        ));
    }

    #[test]
    fn steering_is_polled_before_every_inner_turn() {
        // Two-turn run: first turn returns a tool call, second turn ends.
        let call_id = kage_core::ToolCallId::new("call_x");
        let mock = MockProvider::sequence(vec![
            vec![
                Ok(ProviderEvent::ToolCallStart {
                    id: call_id.clone(),
                    name: "static".into(),
                }),
                Ok(ProviderEvent::ToolCallEnd {
                    id: call_id.clone(),
                    input: serde_json::json!({}),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                }),
            ],
            vec![Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })],
        ]);

        let mut cx = AgentContext::new("mock:m", "").with_workdir("/tmp");
        cx.history.push(user_msg("go"));
        let cfg = LoopConfig::default();
        let mut hooks = CountingSteering::default();
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new().with(std::sync::Arc::new(StaticTool));

        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
        assert!(res.is_ok(), "loop failed: {res:?}");
        assert_eq!(hooks.polls, 2, "steering polled once per inner-loop turn");
    }

    #[derive(Debug, Clone)]
    struct CancellingTool {
        cancel: kage_core::CancelFlag,
    }

    impl kage_tools::Tool for CancellingTool {
        fn name(&self) -> &'static str {
            "cancel_now"
        }
        fn description(&self) -> &'static str {
            "trips the loop's cancel flag mid-run"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk(&self) -> kage_core::Risk {
            kage_core::Risk::Read
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _cx: &kage_tools::ToolContext<'_>,
        ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
            self.cancel.cancel();
            Ok(kage_core::ToolOutput {
                is_error: false,
                text: "cancelled".into(),
                structured: None,
            })
        }
    }

    #[test]
    fn cancellation_triggered_inside_tool_terminates_run_before_next_turn() {
        let call_id = kage_core::ToolCallId::new("call_1");
        let mock = MockProvider::sequence(vec![vec![
            Ok(ProviderEvent::ToolCallStart {
                id: call_id.clone(),
                name: "cancel_now".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: call_id.clone(),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]]);

        let mut cx = AgentContext::new("mock:m", "").with_workdir("/tmp");
        cx.history.push(user_msg("go"));
        let cfg = LoopConfig::default();
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new().with(std::sync::Arc::new(CancellingTool {
            cancel: cancel.clone(),
        }));

        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
        assert!(matches!(res, Err(LoopError::Cancelled)));
        // First turn ran (provider called once); second never did.
        assert_eq!(mock.call_count(), 1);
    }

    #[test]
    fn end_to_end_tool_call_loop() {
        // Turn 1: model emits a tool call. Turn 2: model emits final text.
        let call_id = kage_core::ToolCallId::new("call_1");
        let mock = MockProvider::sequence(vec![
            vec![
                Ok(ProviderEvent::MessageStart),
                Ok(ProviderEvent::ToolCallStart {
                    id: call_id.clone(),
                    name: "static".into(),
                }),
                Ok(ProviderEvent::ToolCallEnd {
                    id: call_id.clone(),
                    input: serde_json::json!({}),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage {
                        input: 5,
                        output: 5,
                        cache_read: 0,
                        cache_write: 0,
                    },
                }),
            ],
            vec![
                Ok(ProviderEvent::TextDelta {
                    delta: "all done".into(),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input: 10,
                        output: 5,
                        cache_read: 0,
                        cache_write: 0,
                    },
                }),
            ],
        ]);

        let mut cx = AgentContext::new("mock:m", "").with_workdir("/tmp");
        cx.history.push(user_msg("do the thing"));
        let cfg = LoopConfig::default();
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new().with(std::sync::Arc::new(StaticTool));

        let mut events = Vec::new();
        let res = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |ev| {
            events.push(ev);
        });
        assert!(res.is_ok(), "loop failed: {res:?}");
        assert_eq!(mock.call_count(), 2);
        assert_eq!(cx.budget.total(), 25);
        // History: user, assistant(tool_call), tool_result, assistant(final).
        assert_eq!(cx.history.len(), 4);
        assert_eq!(cx.history[1].role, Role::Assistant);
        assert_eq!(cx.history[2].role, Role::ToolResult);
        assert_eq!(cx.history[3].role, Role::Assistant);
        match &cx.history[2].content[0] {
            Content::ToolResultBlock {
                output, is_error, ..
            } => {
                assert_eq!(output, "static-result");
                assert!(!is_error);
            }
            other => panic!("expected ToolResultBlock, got {other:?}"),
        }
        // Provider request 2 should include the tool result in messages.
        let req2 = mock.requests().into_iter().nth(1).unwrap();
        assert_eq!(req2.messages.len(), 3);
        assert_eq!(req2.messages[2].role, Role::ToolResult);
        // Provider request should advertise registered tool.
        let req1 = mock.requests().into_iter().next().unwrap();
        assert_eq!(req1.tools.len(), 1);
        assert_eq!(req1.tools[0].name, "static");
    }
}
