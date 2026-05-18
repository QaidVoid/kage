//! `OpenAI` Responses API provider.
//!
//! Distinct from Chat Completions: the Responses API
//! (`POST /v1/responses`) takes an `input` array of typed items
//! (`message`, `function_call`, `function_call_output`, ...) instead
//! of a `messages` array, surfaces native reasoning items, and emits
//! a richer SSE event vocabulary. The Codex (`gpt-5-codex`,
//! `gpt-5.1-codex-*`) models are first-class here.
//!
//! Provider id: `openai-responses`. Users address models as
//! `openai-responses:gpt-5-codex`. The host shares its `OPENAI_API_KEY`
//! with this provider; nothing in the credential store is duplicated.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Role, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StopReason,
    StreamRequest, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// `OpenAI` Responses API provider.
#[derive(Debug)]
pub struct OpenAiResponsesProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    agent: ureq::Agent,
}

impl OpenAiResponsesProvider {
    /// Construct a provider from an API key, using the default base URL.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Construct a provider against a custom base URL.
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            metadata: ProviderMetadata {
                id: "openai-responses".into(),
                display_name: "OpenAI Responses".into(),
                supports_caching: false,
                supports_thinking: true,
                supports_tool_use: true,
            },
            agent: crate::http::build_agent(),
        }
    }
}

impl Provider for OpenAiResponsesProvider {
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
        let url = format!("{}/responses", self.base_url);
        let agent = self.agent.clone();
        let api_key = self.api_key.clone();
        let response = crate::cancelable::cancellable_call(cancel, move || {
            agent
                .post(&url)
                .header("authorization", &format!("Bearer {api_key}"))
                .header("content-type", "application/json")
                .send_json(&body)
                .map_err(crate::http::map_ureq_error)
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::http::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let inner: EventStream = Box::new(ResponsesStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(inner, cancel.clone()))
    }
}

/// Build the JSON body for a Responses API request.
pub(crate) fn build_request_body(req: &StreamRequest, stream: bool) -> Value {
    let input: Vec<Value> = req
        .messages
        .iter()
        .flat_map(internal_message_to_responses)
        .collect();

    let mut body = serde_json::json!({
        "model": req.model,
        "input": input,
        "max_output_tokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "stream": stream,
    });
    if let Some(system) = &req.system {
        body["instructions"] = Value::String(system.clone());
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(level) = req.level
        && let Some(effort) = level.openai_reasoning_effort()
    {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(tool_spec_to_responses).collect());
    }
    body
}

/// Convert one [`ToolSpec`] into the Responses API tool shape.
///
/// Unlike Chat Completions, the Responses API uses a flat `function`
/// type (no nested `function` wrapper): `{ type, name, description,
/// parameters }`.
fn tool_spec_to_responses(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.schema,
    })
}

/// Convert one internal [`Message`] into zero-or-more Responses input
/// items. Assistant messages expand to a `message` item plus one
/// `function_call` item per [`Content::ToolCall`]; tool results become
/// `function_call_output` items.
fn internal_message_to_responses(msg: &Message) -> Vec<Value> {
    match msg.role {
        Role::User => {
            let parts = convert_user_parts(&msg.content);
            if parts.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                })]
            }
        }
        Role::Assistant => convert_assistant_items(&msg.content),
        Role::ToolResult => convert_tool_result_items(&msg.content),
        Role::System => Vec::new(),
    }
}

fn convert_user_parts(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"input_text","text":text})),
            Content::Image { source, .. } => Some(image_part(source)),
            _ => None,
        })
        .collect()
}

fn convert_assistant_items(blocks: &[Content]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut text_parts: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            Content::Text { text } => {
                text_parts.push(serde_json::json!({"type":"output_text","text":text}));
            }
            Content::ToolCall { id, name, input } => {
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id.0,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            _ => {}
        }
    }
    if !text_parts.is_empty() {
        items.insert(
            0,
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": text_parts,
            }),
        );
    }
    items
}

fn convert_tool_result_items(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::ToolResultBlock {
                call_id, output, ..
            } => Some(serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id.0,
                "output": output,
            })),
            _ => None,
        })
        .collect()
}

fn image_part(source: &kage_core::ImageSource) -> Value {
    match source {
        kage_core::ImageSource::Url { url } => serde_json::json!({
            "type": "input_image",
            "image_url": url,
        }),
        kage_core::ImageSource::Base64 { data } => serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:image/png;base64,{data}"),
        }),
    }
}

/// Iterator over a Responses API streaming response.
pub struct ResponsesStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    started: bool,
    /// Function-call assembly state, keyed by `output_index`. The
    /// `ToolCallStart` event fires immediately when `output_item.added`
    /// arrives (the upstream always sends the tool name in that
    /// event); subsequent argument deltas append to `args` and emit
    /// `ToolCallArgsDelta` against the same id.
    tool_calls: BTreeMap<usize, ToolCallBuilder>,
    finish_reason: StopReason,
    usage: kage_core::TokenUsage,
}

