//! Google Gemini provider.
//!
//! Implements `streamGenerateContent` on the Generative Language API. The
//! streaming endpoint returns a JSON array as Server-Sent Events with
//! `data:` chunks. Function calls and text are emitted in the response
//! `candidates[0].content.parts` array.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufReader, Read};

use kage_core::{CancelFlag, Content, Message, Reasoning, Role, ToolCallId};
use serde_json::Value;

use crate::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, ProviderModel,
    StopReason, StreamRequest, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
/// Placeholder Google documents for function calls that carry no real
/// thought signature, such as history from another model.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Google Gemini provider.
#[derive(Debug)]
pub struct GeminiProvider {
    api_key: String,
    base_url: String,
    metadata: ProviderMetadata,
    client: crate::http::HttpClient,
    /// Extra headers sent on every request, after the protocol's own.
    extra_headers: BTreeMap<String, String>,
    /// Models advertised from `Provider::models` (custom providers);
    /// empty lets the catalog drive the picker.
    models: Vec<ProviderModel>,
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
            client: crate::http::HttpClient::new(),
            extra_headers: BTreeMap::new(),
            models: Vec::new(),
        }
    }

    /// Register under `metadata` instead of the built-in identity, for a
    /// user-declared provider that speaks this protocol.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ProviderMetadata) -> Self {
        self.metadata = metadata;
        self
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

    /// Headers every request carries: content type, the key header
    /// (skipped when no key is configured), then the configured extras
    /// in key order. The key travels in a header, never the query
    /// string, so it cannot leak through URL logging or proxy records.
    fn request_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        if !self.api_key.is_empty() {
            headers.push(("x-goog-api-key".to_owned(), self.api_key.clone()));
        }
        for (name, value) in &self.extra_headers {
            headers.push((name.clone(), value.clone()));
        }
        headers
    }
}

impl Provider for GeminiProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn models(&self) -> Vec<ProviderModel> {
        self.models.clone()
    }

    /// Thought signatures go back on their function calls, and thinking
    /// text as `<thinking>` text, see `build_request_body`.
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
        let body = build_request_body(&req);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, req.model
        );
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
        let inner: EventStream = Box::new(GeminiStream::new(reader, cancel.clone()));
        Ok(crate::cancelable::make_cancelable(inner, cancel.clone()))
    }
}

/// Build the JSON body for a Gemini streamGenerateContent request.
pub(crate) fn build_request_body(req: &StreamRequest) -> Value {
    // Gemini's functionResponse is keyed by function NAME, but our
    // result blocks only carry the correlation id; recover the name
    // from the assistant turn that issued each call.
    let names_by_id: HashMap<String, String> = req
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::ToolCall { id, name, .. } => Some((id.0.clone(), name.clone())),
            _ => None,
        })
        .collect();
    let turn_start = req
        .messages
        .iter()
        .rposition(|m| {
            m.role == Role::User && m.content.iter().any(|c| matches!(c, Content::Text { .. }))
        })
        .unwrap_or(0);
    let enforced = enforces_signatures(&req.model);
    let contents: Vec<Value> = req
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            let placeholder = enforced && i >= turn_start;
            internal_message_to_gemini(msg, &names_by_id, &req.model, placeholder)
        })
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
    if let Some(config) = thinking_config(req) {
        body["generationConfig"]["thinkingConfig"] = config;
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::json!([{
            "functionDeclarations": req.tools.iter().map(tool_spec_to_gemini).collect::<Vec<_>>(),
        }]);
    }
    body
}

