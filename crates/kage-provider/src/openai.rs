//! `OpenAI` provider.
//!
//! Implements the Chat Completions streaming API (`POST /v1/chat/completions`)
//! using `ureq`. SSE format differs from Anthropic: chunks are `data:` lines
//! with no `event:` prefix, and the stream terminates with `data: [DONE]`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Reasoning, ReasoningField, Role, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, ProviderModel,
    StopReason, StreamRequest, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// `OpenAI` Chat Completions provider.
#[derive(Debug)]
pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    client: crate::http::HttpClient,
    /// Extra headers sent on every request, after the protocol's own.
    extra_headers: BTreeMap<String, String>,
    /// Models advertised from `Provider::models` (custom providers);
    /// empty lets the catalog drive the picker.
    models: Vec<ProviderModel>,
    /// Request shape the upstream expects.
    dialect: Dialect,
}

/// Request shape of an OpenAI-compatible upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    /// Chat Completions as `OpenAI` documents it.
    OpenAi,
    /// Z.AI and Zhipu: `max_tokens` instead of `max_completion_tokens`,
    /// thinking as `thinking`, and `tool_stream` for streamed tool
    /// call arguments.
    Zai,
}

impl Dialect {
    /// The dialect of provider `id` at `base_url`: Z.AI for the `zai`,
    /// `zai-coding-plan` and `zhipuai-coding-plan` ids and for any
    /// `api.z.ai` or `open.bigmodel.cn` endpoint.
    fn detect(id: &str, base_url: &str) -> Self {
        let zai = matches!(id, "zai" | "zai-coding-plan" | "zhipuai-coding-plan")
            || base_url.contains("api.z.ai")
            || base_url.contains("open.bigmodel.cn");
        if zai { Self::Zai } else { Self::OpenAi }
    }
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
        let base_url = base_url.into();
        Self {
            api_key: api_key.into(),
            dialect: Dialect::detect(&metadata.id, &base_url),
            base_url,
            metadata,
            client: crate::http::HttpClient::new(),
            extra_headers: BTreeMap::new(),
            models: Vec::new(),
        }
    }

    /// Send `headers` on every request, after the protocol's own headers.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Advertise `models` from [`Provider::models`] instead of relying
    /// on the catalog.
    #[must_use]
    pub fn with_models(mut self, models: Vec<ProviderModel>) -> Self {
        self.models = models;
        self
    }

    /// Headers every request carries: content type, the bearer
    /// credential (skipped when no key is configured, e.g. local
    /// endpoints), then the configured extras in key order.
    fn request_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        if !self.api_key.is_empty() {
            headers.push((
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            ));
        }
        for (name, value) in &self.extra_headers {
            headers.push((name.clone(), value.clone()));
        }
        headers
    }

    /// The field `model` reads its reasoning back from during a tool
    /// loop, from the provider's own model list or the catalog.
    fn interleaved(&self, model: &str) -> Option<ReasoningField> {
        match self.models.iter().find(|m| m.id == model) {
            Some(m) => m.interleaved,
            None => crate::catalog::model(&self.metadata.id, model).and_then(|m| m.interleaved),
        }
    }
}

impl Provider for OpenAiProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn models(&self) -> Vec<ProviderModel> {
        self.models.clone()
    }

    /// Interleaved models get the current turn's thinking in their
    /// reasoning field; other models get it as `<thinking>` text, see
    /// `build_request_body`.
    fn preserves_thinking(&self) -> bool {
        true
    }

    fn stream(
        &self,
        req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let interleaved = self.interleaved(&req.model);
        let body = build_request_body(&req, true, interleaved, self.dialect);
        let url = format!("{}/chat/completions", self.base_url);
        let headers = self.request_headers();
        let response = crate::http::send(&self.client, cancel, url, move |agent, url| {
            let mut request = agent.post(url);
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            request.send_json(&body)
        })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(crate::http::read_error_body(status, response));
        }

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let mut stream = OpenAiStream::new(reader, cancel.clone());
        if interleaved == Some(ReasoningField::ReasoningDetails) {
            stream = stream.keeping_reasoning_details();
        }
        let inner: EventStream = Box::new(stream);
        Ok(crate::cancelable::make_cancelable(inner, cancel.clone()))
    }
}