struct ToolCallBuilder {
    id: ToolCallId,
    args: String,
}

impl ResponsesStream {
    /// Construct a stream from any byte source carrying Responses SSE.
    #[must_use]
    pub fn new(reader: Box<dyn Read + Send>, cancel: CancelFlag) -> Self {
        Self {
            reader: BufReader::new(reader),
            cancel,
            pending: VecDeque::new(),
            done: false,
            started: false,
            tool_calls: BTreeMap::new(),
            finish_reason: StopReason::Other,
            usage: kage_core::TokenUsage::default(),
        }
    }

    fn process_chunk(&mut self, data: &str) {
        if data.trim() == "[DONE]" {
            self.emit_message_end();
            return;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.pending
                    .push_back(Err(ProviderError::Decode(e.to_string())));
                return;
            }
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match kind.as_str() {
            "response.created" | "response.in_progress" if !self.started => {
                self.pending.push_back(Ok(ProviderEvent::MessageStart));
                self.started = true;
            }
            "response.output_item.added" => self.on_output_item_added(&value),
            "response.output_text.delta" => self.on_text_delta(&value),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.on_thinking_delta(&value);
            }
            "response.function_call_arguments.delta" => self.on_function_call_args_delta(&value),
            "response.output_item.done" => self.on_output_item_done(&value),
            "response.completed" => {
                self.absorb_completed(&value);
                self.emit_message_end();
            }
            "response.incomplete" => {
                self.absorb_completed(&value);
                self.finish_reason = StopReason::MaxTokens;
                self.emit_message_end();
            }
            "response.failed" | "error" => {
                let msg = value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("openai responses stream failed")
                    .to_owned();
                self.pending
                    .push_back(Err(ProviderError::Decode(format!("responses: {msg}"))));
                self.done = true;
            }
            _ => {}
        }
    }

    fn on_output_item_added(&mut self, value: &Value) {
        let Some(item) = value.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type != "function_call" {
            return;
        }
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = ToolCallId::new(call_id);
        self.tool_calls.insert(
            index,
            ToolCallBuilder {
                id: id.clone(),
                args: String::new(),
            },
        );
        self.pending
            .push_back(Ok(ProviderEvent::ToolCallStart { id, name }));
    }

    fn on_text_delta(&mut self, value: &Value) {
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.pending.push_back(Ok(ProviderEvent::TextDelta {
            delta: delta.to_owned(),
        }));
    }

    fn on_thinking_delta(&mut self, value: &Value) {
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.pending.push_back(Ok(ProviderEvent::ThinkingDelta {
            delta: delta.to_owned(),
        }));
    }

    fn on_function_call_args_delta(&mut self, value: &Value) {
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        let Some(entry) = self.tool_calls.get_mut(&index) else {
            return;
        };
        entry.args.push_str(delta);
        self.pending.push_back(Ok(ProviderEvent::ToolCallArgsDelta {
            id: entry.id.clone(),
            partial: delta.to_owned(),
        }));
    }

    fn on_output_item_done(&mut self, value: &Value) {
        let item_type = value
            .pointer("/item/type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if item_type != "function_call" {
            return;
        }
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let Some(builder) = self.tool_calls.remove(&index) else {
            return;
        };
        // Prefer the assembled-from-deltas args; fall back to the full
        // `arguments` string echoed on the `done` event if no deltas
        // arrived (e.g. servers that batch function-call output).
        let raw = if builder.args.is_empty() {
            value
                .pointer("/item/arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        } else {
            builder.args
        };
        let input = if raw.is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str::<Value>(&raw) {
                Ok(v) => v,
                Err(e) => {
                    self.pending.push_back(Err(ProviderError::Decode(format!(
                        "tool call {} arguments did not parse as JSON: {} (raw: {})",
                        builder.id.0, e, raw
                    ))));
                    self.finish_reason = StopReason::ToolUse;
                    return;
                }
            }
        };
        self.finish_reason = StopReason::ToolUse;
        self.pending.push_back(Ok(ProviderEvent::ToolCallEnd {
            id: builder.id,
            input,
        }));
    }

    fn absorb_completed(&mut self, value: &Value) {
        if let Some(usage) = value.pointer("/response/usage") {
            if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
                self.usage.input = v;
            }
            if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
                self.usage.output = v;
            }
            if let Some(v) = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
            {
                self.usage.cache_read = v;
            }
        }
        // Promote `tool_calls` finish from any in-flight call; otherwise
        // a completed response with no tool calls is an end_turn.
        if matches!(self.finish_reason, StopReason::Other) {
            self.finish_reason = StopReason::EndTurn;
        }
    }

    fn emit_message_end(&mut self) {
        if !self.started {
            self.pending.push_back(Ok(ProviderEvent::MessageStart));
            self.started = true;
        }
        self.pending.push_back(Ok(ProviderEvent::MessageEnd {
            stop_reason: self.finish_reason,
            usage: self.usage,
        }));
        self.done = true;
    }
}

