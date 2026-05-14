//! `OpenAI` provider.
//!
//! Implements the Chat Completions streaming API (`POST /v1/chat/completions`)
//! using `ureq`. SSE format differs from Anthropic: chunks are `data:` lines
//! with no `event:` prefix, and the stream terminates with `data: [DONE]`.

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

/// `OpenAI` Chat Completions provider.
#[derive(Debug)]
pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    agent: ureq::Agent,
}

impl OpenAiProvider {
    /// Construct a provider from an API key, using the default base URL.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Construct a provider against a custom base URL.
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::compatible(
            api_key,
            base_url,
            ProviderMetadata {
                id: "openai".into(),
                display_name: "OpenAI".into(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
        )
    }

    /// Construct an `OpenAI`-compatible provider with caller-supplied metadata.
    ///
    /// Used by adapters for OpenAI-compatible APIs (ZAI, Mistral, Groq,
    /// `OpenRouter`, Cerebras, ...). The `base_url` should include any path
    /// segments before `/chat/completions` (for `OpenAI` itself, that is
    /// the trailing `/v1`).
    #[must_use]
    pub fn compatible(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        metadata: ProviderMetadata,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            metadata,
            agent: build_agent(),
        }
    }
}

/// Construct a [`ureq::Agent`] with status-code-as-error disabled so we can
/// surface the upstream response body in [`ProviderError::Http`] instead of
/// throwing it away.
pub(crate) fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent()
}

impl Provider for OpenAiProvider {
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
        let url = format!("{}/chat/completions", self.base_url);
        let agent = self.agent.clone();
        let api_key = self.api_key.clone();
        let response = crate::cancelable::cancellable_call(cancel, move || {
            agent
                .post(&url)
                .header("authorization", &format!("Bearer {api_key}"))
                .header("content-type", "application/json")
                .send_json(&body)
                .map_err(map_ureq_error)
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let inner: EventStream = Box::new(OpenAiStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(inner, cancel.clone()))
    }
}

/// Read the body of a non-2xx response into [`ProviderError::Http`].
///
/// Caps the body at 8 KiB so a misbehaving upstream cannot blow up our error
/// strings; what we keep is enough to surface the JSON error payload that
/// every major provider returns for 4xx/5xx.
pub(crate) fn read_error_body(
    status: u16,
    response: ureq::http::Response<ureq::Body>,
) -> ProviderError {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(8 * 1024)
        .read_to_end(&mut buf);
    let body = String::from_utf8_lossy(&buf).into_owned();
    ProviderError::Http { status, body }
}

/// Build the JSON body for a Chat Completions request.
pub(crate) fn build_request_body(req: &StreamRequest, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &req.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    }
    for msg in &req.messages {
        if let Some(converted) = internal_message_to_openai(msg) {
            messages.extend(converted);
        }
    }

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "stream": stream,
    });
    if stream {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(level) = req.level
        && let Some(effort) = level.openai_reasoning_effort()
    {
        body["reasoning_effort"] = serde_json::json!(effort);
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::to_value(
            req.tools
                .iter()
                .map(tool_spec_to_openai)
                .collect::<Vec<_>>(),
        )
        .expect("tool spec serializes");
    }
    body
}

fn tool_spec_to_openai(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.schema,
        },
    })
}

fn internal_message_to_openai(msg: &Message) -> Option<Vec<Value>> {
    match msg.role {
        Role::User => {
            let blocks = convert_user_blocks(&msg.content);
            if blocks.is_empty() {
                None
            } else {
                Some(vec![serde_json::json!({
                    "role": "user",
                    "content": blocks,
                })])
            }
        }
        Role::Assistant => Some(vec![convert_assistant_message(&msg.content)]),
        Role::ToolResult => Some(convert_tool_result_messages(&msg.content)),
        Role::System => None,
    }
}

fn convert_user_blocks(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type":"text","text":text})),
            Content::Image { source, .. } => Some(image_to_openai(source)),
            _ => None,
        })
        .collect()
}

