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
                let msg = kage_core::Message::new(
                    kage_core::Role::User,
                    vec![kage_core::Content::Text { text }],
                    cx.history.last().map(|m| m.id),
                );
                hooks.on_user_message(&msg);
                cx.history.push(msg);
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

            let mut req = build_request(cx, tools, provider);
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
                            let requested = e.retry_after();
                            let wait = retry_backoff(attempt, &e);
                            emit_one(
                                hooks,
                                &mut emit,
                                LoopEvent::ProviderRetry {
                                    attempt,
                                    max_attempts: config.max_provider_retries,
                                    wait_secs: wait.as_secs().max(1),
                                    requested_secs: requested.map(|d| d.as_secs()),
                                    error: e.to_string(),
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
                if let Some(msg) = doom.observe(&call.name, &call.input, is_error)
                    && let Some(msg) = hooks.on_doom_loop(&call.name, msg)
                {
                    steering = Some(msg);
                }
            }
            cx.history.extend(results);
            if let Some(text) = steering {
                let msg = kage_core::Message::new(
                    kage_core::Role::User,
                    vec![kage_core::Content::Text { text }],
                    cx.history.last().map(|m| m.id),
                );
                hooks.on_user_message(&msg);
                cx.history.push(msg);
            }
        }

        let Some(text) = drain_messages(config.followup_mode, || hooks.get_followup()) else {
            return Ok(());
        };
        let followup = kage_core::Message::new(
            kage_core::Role::User,
            vec![kage_core::Content::Text { text }],
            cx.history.last().map(|m| m.id),
        );
        hooks.on_user_message(&followup);
        cx.history.push(followup);
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
fn build_request(
    cx: &AgentContext,
    tools: &ToolRegistry,
    provider: &dyn Provider,
) -> StreamRequest {
    let history = if provider.preserves_thinking() {
        cx.history.clone()
    } else {
        flatten_thinking(&cx.history)
    };
    let mut req = StreamRequest::new(&cx.model, history);
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
mod tests;
