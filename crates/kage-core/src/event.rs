//! Streaming events emitted by the agent loop and consumed by hosts.
//!
//! This is the only public interface from the loop to the outside world.
//! TUI, CLI, plugins, and editor integrations each subscribe to the same
//! event alphabet.

use serde::{Deserialize, Serialize};

use crate::message::{MessageId, ToolCallId};

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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Whether the tool reported failure.
    pub is_error: bool,
    /// Stringified output. Goes verbatim into the next assistant turn's input.
    pub text: String,
    /// Optional structured payload for richer rendering by the host.
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
    /// Anything not covered above.
    #[error("{message}")]
    Other {
        /// Human-readable detail.
        message: String,
    },
}

/// One event in the loop's output stream.
///
/// `MessageStart` opens a logical message; `TextDelta` and `ThinkingDelta`
/// carry incremental text; `ToolCallStart` / `ToolCallEnd` bracket each tool
/// invocation; `MessageEnd` closes the message with usage accounting.
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
    /// A tool call has begun. May be followed by zero or more partial-input
    /// updates before [`LoopEvent::ToolCallEnd`].
    ToolCallStart {
        /// Provider-issued correlation id for this call.
        id: ToolCallId,
        /// Name of the tool to invoke.
        name: String,
        /// Tool arguments as parsed so far. May be empty.
        input_partial: serde_json::Value,
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
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: LoopEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn tool_output_omits_structured_when_none() {
        let out = ToolOutput {
            is_error: false,
            text: "ok".into(),
            structured: None,
        };
        let json = serde_json::to_value(&out).unwrap();
        assert!(json.get("structured").is_none());
    }

    #[test]
    fn loop_error_serializes_with_kind_tag() {
        let err = LoopError::Tool {
            name: "bash".into(),
            message: "exit 1".into(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["kind"], "tool");
        assert_eq!(json["name"], "bash");
    }
}