fn convert_assistant_message(blocks: &[Content]) -> Value {
    let mut text_parts: Vec<&str> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            Content::Text { text } => text_parts.push(text),
            Content::ToolCall { id, name, input } => {
                tool_calls.push(serde_json::json!({
                    "id": id.0,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string(),
                    },
                }));
            }
            _ => {}
        }
    }
    let content_value = if text_parts.is_empty() {
        Value::Null
    } else {
        Value::String(text_parts.join(""))
    };
    let mut msg = serde_json::json!({
        "role": "assistant",
        "content": content_value,
    });
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    msg
}

fn convert_tool_result_messages(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::ToolResultBlock {
                call_id, output, ..
            } => Some(serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id.0,
                "content": output,
            })),
            _ => None,
        })
        .collect()
}

fn image_to_openai(source: &kage_core::ImageSource) -> Value {
    match source {
        kage_core::ImageSource::Url { url } => serde_json::json!({
            "type": "image_url",
            "image_url": {"url": url},
        }),
        kage_core::ImageSource::Base64 { data } => serde_json::json!({
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{data}")},
        }),
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

/// Iterator over an `OpenAI` streaming response.
pub struct OpenAiStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    started: bool,
    /// Buffered tool-call assembly state, keyed by index.
    tool_calls: BTreeMap<usize, ToolCallBuilder>,
    /// Most recent finish reason observed.
    finish_reason: StopReason,
    /// Token accounting from the final usage chunk.
    usage: kage_core::TokenUsage,
}

struct ToolCallBuilder {
    id: ToolCallId,
    args: String,
    started: bool,
}

impl OpenAiStream {
    /// Construct a stream from any byte source carrying `OpenAI` SSE.
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
        if data == "[DONE]" {
            self.flush_pending_tool_calls();
            self.pending.push_back(Ok(ProviderEvent::MessageEnd {
                stop_reason: self.finish_reason,
                usage: self.usage,
            }));
            self.done = true;
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
        if !self.started {
            self.pending.push_back(Ok(ProviderEvent::MessageStart));
            self.started = true;
        }
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            self.absorb_usage(usage);
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                self.process_delta(delta);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = parse_finish_reason(reason);
            }
        }
    }

    fn process_delta(&mut self, delta: &Value) {
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                self.pending.push_back(Ok(ProviderEvent::TextDelta {
                    delta: content.to_owned(),
                }));
            }
        }
        let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        for tc in tool_calls {
            self.process_tool_call_delta(tc);
        }
    }

    fn process_tool_call_delta(&mut self, tc: &Value) {
        let index = tc
            .get("index")
            .and_then(Value::as_u64)
            .map_or(0, |v| usize::try_from(v).unwrap_or(0));
        let id_str = tc.get("id").and_then(Value::as_str);
        let function = tc.get("function");
        let name = function.and_then(|f| f.get("name")).and_then(Value::as_str);
        let args = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str);

        let entry = self
            .tool_calls
            .entry(index)
            .or_insert_with(|| ToolCallBuilder {
                id: ToolCallId::new(""),
                args: String::new(),
                started: false,
            });
        if let Some(id) = id_str {
            entry.id = ToolCallId::new(id);
        }
        if !entry.started {
            if let Some(n) = name {
                entry.started = true;
                self.pending.push_back(Ok(ProviderEvent::ToolCallStart {
                    id: entry.id.clone(),
                    name: n.to_owned(),
                }));
            }
        }
        if let Some(partial) = args {
            entry.args.push_str(partial);
            if !partial.is_empty() {
                self.pending.push_back(Ok(ProviderEvent::ToolCallArgsDelta {
                    id: entry.id.clone(),
                    partial: partial.to_owned(),
                }));
            }
        }
    }

    fn flush_pending_tool_calls(&mut self) {
        let calls = std::mem::take(&mut self.tool_calls);
        for (_, builder) in calls {
            if !builder.started {
                continue;
            }
            let input = if builder.args.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str::<Value>(&builder.args) {
                    Ok(v) => v,
                    Err(e) => {
                        self.pending.push_back(Err(ProviderError::Decode(format!(
                            "tool call {} arguments did not parse as JSON: {} (raw: {})",
                            builder.id.0, e, builder.args
                        ))));
                        continue;
                    }
                }
            };
            self.pending.push_back(Ok(ProviderEvent::ToolCallEnd {
                id: builder.id,
                input,
            }));
        }
    }

    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("prompt_tokens").and_then(Value::as_u64) {
            self.usage.input = v;
        }
        if let Some(v) = usage.get("completion_tokens").and_then(Value::as_u64) {
            self.usage.output = v;
        }
    }
}

