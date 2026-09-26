//! Streaming events emitted by the agent loop and consumed by hosts.
//!
//! This is the only public interface from the loop to the outside world.
//! TUI, CLI, plugins, and editor integrations each subscribe to the same
//! event alphabet.

use serde::{Deserialize, Serialize};

use crate::message::{Message, MessageId, ToolCallId};

/// Why a provider's stream ended.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model decided the turn was complete.
    EndTurn,
    /// Hit the max output token limit.
    MaxTokens,
    /// Matched a stop sequence.
    StopSequence,
    /// Stopped because the model emitted tool calls awaiting execution.
    ToolUse,
    /// Anything else (refusal, internal stop, unknown).
    #[default]
    Other,
}

/// Token usage reported by a provider for one assistant turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens consumed from the conversation context.
    pub input: u64,
    /// Tokens emitted by the model.
    pub output: u64,
    /// Tokens served from the provider's prompt cache.
    pub cache_read: u64,
    /// Tokens written to the provider's prompt cache for future reuse.
    pub cache_write: u64,
}

/// Output of a single tool execution carried back through the event stream.
///
/// Tools may also emit progress via a separate channel during execution; this
/// type carries the final result that the loop appends to history as a
/// [`crate::Content::ToolResultBlock`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Whether the tool reported failure.
    pub is_error: bool,
    /// Stringified output. Goes verbatim into the next assistant turn's input.
    pub text: String,
    /// Optional structured detail channel for richer rendering by the host.
    ///
    /// Tools that produce structured data (diffs, file lists, exit codes,
    /// match counts) populate this field so the TUI can render a richer
    /// view than the plain `text` body alone. Tools that produce only
    /// human-readable text leave it as `None`.
    ///
    /// Conventional fields, by tool family:
    /// * `bash`: `{ exit_code, stdout_truncated, stderr_truncated, cwd }`
    /// * `edit`: `{ path, replacements, diff }` (diff is unified-diff text)
    /// * `write`: `{ path, bytes }`
    /// * `read`: typically `None`; the body is the entire payload
    /// * `find` / `glob`: `{ pattern, matches, paths }`
    /// * `grep`: `{ pattern, matches, truncated }`
    /// * `ls`: `{ entries }` (the rendered listing as a list)
    /// * `web_fetch`: `{ url, status, content_type, truncated }`
    ///
    /// Hosts MUST tolerate missing fields: never `unwrap` on a structured
    /// payload. New tools should follow the same pattern when adding
    /// new families.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    /// When every tool in a single batch sets this to `true`, the loop
    /// returns after the turn without consulting follow-ups. Used by
    /// tools like `task_done` that signal a successful exit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminate: bool,
}

/// Per-million-token prices for one model, in USD.
///
/// Cache-read and cache-write are optional because not every provider
/// distinguishes them; [`TokenCost::from_usage`] says how a missing
/// rate is billed.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    /// Dollars per million input (prompt) tokens.
    pub input: f64,
    /// Dollars per million output (completion) tokens.
    pub output: f64,
    /// Dollars per million tokens read from the provider's prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Dollars per million tokens written into the provider's prompt
    /// cache for a future turn to reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Dollar cost of one turn's [`TokenUsage`] given a per-million pricing
/// table. All values in USD.
///
/// Cache-read tokens fall back to the input rate when the model's
/// cache-read price is missing; cache-write falls back to the output
/// rate (a conservative estimate that won't undercount).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenCost {
    /// Dollars spent on prompt tokens this turn.
    pub input: f64,
    /// Dollars spent on completion tokens this turn.
    pub output: f64,
    /// Dollars billed for prompt-cache reads.
    pub cache_read: f64,
    /// Dollars billed for prompt-cache writes.
    pub cache_write: f64,
    /// Sum of the other four fields.
    pub total: f64,
}

