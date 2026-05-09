//! Anthropic provider.
//!
//! Implements the Messages API (`POST /v1/messages`) using `ureq`. The
//! non-streaming `request` method buffers and decodes the full response;
//! the `Provider::stream` impl reads server-sent events line by line and
//! yields [`ProviderEvent`]s as they arrive.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StopReason,
    StreamRequest, ToolSpec,
};

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
            agent: crate::openai::build_agent(),
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

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::openai::read_error_body(status, response));
        }

        let parsed: AnthropicMessage = response
            .into_body()
            .read_json()
            .map_err(|e| ProviderError::Decode(e.to_string()))?;
        Ok(parsed)
    }
}

/// Build the JSON body for a Messages API request.
///
/// Adds Anthropic prompt-cache breakpoints (`cache_control: ephemeral`) to
/// the system prompt and to the last block of the final message. This caches
/// the system prompt + tool definitions + entire prior conversation, so the
/// next turn pays read-rate (~10% of input rate) for everything before the
/// new user message.
pub(crate) fn build_request_body(req: &StreamRequest, stream: bool) -> Value {
    let mut messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(internal_message_to_anthropic)
        .collect();
    if let Some(last) = messages.last_mut() {
        mark_last_block_for_caching(last);
    }

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "stream": stream,
    });

    if let Some(system) = &req.system {
        body["system"] = serde_json::json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }]);
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

fn mark_last_block_for_caching(message: &mut Value) {
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(last) = content.last_mut() else {
        return;
    };
    let Some(obj) = last.as_object_mut() else {
        return;
    };
    obj.insert(
        "cache_control".into(),
        serde_json::json!({"type": "ephemeral"}),
    );
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

impl Provider for AnthropicProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn stream(
        &self,
        req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let body = build_request_body(&req, true);
        let url = format!("{}/v1/messages", self.base_url);
        let response = self
            .agent
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .send_json(&body)
            .map_err(map_ureq_error)?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::openai::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        Ok(Box::new(AnthropicStream::new(reader, cancel.clone())))
    }
}

/// Iterator over a streaming Anthropic Messages response.
///
/// Drives an SSE reader and translates Anthropic's event vocabulary
/// (`message_start`, `content_block_*`, `message_delta`, `message_stop`)
/// into our generic [`ProviderEvent`] alphabet. Polls the [`CancelFlag`]
/// between each SSE event.
pub struct AnthropicStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    state: StreamState,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
}

#[derive(Default)]
struct StreamState {
    blocks: HashMap<usize, BlockBuilder>,
    usage: TokenUsage,
    stop_reason: StopReason,
}

enum BlockBuilder {
    Text,
    Thinking,
    ToolUse {
        id: ToolCallId,
        partial_input: String,
    },
}

impl AnthropicStream {
    /// Construct a stream from any byte source carrying Anthropic SSE.
    #[must_use]
    pub fn new(reader: Box<dyn Read + Send>, cancel: CancelFlag) -> Self {
        Self {
            reader: BufReader::new(reader),
            cancel,
            state: StreamState::default(),
            pending: VecDeque::new(),
            done: false,
        }
    }

    fn process_event(&mut self, name: &str, data: &str) {
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.pending
                    .push_back(Err(ProviderError::Decode(e.to_string())));
                return;
            }
        };
        match name {
            "message_start" => {
                self.pending.push_back(Ok(ProviderEvent::MessageStart));
                if let Some(usage) = value.pointer("/message/usage") {
                    self.absorb_usage(usage);
                }
            }
            "content_block_start" => self.on_block_start(&value),
            "content_block_delta" => self.on_block_delta(&value),
            "content_block_stop" => self.on_block_stop(&value),
            "message_delta" => {
                if let Some(stop) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.state.stop_reason = parse_stop_reason(stop);
                }
                if let Some(usage) = value.pointer("/usage") {
                    self.absorb_usage(usage);
                }
            }
            "message_stop" => {
                self.pending.push_back(Ok(ProviderEvent::MessageEnd {
                    stop_reason: self.state.stop_reason,
                    usage: self.state.usage,
                }));
                self.done = true;
            }
            "error" => {
                let msg = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_owned();
                self.pending.push_back(Err(ProviderError::Decode(msg)));
                self.done = true;
            }
            _ => {}
        }
    }

    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.state.usage.input = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.state.usage.output = v;
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.state.usage.cache_write = v;
        }
        if let Some(v) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.state.usage.cache_read = v;
        }
    }

    fn on_block_start(&mut self, value: &Value) {
        let index = block_index(value);
        let Some(block) = value.get("content_block") else {
            return;
        };
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                self.state.blocks.insert(index, BlockBuilder::Text);
            }
            "thinking" => {
                self.state.blocks.insert(index, BlockBuilder::Thinking);
            }
            "tool_use" => {
                let id_str = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let id = ToolCallId::new(id_str);
                self.state.blocks.insert(
                    index,
                    BlockBuilder::ToolUse {
                        id: id.clone(),
                        partial_input: String::new(),
                    },
                );
                self.pending
                    .push_back(Ok(ProviderEvent::ToolCallStart { id, name }));
            }
            _ => {}
        }
    }

    fn on_block_delta(&mut self, value: &Value) {
        let index = block_index(value);
        let Some(delta) = value.get("delta") else {
            return;
        };
        let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
        match dtype {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.pending
                    .push_back(Ok(ProviderEvent::TextDelta { delta: text }));
            }
            "thinking_delta" => {
                let text = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.pending
                    .push_back(Ok(ProviderEvent::ThinkingDelta { delta: text }));
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Some(BlockBuilder::ToolUse { id, partial_input }) =
                    self.state.blocks.get_mut(&index)
                {
                    partial_input.push_str(&partial);
                    self.pending.push_back(Ok(ProviderEvent::ToolCallArgsDelta {
                        id: id.clone(),
                        partial,
                    }));
                }
            }
            _ => {}
        }
    }

    fn on_block_stop(&mut self, value: &Value) {
        let index = block_index(value);
        if let Some(BlockBuilder::ToolUse { id, partial_input }) = self.state.blocks.remove(&index)
        {
            let input = if partial_input.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&partial_input).unwrap_or(Value::Null)
            };
            self.pending
                .push_back(Ok(ProviderEvent::ToolCallEnd { id, input }));
        }
    }
}

