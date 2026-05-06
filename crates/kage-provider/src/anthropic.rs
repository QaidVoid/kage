//! Anthropic provider.
//!
//! Implements the Messages API (`POST /v1/messages`) using `ureq`. T2.3 covers
//! the non-streaming request path and the wire-format helpers; streaming via
//! SSE lands in T2.4.

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ProviderError, ProviderMetadata, StopReason, StreamRequest, ToolSpec};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// Anthropic provider implementation.
#[derive(Debug)]
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    agent: ureq::Agent,
}

impl AnthropicProvider {
    /// Construct a provider from an API key, using the default base URL.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Construct a provider against a custom base URL (for tests or proxies).
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            metadata: ProviderMetadata {
                id: "anthropic".into(),
                display_name: "Anthropic".into(),
                supports_caching: true,
                supports_thinking: true,
                supports_tool_use: true,
            },
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    /// Static metadata for this provider.
    #[must_use]
    pub fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    /// Issue a non-streaming Messages API request.
    ///
    /// Streaming is added in T2.4; this entry point is kept so callers can
    /// validate the wire format end-to-end and is also the path used when
    /// caller explicitly opts out of streaming.
    pub fn request(
        &self,
        req: &StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<AnthropicMessage, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let body = build_request_body(req, false);
        let url = format!("{}/v1/messages", self.base_url);

        let response = self
            .agent
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .send_json(&body)
            .map_err(map_ureq_error)?;

        let parsed: AnthropicMessage = response
            .into_body()
            .read_json()
            .map_err(|e| ProviderError::Decode(e.to_string()))?;
        Ok(parsed)
    }
}

/// Build the JSON body for a Messages API request.
pub(crate) fn build_request_body(req: &StreamRequest, stream: bool) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(internal_message_to_anthropic)
        .collect();

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "stream": stream,
    });

    if let Some(system) = &req.system {
        body["system"] = Value::String(system.clone());
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::to_value(
            req.tools
                .iter()
                .map(tool_spec_to_anthropic)
                .collect::<Vec<_>>(),
        )
        .expect("tool spec serializes");
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(thinking) = &req.thinking {
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": thinking.budget_tokens,
        });
    }
    body
}

fn tool_spec_to_anthropic(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.schema,
    })
}

/// Convert an internal [`Message`] into the Anthropic wire shape.
///
/// Returns `None` for messages that should not be sent (system messages live
/// in the top-level `system` field; custom plugin content has no wire form).
fn internal_message_to_anthropic(msg: &Message) -> Option<Value> {
    let (role, blocks) = match msg.role {
        Role::User => ("user", convert_user_blocks(&msg.content)),
        Role::Assistant => ("assistant", convert_assistant_blocks(&msg.content)),
        Role::ToolResult => ("user", convert_tool_result_blocks(&msg.content)),
        Role::System => return None,
    };
    if blocks.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": role,
        "content": blocks,
    }))
}

fn convert_user_blocks(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"text","text":text})),
            Content::Image { source, mime } => Some(image_to_anthropic(source, mime)),
            _ => None,
        })
        .collect()
}