impl Iterator for ResponsesStream {
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
            match read_chunk(&mut self.reader) {
                Ok(Some(data)) => self.process_chunk(&data),
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

/// Read one `data:` chunk from a Responses API SSE stream. The
/// upstream uses the standard `event: <name>\ndata: <json>\n\n` form,
/// but the JSON body carries its own `type` field so we ignore the
/// `event:` line and parse from `data:` directly.
fn read_chunk<R: BufRead>(reader: &mut R) -> Result<Option<String>, ProviderError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() || trimmed.starts_with(':') || trimmed.starts_with("event:") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            return Ok(Some(rest.trim_start().to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn body_uses_input_array_and_instructions_field() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, true);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "you are kage");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn body_uses_flat_function_tool_shape() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req, true);
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "read");
        assert_eq!(tool["description"], "read a file");
        assert_eq!(tool["parameters"]["type"], "object");
        // No nested {"function": {...}} envelope (that's Chat Completions).
        assert!(tool.get("function").is_none());
    }

    #[test]
    fn body_includes_reasoning_block_when_level_set() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::High);
        let body = build_request_body(&req, true);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn body_omits_reasoning_when_level_off() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::Off);
        let body = build_request_body(&req, true);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn assistant_with_tool_call_emits_function_call_item() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "read".into(),
                input: serde_json::json!({"path":"/x"}),
            }],
            None,
        );
        let req = StreamRequest::new("gpt-5", vec![user_msg("read"), assistant]);
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        let last = &input[input.len() - 1];
        assert_eq!(last["type"], "function_call");
        assert_eq!(last["call_id"], "call_1");
        assert_eq!(last["name"], "read");
        let args_str = last["arguments"].as_str().unwrap();
        let args: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["path"], "/x");
    }

    #[test]
    fn assistant_with_text_and_tool_call_emits_both_items_in_order() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Text {
                    text: "let me check".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("call_1"),
                    name: "read".into(),
                    input: serde_json::json!({"path":"/x"}),
                },
            ],
            None,
        );
        let req = StreamRequest::new("gpt-5", vec![user_msg("read"), assistant]);
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        // user, then assistant message, then function_call.
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
    }

    #[test]
    fn tool_result_emits_function_call_output_item() {
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("call_1"),
                output: "127.0.0.1".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new("gpt-5", vec![result]);
        let body = build_request_body(&req, true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["output"], "127.0.0.1");
    }

    fn stream_from_bytes(bytes: &'static [u8]) -> ResponsesStream {
        ResponsesStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    fn collect_ok(stream: ResponsesStream) -> Vec<ProviderEvent> {
        stream.map(|r| r.expect("stream item is Ok")).collect()
    }

    #[test]
    fn stream_emits_text_deltas_and_message_end() {
        let bytes: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(matches!(events[0], ProviderEvent::MessageStart));
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hello", " world"]);
        if let Some(ProviderEvent::MessageEnd { stop_reason, usage }) = events.last() {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input, 10);
            assert_eq!(usage.output, 2);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_emits_thinking_deltas_for_reasoning_summary() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"considering\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let thinking: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ThinkingDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["considering"]);
    }

    #[test]
    fn stream_assembles_tool_call_from_arg_deltas() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"/tmp\\\"}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":3}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let start = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .expect("ToolCallStart present");
        if let ProviderEvent::ToolCallStart { id, name } = start {
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
        }
    }

    #[test]
    fn stream_falls_back_to_arguments_on_output_item_done_when_no_deltas() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"ls\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"ls\",\"arguments\":\"{\\\"path\\\":\\\"/\\\"}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { id, input } = end {
            assert_eq!(id.0, "call_2");
            assert_eq!(input["path"], "/");
        }
    }

    #[test]
    fn stream_emits_decode_error_when_tool_args_are_malformed_json() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"write\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{not json\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let s = stream_from_bytes(bytes);
        let events: Vec<_> = s.collect();
        let decode_err = events
            .iter()
            .find(|r| matches!(r, Err(ProviderError::Decode(_))))
            .expect("expected a Decode error event for malformed tool args");
        if let Err(ProviderError::Decode(msg)) = decode_err {
            assert!(msg.contains("call_1"));
            assert!(msg.contains("{not json"));
        }
    }

    #[test]
    fn stream_propagates_failed_event_as_decode_error() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.failed\",\"error\":{\"message\":\"rate limited\"}}\n\n";
        let s = stream_from_bytes(bytes);
        let events: Vec<_> = s.collect();
        let err = events
            .iter()
            .find(|r| matches!(r, Err(ProviderError::Decode(_))))
            .expect("expected a Decode error for response.failed");
        if let Err(ProviderError::Decode(msg)) = err {
            assert!(msg.contains("rate limited"));
        }
    }

    #[test]
    fn stream_yields_cancelled_when_flag_set() {
        let bytes: &[u8] = b"data: {\"type\":\"response.created\"}\n\n";
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = ResponsesStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }
}