/// How assistant thinking goes out on one message.
#[derive(Clone, Copy)]
enum ThinkingReplay {
    /// As `<thinking>` text in `content`, for models that do not take
    /// reasoning back.
    Text,
    /// In the model's reasoning field, for the current turn of an
    /// interleaved model.
    Field(ReasoningField),
    /// Left out, for earlier turns of an interleaved model.
    Drop,
}

/// Build the JSON body for a Chat Completions request. `interleaved`
/// names the field the model reads its reasoning back from: the
/// current turn (everything after the last user message) carries its
/// thinking there and earlier turns leave it out, as the providers
/// ask. Without it, thinking goes as `<thinking>` text.
fn build_request_body(
    req: &StreamRequest,
    stream: bool,
    interleaved: Option<ReasoningField>,
    dialect: Dialect,
) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &req.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    }
    let turn_start = req
        .messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .map_or(0, |i| i + 1);
    for (i, msg) in req.messages.iter().enumerate() {
        let replay = match interleaved {
            None => ThinkingReplay::Text,
            Some(field) if i >= turn_start => ThinkingReplay::Field(field),
            Some(_) => ThinkingReplay::Drop,
        };
        if let Some(converted) = internal_message_to_openai(msg, replay, &req.model) {
            messages.extend(converted);
        }
    }

    let max_tokens_field = match dialect {
        Dialect::OpenAi => "max_completion_tokens",
        Dialect::Zai => "max_tokens",
    };
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": stream,
    });
    body[max_tokens_field] =
        serde_json::json!(req.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS));
    if stream {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    match dialect {
        Dialect::OpenAi => apply_reasoning(&mut body, req),
        Dialect::Zai => apply_zai_thinking(&mut body, req),
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::to_value(
            req.tools
                .iter()
                .map(tool_spec_to_openai)
                .collect::<Vec<_>>(),
        )
        .expect("tool spec serializes");
        if dialect == Dialect::Zai && !req.model.starts_with("glm-4.5") {
            body["tool_stream"] = Value::Bool(true);
        }
    }
    body
}

/// Set the reasoning fields for `req`: `reasoning_effort` with the
/// model's own effort value on effort models, `thinking.type` on toggle
/// models (the Z.AI, `DeepSeek` and Moonshot shape), and the generic
/// effort mapping on budget models and models the catalog lacks.
fn apply_reasoning(body: &mut Value, req: &StreamRequest) {
    let Some(level) = req.level else {
        return;
    };
    let toggle = |on: bool| serde_json::json!({"type": if on { "enabled" } else { "disabled" }});
    match req.reasoning {
        Reasoning::None | Reasoning::Fixed => {}
        Reasoning::Effort { .. } => match req.reasoning.effort(level) {
            Some(effort) => body["reasoning_effort"] = serde_json::json!(effort.as_str()),
            None if level.is_off() && req.reasoning.has_toggle() => {
                body["thinking"] = toggle(false);
            }
            None => {}
        },
        Reasoning::Toggle => body["thinking"] = toggle(!level.is_off()),
        Reasoning::Unknown | Reasoning::Budget { .. } => {
            if let Some(effort) = level.openai_reasoning_effort() {
                body["reasoning_effort"] = serde_json::json!(effort);
            }
        }
    }
}