fn convert_assistant_blocks(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"text","text":text})),
            Content::Thinking { text } => Some(serde_json::json!({
                "type":"thinking",
                "thinking":text,
            })),
            Content::ToolCall { id, name, input } => Some(serde_json::json!({
                "type":"tool_use",
                "id": id.0,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

fn convert_tool_result_blocks(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => Some(serde_json::json!({
                "type":"tool_result",
                "tool_use_id": call_id.0,
                "content": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

fn image_to_anthropic(source: &kage_core::ImageSource, mime: &str) -> Value {
    match source {
        kage_core::ImageSource::Url { url } => serde_json::json!({
            "type":"image",
            "source": {"type":"url", "url": url},
        }),
        kage_core::ImageSource::Base64 { data } => serde_json::json!({
            "type":"image",
            "source": {"type":"base64", "media_type": mime, "data": data},
        }),
    }
}

/// Decoded Messages API response.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AnthropicMessage {
    /// Model-issued message id.
    pub id: String,
    /// Always `"message"` in current Anthropic responses.
    #[serde(rename = "type")]
    pub kind: String,
    /// Always `"assistant"`.
    pub role: String,
    /// Model id that generated the response.
    pub model: String,
    /// Content blocks (text, thinking, `tool_use`).
    pub content: Vec<Value>,
    /// Why the model stopped.
    pub stop_reason: Option<String>,
    /// Stop sequence that triggered the stop, if any.
    pub stop_sequence: Option<String>,
    /// Token usage for this turn.
    pub usage: AnthropicUsage,
}

/// Token accounting block from the response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct AnthropicUsage {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens produced.
    pub output_tokens: u64,
    /// Tokens written to the prompt cache.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens served from the prompt cache.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl AnthropicMessage {
    /// Convert the API response into our internal types.
    ///
    /// Returns the assembled assistant message, the stop reason, and token
    /// usage. Unknown content block types are skipped silently.
    #[must_use]
    pub fn into_internal(self) -> (Message, StopReason, TokenUsage) {
        let mut content = Vec::with_capacity(self.content.len());
        for block in self.content {
            if let Some(c) = anthropic_block_to_content(&block) {
                content.push(c);
            }
        }
        let message = Message::new(Role::Assistant, content, None);
        let usage = TokenUsage {
            input: self.usage.input_tokens,
            output: self.usage.output_tokens,
            cache_read: self.usage.cache_read_input_tokens,
            cache_write: self.usage.cache_creation_input_tokens,
        };
        let stop = match self.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            Some("tool_use") => StopReason::ToolUse,
            _ => StopReason::Other,
        };
        (message, stop, usage)
    }
}

fn anthropic_block_to_content(block: &Value) -> Option<Content> {
    let kind = block.get("type")?.as_str()?;
    match kind {
        "text" => Some(Content::Text {
            text: block.get("text")?.as_str()?.to_owned(),
        }),
        "thinking" => Some(Content::Thinking {
            text: block.get("thinking")?.as_str()?.to_owned(),
        }),
        "tool_use" => {
            let id = block.get("id")?.as_str()?.to_owned();
            let name = block.get("name")?.as_str()?.to_owned();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            Some(Content::ToolCall {
                id: ToolCallId::new(id),
                name,
                input,
            })
        }
        _ => None,
    }
}

fn map_ureq_error(err: ureq::Error) -> ProviderError {
    match err {
        ureq::Error::StatusCode(code) => ProviderError::Http {
            status: code,
            body: String::new(),
        },
        ureq::Error::Io(e) => ProviderError::Transport(e.to_string()),
        other => ProviderError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_core::{Content, Message, Role};

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
    fn body_sets_model_and_messages() {
        let req = StreamRequest::new("claude-sonnet-4-6", vec![user_msg("hi")]);
        let body = build_request_body(&req, false);
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["stream"], false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hi");
    }

    #[test]
    fn body_promotes_system_to_top_level() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, false);
        assert_eq!(body["system"], "you are kage");
    }

    #[test]
    fn body_drops_system_role_messages() {
        let mut req = StreamRequest::new(
            "m",
            vec![
                Message::new(
                    Role::System,
                    vec![Content::Text {
                        text: "ignored".into(),
                    }],
                    None,
                ),
                user_msg("hi"),
            ],
        );
        req.system = Some("the real system prompt".into());
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn body_includes_tools_when_present() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req, false);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[0]["description"], "read a file");
        assert_eq!(
            tools[0]["input_schema"],
            serde_json::json!({"type":"object"})
        );
    }

    #[test]
    fn body_includes_thinking_when_configured() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.thinking = Some(crate::ThinkingConfig {
            budget_tokens: 12_000,
        });
        let body = build_request_body(&req, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 12_000);
    }

    #[test]
    fn body_uses_default_max_tokens_when_unset() {
        let req = StreamRequest::new("m", vec![user_msg("hi")]);
        let body = build_request_body(&req, false);
        assert_eq!(body["max_tokens"], 4_096);
    }

    #[test]
    fn assistant_message_with_tool_call_serializes() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "read".into(),
                input: serde_json::json!({"path":"/etc/hosts"}),
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![user_msg("read hosts"), assistant]);
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        let blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_1");
        assert_eq!(blocks[0]["name"], "read");
        assert_eq!(blocks[0]["input"]["path"], "/etc/hosts");
    }

    #[test]
    fn tool_result_message_uses_user_role() {
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("call_1"),
                output: "127.0.0.1".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![result]);
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        let blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_1");
        assert_eq!(blocks[0]["content"], "127.0.0.1");
        assert_eq!(blocks[0]["is_error"], false);
    }

    #[test]
    fn parse_response_extracts_text_and_usage() {
        let json = serde_json::json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type":"text","text":"hello"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let parsed: AnthropicMessage = serde_json::from_value(json).unwrap();
        let (msg, stop, usage) = parsed.into_internal();
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 1);
        if let Content::Text { text } = &msg.content[0] {
            assert_eq!(text, "hello");
        } else {
            panic!("expected Text content");
        }
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 5);
    }

    /// Round-trip the real Anthropic API. Opt-in: requires `ANTHROPIC_API_KEY`
    /// in the environment. Run with:
    ///
    /// ```sh
    /// ANTHROPIC_API_KEY=sk-ant-... cargo test -p kage-provider -- --ignored anthropic_live
    /// ```
    #[test]
    #[ignore = "requires ANTHROPIC_API_KEY"]
    fn anthropic_live_smoke() {
        let key =
            std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY to run this test");
        let provider = AnthropicProvider::new(key);
        let req = StreamRequest::new(
            "claude-haiku-4-5-20251001",
            vec![Message::new(
                Role::User,
                vec![Content::Text {
                    text: "Reply with exactly the word: pong".into(),
                }],
                None,
            )],
        );
        let resp = provider
            .request(&req, &CancelFlag::new())
            .expect("request succeeds");
        let (msg, _stop, usage) = resp.into_internal();
        assert!(!msg.content.is_empty(), "response has at least one block");
        assert!(usage.input > 0, "input tokens reported");
        assert!(usage.output > 0, "output tokens reported");
    }

    #[test]
    fn parse_response_extracts_tool_call_and_cache_tokens() {
        let json = serde_json::json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type":"text","text":"reading"},
                {"type":"tool_use","id":"call_1","name":"read","input":{"path":"/x"}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_creation_input_tokens": 50,
                "cache_read_input_tokens": 80
            }
        });
        let parsed: AnthropicMessage = serde_json::from_value(json).unwrap();
        let (msg, stop, usage) = parsed.into_internal();
        assert_eq!(msg.content.len(), 2);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.cache_read, 80);
        assert_eq!(usage.cache_write, 50);
    }
}
