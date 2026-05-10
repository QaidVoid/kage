//! Google Gemini provider.
//!
//! Implements `streamGenerateContent` on the Generative Language API. The
//! streaming endpoint returns a JSON array as Server-Sent Events with
//! `data:` chunks. Function calls and text are emitted in the response
//! `candidates[0].content.parts` array.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Role, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StopReason,
    StreamRequest, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// Google Gemini provider.
#[derive(Debug)]
pub struct GeminiProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    agent: ureq::Agent,
}

impl GeminiProvider {
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
                id: "gemini".into(),
                display_name: "Google Gemini".into(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
            agent: crate::openai::build_agent(),
        }
    }
}

impl Provider for GeminiProvider {
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
        let body = build_request_body(&req);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url, req.model, self.api_key,
        );
        let response = self
            .agent
            .post(&url)
            .header("content-type", "application/json")
            .send_json(&body)
            .map_err(map_ureq_error)?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::openai::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let inner: EventStream = Box::new(GeminiStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(inner, cancel.clone()))
    }
}

/// Build the JSON body for a Gemini streamGenerateContent request.
pub(crate) fn build_request_body(req: &StreamRequest) -> Value {
    let contents: Vec<Value> = req
        .messages
        .iter()
        .filter_map(internal_message_to_gemini)
        .collect();

    let mut body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        },
    });

    if let Some(system) = &req.system {
        body["systemInstruction"] = serde_json::json!({
            "parts": [{"text": system}],
        });
    }
    if let Some(temp) = req.temperature {
        body["generationConfig"]["temperature"] = serde_json::json!(temp);
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::json!([{
            "functionDeclarations": req.tools.iter().map(tool_spec_to_gemini).collect::<Vec<_>>(),
        }]);
    }
    body
}

fn tool_spec_to_gemini(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.schema,
    })
}

fn internal_message_to_gemini(msg: &Message) -> Option<Value> {
    let (role, parts) = match msg.role {
        Role::User => ("user", convert_user_parts(&msg.content)),
        Role::Assistant => ("model", convert_assistant_parts(&msg.content)),
        Role::ToolResult => ("user", convert_tool_result_parts(&msg.content)),
        Role::System => return None,
    };
    if parts.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": role,
        "parts": parts,
    }))
}

fn convert_user_parts(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"text": text})),
            Content::Image { source, mime } => Some(image_part(source, mime)),
            _ => None,
        })
        .collect()
}

fn convert_assistant_parts(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"text": text})),
            Content::ToolCall { name, input, .. } => Some(serde_json::json!({
                "functionCall": {
                    "name": name,
                    "args": input,
                },
            })),
            _ => None,
        })
        .collect()
}

fn convert_tool_result_parts(blocks: &[Content]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::ToolResultBlock {
                call_id, output, ..
            } => Some(serde_json::json!({
                "functionResponse": {
                    "name": call_id.0,
                    "response": {"output": output},
                },
            })),
            _ => None,
        })
        .collect()
}

