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

use std::time::Duration;

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId};
use kage_provider::{Provider, ProviderError, StreamRequest};
use kage_tools::ToolRegistry;

use crate::compact::maybe_compact;
use crate::dispatch::{dispatch_tool_calls, dispatch_tool_calls_parallel, unrun_results};
use crate::doom::DoomTracker;
use crate::stream::{TurnFailure, TurnResult, collect_turn};
use crate::{AgentContext, Hooks, LoopConfig, SteeringMode};

/// Drive one agent run to completion.
///
/// `cx` carries the conversation forward: the caller is expected to push the
/// initiating user message into `cx.history` before calling this. On return,
/// `cx.history` reflects every message produced during the run, and
/// `cx.budget` is updated from provider-reported usage. Every message the
/// loop appends is announced with [`LoopEvent::MessageAppended`]; the
/// initiating message is the caller's to announce.
///
/// Streaming events are delivered to `emit` in order.
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
            return finish_cancelled(&mut emit);
        }

        loop {
            if cancel.is_cancelled() {
                return finish_cancelled(&mut emit);
            }

            if let Some(text) = drain_messages(config.steering_mode, || hooks.get_steering()) {
                push_user_text(cx, &mut emit, text);
            }

            if let Err(kind) = maybe_compact(cx, config, provider, cancel, hooks, &mut emit) {
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            emit(LoopEvent::TurnStarted { index: turn_index });

            if let Err(message) = hooks.transform_context(&mut cx.history) {
                let kind = LoopError::HookFailed {
                    hook: "transform_context".to_owned(),
                    message,
                };
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }

            let mut req = build_request(cx, tools, provider);
            if let Err(message) = hooks.transform_provider_request(&mut req) {
                let kind = LoopError::HookFailed {
                    hook: "transform_provider_request".to_owned(),
                    message,
                };
                emit(LoopEvent::Error { kind: kind.clone() });
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
                        return finish_cancelled(&mut emit);
                    }
                    match stream_one_attempt(provider, req.clone(), parent, cancel, &mut emit) {
                        Ok(t) => break t,
                        Err(TurnFailure::Fatal(kind)) => {
                            emit(LoopEvent::Error { kind: kind.clone() });
                            return Err(kind);
                        }
                        Err(TurnFailure::Provider(_)) if cancel.is_cancelled() => {
                            return finish_cancelled(&mut emit);
                        }
                        Err(TurnFailure::Provider(e)) => {
                            let exhausted = attempt >= config.max_provider_retries;
                            if exhausted || !e.is_transient() {
                                let kind = match e {
                                    ProviderError::Auth(message) => LoopError::Auth { message },
                                    other => LoopError::Provider {
                                        message: other.to_string(),
                                    },
                                };
                                emit(LoopEvent::Error { kind: kind.clone() });
                                return Err(kind);
                            }
                            attempt += 1;
                            let requested = e.retry_after();
                            let wait = retry_backoff(attempt, &e);
                            emit(LoopEvent::ProviderRetry {
                                attempt,
                                max_attempts: config.max_provider_retries,
                                wait_secs: wait.as_secs().max(1),
                                requested_secs: requested.map(|d| d.as_secs()),
                                error: e.to_string(),
                            });
                            if !sleep_cancelable(cancel, wait) {
                                return finish_cancelled(&mut emit);
                            }
                        }
                    }
                }
            };

            cx.budget.add(turn.usage);
            let turn_usage = turn.usage;
            let assistant_id = turn.message.id;
            let pending = turn.tool_calls.clone();
            append(cx, &mut emit, turn.message);

            let had_tool_calls = !pending.is_empty();
            emit(LoopEvent::TurnEnded {
                index: turn_index,
                had_tool_calls,
            });
            let summary = crate::hooks::TurnSummary {
                index: turn_index,
                had_tool_calls,
                usage: turn_usage,
            };
            turn_index = turn_index.saturating_add(1);
            if hooks.should_stop_after_turn(&summary) {
                let unrun = unrun_results(pending, assistant_id, &mut emit);
                append_all(cx, &mut emit, unrun);
                return Ok(());
            }

            if !had_tool_calls {
                break;
            }

            let workdir = cx.workdir.clone();
            // Parallel dispatch when the loop is configured for it and no
            // tool in the batch overrides to Sequential (e.g. `bash`), or
            // when every tool in the batch declares itself Parallel.
            let mode_of = |name: &str| tools.get(name).and_then(|t| t.execution_mode());
            let any_sequential = pending
                .iter()
                .any(|call| mode_of(&call.name) == Some(kage_tools::ExecMode::Sequential));
            let all_parallel = pending
                .iter()
                .all(|call| mode_of(&call.name) == Some(kage_tools::ExecMode::Parallel));
            let dispatch = if (config.parallel_tools && !any_sequential) || all_parallel {
                dispatch_tool_calls_parallel
            } else {
                dispatch_tool_calls
            };
            let outcome = dispatch(
                pending.clone(),
                tools,
                &workdir,
                cancel,
                cx.confine_paths,
                assistant_id,
                hooks,
                &mut emit,
            );
            if let Some(kind) = outcome.error {
                // Every tool_use in the assistant message now has an answer
                // in `outcome.results`; append them so in-memory history and
                // the persisted session never carry a dangling tool_use.
                append_all(cx, &mut emit, outcome.results);
                emit(LoopEvent::Error { kind: kind.clone() });
                return Err(kind);
            }
            let results = outcome.results;
            // If every tool in the batch signaled `terminate`, persist the
            // results and exit the run cleanly. The loop never asks the
            // model for another turn, never dequeues a follow-up.
            if outcome.all_terminate {
                append_all(cx, &mut emit, results);
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
            append_all(cx, &mut emit, results);
            if let Some(text) = steering {
                push_user_text(cx, &mut emit, text);
            }
        }

        let Some(text) = drain_messages(config.followup_mode, || hooks.get_followup()) else {
            return Ok(());
        };
        push_user_text(cx, &mut emit, text);
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