impl TokenCost {
    /// Apply per-million pricing to a `TokenUsage` to produce a
    /// concrete cost in dollars. Each component is computed as
    /// `tokens * rate_per_million / 1_000_000`.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts stay far below 2^52, where f64 is exact"
    )]
    pub fn from_usage(
        usage: &TokenUsage,
        input_per_m: f64,
        output_per_m: f64,
        cache_read_per_m: Option<f64>,
        cache_write_per_m: Option<f64>,
    ) -> Self {
        let cache_read_rate = cache_read_per_m.unwrap_or(input_per_m);
        let cache_write_rate = cache_write_per_m.unwrap_or(output_per_m);
        let scale = 1_000_000.0;
        let input = usage.input as f64 * input_per_m / scale;
        let output = usage.output as f64 * output_per_m / scale;
        let cache_read = usage.cache_read as f64 * cache_read_rate / scale;
        let cache_write = usage.cache_write as f64 * cache_write_rate / scale;
        Self {
            input,
            output,
            cache_read,
            cache_write,
            total: input + output + cache_read + cache_write,
        }
    }

    /// Add `other` into `self` field-by-field. Used to accumulate
    /// session-level cost across turns.
    pub fn add(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total += other.total;
    }
}

/// Mid-execution progress update emitted by a long-running tool.
///
/// Tools call `ToolContext::update` from `kage-tools` during `execute`
/// to stream progress without buffering everything into the final
/// [`ToolOutput::text`]. The loop wraps each call in a
/// [`LoopEvent::ToolUpdate`] event the host can render live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolUpdate {
    /// Human-readable progress line (e.g. `"12/45 crates compiled"`).
    pub content: String,
    /// Optional structured payload mirroring [`ToolOutput::structured`] for
    /// richer rendering by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

/// Reason the loop emitted an [`LoopEvent::Error`].
///
/// Carried inside events so it can travel across the rpc boundary without
/// losing the variant. Hosts pattern-match on the variant; the trailing
/// `message` fields are human-readable but not API-stable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopError {
    /// Provider request or stream failed.
    #[error("provider error: {message}")]
    Provider {
        /// Human-readable detail.
        message: String,
    },
    /// The provider rejected the credentials (missing, expired, or
    /// revoked). Re-authenticating is the remedy; retrying is not.
    #[error("authentication failed: {message}")]
    Auth {
        /// Provider-supplied detail, without the raw response body.
        message: String,
    },
    /// A tool invocation raised an error the loop could not recover from.
    #[error("tool '{name}' error: {message}")]
    Tool {
        /// Tool name.
        name: String,
        /// Human-readable detail.
        message: String,
    },
    /// The loop was cancelled via its cancellation token.
    #[error("cancelled")]
    Cancelled,
    /// The conversation history exceeded the context window even after
    /// compaction.
    #[error("context overflow: history exceeds the model's window even after compaction")]
    ContextOverflow,
    /// A host-supplied hook returned an error the loop could not recover
    /// from. Carries the hook name so the host can surface which extension
    /// point failed (for example `transform_context`).
    #[error("hook '{hook}' failed: {message}")]
    HookFailed {
        /// Identifier of the hook method that failed.
        hook: String,
        /// Human-readable detail.
        message: String,
    },
    /// The prompt was refused before it reached the model, such as an
    /// MCP prompt command missing a required argument.
    #[error("{message}")]
    InvalidPrompt {
        /// Human-readable detail.
        message: String,
    },
    /// Anything not covered above.
    #[error("{message}")]
    Other {
        /// Human-readable detail.
        message: String,
    },
}

/// Text of the error result the loop records for a tool call that was
/// cancelled before it produced an output. Hosts match it to show the
/// call as interrupted rather than failed.
pub const TOOL_CANCELLED_TEXT: &str = "tool call cancelled before completion";

/// Body of the `agent` tool result for an agent that ended without a
/// reply. Hosts match it to say the agent was stopped before replying.
pub const AGENT_NO_REPLY_TEXT: &str = "(the agent produced no reply)";

