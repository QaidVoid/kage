//! Agent conversation message and content block types.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Unique identifier for a message in a session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub Ulid);

impl MessageId {
    /// Generate a fresh message id.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for a single tool invocation issued by an assistant.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    /// Wrap an arbitrary id string emitted by a provider.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Source of a message in the conversation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Sent by the human end-user.
    User,
    /// Sent by the model.
    Assistant,
    /// Carries the result of a tool execution back to the model.
    ToolResult,
    /// System / instruction prompt anchored at the conversation root.
    System,
}

/// One block of content within a message.
///
/// Multiple blocks per message let an assistant turn carry, for example,
/// thinking + text + several tool calls without ambiguity in ordering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain user-visible text.
    Text {
        /// The text body.
        text: String,
    },
    /// Hidden chain-of-thought emitted by the model.
    Thinking {
        /// The thinking body.
        text: String,
    },
    /// An image attached to the message.
    Image {
        /// Where the image bytes live (URL or inline base64).
        source: ImageSource,
        /// MIME type, for example `image/png`.
        mime: String,
    },
    /// A request from the model to invoke a tool.
    ToolCall {
        /// Provider-issued correlation id.
        id: ToolCallId,
        /// Tool name as registered in the tool registry.
        name: String,
        /// Tool input arguments as JSON.
        input: serde_json::Value,
    },
    /// The result of a previously-issued tool call, fed back to the model.
    ToolResultBlock {
        /// The id of the tool call this result corresponds to.
        call_id: ToolCallId,
        /// Stringified output. Structured output goes in [`Content::Custom`].
        output: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Plugin-provided content type, opaque to the core loop.
    Custom {
        /// Plugin-defined kind tag, namespaced like `plugin:tps`.
        kind: String,
        /// Arbitrary JSON payload.
        data: serde_json::Value,
    },
}

/// Reference to an image, either remote or inline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    /// Remote URL.
    Url {
        /// HTTP(S) URL.
        url: String,
    },
    /// Inline base64-encoded bytes.
    Base64 {
        /// Base64 payload, no data: prefix.
        data: String,
    },
}

/// One turn in the conversation.
///
/// A `Message` is the persisted unit of conversation history. Streaming
/// deltas are reassembled into a single `Message` before being appended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who or what produced this message.
    pub role: Role,
    /// Ordered content blocks.
    pub content: Vec<Content>,
    /// Stable identifier within a session.
    pub id: MessageId,
    /// Parent message id, used for branching and tree navigation.
    pub parent: Option<MessageId>,
    /// UTC timestamp at message creation.
    pub ts: DateTime<Utc>,
}

impl Message {
    /// Construct a message with a fresh id and the current timestamp.
    #[must_use]
    pub fn new(role: Role, content: Vec<Content>, parent: Option<MessageId>) -> Self {
        Self {
            role,
            content,
            id: MessageId::new(),
            parent,
            ts: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrips_through_json() {
        let original = Message {
            role: Role::Assistant,
            content: vec![
                Content::Thinking {
                    text: "let me think".into(),
                },
                Content::Text {
                    text: "the answer is 42".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_01"),
                    name: "calc".into(),
                    input: serde_json::json!({"expr": "6 * 7"}),
                },
            ],
            id: MessageId::new(),
            parent: Some(MessageId::new()),
            ts: Utc::now(),
        };

        let encoded = serde_json::to_string(&original).expect("serialize");
        let decoded: Message = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn role_serializes_as_snake_case() {
        let json = serde_json::to_string(&Role::ToolResult).unwrap();
        assert_eq!(json, "\"tool_result\"");
    }

    #[test]
    fn content_carries_type_tag() {
        let block = Content::Text { text: "hi".into() };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hi");
    }
}
