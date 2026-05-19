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

use std::time::Duration;

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId};
use kage_provider::{Provider, ProviderError, StreamRequest};
use kage_tools::ToolRegistry;

use crate::compact::maybe_compact;
use crate::dispatch::{dispatch_tool_calls, dispatch_tool_calls_parallel};
use crate::doom::DoomTracker;
use crate::stream::{TurnFailure, TurnResult, collect_turn};
use crate::{AgentContext, Hooks, LoopConfig, SteeringMode};

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
#[allow(clippy::too_many_lines)]
pub fn run<F>(
    provider: &dyn Provider,
    tools: &ToolRegistry,
    cx: &mut AgentContext,
    config: LoopConfig,
    hooks: &mut dyn Hooks,
    cancel: &CancelFlag,
    mut emit: F,
) -> Result<(), LoopError>
where
    F: FnMut(LoopEvent),
{
    let mut doom = DoomTracker::default();
    let mut turn_index: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            return finish_cancelled(hooks, &mut emit);
        }

        loop {
            if cancel.is_cancelled() {
                return finish_cancelled(hooks, &mut emit);
            }

            if let Some(text) = drain_messages(config.steering_mode, || hooks.get_steering()) {
                cx.history.push(kage_core::Message::new(
                    kage_core::Role::User,
                    vec![kage_core::Content::Text { text }],
                    cx.history.last().map(|m| m.id),
                ));
            }

            if let Err(kind) = maybe_compact(cx, config, provider, cancel, hooks, &mut emit) {
                emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            hooks.on_turn_start(turn_index);

            if let Err(message) = hooks.transform_context(&mut cx.history) {
                let kind = LoopError::HookFailed {
                    hook: "transform_context".to_owned(),
                    message,
                };
                emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            let mut req = build_request(cx, tools);
            if let Err(message) = hooks.transform_provider_request(&mut req) {
                let kind = LoopError::HookFailed {
                    hook: "transform_provider_request".to_owned(),
                    message,
                };
                emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }
            let parent = cx.history.last().map(|m| m.id);
            // Auto-retry a transiently-failed turn. The conversation
            // context is unchanged across attempts - no assistant
            // message is appended on failure - so re-issuing the
            // identical request is a clean re-request, not a resume
            // (SSE has no resume token). The Notice emitted between
            // attempts breaks the live-assistant block, so the
            // retry's text starts a fresh block in the UI instead of
            // concatenating onto the dropped partial; the recording
            // hook likewise resets on the retry's MessageStart, so
            // only the successful turn is persisted.
            let turn = {
                let mut attempt: u32 = 0;
                loop {
                    if cancel.is_cancelled() {
                        return finish_cancelled(hooks, &mut emit);
                    }
                    match stream_one_attempt(
                        provider,
                        req.clone(),
                        parent,
                        cancel,
                        hooks,
                        &mut emit,
                    ) {
                        Ok(t) => break t,
                        Err(TurnFailure::Fatal(kind)) => {
                            emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                            return Err(kind);
                        }
                        Err(TurnFailure::Provider(e)) => {
                            let exhausted = attempt >= config.max_provider_retries;
                            if exhausted || !e.is_transient() || cancel.is_cancelled() {
                                let kind = LoopError::Provider {
                                    message: e.to_string(),
                                };
                                emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                                return Err(kind);
                            }
                            attempt += 1;
                            let wait = retry_backoff(attempt, &e);
                            emit_one(
                                hooks,
                                &mut emit,
                                LoopEvent::Notice {
                                    message: format!(
                                        "provider error ({e}); retrying {attempt}/{} in {}s",
                                        config.max_provider_retries,
                                        wait.as_secs().max(1)
                                    ),
                                },
                            );
                            if !sleep_cancelable(cancel, wait) {
                                return finish_cancelled(hooks, &mut emit);
                            }
                        }
                    }
                }
            };

            cx.budget.add(turn.usage);
            let turn_usage = turn.usage;
            let assistant_id = turn.message.id;
            let pending = turn.tool_calls.clone();
            cx.history.push(turn.message);

            let had_tool_calls = !pending.is_empty();
            hooks.on_turn_end(turn_index, had_tool_calls);
            let summary = crate::hooks::TurnSummary {
                index: turn_index,
                had_tool_calls,
                usage: turn_usage,
            };
            turn_index = turn_index.saturating_add(1);
            if hooks.should_stop_after_turn(&summary) {
                return Ok(());
            }

            if !had_tool_calls {
                break;
            }

            let workdir = cx.workdir.clone();
            // Parallel dispatch only when the loop is configured for it AND
            // no tool in the batch overrides to Sequential. Any sequential
            // tool (e.g. `bash`) forces the whole batch to single-thread
            // execution so it cannot race with the others.
            let any_sequential = pending.iter().any(|call| {
                tools.get(&call.name).is_some_and(|t| {
                    matches!(t.execution_mode(), Some(kage_tools::ExecMode::Sequential))
                })
            });
            let dispatch = if config.parallel_tools && !any_sequential {
                dispatch_tool_calls_parallel
            } else {
                dispatch_tool_calls
            };
            let outcome = match dispatch(
                pending.clone(),
                tools,
                &workdir,
                cancel,
                assistant_id,
                hooks,
                &mut emit,
            ) {
                Ok(o) => o,
                Err(kind) => {
                    emit_one(hooks, &mut emit, LoopEvent::Error { kind: kind.clone() });
                    return Err(kind);
                }
            };
            let results = outcome.results;
            // If every tool in the batch signaled `terminate`, persist the
            // results and exit the run cleanly. The loop never asks the
            // model for another turn, never dequeues a follow-up.
            if outcome.all_terminate {
                cx.history.extend(results);
                return Ok(());
            }

            let mut steering = None;
            for (call, result) in pending.iter().zip(&results) {
                let is_error = matches!(
                    result.content.first(),
                    Some(kage_core::Content::ToolResultBlock { is_error: true, .. })
                );
                if let Some(msg) = doom.observe(&call.name, &call.input, is_error) {
                    steering = Some(msg);
                }
            }
            cx.history.extend(results);
            if let Some(text) = steering {
                cx.history.push(kage_core::Message::new(
                    kage_core::Role::User,
                    vec![kage_core::Content::Text { text }],
                    cx.history.last().map(|m| m.id),
                ));
            }
        }

        let Some(text) = drain_messages(config.followup_mode, || hooks.get_followup()) else {
            return Ok(());
        };
        cx.history.push(kage_core::Message::new(
            kage_core::Role::User,
            vec![kage_core::Content::Text { text }],
            cx.history.last().map(|m| m.id),
        ));
    }
}