/// Append `message` to history and announce it with
/// [`LoopEvent::MessageAppended`].
fn append<F: FnMut(LoopEvent)>(cx: &mut AgentContext, emit: &mut F, message: Message) {
    cx.history.push(message.clone());
    emit(LoopEvent::MessageAppended { message });
}

fn append_all<F: FnMut(LoopEvent)>(cx: &mut AgentContext, emit: &mut F, messages: Vec<Message>) {
    for message in messages {
        append(cx, emit, message);
    }
}

/// Append a user text message that the loop injected (steering, follow-up,
/// or a doom-loop nudge).
fn push_user_text<F: FnMut(LoopEvent)>(cx: &mut AgentContext, emit: &mut F, text: String) {
    let message = Message::new(
        kage_core::Role::User,
        vec![Content::Text { text }],
        cx.history.last().map(|m| m.id),
    );
    append(cx, emit, message);
}

/// Rewrite persisted `Content::Thinking` blocks into inline
/// `<thinking>...</thinking>` text before a request is built.
///
/// Thinking blocks are not portable across a request boundary. kage
/// never persists the cryptographic signature Anthropic requires to
/// replay a native thinking block, so sending one back is rejected by
/// that API; the `OpenAI` chat-completions and Gemini providers drop
/// unknown content silently, losing the reasoning chain outright.
/// Switching models mid-session makes both failure modes worse.
/// Flattening historical thinking to plain text keeps the reasoning
/// visible to whatever provider runs the next turn, regardless of
/// which produced it. Providers that can accept native blocks opt out
/// via [`Provider::preserves_thinking`].
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
    req.level = cx.reasoning.resolve(cx.thinking_level);
    req.reasoning = cx.reasoning;
    req
}

fn finish_cancelled<F: FnMut(LoopEvent)>(emit: &mut F) -> Result<(), LoopError> {
    emit(LoopEvent::Error {
        kind: LoopError::Cancelled,
    });
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
    emit: &mut F,
) -> Result<TurnResult, TurnFailure> {
    let stream = provider
        .stream(req, cancel)
        .map_err(TurnFailure::Provider)?;
    collect_turn(parent, stream, cancel, emit)
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
/// The wait wakes the moment the flag is set, so a cancel aborts the
/// backoff at once instead of after the full delay.
fn sleep_cancelable(cancel: &CancelFlag, dur: Duration) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    cancel.watch().receiver().recv_timeout(dur).is_err() && !cancel.is_cancelled()
}

#[cfg(test)]
mod tests;