/// Set the reasoning fields for `req` on a Z.AI upstream: `thinking`
/// switched on (keeping earlier reasoning, `clear_thinking: false`)
/// for every level but off, plus the model's own `reasoning_effort`
/// on effort models.
fn apply_zai_thinking(body: &mut Value, req: &StreamRequest) {
    let Some(level) = req.level else {
        return;
    };
    if matches!(req.reasoning, Reasoning::None | Reasoning::Fixed) {
        return;
    }
    if level.is_off() {
        body["thinking"] = serde_json::json!({"type": "disabled"});
        return;
    }
    body["thinking"] = serde_json::json!({"type": "enabled", "clear_thinking": false});
    if let Some(effort) = req.reasoning.effort(level) {
        body["reasoning_effort"] = serde_json::json!(effort.as_str());
    }
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

fn internal_message_to_openai(
    msg: &Message,
    replay: ThinkingReplay,
    model: &str,
) -> Option<Vec<Value>> {
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
        Role::Assistant => Some(vec![convert_assistant_message(&msg.content, replay, model)]),
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

fn convert_assistant_message(blocks: &[Content], replay: ThinkingReplay, model: &str) -> Value {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut reasoning = String::new();
    let mut details: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            Content::Text { text } => text_parts.push(text.clone()),
            Content::Thinking {
                text, signature, ..
            } => match replay {
                ThinkingReplay::Text => text_parts.extend(Content::flattened_thinking(text)),
                ThinkingReplay::Field(_) => {
                    reasoning.push_str(text);
                    if let Some(sig) = signature.as_ref().filter(|s| s.model == model)
                        && let Ok(Value::Array(entries)) = serde_json::from_str(&sig.data)
                    {
                        details.extend(entries);
                    }
                }
                ThinkingReplay::Drop => {}
            },
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
    match replay {
        ThinkingReplay::Field(ReasoningField::ReasoningContent) if !reasoning.is_empty() => {
            msg["reasoning_content"] = Value::String(reasoning);
        }
        ThinkingReplay::Field(ReasoningField::ReasoningDetails) => {
            if details.is_empty() && !reasoning.is_empty() {
                details.push(serde_json::json!({"type": "reasoning.text", "text": reasoning}));
            }
            if !details.is_empty() {
                msg["reasoning_details"] = Value::Array(details);
            }
        }
        _ => {}
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
    /// `OpenRouter` `reasoning_details` of the reasoning in flight;
    /// `None` when the stream does not keep them.
    details: Option<Vec<Value>>,
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
            details: None,
        }
    }

    /// Keep the `reasoning_details` the model streams and hand them
    /// over as the reasoning's [`ProviderEvent::ThinkingSignature`], for
    /// models that read them back.
    #[must_use]
    pub fn keeping_reasoning_details(mut self) -> Self {
        self.details = Some(Vec::new());
        self
    }

    fn process_chunk(&mut self, data: &str) {
        if data == "[DONE]" {
            self.flush_details();
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
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            let kind = err
                .get("code")
                .filter(|v| !v.is_null())
                .or_else(|| err.get("type"))
                .filter(|v| !v.is_null())
                .and_then(Value::as_str)
                .unwrap_or("error");
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider stream failed");
            self.pending
                .push_back(Err(ProviderError::from_stream_error(kind, msg)));
            self.done = true;
            return;
        }
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
        // OpenAI-compatible reasoning models stream their thinking on
        // a side channel, not in `content`: GLM / Zhipu / DeepSeek use
        // `reasoning_content`, OpenRouter and others use `reasoning`.
        // Surface it as a thinking delta so it is not silently lost.
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            self.pending.push_back(Ok(ProviderEvent::ThinkingDelta {
                delta: reasoning.to_owned(),
            }));
        }
        if let Some(details) = self.details.as_mut()
            && let Some(entries) = delta.get("reasoning_details").and_then(Value::as_array)
        {
            for entry in entries {
                merge_detail(details, entry);
            }
        }
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                self.flush_details();
                self.pending.push_back(Ok(ProviderEvent::TextDelta {
                    delta: content.to_owned(),
                }));
            }
        }
        let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        self.flush_details();
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

    /// Close the reasoning in flight with the `reasoning_details` kept
    /// for it.
    fn flush_details(&mut self) {
        let Some(details) = self.details.as_mut().filter(|d| !d.is_empty()) else {
            return;
        };
        let data = Value::Array(std::mem::take(details)).to_string();
        self.pending
            .push_back(Ok(ProviderEvent::ThinkingSignature { data }));
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

impl crate::sse::SseStreamCore for OpenAiStream {
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
    fn process(&mut self, _name: &str, data: &str) {
        self.process_chunk(data);
    }
    fn on_eof(&mut self) {
        // A compatible upstream can end without `[DONE]`. The turn must
        // still complete with what arrived - final usage included -
        // instead of being thrown away and retried from scratch. The
        // `started` guard keeps a stream that died before any output
        // from inventing a turn.
        if self.started {
            self.flush_details();
            self.flush_pending_tool_calls();
            self.pending.push_back(Ok(ProviderEvent::MessageEnd {
                stop_reason: self.finish_reason,
                usage: self.usage,
            }));
        }
    }
}

impl Iterator for OpenAiStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        crate::sse::sse_next(self)
    }
}

