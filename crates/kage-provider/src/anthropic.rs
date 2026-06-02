//! Anthropic provider.
//!
//! Implements the Messages API (`POST /v1/messages`) using `ureq`. The
//! non-streaming `request` method buffers and decodes the full response;
//! the `Provider::stream` impl reads server-sent events line by line and
//! yields [`ProviderEvent`]s as they arrive.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read};

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
    client: crate::http::HttpClient,
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
            client: crate::http::HttpClient::new(),
        }
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

        let response = crate::http::send_blocking(&self.client, |agent| {
            agent
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .send_json(&body)
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::http::read_error_body(status, response));
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
    if let Some(budget) = resolve_thinking_budget(req) {
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }
    body
}

/// Resolve the thinking budget for an Anthropic request.
///
/// Prefers an explicit [`crate::ThinkingConfig`] when set; otherwise
/// looks up the [`crate::ThinkingLevel`] in the catalog's per-model
/// table (falling back to [`crate::ThinkingLevel::default_budget_tokens`]).
/// Returns `None` when neither is set or when the level is `Off`.
fn resolve_thinking_budget(req: &StreamRequest) -> Option<u32> {
    if let Some(thinking) = &req.thinking {
        return Some(thinking.budget_tokens);
    }
    let level = req.level?;
    if level.is_off() {
        return None;
    }
    crate::catalog::model("anthropic", &req.model)
        .and_then(|m| m.thinking_budget(level))
        .or_else(|| level.default_budget_tokens())
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
            let input = block
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            Some(Content::ToolCall {
                id: ToolCallId::new(id),
                name,
                input,
            })
        }
        _ => None,
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
        let api_key = self.api_key.clone();
        let response = crate::http::send(&self.client, cancel, move |agent| {
            agent
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .send_json(&body)
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::http::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let inner: EventStream = Box::new(AnthropicStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(inner, cancel.clone()))
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
                match serde_json::from_str::<Value>(&partial_input) {
                    Ok(v) => v,
                    Err(e) => {
                        self.pending.push_back(Err(ProviderError::Decode(format!(
                            "tool call {} input did not parse as JSON: {} (raw: {})",
                            id.0, e, partial_input
                        ))));
                        return;
                    }
                }
            };
            self.pending
                .push_back(Ok(ProviderEvent::ToolCallEnd { id, input }));
        }
    }
}

impl crate::sse::SseStreamCore for AnthropicStream {
    fn reader(&mut self) -> &mut BufReader<Box<dyn Read + Send>> {
        &mut self.reader
    }
    fn cancel(&self) -> &CancelFlag {
        &self.cancel
    }
    fn pending(&mut self) -> &mut VecDeque<Result<ProviderEvent, ProviderError>> {
        &mut self.pending
    }
    fn is_done(&self) -> bool {
        self.done
    }
    fn set_done(&mut self) {
        self.done = true;
    }
    fn process(&mut self, name: &str, data: &str) {
        self.process_event(name, data);
    }
}

impl Iterator for AnthropicStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        crate::sse::sse_next(self)
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
mod tests;