/// Drain queued messages from a hook poll according to `mode`. In
/// `OneAtATime`, polls once and returns whatever the hook gave us. In
/// `All`, polls repeatedly until the hook returns `None`, then joins the
/// collected messages with blank-line separators.
///
/// Returns `None` when the hook had nothing to give on the first poll.
fn drain_messages<F: FnMut() -> Option<String>>(mode: SteeringMode, mut poll: F) -> Option<String> {
    let first = poll()?;
    if mode == SteeringMode::OneAtATime {
        return Some(first);
    }
    let mut out = first;
    while let Some(next) = poll() {
        out.push_str("\n\n");
        out.push_str(&next);
    }
    Some(out)
}

/// Emit one event to both the host's `Hooks::on_event` and the user emit
/// callback, in that order.
pub(crate) fn emit_one<F: FnMut(LoopEvent)>(hooks: &mut dyn Hooks, emit: &mut F, event: LoopEvent) {
    hooks.on_event(&event);
    emit(event);
}

/// Rewrite persisted `Content::Thinking` blocks into inline
/// `<thinking>...</thinking>` text before a request is built.
///
/// Thinking blocks are not portable across a request boundary. kage
/// never persists the cryptographic signature Anthropic requires to
/// replay a native thinking block, so sending one back is rejected by
/// that API; `OpenAI` and Gemini drop unknown content silently, losing
/// the reasoning chain outright. Switching models mid-session makes
/// both failure modes worse. Flattening historical thinking to plain
/// text keeps the reasoning visible to whatever provider runs the
/// next turn, regardless of which produced it.
///
/// Only persisted history is touched. The in-flight assistant turn is
/// not appended to `cx.history` until after it has streamed, so live
/// thinking deltas reach the UI unmodified.
fn flatten_thinking(history: &[Message]) -> Vec<Message> {
    history
        .iter()
        .map(|msg| {
            if !msg
                .content
                .iter()
                .any(|c| matches!(c, Content::Thinking { .. }))
            {
                return msg.clone();
            }
            // Build the rewritten content directly instead of
            // `msg.clone()` then overwriting `.content`: the other
            // fields are all `Copy`, so `..*msg` copies them and the
            // original content vec is never cloned just to be dropped.
            let content = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Thinking { text } if text.trim().is_empty() => None,
                    Content::Thinking { text } => Some(Content::Text {
                        text: format!("<thinking>\n{text}\n</thinking>"),
                    }),
                    other => Some(other.clone()),
                })
                .collect();
            Message { content, ..*msg }
        })
        .collect()
}

