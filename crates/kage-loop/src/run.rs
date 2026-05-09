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
use kage_provider::{Provider, StopReason, StreamRequest};
use kage_tools::ToolRegistry;

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
    let _ = tools;
    let mut iterations: u32 = 0;

    loop {
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

        // T4.4 turns the `panic!` below into a `continue`, at which point the
        // inner loop genuinely loops. Until then, clippy correctly observes
        // it never re-iterates; allow the lint as a known-temporary.
        #[allow(clippy::never_loop)]
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

            let req = build_request(cx);
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
            let had_tool_calls = !turn.tool_calls.is_empty();
            cx.history.push(turn.message);

            // T4.4 replaces the panic with real dispatch + `continue`.
            assert!(
                !(had_tool_calls || turn.stop_reason == StopReason::ToolUse),
                "T4.3 shell does not yet dispatch tool calls; T4.4 fills this in",
            );
            break;
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
///
/// Tools list is omitted in the T4.3 shell; T4.4 plugs in
/// `tools.list_for_provider()`.
fn build_request(cx: &AgentContext) -> StreamRequest {
    let mut req = StreamRequest::new(&cx.model, cx.history.clone());
    if !cx.system_prompt.is_empty() {
        req.system = Some(cx.system_prompt.clone());
    }
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

    #[test]
    #[should_panic(expected = "T4.3 shell does not yet dispatch tool calls")]
    fn shell_panics_on_tool_call_per_t43_contract() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: kage_core::ToolCallId::new("call_1"),
                name: "read".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();
        let _ = run(&mock, &registry, &mut cx, &cfg, &mut hooks, &cancel, |_| {});
    }
}