fn image_part(source: &kage_core::ImageSource, mime: &str) -> Value {
    match source {
        kage_core::ImageSource::Base64 { data } => serde_json::json!({
            "inlineData": {
                "mimeType": mime,
                "data": data,
            },
        }),
        kage_core::ImageSource::Url { url } => serde_json::json!({
            "fileData": {
                "mimeType": mime,
                "fileUri": url,
            },
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

/// Iterator over a Gemini streaming response.
pub struct GeminiStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    started: bool,
    /// Stable id assigned to each unique tool name encountered, since
    /// Gemini does not emit per-call ids and we need correlation ids.
    tool_call_ids: BTreeMap<String, ToolCallId>,
    finish_reason: StopReason,
    usage: kage_core::TokenUsage,
}

impl GeminiStream {
    /// Construct a stream from any byte source carrying Gemini SSE.
    #[must_use]
    pub fn new(reader: Box<dyn Read + Send>, cancel: CancelFlag) -> Self {
        Self {
            reader: BufReader::new(reader),
            cancel,
            pending: VecDeque::new(),
            done: false,
            started: false,
            tool_call_ids: BTreeMap::new(),
            finish_reason: StopReason::Other,
            usage: kage_core::TokenUsage::default(),
        }
    }

    fn process_chunk(&mut self, data: &str) {
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
        if let Some(usage) = value.get("usageMetadata") {
            self.absorb_usage(usage);
        }
        let Some(candidates) = value.get("candidates").and_then(Value::as_array) else {
            return;
        };
        for candidate in candidates {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    self.process_part(part);
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish_reason = parse_finish_reason(reason);
            }
        }
    }

    fn process_part(&mut self, part: &Value) {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                self.pending.push_back(Ok(ProviderEvent::TextDelta {
                    delta: text.to_owned(),
                }));
            }
            return;
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let args = call
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            let id = self
                .tool_call_ids
                .entry(name.clone())
                .or_insert_with(|| ToolCallId::new(format!("gemini_{name}")))
                .clone();
            self.pending.push_back(Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name,
            }));
            self.pending
                .push_back(Ok(ProviderEvent::ToolCallEnd { id, input: args }));
        }
    }

    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("promptTokenCount").and_then(Value::as_u64) {
            self.usage.input = v;
        }
        if let Some(v) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
            self.usage.output = v;
        }
    }

    fn emit_message_end(&mut self) {
        self.pending.push_back(Ok(ProviderEvent::MessageEnd {
            stop_reason: self.finish_reason,
            usage: self.usage,
        }));
        self.done = true;
    }
}

impl Iterator for GeminiStream {
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
                    if self.started {
                        self.emit_message_end();
                        continue;
                    }
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
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
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
    fn body_has_contents_and_generation_config() {
        let req = StreamRequest::new("gemini-2.0-flash", vec![user_msg("hi")]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 4_096);
    }

    #[test]
    fn body_promotes_system_to_system_instruction() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req);
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "you are kage"
        );
    }

    #[test]
    fn body_wraps_tools_in_function_declarations() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body = build_request_body(&req);
        let decls = body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls[0]["name"], "read");
        assert_eq!(decls[0]["parameters"]["type"], "object");
    }

    #[test]
    fn assistant_with_tool_call_emits_function_call_part() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("ignored"),
                name: "read".into(),
                input: serde_json::json!({"path":"/x"}),
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![user_msg("read"), assistant]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "read");
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["args"]["path"],
            "/x"
        );
    }

    #[test]
    fn tool_result_emits_function_response() {
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("read"),
                output: "127.0.0.1".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![result]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        let fr = &contents[0]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "read");
        assert_eq!(fr["response"]["output"], "127.0.0.1");
    }

    fn stream_from_bytes(bytes: &'static [u8]) -> GeminiStream {
        GeminiStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    fn collect_ok(stream: GeminiStream) -> Vec<ProviderEvent> {
        stream.map(|r| r.expect("stream item is Ok")).collect()
    }

    #[test]
    fn stream_emits_text_deltas() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}],\"role\":\"model\"},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1}}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n\n";
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
            assert_eq!(usage.input, 5);
            assert_eq!(usage.output, 2);
        } else {
            panic!("expected MessageEnd");
        }
    }

    #[test]
    fn stream_emits_function_call_as_start_and_end() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"/tmp\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":3}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let start = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .expect("ToolCallStart present");
        if let ProviderEvent::ToolCallStart { name, .. } = start {
            assert_eq!(name, "read");
        }
        let end = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        if let ProviderEvent::ToolCallEnd { input, .. } = end {
            assert_eq!(input["path"], "/tmp");
        }
    }

    #[test]
    fn stream_yields_cancelled_when_flag_set() {
        let bytes: &[u8] = b"data: {\"candidates\":[]}\n\n";
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = GeminiStream::new(Box::new(std::io::Cursor::new(bytes)), cancel);
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }
}