/// The `thinkingConfig` for a Gemini request, or `None` to leave it
/// out. An explicit [`crate::ThinkingConfig`] budget wins. Otherwise
/// effort models get a `thinkingLevel`, toggle models the dynamic
/// budget (`-1`), and budget models a budget within their bounds. Off
/// sends a zero budget to models that can switch thinking off.
fn thinking_config(req: &StreamRequest) -> Option<Value> {
    if let Some(thinking) = &req.thinking {
        return Some(serde_json::json!({"thinkingBudget": thinking.budget_tokens}));
    }
    let level = req.level?;
    let budget = |tokens: i64| Some(serde_json::json!({"thinkingBudget": tokens}));
    match req.reasoning {
        Reasoning::None | Reasoning::Fixed => None,
        Reasoning::Effort { .. } if !level.is_off() => req
            .reasoning
            .effort(level)
            .map(|e| serde_json::json!({"thinkingLevel": e.as_str()})),
        _ if level.is_off() => budget(0).filter(|_| req.reasoning.has_toggle()),
        Reasoning::Toggle => budget(-1),
        Reasoning::Unknown | Reasoning::Effort { .. } | Reasoning::Budget { .. } => req
            .reasoning
            .budget(level)
            .and_then(|tokens| budget(i64::from(tokens))),
    }
}

/// Whether `model` rejects a current-turn function call without a
/// thought signature, which Gemini 3 and later do.
fn enforces_signatures(model: &str) -> bool {
    let name = model.rsplit('/').next().unwrap_or(model);
    name.strip_prefix("gemini-")
        .map(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("")
        })
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 3)
}

fn tool_spec_to_gemini(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "parameters": gemini_schema(&spec.schema),
    })
}

/// Map a draft-2020-12 JSON Schema (what `schemars` emits) into the
/// `OpenAPI` subset Gemini's `functionDeclarations.parameters` accepts:
/// `$ref`s inlined from `$defs`, draft keys dropped, `type` arrays
/// (how `Option<T>` fields arrive) turned into Gemini's `nullable`
/// flag, and `uint*` formats renamed to the ones Gemini knows.
fn gemini_schema(schema: &Value) -> Value {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    convert_schema(schema, &defs, 0)
}