impl Iterator for AnthropicStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if self.done {
                return None;
            }
            if self.cancel.is_cancelled() {
                self.done = true;
                return Some(Err(ProviderError::Cancelled));
            }
            match read_sse_event(&mut self.reader) {
                Ok(Some(event)) => self.process_event(&event.name, &event.data),
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

#[derive(Debug)]
struct SseEvent {
    name: String,
    data: String,
}

fn read_sse_event<R: BufRead>(reader: &mut R) -> Result<Option<SseEvent>, ProviderError> {
    let mut name = String::new();
    let mut data = String::new();
    let mut have_content = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if n == 0 {
            if have_content {
                return Ok(Some(SseEvent { name, data }));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if have_content {
                return Ok(Some(SseEvent { name, data }));
            }
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            rest.trim_start().clone_into(&mut name);
            have_content = true;
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            have_content = true;
        }
    }
}

fn block_index(value: &Value) -> usize {
    value
        .get("index")
        .and_then(Value::as_u64)
        .map_or(0, |v| usize::try_from(v).unwrap_or(0))
}

fn parse_stop_reason(value: &str) -> StopReason {
    match value {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
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
    fn body_promotes_system_to_cached_array() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, false);
        let system = body["system"].as_array().expect("system is array");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "you are kage");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn body_marks_last_message_block_for_caching() {
        let req = StreamRequest::new("m", vec![user_msg("hello"), user_msg("again")]);
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        let last = &messages[messages.len() - 1];
        let blocks = last["content"].as_array().unwrap();
        let last_block = &blocks[blocks.len() - 1];
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn body_does_not_mark_earlier_messages() {
        let req = StreamRequest::new("m", vec![user_msg("first"), user_msg("second")]);
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        let first = &messages[0];
        let blocks = first["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
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

    fn stream_from_bytes(bytes: &'static [u8]) -> AnthropicStream {
        AnthropicStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    fn collect_ok(stream: AnthropicStream) -> Vec<ProviderEvent> {
        stream.map(|r| r.expect("stream item is Ok")).collect()
    }

    #[test]
    fn sse_parser_extracts_event_and_data() {
        let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
        let first = read_sse_event(&mut reader).unwrap().unwrap();
        assert_eq!(first.name, "message_start");
        assert!(first.data.contains("input_tokens"));
        let second = read_sse_event(&mut reader).unwrap().unwrap();
        assert_eq!(second.name, "message_stop");
        assert!(read_sse_event(&mut reader).unwrap().is_none());
    }

    #[test]
    fn sse_parser_ignores_comments_and_blank_lines() {
        let bytes: &[u8] =
            b": this is a comment\n\nevent: ping\ndata: {}\n\nevent: ping\ndata: {}\n\n";
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
        let first = read_sse_event(&mut reader).unwrap().unwrap();
        assert_eq!(first.name, "ping");
        let second = read_sse_event(&mut reader).unwrap().unwrap();
        assert_eq!(second.name, "ping");
    }

    #[test]
    fn stream_emits_text_deltas() {
        let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(matches!(events[0], ProviderEvent::MessageStart));
        assert!(
            matches!(&events[1], ProviderEvent::TextDelta { delta } if delta == "hello"),
            "got {:?}",
            events[1]
        );
        assert!(
            matches!(&events[2], ProviderEvent::TextDelta { delta } if delta == " world"),
            "got {:?}",
            events[2]
        );
        if let ProviderEvent::MessageEnd { stop_reason, usage } = events.last().unwrap() {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input, 5);
            assert_eq!(usage.output, 2);
        } else {
            panic!("expected MessageEnd at end, got {:?}", events.last());
        }
    }

    #[test]
    fn stream_emits_thinking_delta() {
        let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning...\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(events.iter().any(
            |e| matches!(e, ProviderEvent::ThinkingDelta { delta } if delta == "reasoning...")
        ));
    }

    #[test]
    fn stream_assembles_tool_call() {
        let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));

        let start_idx = events
            .iter()
            .position(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .expect("ToolCallStart present");
        if let ProviderEvent::ToolCallStart { id, name } = &events[start_idx] {
            assert_eq!(id.0, "call_1");
            assert_eq!(name, "read");
        }

        let args_count = events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ToolCallArgsDelta { .. }))
            .count();
        assert_eq!(args_count, 2);

        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { id, input } = end {
            assert_eq!(id.0, "call_1");
            assert_eq!(input["path"], "/tmp");
        }

        if let Some(ProviderEvent::MessageEnd { stop_reason, .. }) = events.last() {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_yields_cancelled_when_flag_set() {
        let bytes: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let cancel = CancelFlag::new();
        cancel.cancel();
        let stream = AnthropicStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
        let mut events = stream;
        let first = events.next();
        assert!(matches!(first, Some(Err(ProviderError::Cancelled))));
        assert!(events.next().is_none());
    }

    #[test]
    fn stream_propagates_error_event() {
        let bytes: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers are overloaded\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(ProviderError::Decode(msg)) => {
                assert!(msg.contains("overloaded"), "got {msg}");
            }
            other => panic!("expected Decode error, got {other:?}"),
        }
    }
}