/// One event in the loop's output stream.
///
/// `MessageStart` opens a logical message; `TextDelta` and `ThinkingDelta`
/// carry incremental text; `ToolCallStart` / `ToolCallEnd` bracket each tool
/// invocation, with `ToolExecutionStart` marking when the tool runs; `MessageEnd` closes the message with usage accounting.
/// `Compaction` reports a context summarization; `Error` is terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoopEvent {
    /// A new assistant message has begun.
    MessageStart {
        /// Id of the message being assembled.
        id: MessageId,
    },
    /// Incremental text delta to append to the current message.
    TextDelta {
        /// Id of the message receiving the delta.
        id: MessageId,
        /// Text chunk to append.
        delta: String,
    },
    /// Incremental thinking delta to append to the current message.
    ThinkingDelta {
        /// Id of the message receiving the delta.
        id: MessageId,
        /// Thinking chunk to append.
        delta: String,
    },
    /// The model finished streaming a tool call's arguments. The call
    /// has not started executing: it may still wait for approval or for
    /// earlier calls. [`LoopEvent::ToolExecutionStart`] marks the start
    /// of execution and [`LoopEvent::ToolCallEnd`] its result.
    ToolCallStart {
        /// Provider-issued correlation id for this call.
        id: ToolCallId,
        /// Name of the tool to invoke.
        name: String,
        /// Tool arguments as parsed so far. May be empty.
        input_partial: serde_json::Value,
    },
    /// Progressive, best-effort tool-call argument update emitted
    /// while the model is still streaming the call's JSON. Purely a
    /// UI hint: the authoritative call is delivered exactly once by
    /// [`LoopEvent::ToolCallStart`] when the arguments are complete,
    /// so recording and plugin hooks ignore this variant.
    ToolCallArgsDelta {
        /// Correlation id matching the eventual
        /// [`LoopEvent::ToolCallStart`].
        id: ToolCallId,
        /// Tool name, known from the call's start.
        name: String,
        /// Arguments parsed so far. An empty object until the first
        /// fragment forms valid JSON.
        input_partial: serde_json::Value,
    },
    /// A tool call passed `before_tool_call`, including any approval,
    /// and its tool begins executing now. Not emitted for a call the
    /// host short-circuited. Live.
    ToolExecutionStart {
        /// Correlation id matching the prior [`LoopEvent::ToolCallStart`].
        id: ToolCallId,
    },
    /// Mid-execution progress update from a running tool.
    ToolUpdate {
        /// Correlation id matching the prior [`LoopEvent::ToolCallStart`].
        id: ToolCallId,
        /// The progress payload the tool reported.
        update: ToolUpdate,
    },
    /// A tool call has completed.
    ToolCallEnd {
        /// Correlation id matching the prior [`LoopEvent::ToolCallStart`].
        id: ToolCallId,
        /// Final output of the tool invocation.
        output: ToolOutput,
    },
    /// The current assistant message has finished.
    MessageEnd {
        /// Id of the message that just ended.
        id: MessageId,
        /// Token usage for this turn.
        usage: TokenUsage,
        /// Why the provider stream ended. `MaxTokens` means the reply
        /// was cut off mid-generation; hosts surface this so a silent
        /// truncation is never mistaken for a complete answer.
        #[serde(default)]
        stop_reason: StopReason,
    },
    /// A message was appended to the conversation history: the initial
    /// prompt, a steered or follow-up prompt, an assistant reply, or a
    /// tool result. Recording these in order reproduces the history
    /// exactly. Durable.
    MessageAppended {
        /// The appended message, with its parent link set.
        message: Message,
    },
    /// A turn (one provider round trip plus its tool calls) began. Durable.
    TurnStarted {
        /// Zero-based turn index within the run.
        index: u32,
    },
    /// A turn finished. Durable.
    TurnEnded {
        /// Zero-based turn index within the run.
        index: u32,
        /// Whether the assistant requested tool calls this turn.
        had_tool_calls: bool,
    },
    /// Older turns were summarized to fit the context window.
    Compaction {
        /// Number of recent turns kept verbatim.
        kept: usize,
        /// Number of older turns replaced by a summary.
        summarized: usize,
        /// Text of the synthetic message that replaced the summarized turns.
        summary: String,
    },
    /// A transient provider failure that the loop is about to retry.
    /// Surfaced to the user as a non-persisted notice so the host can
    /// show "retrying 2/4 in 8s" (and, when the server asked for a
    /// longer wait than the loop's cap, "anthropic asked for 5m").
    ProviderRetry {
        /// 1-based attempt counter for the next retry.
        attempt: u32,
        /// Configured retry ceiling.
        max_attempts: u32,
        /// Seconds the loop will actually sleep before retrying. Always
        /// less than or equal to the loop's internal cap (60s).
        wait_secs: u64,
        /// Seconds the server asked for via `Retry-After` when present.
        /// `Some` whenever the provider returned a hint, even if the
        /// hint did not exceed the cap; UIs decide whether to surface
        /// it. `None` for purely client-side backoff.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_secs: Option<u64>,
        /// Stringified underlying error for human display.
        error: String,
    },
    /// Terminal error.
    Error {
        /// Variant detailing the failure.
        kind: LoopError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_default_is_zero() {
        let u = TokenUsage::default();
        assert_eq!(u.input, 0);
        assert_eq!(u.output, 0);
        assert_eq!(u.cache_read, 0);
        assert_eq!(u.cache_write, 0);
    }

    #[test]
    fn loop_event_carries_type_tag() {
        let ev = LoopEvent::TextDelta {
            id: MessageId::new(),
            delta: "hi".into(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["delta"], "hi");
    }

    #[test]
    fn loop_event_roundtrips() {
        let ev = LoopEvent::MessageEnd {
            id: MessageId::new(),
            usage: TokenUsage {
                input: 100,
                output: 50,
                cache_read: 80,
                cache_write: 20,
            },
            stop_reason: StopReason::MaxTokens,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: LoopEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn message_end_deserializes_without_stop_reason() {
        let ev = LoopEvent::MessageEnd {
            id: MessageId::new(),
            usage: TokenUsage {
                input: 1,
                output: 2,
                cache_read: 0,
                cache_write: 0,
            },
            stop_reason: StopReason::Other,
        };
        let mut json = serde_json::to_value(&ev).unwrap();
        json.as_object_mut().unwrap().remove("stop_reason");
        let back: LoopEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn tool_output_omits_structured_when_none() {
        let out = ToolOutput {
            is_error: false,
            text: "ok".into(),
            structured: None,
            terminate: false,
        };
        let json = serde_json::to_value(&out).unwrap();
        assert!(json.get("structured").is_none());
    }

    #[test]
    fn tool_output_omits_terminate_when_default_false() {
        let out = ToolOutput {
            is_error: false,
            text: "ok".into(),
            structured: None,
            terminate: false,
        };
        let json = serde_json::to_value(&out).unwrap();
        assert!(json.get("terminate").is_none());
    }

    #[test]
    fn tool_output_emits_terminate_when_true() {
        let out = ToolOutput {
            is_error: false,
            text: "done".into(),
            structured: None,
            terminate: true,
        };
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["terminate"], true);
    }

    #[test]
    fn loop_error_serializes_with_kind_tag() {
        let err = LoopError::Tool {
            name: "shell".into(),
            message: "exit 1".into(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["kind"], "tool");
        assert_eq!(json["name"], "shell");
    }

    #[test]
    fn auth_error_serializes_with_auth_kind() {
        let err = LoopError::Auth {
            message: "token expired".into(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["kind"], "auth");
        assert_eq!(json["message"], "token expired");
        assert_eq!(err.to_string(), "authentication failed: token expired");
    }
}
