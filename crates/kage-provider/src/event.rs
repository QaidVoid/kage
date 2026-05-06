//! Streaming events emitted by a [`Provider`](crate::Provider).
//!
//! Narrower alphabet than [`kage_core::LoopEvent`]: providers report deltas
//! and stop reasons; the loop is responsible for tool execution outcomes.

use kage_core::{TokenUsage, ToolCallId};
use serde::{Deserialize, Serialize};

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

/// One event in a provider's streaming response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Provider acknowledged the request and is starting a response.
    MessageStart,
    /// Incremental text body delta.
    TextDelta {
        /// Text chunk to append to the in-flight assistant message.
        delta: String,
    },
    /// Incremental thinking-block delta.
    ThinkingDelta {
        /// Thinking chunk to append.
        delta: String,
    },
    /// A tool call has begun. Followed by zero or more
    /// [`ProviderEvent::ToolCallArgsDelta`] then one
    /// [`ProviderEvent::ToolCallEnd`].
    ToolCallStart {
        /// Provider-issued correlation id.
        id: ToolCallId,
        /// Tool name as the model invoked it.
        name: String,
    },
    /// Partial JSON for the in-flight tool call's arguments.
    ToolCallArgsDelta {
        /// Correlation id matching the prior `ToolCallStart`.
        id: ToolCallId,
        /// Partial JSON fragment to append.
        partial: String,
    },
    /// Tool call arguments are now complete.
    ToolCallEnd {
        /// Correlation id matching the prior `ToolCallStart`.
        id: ToolCallId,
        /// Final, fully-assembled tool input.
        input: serde_json::Value,
    },
    /// Stream is over.
    MessageEnd {
        /// Why the model stopped.
        stop_reason: StopReason,
        /// Token accounting for this turn.
        usage: TokenUsage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_event_carries_type_tag() {
        let ev = ProviderEvent::TextDelta { delta: "hi".into() };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["delta"], "hi");
    }

    #[test]
    fn message_end_roundtrips() {
        let ev = ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input: 10,
                output: 20,
                cache_read: 0,
                cache_write: 0,
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: ProviderEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn stop_reason_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool_use\""
        );
    }
}
