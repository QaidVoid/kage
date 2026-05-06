//! Streaming request shape passed to [`Provider::stream`](crate::Provider::stream).

use kage_core::{Message, ToolSpec};
use serde::{Deserialize, Serialize};

/// Optional thinking-budget configuration.
///
/// Anthropic-specific in 0.1, but exposed generically. Providers that do
/// not support thinking tokens ignore the field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Soft budget on thinking tokens emitted by the model.
    pub budget_tokens: u32,
}

/// A single streaming request to a provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamRequest {
    /// Model id, the part after `provider:` in the registry resolver.
    pub model: String,
    /// Conversation history, ending with the most recent user turn.
    pub messages: Vec<Message>,
    /// System prompt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Tools available to the model this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// Maximum number of output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional thinking budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

impl StreamRequest {
    /// Construct a minimal request with just a model id and message history.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            system: None,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            thinking: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_core::Role;

    #[test]
    fn new_omits_optional_fields() {
        let req = StreamRequest::new("anthropic:claude-sonnet-4-6", vec![]);
        assert_eq!(req.model, "anthropic:claude-sonnet-4-6");
        assert!(req.messages.is_empty());
        assert!(req.system.is_none());
        assert!(req.tools.is_empty());
        assert!(req.max_output_tokens.is_none());
        assert!(req.temperature.is_none());
        assert!(req.thinking.is_none());
    }

    #[test]
    fn empty_optional_fields_omitted_from_json() {
        let req = StreamRequest::new("m", vec![]);
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none());
        assert!(json.get("tools").is_none());
        assert!(json.get("max_output_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn populated_request_roundtrips() {
        let mut req = StreamRequest::new(
            "openai:gpt-4o",
            vec![Message::new(Role::User, vec![], None)],
        );
        req.system = Some("you are helpful".into());
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        req.max_output_tokens = Some(2_000);
        req.temperature = Some(0.7);
        req.thinking = Some(ThinkingConfig {
            budget_tokens: 5_000,
        });

        let s = serde_json::to_string(&req).unwrap();
        let back: StreamRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }
}