/// Construct the next [`StreamRequest`] from the current agent context.
fn build_request(cx: &AgentContext, tools: &ToolRegistry) -> StreamRequest {
    let mut req = StreamRequest::new(&cx.model, flatten_thinking(&cx.history));
    if !cx.system_prompt.is_empty() {
        req.system = Some(cx.system_prompt.clone());
    }
    req.tools = tools.list_for_provider();
    req.max_output_tokens = cx.max_output_tokens;
    req.level = cx.thinking_level;
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

/// Issue one streaming attempt: open the provider stream and drain it
/// into a finished turn. A pre-stream failure and a mid-stream failure
/// are unified into the same [`TurnFailure`] so the caller's retry
/// logic does not care which phase broke.
fn stream_one_attempt<F: FnMut(LoopEvent)>(
    provider: &dyn Provider,
    req: StreamRequest,
    parent: Option<MessageId>,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<TurnResult, TurnFailure> {
    let stream = provider
        .stream(req, cancel)
        .map_err(TurnFailure::Provider)?;
    collect_turn(parent, stream, cancel, hooks, emit)
}

/// Backoff before retry `attempt` (1-based). A provider `retry_after`
/// hint wins (capped at 60s so a hostile header cannot park the loop);
/// otherwise exponential 1s, 2s, 4s, 8s, 16s capped at 30s.
fn retry_backoff(attempt: u32, err: &ProviderError) -> Duration {
    if let Some(hint) = err.retry_after() {
        return hint.min(Duration::from_secs(60));
    }
    let secs = 1u64 << attempt.saturating_sub(1).min(5);
    Duration::from_secs(secs.min(30))
}

/// Sleep `dur`, returning `false` if `cancel` tripped during the wait.
/// Checked in short slices so a cancel aborts the backoff promptly
/// instead of after the full delay.
fn sleep_cancelable(cancel: &CancelFlag, dur: Duration) -> bool {
    let slice = Duration::from_millis(100);
    let mut left = dur;
    while !left.is_zero() {
        if cancel.is_cancelled() {
            return false;
        }
        let step = slice.min(left);
        std::thread::sleep(step);
        left -= step;
    }
    !cancel.is_cancelled()
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

    #[test]
    fn build_request_forwards_max_output_tokens_from_context() {
        let mut cx = AgentContext::new("m", "");
        cx.history.push(user_msg("hi"));
        cx = cx.with_max_output_tokens(32_000);
        let tools = ToolRegistry::new();
        let req = build_request(&cx, &tools);
        assert_eq!(req.max_output_tokens, Some(32_000));
    }

    #[test]
    fn build_request_leaves_max_output_tokens_unset_when_context_default() {
        let mut cx = AgentContext::new("m", "");
        cx.history.push(user_msg("hi"));
        let tools = ToolRegistry::new();
        let req = build_request(&cx, &tools);
        assert!(req.max_output_tokens.is_none());
    }

    fn assistant_msg(content: Vec<Content>) -> Message {
        Message::new(Role::Assistant, content, None)
    }

    #[test]
    fn flatten_thinking_rewrites_thinking_to_tagged_text() {
        let history = vec![assistant_msg(vec![
            Content::Thinking {
                text: "weigh the options".to_owned(),
            },
            Content::Text {
                text: "the answer is 42".to_owned(),
            },
        ])];
        let flat = flatten_thinking(&history);
        assert_eq!(
            flat[0].content,
            vec![
                Content::Text {
                    text: "<thinking>\nweigh the options\n</thinking>".to_owned(),
                },
                Content::Text {
                    text: "the answer is 42".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn flatten_thinking_drops_empty_thinking_blocks() {
        let history = vec![assistant_msg(vec![
            Content::Thinking {
                text: "   ".to_owned(),
            },
            Content::Text {
                text: "done".to_owned(),
            },
        ])];
        let flat = flatten_thinking(&history);
        assert_eq!(
            flat[0].content,
            vec![Content::Text {
                text: "done".to_owned(),
            }]
        );
    }

    #[test]
    fn flatten_thinking_leaves_non_thinking_messages_unchanged() {
        let history = vec![
            user_msg("hi"),
            assistant_msg(vec![Content::Text {
                text: "hello".to_owned(),
            }]),
        ];
        assert_eq!(flatten_thinking(&history), history);
    }

    #[test]
    fn build_request_flattens_history_thinking() {
        let mut cx = AgentContext::new("m", "");
        cx.history.push(user_msg("question"));
        cx.history.push(assistant_msg(vec![Content::Thinking {
            text: "reason".to_owned(),
        }]));
        let tools = ToolRegistry::new();
        let req = build_request(&cx, &tools);
        assert!(
            !req.messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|c| matches!(c, Content::Thinking { .. })),
            "no native thinking blocks should survive into the request"
        );
        assert!(req.messages[1].content.iter().any(|c| matches!(
            c,
            Content::Text { text } if text.contains("<thinking>") && text.contains("reason")
        )));
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
        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
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

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
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

        let _ = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        let req = mock.last_request().unwrap();
        let last = req.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(matches!(
            &last.content[0],
            Content::Text { text } if text == "be terse"
        ));
    }

    #[derive(Default)]
    struct TurnRecording {
        starts: Vec<u32>,
        ends: Vec<(u32, bool)>,
    }

    impl Hooks for TurnRecording {
        fn on_turn_start(&mut self, index: u32) {
            self.starts.push(index);
        }
        fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
            self.ends.push((index, had_tool_calls));
        }
    }

    #[test]
    fn turn_boundaries_fire_once_for_text_only_turn() {
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
        let mut hooks = TurnRecording::default();
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(hooks.starts, vec![0]);
        assert_eq!(hooks.ends, vec![(0, false)]);
    }

    struct Combined {
        inner: TurnRecording,
        follow: OneFollowup,
    }
    impl Hooks for Combined {
        fn on_turn_start(&mut self, index: u32) {
            self.inner.on_turn_start(index);
        }
        fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
            self.inner.on_turn_end(index, had_tool_calls);
        }
        fn get_followup(&mut self) -> Option<String> {
            self.follow.get_followup()
        }
    }

    #[test]
    fn turn_index_advances_across_followup_rounds() {
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
        let mut hooks = Combined {
            inner: TurnRecording::default(),
            follow: OneFollowup(true),
        };
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(hooks.inner.starts, vec![0, 1]);
        assert_eq!(hooks.inner.ends, vec![(0, false), (1, false)]);
    }

    struct StaticTransform {
        injection: String,
        calls: u32,
    }
    impl Hooks for StaticTransform {
        fn transform_context(
            &mut self,
            messages: &mut Vec<kage_core::Message>,
        ) -> Result<(), String> {
            self.calls = self.calls.saturating_add(1);
            messages.push(kage_core::Message::new(
                kage_core::Role::User,
                vec![Content::Text {
                    text: self.injection.clone(),
                }],
                messages.last().map(|m| m.id),
            ));
            Ok(())
        }
    }

    #[test]
    fn transform_context_runs_before_each_provider_call() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hello"));
        let cfg = LoopConfig::default();
        let mut hooks = StaticTransform {
            injection: "from-hook".into(),
            calls: 0,
        };
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(hooks.calls, 1);
        let req = mock.last_request().unwrap();
        let last = req.messages.last().unwrap();
        assert!(matches!(
            &last.content[0],
            Content::Text { text } if text == "from-hook"
        ));
    }

    struct FailingTransform;
    impl Hooks for FailingTransform {
        fn transform_context(
            &mut self,
            _messages: &mut Vec<kage_core::Message>,
        ) -> Result<(), String> {
            Err("transform exploded".into())
        }
    }

    #[test]
    fn transform_context_error_aborts_with_hook_failed() {
        let mock = MockProvider::replaying(vec![]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = FailingTransform;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let mut errors = Vec::new();
        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
            if let LoopEvent::Error { kind } = ev {
                errors.push(kind);
            }
        });
        match res {
            Err(LoopError::HookFailed { hook, message }) => {
                assert_eq!(hook, "transform_context");
                assert_eq!(message, "transform exploded");
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
        assert_eq!(errors.len(), 1);
        assert_eq!(mock.call_count(), 0, "provider never called on hook error");
    }

    struct StopAfterFirstTurn {
        polls: u32,
    }
    impl Hooks for StopAfterFirstTurn {
        fn should_stop_after_turn(&mut self, _summary: &crate::TurnSummary) -> bool {
            self.polls = self.polls.saturating_add(1);
            true
        }
        fn get_followup(&mut self) -> Option<String> {
            Some("must-not-be-asked".into())
        }
    }

    #[test]
    fn should_stop_after_turn_suppresses_followup_and_returns_ok() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = StopAfterFirstTurn { polls: 0 };
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(hooks.polls, 1, "predicate fired exactly once");
        assert_eq!(
            mock.call_count(),
            1,
            "followup never dequeued, no second turn"
        );
    }

    #[derive(Debug)]
    struct AlwaysCallTool;

    impl kage_tools::Tool for AlwaysCallTool {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn description(&self) -> &'static str {
            "no-op tool"
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
            _cx: &kage_tools::ToolContext,
        ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
            Ok(kage_core::ToolOutput {
                is_error: false,
                text: "ran".into(),
                structured: None,
                terminate: false,
            })
        }
    }

    #[test]
    fn should_stop_after_turn_short_circuits_pending_tool_calls() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: kage_core::ToolCallId::new("call_1"),
                name: "noop".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: kage_core::ToolCallId::new("call_1"),
                partial: "{}".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: kage_core::ToolCallId::new("call_1"),
                input: serde_json::json!({}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("plan it"));
        let cfg = LoopConfig::default();
        let mut hooks = StopAfterFirstTurn { polls: 0 };
        let cancel = CancelFlag::new();
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(AlwaysCallTool));

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(hooks.polls, 1);
        assert_eq!(mock.call_count(), 1);
        let saw_tool_result = cx.history.iter().any(|m| {
            m.content
                .iter()
                .any(|c| matches!(c, kage_core::Content::ToolResultBlock { .. }))
        });
        assert!(
            !saw_tool_result,
            "stop predicate must run before tool dispatch"
        );
    }

    struct QueuedSteering(std::collections::VecDeque<String>);
    impl Hooks for QueuedSteering {
        fn get_steering(&mut self) -> Option<String> {
            self.0.pop_front()
        }
    }

    struct CombinedSteering {
        queued: QueuedSteering,
        followup: OneFollowup,
    }
    impl Hooks for CombinedSteering {
        fn get_steering(&mut self) -> Option<String> {
            self.queued.get_steering()
        }
        fn get_followup(&mut self) -> Option<String> {
            self.followup.get_followup()
        }
    }

    #[test]
    fn steering_one_at_a_time_drains_one_per_turn() {
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
        let mut hooks = CombinedSteering {
            queued: QueuedSteering(["one".into(), "two".into()].into_iter().collect()),
            followup: OneFollowup(true),
        };
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        let req1 = mock.last_request().unwrap();
        let texts: Vec<&str> = req1
            .messages
            .iter()
            .filter_map(|m| match m.content.first() {
                Some(Content::Text { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"one"));
        assert!(texts.contains(&"two"));
    }

    #[test]
    fn steering_all_mode_concatenates_in_one_turn() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);

        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig {
            steering_mode: SteeringMode::All,
            ..LoopConfig::default()
        };
        let mut hooks = QueuedSteering(["one".into(), "two".into(), "three".into()].into());
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        let req = mock.last_request().unwrap();
        let merged = req
            .messages
            .iter()
            .filter_map(|m| match m.content.first() {
                Some(Content::Text { text }) => Some(text.as_str()),
                _ => None,
            })
            .find(|t| t.contains("one") && t.contains("two") && t.contains("three"))
            .expect("one merged user message with all three texts");
        assert_eq!(merged, "one\n\ntwo\n\nthree");
    }

    struct RequestSpy {
        seen_model: Option<String>,
        rewrite_system_to: Option<String>,
    }
    impl Hooks for RequestSpy {
        fn transform_provider_request(
            &mut self,
            req: &mut kage_provider::StreamRequest,
        ) -> Result<(), String> {
            self.seen_model = Some(req.model.clone());
            if let Some(s) = &self.rewrite_system_to {
                req.system = Some(s.clone());
            }
            Ok(())
        }
    }

    #[test]
    fn transform_provider_request_observes_and_can_rewrite() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hi"));
        let cfg = LoopConfig::default();
        let mut hooks = RequestSpy {
            seen_model: None,
            rewrite_system_to: Some("rewritten".into()),
        };
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(hooks.seen_model.as_deref(), Some("mock:m"));
        let req = mock.last_request().unwrap();
        assert_eq!(req.system.as_deref(), Some("rewritten"));
    }

    #[derive(Debug)]
    struct SleepTool {
        millis: u64,
    }

    impl kage_tools::Tool for SleepTool {
        fn name(&self) -> &'static str {
            "sleep"
        }
        fn description(&self) -> &'static str {
            "sleeps"
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
            std::thread::sleep(std::time::Duration::from_millis(self.millis));
            Ok(kage_core::ToolOutput {
                is_error: false,
                text: "ok".into(),
                structured: None,
                terminate: false,
            })
        }
    }

    #[derive(Debug)]
    struct SeqSleepTool;

    impl kage_tools::Tool for SeqSleepTool {
        fn name(&self) -> &'static str {
            "seq_sleep"
        }
        fn description(&self) -> &'static str {
            "sleeps and requires sequential dispatch"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk(&self) -> kage_core::Risk {
            kage_core::Risk::Exec
        }
        fn execution_mode(&self) -> Option<kage_tools::ExecMode> {
            Some(kage_tools::ExecMode::Sequential)
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _cx: &kage_tools::ToolContext<'_>,
        ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(kage_core::ToolOutput {
                is_error: false,
                text: "ok".into(),
                structured: None,
                terminate: false,
            })
        }
    }

    fn three_tool_call_turn() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
        let mut events = vec![Ok(ProviderEvent::MessageStart)];
        for i in 0..3 {
            let id = kage_core::ToolCallId::new(format!("call_{i}"));
            events.push(Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: if i == 0 {
                    "seq_sleep".into()
                } else {
                    "sleep".into()
                },
            }));
            events.push(Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: "{}".into(),
            }));
            events.push(Ok(ProviderEvent::ToolCallEnd {
                id,
                input: serde_json::json!({}),
            }));
        }
        events.push(Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }));
        events
    }

    #[test]
    fn sequential_tool_in_batch_downgrades_parallel_dispatch() {
        let mock = MockProvider::sequence(vec![
            three_tool_call_turn(),
            vec![Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })],
        ]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("go"));
        let cfg = LoopConfig {
            parallel_tools: true,
            ..LoopConfig::default()
        };
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(SeqSleepTool));
        registry.register(std::sync::Arc::new(SleepTool { millis: 100 }));

        let start = std::time::Instant::now();
        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        let elapsed = start.elapsed();
        // Three 100ms tools serialized take ~300ms+; parallel would take ~100ms.
        assert!(
            elapsed.as_millis() >= 250,
            "expected sequential fallback, elapsed {}ms",
            elapsed.as_millis(),
        );
    }

    #[derive(Debug)]
    struct TaskDoneTool;

    impl kage_tools::Tool for TaskDoneTool {
        fn name(&self) -> &'static str {
            "task_done"
        }
        fn description(&self) -> &'static str {
            "signals successful completion"
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
                text: "done".into(),
                structured: None,
                terminate: true,
            })
        }
    }

    struct AlwaysFollowup;
    impl Hooks for AlwaysFollowup {
        fn get_followup(&mut self) -> Option<String> {
            Some("ignored".into())
        }
    }

    #[test]
    fn terminate_flag_short_circuits_run_with_no_followup() {
        let mock = MockProvider::sequence(vec![
            vec![
                Ok(ProviderEvent::MessageStart),
                Ok(ProviderEvent::ToolCallStart {
                    id: kage_core::ToolCallId::new("call_done"),
                    name: "task_done".into(),
                }),
                Ok(ProviderEvent::ToolCallArgsDelta {
                    id: kage_core::ToolCallId::new("call_done"),
                    partial: "{}".into(),
                }),
                Ok(ProviderEvent::ToolCallEnd {
                    id: kage_core::ToolCallId::new("call_done"),
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
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("do it"));
        let cfg = LoopConfig::default();
        let mut hooks = AlwaysFollowup;
        let cancel = CancelFlag::new();
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(TaskDoneTool));

        run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {}).unwrap();
        assert_eq!(
            mock.call_count(),
            1,
            "followup never dequeued after terminate"
        );
        let last = cx.history.last().unwrap();
        assert_eq!(last.role, kage_core::Role::ToolResult);
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
        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
            if let LoopEvent::Error { kind } = ev {
                errors.push(kind);
            }
        });
        assert!(matches!(res, Err(LoopError::Cancelled)));
        assert_eq!(errors, vec![LoopError::Cancelled]);
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
                terminate: false,
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

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
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

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
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
                terminate: false,
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

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(matches!(res, Err(LoopError::Cancelled)));
        // First turn ran (provider called once); second never did.
        assert_eq!(mock.call_count(), 1);
    }

    #[derive(Debug)]
    struct AlwaysFailTool;

    impl kage_tools::Tool for AlwaysFailTool {
        fn name(&self) -> &'static str {
            "always_fail"
        }
        fn description(&self) -> &'static str {
            "always returns is_error=true"
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
                is_error: true,
                text: "boom".into(),
                structured: None,
                terminate: false,
            })
        }
    }

    #[test]
    fn doom_loop_steers_after_three_repeat_failures() {
        // Four turns: each emits the same failing tool call. After the third
        // dispatch, the doom tracker injects steering before the fourth
        // provider call.
        let mut counter = 0u32;
        let mut make_turn = || {
            counter += 1;
            let id = kage_core::ToolCallId::new(format!("call_{counter}"));
            vec![
                Ok(ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: "always_fail".into(),
                }),
                Ok(ProviderEvent::ToolCallEnd {
                    id,
                    input: serde_json::json!({"x": 1}),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                }),
            ]
        };
        let mock = MockProvider::sequence(vec![
            make_turn(),
            make_turn(),
            make_turn(),
            // Fourth turn ends the loop without another tool call.
            vec![
                Ok(ProviderEvent::TextDelta {
                    delta: "ok stopping".into(),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                }),
            ],
        ]);

        let mut cx = AgentContext::new("mock:m", "").with_workdir("/tmp");
        cx.history.push(user_msg("try the thing"));
        let cfg = LoopConfig::default();
        let mut hooks = NoopHooks;
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new().with(std::sync::Arc::new(AlwaysFailTool));

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(res.is_ok(), "loop failed: {res:?}");

        // The fourth provider request should have a steering user message in
        // its tail (after the last tool result).
        let req4 = mock.requests().into_iter().nth(3).unwrap();
        let tail = req4.messages.last().unwrap();
        assert_eq!(tail.role, Role::User);
        match &tail.content[0] {
            Content::Text { text } => {
                assert!(text.contains("'always_fail'"));
                assert!(text.contains("different approach"));
            }
            other => panic!("expected steering Text, got {other:?}"),
        }
    }

    #[derive(Debug, Default)]
    struct CountingTool {
        calls: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    impl kage_tools::Tool for CountingTool {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn description(&self) -> &'static str {
            "records every input it receives"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk(&self) -> kage_core::Risk {
            kage_core::Risk::Read
        }
        fn execute(
            &self,
            input: serde_json::Value,
            _cx: &kage_tools::ToolContext<'_>,
        ) -> Result<kage_core::ToolOutput, kage_tools::ToolError> {
            self.calls.lock().expect("not poisoned").push(input.clone());
            Ok(kage_core::ToolOutput {
                is_error: false,
                text: format!("ran #{}", self.calls.lock().expect("not poisoned").len()),
                structured: None,
                terminate: false,
            })
        }
    }

    #[derive(Default)]
    struct OrderRecording {
        order: Vec<String>,
    }
    impl Hooks for OrderRecording {
        fn before_tool_call(
            &mut self,
            name: &str,
            _input: &serde_json::Value,
        ) -> Option<kage_core::ToolOutput> {
            self.order.push(format!("before:{name}"));
            None
        }
        fn after_tool_call(
            &mut self,
            name: &str,
            output: kage_core::ToolOutput,
        ) -> kage_core::ToolOutput {
            self.order.push(format!("after:{name}"));
            output
        }
        fn on_event(&mut self, event: &LoopEvent) {
            let tag = serde_json::to_value(event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned();
            self.order.push(format!("event:{tag}"));
        }
    }

    #[test]
    fn end_to_end_event_ordering_and_hook_callbacks() {
        let call_id = kage_core::ToolCallId::new("call_e2e");
        let mock = MockProvider::sequence(vec![
            vec![
                Ok(ProviderEvent::MessageStart),
                Ok(ProviderEvent::TextDelta {
                    delta: "calling tool".into(),
                }),
                Ok(ProviderEvent::ToolCallStart {
                    id: call_id.clone(),
                    name: "counting".into(),
                }),
                Ok(ProviderEvent::ToolCallEnd {
                    id: call_id.clone(),
                    input: serde_json::json!({"k": "v"}),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                }),
            ],
            vec![
                Ok(ProviderEvent::TextDelta {
                    delta: "done".into(),
                }),
                Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                }),
            ],
        ]);

        let counting = std::sync::Arc::new(CountingTool::default());
        let registry = ToolRegistry::new().with(counting.clone());

        let mut cx = AgentContext::new("mock:m", "be helpful").with_workdir("/tmp");
        cx.history.push(user_msg("kick off"));
        let cfg = LoopConfig::default();
        let mut hooks = OrderRecording::default();
        let cancel = CancelFlag::new();

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(res.is_ok());

        // Tool ran exactly once with the expected input.
        let recorded = counting.calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0]["k"], "v");

        // History: user, assistant(text+tool_call), tool_result, assistant(text).
        assert_eq!(cx.history.len(), 4);
        assert_eq!(cx.history[0].role, Role::User);
        assert_eq!(cx.history[1].role, Role::Assistant);
        assert_eq!(cx.history[2].role, Role::ToolResult);
        assert_eq!(cx.history[3].role, Role::Assistant);

        // Hook ordering across the whole run.
        let order: Vec<&str> = hooks.order.iter().map(String::as_str).collect();
        let before_pos = order
            .iter()
            .position(|s| *s == "before:counting")
            .expect("before fired");
        let after_pos = order
            .iter()
            .position(|s| *s == "after:counting")
            .expect("after fired");
        assert!(before_pos < after_pos, "before must precede after");

        // Event order: MessageStart, TextDelta, ToolCallArgsDelta (the
        // early UI hint at provider tool-call start), ToolCallStart
        // (authoritative, at provider tool-call end), MessageEnd
        // (turn 1), ToolCallEnd, TextDelta (turn 2), MessageEnd.
        let event_seq: Vec<&str> = order
            .iter()
            .filter_map(|s| s.strip_prefix("event:"))
            .collect();
        assert!(event_seq.starts_with(&[
            "message_start",
            "text_delta",
            "tool_call_args_delta",
            "tool_call_start",
        ]));
        assert!(event_seq.contains(&"tool_call_end"));
        assert!(event_seq.last() == Some(&"message_end"));
        // The before/after hook must bracket the tool_call_end event.
        let tcend = order
            .iter()
            .position(|s| *s == "event:tool_call_end")
            .unwrap();
        assert!(before_pos < tcend && tcend == after_pos + 1);
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
        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |ev| {
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

    #[derive(Default)]
    struct EventLog {
        events: Vec<LoopEvent>,
        cancel_on_notice: Option<CancelFlag>,
    }
    impl Hooks for EventLog {
        fn on_event(&mut self, event: &LoopEvent) {
            if let LoopEvent::Notice { .. } = event
                && let Some(c) = &self.cancel_on_notice
            {
                c.cancel();
            }
            self.events.push(event.clone());
        }
    }
    impl EventLog {
        fn notices(&self) -> usize {
            self.events
                .iter()
                .filter(|e| matches!(e, LoopEvent::Notice { .. }))
                .count()
        }
    }

    fn transient_turn() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
        vec![Err(kage_provider::ProviderError::Transport("boom".into()))]
    }

    fn good_turn() -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]
    }

    #[test]
    fn transient_provider_failure_is_retried_then_succeeds() {
        let mock = MockProvider::sequence(vec![transient_turn(), good_turn()]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hello"));
        let cfg = LoopConfig::default();
        let mut hooks = EventLog::default();
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(res.is_ok(), "run should recover, got {res:?}");
        assert_eq!(mock.call_count(), 2, "one retry after the transient fail");
        assert_eq!(hooks.notices(), 1, "exactly one retry notice");
        assert!(cx.history.iter().any(|m| m.role == Role::Assistant));
    }

    #[test]
    fn non_transient_failure_is_not_retried() {
        let mock = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
            "no key".into(),
        ))]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hello"));
        let cfg = LoopConfig::default();
        let mut hooks = EventLog::default();
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(matches!(res, Err(LoopError::Provider { .. })));
        assert_eq!(mock.call_count(), 1, "auth error must not retry");
        assert_eq!(hooks.notices(), 0);
    }

    #[test]
    fn retries_are_bounded_then_surface_the_error() {
        let mock = MockProvider::sequence(vec![transient_turn(), transient_turn()]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hello"));
        let cfg = LoopConfig {
            max_provider_retries: 1,
            ..LoopConfig::default()
        };
        let mut hooks = EventLog::default();
        let cancel = CancelFlag::new();
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(matches!(res, Err(LoopError::Provider { .. })));
        assert_eq!(mock.call_count(), 2, "initial attempt + one bounded retry");
        assert_eq!(hooks.notices(), 1);
    }

    #[test]
    fn cancel_during_backoff_aborts_cleanly() {
        let mock = MockProvider::sequence(vec![transient_turn(), good_turn()]);
        let mut cx = AgentContext::new("mock:m", "");
        cx.history.push(user_msg("hello"));
        let cfg = LoopConfig::default();
        let cancel = CancelFlag::new();
        let mut hooks = EventLog {
            events: Vec::new(),
            cancel_on_notice: Some(cancel.clone()),
        };
        let registry = ToolRegistry::new();

        let res = run(&mock, &registry, &mut cx, cfg, &mut hooks, &cancel, |_| {});
        assert!(matches!(res, Err(LoopError::Cancelled)));
        assert_eq!(mock.call_count(), 1, "retry never issued after cancel");
    }
}