impl Iterator for OpenAiStream {
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

/// Read one `data:` chunk from an `OpenAI` SSE stream.
///
/// Returns `Ok(Some(payload))` when a chunk is found, `Ok(None)` at EOF.
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
        if trimmed.is_empty() || trimmed.starts_with(':') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            return Ok(Some(rest.trim_start().to_owned()));
        }
    }
}

fn parse_finish_reason(value: &str) -> StopReason {
    match value {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        _ => StopReason::Other,
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
    fn body_includes_model_and_messages() {
        let req = StreamRequest::new("gpt-4o", vec![user_msg("hi")]);
        let body = build_request_body(&req, false);
        assert_eq!(body["model"], "gpt-4o");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn body_prepends_system_message() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "you are kage");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn body_wraps_tools_in_function_envelope() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req, false);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn body_translates_thinking_level_to_reasoning_effort() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::Medium);
        let body = build_request_body(&req, false);
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn body_caps_xhigh_at_high_for_openai() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::XHigh);
        let body = build_request_body(&req, false);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn body_omits_reasoning_effort_when_level_off() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::Off);
        let body = build_request_body(&req, false);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn body_includes_stream_options_only_when_streaming() {
        let req = StreamRequest::new("m", vec![user_msg("hi")]);
        let body = build_request_body(&req, false);
        assert!(body.get("stream_options").is_none());
        let body = build_request_body(&req, true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn assistant_with_tool_call_stringifies_arguments() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "read".into(),
                input: serde_json::json!({"path":"/x"}),
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![user_msg("read"), assistant]);
        let body = build_request_body(&req, false);
        let messages = body["messages"].as_array().unwrap();
        let last = &messages[messages.len() - 1];
        assert_eq!(last["role"], "assistant");
        assert!(last["content"].is_null());
        let tc = &last["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["function"]["name"], "read");
        let args_str = tc["function"]["arguments"].as_str().unwrap();
        let args: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["path"], "/x");
    }

    #[test]
    fn tool_result_emits_role_tool_message() {
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
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_1");
        assert_eq!(messages[0]["content"], "127.0.0.1");
    }

    fn stream_from_bytes(bytes: &'static [u8]) -> OpenAiStream {
        OpenAiStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    fn collect_ok(stream: OpenAiStream) -> Vec<ProviderEvent> {
        stream.map(|r| r.expect("stream item is Ok")).collect()
    }

    #[test]
    fn stream_emits_text_deltas_and_message_end() {
        let bytes: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
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
        if let ProviderEvent::MessageEnd { stop_reason, usage } = events.last().unwrap() {
            assert_eq!(*stop_reason, StopReason::EndTurn);
            assert_eq!(usage.input, 10);
            assert_eq!(usage.output, 2);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_assembles_tool_call_from_arg_chunks() {
        let bytes: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n";
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
    fn stream_emits_decode_error_when_tool_args_are_malformed_json() {
        let bytes: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{not json\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
        let s = stream_from_bytes(bytes);
        let events: Vec<_> = s.collect();
        let decode_err = events
            .iter()
            .find(|r| matches!(r, Err(ProviderError::Decode(_))))
            .expect("expected a Decode error event for malformed tool args");
        if let Err(ProviderError::Decode(msg)) = decode_err {
            assert!(msg.contains("call_1"), "error should name the tool call id");
            assert!(
                msg.contains("{not json"),
                "error should include the raw args"
            );
        }
        assert!(
            !events
                .iter()
                .any(|r| matches!(r, Ok(ProviderEvent::ToolCallEnd { .. }))),
            "no ToolCallEnd should fire when args fail to parse"
        );
    }

    #[test]
    fn stream_yields_cancelled_when_flag_set() {
        let bytes: &[u8] =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = OpenAiStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }
}