fn convert_schema(value: &Value, defs: &Value, depth: usize) -> Value {
    // Cycle guard: schemars output for plain structs is acyclic, but a
    // hand-written schema could reference itself.
    if depth > 16 {
        return value.clone();
    }
    match value {
        Value::Object(map) => {
            // Inline the referenced definition whole; it is converted
            // like any other schema node.
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(target) = defs.get(reference.trim_start_matches("#/$defs/"))
            {
                return convert_schema(target, defs, depth + 1);
            }
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                match key.as_str() {
                    "$schema" | "$ref" | "$defs" => {}
                    "type" => match val {
                        Value::String(t) => {
                            out.insert("type".to_owned(), Value::String(t.clone()));
                        }
                        // `[T, "null"]` is OpenAPI's forbidden spelling
                        // of an optional value; Gemini wants `nullable`.
                        Value::Array(types) => {
                            let base = types.iter().find(|t| t.as_str() != Some("null"));
                            if let Some(base) = base {
                                out.insert("type".to_owned(), base.clone());
                                if types.contains(&Value::String("null".into())) {
                                    out.insert("nullable".to_owned(), Value::Bool(true));
                                }
                            }
                        }
                        other => {
                            out.insert("type".to_owned(), other.clone());
                        }
                    },
                    "format" => {
                        let name = val.as_str().unwrap_or_default();
                        let mapped = match name {
                            "uint32" => "int32",
                            "uint64" => "int64",
                            other => other,
                        };
                        out.insert("format".to_owned(), Value::String(mapped.to_owned()));
                    }
                    _ => {
                        out.insert(key.clone(), convert_schema(val, defs, depth + 1));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| convert_schema(item, defs, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn internal_message_to_gemini(
    msg: &Message,
    names_by_id: &HashMap<String, String>,
    model: &str,
    placeholder: bool,
) -> Option<Value> {
    let (role, parts) = match msg.role {
        Role::User => ("user", convert_user_parts(&msg.content)),
        Role::Assistant => (
            "model",
            convert_assistant_parts(&msg.content, model, placeholder),
        ),
        Role::ToolResult => ("user", convert_tool_result_parts(&msg.content, names_by_id)),
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

/// Model parts on the wire. A thought signature `model` produced goes
/// back on the function call that follows it, which Gemini 3 requires
/// on the first call of every step in the current turn. With
/// `placeholder`, a first call that has no real signature carries
/// [`SKIP_SIGNATURE`] instead. Other thinking goes as `<thinking>` text.
fn convert_assistant_parts(blocks: &[Content], model: &str, placeholder: bool) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut signature: Option<&str> = None;
    let mut first_call = true;
    for block in blocks {
        match block {
            Content::Text { text } => parts.push(serde_json::json!({"text": text})),
            Content::Thinking {
                signature: Some(sig),
                ..
            } if sig.model == model && !sig.redacted => signature = Some(&sig.data),
            Content::Thinking { text, .. } => {
                if let Some(text) = Content::flattened_thinking(text) {
                    parts.push(serde_json::json!({"text": text}));
                }
            }
            Content::ToolCall { name, input, .. } => {
                let mut part = serde_json::json!({
                    "functionCall": {
                        "name": name,
                        "args": input,
                    },
                });
                if let Some(sig) = signature.take() {
                    part["thoughtSignature"] = Value::String(sig.to_owned());
                } else if placeholder && first_call {
                    part["thoughtSignature"] = Value::String(SKIP_SIGNATURE.to_owned());
                }
                first_call = false;
                parts.push(part);
            }
            _ => {}
        }
    }
    parts
}

fn convert_tool_result_parts(
    blocks: &[Content],
    names_by_id: &HashMap<String, String>,
) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|c| match c {
            Content::ToolResultBlock {
                call_id, output, ..
            } => {
                let name = names_by_id
                    .get(&call_id.0)
                    .map_or_else(|| tool_name_from_call_id(call_id), String::as_str);
                Some(serde_json::json!({
                    "functionResponse": {
                        "name": name,
                        "response": {"output": output},
                    },
                }))
            }
            _ => None,
        })
        .collect()
}

/// Recover the function name from a synthesized `gemini_{name}_{n}`
/// correlation id when no matching assistant tool call is in history
/// (resumed sessions written by older kage versions). Ids without the
/// trailing counter decode as the whole `gemini_` suffix.
fn tool_name_from_call_id(id: &ToolCallId) -> &str {
    let rest = id.0.strip_prefix("gemini_").unwrap_or(&id.0);
    match rest.rsplit_once('_') {
        Some((name, counter))
            if !counter.is_empty() && counter.bytes().all(|b| b.is_ascii_digit()) =>
        {
            name
        }
        _ => rest,
    }
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

/// Iterator over a Gemini streaming response.
pub struct GeminiStream {
    reader: BufReader<Box<dyn Read + Send>>,
    cancel: CancelFlag,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    started: bool,
    /// Function calls seen in this stream. Gemini emits no per-call ids,
    /// so each call gets a fresh `gemini_{name}_{n}` correlation id;
    /// name-keyed reuse would collide when one turn makes parallel
    /// same-name calls.
    tool_call_count: usize,
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
            tool_call_count: 0,
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
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            let kind = err
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| err.get("code").map(Value::to_string))
                .unwrap_or_else(|| "error".to_owned());
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider stream failed");
            self.pending
                .push_back(Err(ProviderError::from_stream_error(&kind, msg)));
            self.done = true;
            return;
        }
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
            if let Some(data) = part.get("thoughtSignature").and_then(Value::as_str) {
                self.pending.push_back(Ok(ProviderEvent::ThinkingSignature {
                    data: data.to_owned(),
                }));
            }
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let args = call
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            self.tool_call_count += 1;
            let id = ToolCallId::new(format!("gemini_{name}_{}", self.tool_call_count));
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
        // Gemini 2.5+ reports thinking tokens separately and excludes
        // them from `candidatesTokenCount`; every other provider counts
        // them in the output total, so fold them in for consistent
        // cost and context accounting.
        if let Some(v) = usage.get("thoughtsTokenCount").and_then(Value::as_u64) {
            self.usage.output = self.usage.output.saturating_add(v);
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

impl crate::sse::SseStreamCore for GeminiStream {
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
        // Gemini's SSE ends without an explicit terminal frame, so a
        // turn that produced any output gets a synthesized MessageEnd
        // (the `started` guard mirrors the original EOF branch).
        if self.started {
            self.emit_message_end();
        }
    }
}

impl Iterator for GeminiStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        crate::sse::sse_next(self)
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
    use crate::testing::{collect_ok, user_msg};

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
    fn body_translates_thinking_level_to_budget() {
        let mut req = StreamRequest::new("gemini-2.5-pro", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::High);
        let body = build_request_body(&req);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            crate::ThinkingLevel::High.default_budget_tokens().unwrap()
        );
    }

    #[test]
    fn body_omits_thinking_config_when_level_off() {
        let mut req = StreamRequest::new("gemini-2.5-pro", vec![user_msg("hi")]);
        req.level = Some(crate::ThinkingLevel::Off);
        let body = build_request_body(&req);
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
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

    /// `edit`'s derived schema is the shape that used to go over the
    /// wire raw: `$schema`, `$defs` + `$ref`, a `type` array from the
    /// `Option<Vec<..>>` field, and a `uint32` format. Gemini's
    /// `OpenAPI` subset rejects those.
    #[test]
    fn tool_schema_is_mapped_to_the_openapi_subset() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "Range": {
                    "type": "object",
                    "properties": {
                        "start": {"format": "uint32", "minimum": 0, "type": "integer"}
                    }
                }
            },
            "properties": {
                "path": {"type": "string"},
                "changes": {
                    "type": ["array", "null"],
                    "items": {"$ref": "#/$defs/Range"}
                }
            },
            "required": ["path"],
            "type": "object"
        });
        let mut req = StreamRequest::new("m", vec![user_msg("hi")]);
        req.tools = vec![ToolSpec {
            name: "edit".into(),
            description: "edit a file".into(),
            schema,
        }];
        let body = build_request_body(&req);
        let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        let text = params.to_string();
        assert!(!text.contains("$schema"), "{text}");
        assert!(!text.contains("$ref"), "{text}");
        assert!(!text.contains("$defs"), "{text}");
        let changes = &params["properties"]["changes"];
        assert_eq!(changes["type"], "array");
        assert_eq!(changes["nullable"], true);
        let start = &changes["items"]["properties"]["start"];
        assert_eq!(start["format"], "int32");
        assert_eq!(params["properties"]["path"]["type"], "string");
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
    fn thought_signature_goes_back_on_its_function_call() {
        let signed = |model: &str| {
            Some(kage_core::ThinkingSignature {
                model: model.into(),
                data: "sig".into(),
                redacted: false,
            })
        };
        let call = |id: &str| Content::ToolCall {
            id: ToolCallId::new(id),
            name: "read".into(),
            input: serde_json::json!({}),
        };
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: String::new(),
                    signature: signed("gemini-3"),
                    duration_ms: None,
                },
                call("a"),
                call("b"),
                Content::Thinking {
                    text: "other model".into(),
                    signature: signed("claude-x"),
                    duration_ms: None,
                },
                call("c"),
            ],
            None,
        );
        let req = StreamRequest::new("gemini-3", vec![user_msg("read"), assistant]);
        let body = build_request_body(&req);
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0]["thoughtSignature"], "sig");
        assert_eq!(parts[0]["functionCall"]["name"], "read");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(parts[2]["text"], "<thinking>\nother model\n</thinking>");
        assert!(parts[3].get("thoughtSignature").is_none());
    }

    fn unsigned_loop() -> Vec<Message> {
        let call = |id: &str| Content::ToolCall {
            id: ToolCallId::new(id),
            name: "read".into(),
            input: serde_json::json!({}),
        };
        let result = |id: &str| {
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new(id),
                    output: "ok".into(),
                    is_error: false,
                }],
                None,
            )
        };
        let assistant = |content| Message::new(Role::Assistant, content, None);
        vec![
            user_msg("earlier"),
            assistant(vec![call("x")]),
            result("x"),
            assistant(vec![Content::Text {
                text: "done".into(),
            }]),
            user_msg("now"),
            assistant(vec![call("a"), call("b")]),
            result("a"),
            result("b"),
            assistant(vec![
                Content::Thinking {
                    text: "switched".into(),
                    signature: Some(kage_core::ThinkingSignature {
                        model: "claude-x".into(),
                        data: "foreign".into(),
                        redacted: false,
                    }),
                    duration_ms: None,
                },
                call("c"),
            ]),
            result("c"),
            assistant(vec![Content::Text {
                text: "all read".into(),
            }]),
        ]
    }

    #[test]
    fn placeholder_signs_first_call_of_each_current_step() {
        let req = StreamRequest::new("gemini-3-pro-preview", unsigned_loop());
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        let sig =
            |content: usize, part: usize| contents[content]["parts"][part].get("thoughtSignature");
        assert!(sig(1, 0).is_none());
        assert!(sig(3, 0).is_none());
        assert_eq!(sig(5, 0).unwrap(), SKIP_SIGNATURE);
        assert!(sig(5, 1).is_none());
        assert_eq!(
            contents[8]["parts"][0]["text"],
            "<thinking>\nswitched\n</thinking>"
        );
        assert_eq!(sig(8, 1).unwrap(), SKIP_SIGNATURE);
        assert!(sig(10, 0).is_none());
    }

    #[test]
    fn placeholder_is_left_out_before_gemini_3() {
        let req = StreamRequest::new("gemini-2.5-pro", unsigned_loop());
        let body = build_request_body(&req);
        let signed = body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap())
            .any(|p| p.get("thoughtSignature").is_some());
        assert!(!signed);
    }

    #[test]
    fn signature_enforcement_starts_at_gemini_3() {
        for model in [
            "gemini-3-flash-preview",
            "gemini-3.1-pro-preview",
            "models/gemini-3-pro",
            "google/gemini-3.5-flash",
            "gemini-10-pro",
        ] {
            assert!(enforces_signatures(model), "{model}");
        }
        for model in ["gemini-2.5-pro", "gemini-exp-1206", "gemini-", "m"] {
            assert!(!enforces_signatures(model), "{model}");
        }
    }

    #[test]
    fn stream_captures_thought_signature_before_its_call() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{}},\"thoughtSignature\":\"sig\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        assert!(matches!(
            &events[1],
            ProviderEvent::ThinkingSignature { data } if data == "sig"
        ));
        assert!(matches!(&events[2], ProviderEvent::ToolCallStart { .. }));
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

    #[test]
    fn stream_error_chunk_resource_exhausted_classifies_as_rate_limited() {
        let bytes: &[u8] = b"data: {\"error\":{\"code\":429,\"message\":\"Resource has been exhausted\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        assert!(
            matches!(first, Err(ProviderError::RateLimited { retry_after: None })),
            "got {first:?}"
        );
        assert!(events.next().is_none(), "stream ends after the error chunk");
    }

    #[test]
    fn stream_error_chunk_unavailable_is_transient() {
        let bytes: &[u8] = b"data: {\"error\":{\"code\":503,\"message\":\"The service is currently unavailable\",\"status\":\"UNAVAILABLE\"}}\n\n";
        let mut events = stream_from_bytes(bytes);
        let first = events.next().unwrap();
        match first {
            Err(err) => assert!(err.is_transient(), "503 should retry: {err:?}"),
            other => panic!("expected Err, got {other:?}"),
        }
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
    fn usage_folds_thinking_tokens_into_output() {
        // Gemini 2.5+ excludes thinking from `candidatesTokenCount`;
        // every other provider counts thinking in the output total.
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":4}}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        if let Some(ProviderEvent::MessageEnd { usage, .. }) = events.last() {
            assert_eq!(usage.input, 5);
            assert_eq!(usage.output, 6);
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
    fn parallel_same_name_function_calls_get_distinct_ids() {
        let bytes: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"/a\"}}},{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"/b\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n";
        let events = collect_ok(stream_from_bytes(bytes));
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolCallStart { id, .. } => Some(id.0.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "both calls must surface");
        assert_ne!(ids[0], ids[1], "same-name parallel calls need unique ids");
    }

    #[test]
    fn tool_result_response_names_the_function_from_its_call() {
        let assistant = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("gemini_read_1"),
                name: "read".into(),
                input: serde_json::json!({"path": "/a"}),
            }],
            None,
        );
        let result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("gemini_read_1"),
                output: "ok".into(),
                is_error: false,
            }],
            None,
        );
        let req = StreamRequest::new("m", vec![user_msg("go"), assistant, result]);
        let body = build_request_body(&req);
        let contents = body["contents"].as_array().unwrap();
        let fr = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "read");
    }

    #[test]
    fn tool_name_from_call_id_parses_new_and_legacy_formats() {
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_read_1")),
            "read"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_my_tool_12")),
            "my_tool"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_read")),
            "read"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("gemini_my_tool")),
            "my_tool"
        );
        assert_eq!(
            tool_name_from_call_id(&ToolCallId::new("call_abc")),
            "call_abc"
        );
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

    #[test]
    fn request_headers_include_auth_then_extras_in_key_order() {
        let mut extras = BTreeMap::new();
        extras.insert("X-B".to_owned(), "2".to_owned());
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = GeminiProvider::new("k").with_extra_headers(extras);
        assert_eq!(
            provider.request_headers(),
            vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-goog-api-key".to_owned(), "k".to_owned()),
                ("X-A".to_owned(), "1".to_owned()),
                ("X-B".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn request_headers_skip_auth_when_key_is_empty() {
        let mut extras = BTreeMap::new();
        extras.insert("X-A".to_owned(), "1".to_owned());
        let provider = GeminiProvider::new("").with_extra_headers(extras);
        let headers = provider.request_headers();
        assert!(
            headers.iter().all(|(name, _)| name != "x-goog-api-key"),
            "no credential header without a key: {headers:?}"
        );
        assert!(headers.contains(&("X-A".to_owned(), "1".to_owned())));
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
        let provider = GeminiProvider::new("k").with_models(models.clone());
        assert_eq!(provider.models(), models);
        assert!(GeminiProvider::new("k").models().is_empty());
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

    fn config(reasoning: Reasoning, level: ThinkingLevel) -> Value {
        build_request_body(&request(reasoning, level))["generationConfig"]["thinkingConfig"].clone()
    }

    #[test]
    fn effort_models_get_a_thinking_level() {
        let r = effort(&[Effort::Minimal, Effort::Low, Effort::High], false);
        assert_eq!(
            config(r, ThinkingLevel::High),
            serde_json::json!({"thinkingLevel": "high"})
        );
    }

    #[test]
    fn budget_models_stay_within_bounds_and_toggle_off_to_zero() {
        let r = Reasoning::Budget {
            min: 0,
            max: Some(24_576),
            toggle: true,
        };
        assert_eq!(
            config(r, ThinkingLevel::XHigh),
            serde_json::json!({"thinkingBudget": 24_576})
        );
        assert_eq!(
            config(r, ThinkingLevel::Off),
            serde_json::json!({"thinkingBudget": 0})
        );
        assert_eq!(
            config(Reasoning::Toggle, ThinkingLevel::High),
            serde_json::json!({"thinkingBudget": -1})
        );
        assert!(config(Reasoning::None, ThinkingLevel::High).is_null());
    }
}