/// Add a streamed `reasoning_details` entry, extending the last entry
/// when it continues it (same `type` and `index`), so a block streamed
/// in many chunks is kept as one entry.
fn merge_detail(details: &mut Vec<Value>, entry: &Value) {
    let Some(fields) = entry.as_object() else {
        return;
    };
    let continues = details.last().is_some_and(|last| {
        last.get("type") == entry.get("type") && last.get("index") == entry.get("index")
    });
    let Some(last) = details
        .last_mut()
        .and_then(Value::as_object_mut)
        .filter(|_| continues)
    else {
        details.push(entry.clone());
        return;
    };
    for (key, value) in fields {
        let appends = matches!(key.as_str(), "text" | "summary" | "data");
        match (last.get_mut(key), value) {
            (_, Value::Null) => {}
            (Some(Value::String(old)), Value::String(more)) if appends => old.push_str(more),
            _ => {
                last.insert(key.clone(), value.clone());
            }
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
    use crate::testing::{collect_ok, user_msg};

    #[test]
    fn body_includes_model_and_messages() {
        let req = StreamRequest::new("gpt-4o", vec![user_msg("hi")]);
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        assert_eq!(body["model"], "gpt-4o");
        // max_tokens is deprecated on Chat Completions (and rejected for
        // reasoning models); the replacement must be sent instead.
        assert_eq!(body["max_completion_tokens"], 4_096);
        assert!(body.get("max_tokens").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn body_prepends_system_message() {
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
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
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn body_translates_thinking_level_to_reasoning_effort() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::Medium);
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn body_caps_xhigh_at_high_for_openai() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::XHigh);
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn body_omits_reasoning_effort_when_level_off() {
        let mut req = StreamRequest::new("gpt-5", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::Off);
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn body_includes_stream_options_only_when_streaming() {
        let req = StreamRequest::new("m", vec![user_msg("hi")]);
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        assert!(body.get("stream_options").is_none());
        let body = build_request_body(&req, true, None, Dialect::OpenAi);
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
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
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
        let body = build_request_body(&req, false, None, Dialect::OpenAi);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_1");
        assert_eq!(messages[0]["content"], "127.0.0.1");
    }

    fn stream_from_bytes(bytes: &'static [u8]) -> OpenAiStream {
        OpenAiStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
    }

    /// A compatible upstream that ends without `[DONE]` must complete
    /// the turn at EOF, delivering the usage the final chunk carried,
    /// instead of leaving the loop to discard and re-request it.
    #[test]
    fn eof_without_done_completes_the_turn() {
        let bytes: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5,\"finish_reason\":\"stop\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let mut saw_text = false;
        let (usage, reason) = loop {
            match events.next() {
                Some(Ok(ProviderEvent::MessageStart)) => {}
                Some(Ok(ProviderEvent::TextDelta { .. })) => saw_text = true,
                Some(Ok(ProviderEvent::MessageEnd { usage, stop_reason })) => {
                    assert!(saw_text, "terminal frame without output");
                    break (usage, stop_reason);
                }
                other => panic!("unexpected event {other:?}"),
            }
        };
        assert_eq!(usage.input, 3);
        assert_eq!(usage.output, 5);
        assert_eq!(reason, StopReason::Other);
        assert!(events.next().is_none());
    }

    #[test]
    fn eof_before_any_output_invents_nothing() {
        let mut events = stream_from_bytes(b"");
        assert!(events.next().is_none());
    }

    #[test]
    fn stream_error_chunk_surfaces_instead_of_silent_drop() {
        let bytes: &[u8] = b"data: {\"error\":{\"message\":\"The server had an error while processing your request.\",\"type\":\"server_error\",\"param\":null,\"code\":null}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(err) => {
                assert!(err.is_transient(), "server_error should retry: {err:?}");
                assert!(err.to_string().contains("server_error"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
        assert!(events.next().is_none(), "stream ends after the error chunk");
    }

    #[test]
    fn stream_error_chunk_rate_limit_classifies_as_rate_limited() {
        let bytes: &[u8] = b"data: {\"error\":{\"message\":\"Rate limit reached\",\"type\":\"requests\",\"param\":null,\"code\":\"rate_limit_exceeded\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        assert!(
            matches!(first, Err(ProviderError::RateLimited { retry_after: None })),
            "got {first:?}"
        );
    }

    #[test]
    fn stream_null_error_chunk_is_ignored() {
        let bytes: &[u8] = b"data: {\"error\":null,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::TextDelta { delta } if delta == "hi")),
            "a null error key must not break the stream"
        );
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
    fn stream_emits_thinking_from_reasoning_content_side_channel() {
        // GLM/Zhipu/DeepSeek stream reasoning on `reasoning_content`;
        // OpenRouter and others use `reasoning`. Both become thinking.
        let bytes: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"let me\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\" think\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let thinking: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ThinkingDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["let me", " think"]);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["answer"]);
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

    #[test]
    fn request_headers_include_auth_then_extras_in_key_order() {
        let mut extras = BTreeMap::new();
        extras.insert("X-B".to_owned(), "2".to_owned());
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = OpenAiProvider::new("k").with_extra_headers(extras);
        assert_eq!(
            provider.request_headers(),
            vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("authorization".to_owned(), "Bearer k".to_owned()),
                ("X-A".to_owned(), "1".to_owned()),
                ("X-B".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn request_headers_skip_auth_when_key_is_empty() {
        let mut extras = BTreeMap::new();
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = OpenAiProvider::new("").with_extra_headers(extras);
        let headers = provider.request_headers();
        assert!(
            headers.iter().all(|(name, _)| name != "authorization"),
            "no credential header without a key: {headers:?}"
        );
        assert!(headers.contains(&("X-A".to_owned(), "1".to_owned())));
    }

    fn thinking(text: &str, signature: Option<kage_core::ThinkingSignature>) -> Content {
        Content::Thinking {
            text: text.to_owned(),
            signature,
            duration_ms: None,
        }
    }

    fn tool_loop_history() -> Vec<Message> {
        vec![
            user_msg("first"),
            Message::new(
                Role::Assistant,
                vec![
                    thinking("old reasoning", None),
                    Content::Text {
                        text: "answer".into(),
                    },
                ],
                None,
            ),
            user_msg("second"),
            Message::new(
                Role::Assistant,
                vec![
                    thinking("need a file", None),
                    Content::ToolCall {
                        id: ToolCallId::new("call_1"),
                        name: "read".into(),
                        input: serde_json::json!({}),
                    },
                ],
                None,
            ),
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("call_1"),
                    output: "text".into(),
                    is_error: false,
                }],
                None,
            ),
        ]
    }

    #[test]
    fn interleaved_models_get_only_the_current_turns_reasoning() {
        let req = StreamRequest::new("glm", tool_loop_history());
        let body = build_request_body(
            &req,
            true,
            Some(ReasoningField::ReasoningContent),
            Dialect::OpenAi,
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["content"], "answer");
        assert!(messages[1].get("reasoning_content").is_none());
        assert_eq!(messages[3]["reasoning_content"], "need a file");
        assert!(messages[3]["content"].is_null());
        assert_eq!(messages[3]["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn other_models_get_thinking_as_text_and_no_reasoning_field() {
        let req = StreamRequest::new("gpt", tool_loop_history());
        let body = build_request_body(&req, true, None, Dialect::OpenAi);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[1]["content"],
            "<thinking>\nold reasoning\n</thinking>answer"
        );
        assert_eq!(
            messages[3]["content"],
            "<thinking>\nneed a file\n</thinking>"
        );
        assert!(
            messages
                .iter()
                .all(|m| m.get("reasoning_content").is_none()
                    && m.get("reasoning_details").is_none())
        );
    }

    #[test]
    fn reasoning_details_go_back_as_kept_or_built_from_text() {
        let details = r#"[{"type":"reasoning.encrypted","data":"enc"}]"#;
        let signed = Some(kage_core::ThinkingSignature {
            model: "gemini-3".into(),
            data: details.into(),
            redacted: false,
        });
        let mut history = tool_loop_history();
        history[3].content[0] = thinking("need a file", signed);
        let req = StreamRequest::new("gemini-3", history.clone());
        let body = build_request_body(
            &req,
            true,
            Some(ReasoningField::ReasoningDetails),
            Dialect::OpenAi,
        );
        assert_eq!(
            body["messages"][3]["reasoning_details"],
            serde_json::json!([{"type": "reasoning.encrypted", "data": "enc"}])
        );
        let req = StreamRequest::new("kimi", history);
        let body = build_request_body(
            &req,
            true,
            Some(ReasoningField::ReasoningDetails),
            Dialect::OpenAi,
        );
        assert_eq!(
            body["messages"][3]["reasoning_details"],
            serde_json::json!([{"type": "reasoning.text", "text": "need a file"}])
        );
    }

    #[test]
    fn interleaved_field_comes_from_own_models_then_catalog() {
        let custom = OpenAiProvider::new("k").with_models(vec![ProviderModel {
            id: "local".into(),
            interleaved: Some(ReasoningField::ReasoningContent),
            ..ProviderModel::default()
        }]);
        assert_eq!(
            custom.interleaved("local"),
            Some(ReasoningField::ReasoningContent)
        );
        assert_eq!(custom.interleaved("gpt-5"), None);
        let deepseek = crate::compat::COMPAT_PROVIDERS
            .iter()
            .find(|p| p.id == "deepseek")
            .unwrap()
            .build("k");
        assert_eq!(
            deepseek.interleaved("deepseek-v4-pro"),
            Some(ReasoningField::ReasoningContent)
        );
    }

    #[test]
    fn stream_keeps_reasoning_details_merged_before_the_tool_call() {
        let bytes: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"let \",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"let \",\"index\":0,\"format\":\"x\"}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"me\",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"me\",\"index\":0,\"signature\":\"s\"}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n";
        let stream = OpenAiStream::new(Box::new(std::io::Cursor::new(bytes)), CancelFlag::new())
            .keeping_reasoning_details();
        let events = collect_ok(stream);
        let sig = events
            .iter()
            .position(|e| matches!(e, ProviderEvent::ThinkingSignature { .. }))
            .expect("details kept");
        let start = events
            .iter()
            .position(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
            .unwrap();
        assert!(sig < start);
        let ProviderEvent::ThinkingSignature { data } = &events[sig] else {
            unreachable!()
        };
        let details: Value = serde_json::from_str(data).unwrap();
        assert_eq!(
            details,
            serde_json::json!([{
                "type": "reasoning.text", "text": "let me", "index": 0,
                "format": "x", "signature": "s",
            }])
        );
        let plain = collect_ok(stream_from_bytes(bytes));
        assert!(
            !plain
                .iter()
                .any(|e| matches!(e, ProviderEvent::ThinkingSignature { .. }))
        );
    }

    #[test]
    fn zai_endpoints_are_detected_by_id_or_base_url() {
        for id in ["zai", "zai-coding-plan", "zhipuai-coding-plan"] {
            let entry = crate::compat::COMPAT_PROVIDERS
                .iter()
                .find(|p| p.id == id)
                .unwrap();
            assert_eq!(entry.build("k").dialect, Dialect::Zai, "{id}");
            let moved = entry.build_with_base_url("k", "http://127.0.0.1:1/v4");
            assert_eq!(moved.dialect, Dialect::Zai, "{id} with base_url");
        }
        let custom = |url: &str| {
            let metadata = ProviderMetadata {
                id: "mine".into(),
                display_name: "Mine".into(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            };
            OpenAiProvider::compatible("k", url, metadata).dialect
        };
        assert_eq!(custom("https://open.bigmodel.cn/api/paas/v4"), Dialect::Zai);
        assert_eq!(custom("https://api.z.ai/api/paas/v4"), Dialect::Zai);
        assert_eq!(custom("https://api.deepseek.com/v1"), Dialect::OpenAi);
        assert_eq!(OpenAiProvider::new("k").dialect, Dialect::OpenAi);
    }

    #[test]
    fn zai_body_uses_max_tokens_and_a_system_role() {
        let mut req = StreamRequest::new("glm-5.3", vec![user_msg("hi")]);
        req.system = Some("you are kage".into());
        let body = build_request_body(&req, true, None, Dialect::Zai);
        assert_eq!(body["max_tokens"], 4_096);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("store").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["role"] != "developer")
        );
    }

    #[test]
    fn zai_streams_tool_arguments_except_on_glm_4_5() {
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        let body_for = |model: &str, tools: &[ToolSpec], dialect| {
            let mut req = StreamRequest::new(model, vec![user_msg("hi")]);
            req.tools = tools.to_vec();
            build_request_body(&req, true, None, dialect)
        };
        assert_eq!(
            body_for("glm-5.3", &tools, Dialect::Zai)["tool_stream"],
            true
        );
        assert!(
            body_for("glm-4.5-air", &tools, Dialect::Zai)
                .get("tool_stream")
                .is_none()
        );
        assert!(
            body_for("glm-5.3", &[], Dialect::Zai)
                .get("tool_stream")
                .is_none()
        );
        assert!(
            body_for("glm-5.3", &tools, Dialect::OpenAi)
                .get("tool_stream")
                .is_none()
        );
    }

    #[test]
    fn stream_assembles_zai_tool_stream_deltas() {
        // Z.AI's `tool_stream` shape: every chunk repeats the call's id,
        // index and type, only the first names the function, and the
        // arguments arrive in pieces; parallel calls interleave by index.
        let bytes: &[u8] = b"data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"look\"}}]}\n\n\
data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"call_a\",\"index\":0,\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"call_a\",\"index\":0,\"type\":\"function\",\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"call_b\",\"index\":1,\"type\":\"function\",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}}]}}]}\n\n\
data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"call_a\",\"index\":0,\"type\":\"function\",\"function\":{\"arguments\":\"\\\"/x\\\"}\"}}]}}]}\n\n\
data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n\
data: [DONE]\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let starts: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolCallStart { id, name } => Some((id.0.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [("call_a", "read"), ("call_b", "ls")]);
        let ends: Vec<(&str, Value)> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolCallEnd { id, input } => Some((id.0.as_str(), input.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            ends,
            [
                ("call_a", serde_json::json!({"path": "/x"})),
                ("call_b", serde_json::json!({})),
            ]
        );
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn with_models_overrides_advertised_models() {
        let models = vec![ProviderModel {
            id: "test-model".to_owned(),
            name: "Test Model".to_owned(),
            context: Some(128_000),
            max_output: Some(8_192),
            ..ProviderModel::default()
        }];
        let provider = OpenAiProvider::new("k").with_models(models.clone());
        assert_eq!(provider.models(), models);
        assert!(OpenAiProvider::new("k").models().is_empty());
    }
}

#[cfg(test)]
mod thinking_tests {
    use kage_core::{Effort, Efforts, ThinkingLevel};

    use super::*;

    fn request(reasoning: Reasoning, level: ThinkingLevel) -> StreamRequest {
        let user = Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None);
        let mut req = StreamRequest::new("m", vec![user]);
        req.reasoning = reasoning;
        req.level = Some(level);
        req
    }

    fn effort(values: &[Effort], toggle: bool) -> Reasoning {
        Reasoning::Effort {
            efforts: Efforts::of(values),
            toggle,
        }
    }

    #[test]
    fn effort_models_send_their_own_effort_values() {
        let r = effort(
            &[Effort::None, Effort::Low, Effort::High, Effort::Max],
            false,
        );
        let body = build_request_body(
            &request(r, ThinkingLevel::XHigh),
            true,
            None,
            Dialect::OpenAi,
        );
        assert_eq!(body["reasoning_effort"], "max");
        let body = build_request_body(&request(r, ThinkingLevel::Off), true, None, Dialect::OpenAi);
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[test]
    fn toggle_models_switch_thinking_on_and_off() {
        let on = build_request_body(
            &request(Reasoning::Toggle, ThinkingLevel::High),
            true,
            None,
            Dialect::OpenAi,
        );
        assert_eq!(on["thinking"]["type"], "enabled");
        assert!(on.get("reasoning_effort").is_none());
        let off = build_request_body(
            &request(Reasoning::Toggle, ThinkingLevel::Off),
            true,
            None,
            Dialect::OpenAi,
        );
        assert_eq!(off["thinking"]["type"], "disabled");
        let r = effort(&[Effort::Low, Effort::High], true);
        let off = build_request_body(&request(r, ThinkingLevel::Off), true, None, Dialect::OpenAi);
        assert_eq!(off["thinking"]["type"], "disabled");
        assert!(off.get("reasoning_effort").is_none());
    }

    #[test]
    fn zai_effort_models_send_thinking_and_their_own_effort() {
        let r = effort(&[Effort::Low, Effort::High, Effort::Max], false);
        let body = build_request_body(&request(r, ThinkingLevel::XHigh), true, None, Dialect::Zai);
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "clear_thinking": false})
        );
        assert_eq!(body["reasoning_effort"], "max");
        let body = build_request_body(&request(r, ThinkingLevel::Low), true, None, Dialect::Zai);
        assert_eq!(body["reasoning_effort"], "low");
        let r = effort(&[Effort::None, Effort::High], false);
        let off = build_request_body(&request(r, ThinkingLevel::Off), true, None, Dialect::Zai);
        assert_eq!(off["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(off.get("reasoning_effort").is_none());
    }

    #[test]
    fn zai_toggle_models_send_only_thinking() {
        let on = request(Reasoning::Toggle, ThinkingLevel::High);
        let on = build_request_body(&on, true, None, Dialect::Zai);
        assert_eq!(
            on["thinking"],
            serde_json::json!({"type": "enabled", "clear_thinking": false})
        );
        assert!(on.get("reasoning_effort").is_none());
        let off = request(Reasoning::Toggle, ThinkingLevel::Off);
        let off = build_request_body(&off, true, None, Dialect::Zai);
        assert_eq!(off["thinking"], serde_json::json!({"type": "disabled"}));
        let unknown = request(Reasoning::Unknown, ThinkingLevel::Medium);
        let unknown = build_request_body(&unknown, true, None, Dialect::Zai);
        assert_eq!(unknown["thinking"]["type"], "enabled");
        assert!(unknown.get("reasoning_effort").is_none());
        let fixed = request(Reasoning::Fixed, ThinkingLevel::High);
        let fixed = build_request_body(&fixed, true, None, Dialect::Zai);
        assert!(fixed.get("thinking").is_none());
    }

    #[test]
    fn fixed_models_send_nothing() {
        let body = build_request_body(
            &request(Reasoning::Fixed, ThinkingLevel::High),
            true,
            None,
            Dialect::OpenAi,
        );
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }
}
