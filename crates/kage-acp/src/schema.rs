//! ACP request / response / notification schema (JSON-RPC 2.0).
//!
//! The wire format is JSON-RPC 2.0 carried by [`crate::framing`]. A
//! client sends requests (with an `id`) or notifications (no `id`);
//! the server replies with a result or error and streams loop
//! progress back as notifications.
//!
//! Supported methods: `initialize`, `prompt`, `cancel`,
//! `permission/respond`, `session/load`, `session/list`.

use serde::Deserialize;

/// JSON-RPC error codes the server emits. The negative range is
/// reserved by the spec; these are the standard ones.
pub mod error_code {
    /// Invalid JSON was received (handled at the framing layer, but
    /// kept here for completeness).
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal server error.
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// A JSON-RPC request id. Echoed verbatim on the response; the spec
/// allows a number or a string, so it is kept as a raw value.
pub type RequestId = serde_json::Value;

/// A failure decoding an incoming message into an [`AcpRequest`].
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The message was not a JSON object.
    #[error("acp: not a JSON-RPC object")]
    NotObject,
    /// The `method` field was absent or not a string.
    #[error("acp: missing or non-string `method`")]
    MissingMethod,
    /// The method name is not one this server implements.
    #[error("acp: unknown method `{0}`")]
    UnknownMethod(String),
    /// The `params` object did not match the method's schema.
    #[error("acp: bad params for `{method}`: {source}")]
    BadParams {
        /// The method whose params failed to decode.
        method: &'static str,
        /// The underlying serde failure.
        source: serde_json::Error,
    },
}

/// A decoded client -> server message.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpRequest {
    /// Present for requests (the server must reply with this id);
    /// `None` for notifications (no reply is sent).
    pub id: Option<RequestId>,
    /// The method and its decoded parameters.
    pub call: AcpCall,
}

/// One of the six supported methods, with parameters already decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpCall {
    /// Handshake; the client announces itself and the server replies
    /// with its capabilities.
    Initialize(InitializeParams),
    /// Run one prompt through the agent loop, streaming events back.
    Prompt(PromptParams),
    /// Cancel the in-flight prompt, if any.
    Cancel,
    /// The client's verdict for a pending `permission/request`.
    PermissionRespond(PermissionResponse),
    /// Replay a recorded session into the agent context.
    SessionLoad(SessionLoadParams),
    /// List recorded sessions.
    SessionList,
}

/// Parameters for `initialize`. All fields optional so older or
/// minimal clients still hand-shake.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct InitializeParams {
    /// Protocol version the client speaks, if it announces one.
    #[serde(default)]
    pub protocol_version: Option<String>,
}

/// Parameters for `prompt`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PromptParams {
    /// The user prompt text.
    pub prompt: String,
    /// Optional `provider:model` override for this run.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional session id (or prefix) to continue from.
    #[serde(default)]
    pub session: Option<String>,
}

/// Parameters for `session/load`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionLoadParams {
    /// Session id or unique prefix to replay.
    pub id: String,
}

/// Parameters for `permission/respond`: the client's allow/deny
/// verdict for a tool call the server asked about.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionResponse {
    /// Correlates with the `id` of the `permission/request`
    /// notification this answers.
    pub id: String,
    /// `true` to run the tool call, `false` to block it.
    pub allow: bool,
    /// Optional human-readable reason, surfaced when denied.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Decode a method's `params` object, tagging failures with the
/// method name for a clear error.
fn decode_params<T: serde::de::DeserializeOwned>(
    method: &'static str,
    params: serde_json::Value,
) -> Result<T, SchemaError> {
    serde_json::from_value(params).map_err(|source| SchemaError::BadParams { method, source })
}

/// Decode one incoming JSON-RPC message.
///
/// # Errors
///
/// Returns [`SchemaError::NotObject`] when `value` is not an object,
/// [`SchemaError::MissingMethod`] when `method` is absent,
/// [`SchemaError::UnknownMethod`] for an unsupported method, or
/// [`SchemaError::BadParams`] when `params` does not match the
/// method's schema.
pub fn parse_request(value: &serde_json::Value) -> Result<AcpRequest, SchemaError> {
    let obj = value.as_object().ok_or(SchemaError::NotObject)?;
    let method = obj
        .get("method")
        .and_then(serde_json::Value::as_str)
        .ok_or(SchemaError::MissingMethod)?;
    let id = obj.get("id").cloned();
    // Absent params default to an empty object, not null, so methods
    // whose params are all-optional (`initialize`) still decode.
    let params = obj
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let call = match method {
        "initialize" => AcpCall::Initialize(decode_params("initialize", params)?),
        "prompt" => AcpCall::Prompt(decode_params("prompt", params)?),
        "cancel" => AcpCall::Cancel,
        "permission/respond" => {
            AcpCall::PermissionRespond(decode_params("permission/respond", params)?)
        }
        "session/load" => AcpCall::SessionLoad(decode_params("session/load", params)?),
        "session/list" => AcpCall::SessionList,
        other => return Err(SchemaError::UnknownMethod(other.to_owned())),
    };
    Ok(AcpRequest { id, call })
}

/// Build a successful JSON-RPC response for `id`. Takes `result` by
/// value and moves it into the envelope (no clone).
#[must_use]
pub fn response_result(id: &RequestId, result: serde_json::Value) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(3);
    obj.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
    obj.insert("id".to_owned(), id.clone());
    obj.insert("result".to_owned(), result);
    serde_json::Value::Object(obj)
}

/// Build a JSON-RPC error response for `id`.
#[must_use]
pub fn response_error(id: &RequestId, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Build a server -> client notification (no `id`, no reply). Takes
/// `params` by value and moves it into the envelope (no clone).
#[must_use]
pub fn notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(3);
    obj.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
    obj.insert("method".to_owned(), serde_json::Value::from(method));
    obj.insert("params".to_owned(), params);
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prompt_with_optional_fields() {
        let v = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "prompt",
            "params": {"prompt": "hi", "model": "anthropic:claude"},
        });
        let req = parse_request(&v).unwrap();
        assert_eq!(req.id, Some(serde_json::json!(7)));
        assert_eq!(
            req.call,
            AcpCall::Prompt(PromptParams {
                prompt: "hi".into(),
                model: Some("anthropic:claude".into()),
                session: None,
            })
        );
    }

    #[test]
    fn parses_cancel_and_session_list_without_params() {
        let c = parse_request(&serde_json::json!({"id": 1, "method": "cancel"})).unwrap();
        assert_eq!(c.call, AcpCall::Cancel);
        let l = parse_request(&serde_json::json!({"method": "session/list"})).unwrap();
        assert_eq!(l.call, AcpCall::SessionList);
        assert_eq!(l.id, None, "no id means notification");
    }

    #[test]
    fn parses_permission_respond() {
        let v = serde_json::json!({
            "id": "x",
            "method": "permission/respond",
            "params": {"id": "perm-1", "allow": false, "reason": "nope"},
        });
        let req = parse_request(&v).unwrap();
        assert_eq!(
            req.call,
            AcpCall::PermissionRespond(PermissionResponse {
                id: "perm-1".into(),
                allow: false,
                reason: Some("nope".into()),
            })
        );
    }

    #[test]
    fn unknown_method_errors() {
        let e = parse_request(&serde_json::json!({"id": 1, "method": "boom"})).unwrap_err();
        assert!(matches!(e, SchemaError::UnknownMethod(m) if m == "boom"));
    }

    #[test]
    fn missing_method_errors() {
        let e = parse_request(&serde_json::json!({"id": 1})).unwrap_err();
        assert!(matches!(e, SchemaError::MissingMethod));
    }

    #[test]
    fn bad_params_errors_with_method() {
        let e = parse_request(&serde_json::json!({
            "id": 1, "method": "prompt", "params": {"model": 5},
        }))
        .unwrap_err();
        assert!(matches!(
            e,
            SchemaError::BadParams {
                method: "prompt",
                ..
            }
        ));
    }

    #[test]
    fn builders_shape_envelopes() {
        let id = serde_json::json!("abc");
        let ok = response_result(&id, serde_json::json!({"ok": true}));
        assert_eq!(ok["jsonrpc"], "2.0");
        assert_eq!(ok["id"], "abc");
        assert_eq!(ok["result"]["ok"], true);

        let err = response_error(&id, error_code::METHOD_NOT_FOUND, "no");
        assert_eq!(err["error"]["code"], error_code::METHOD_NOT_FOUND);
        assert_eq!(err["error"]["message"], "no");

        let note = notification("event", serde_json::json!({"type": "message_end"}));
        assert!(note.get("id").is_none());
        assert_eq!(note["method"], "event");
        assert_eq!(note["params"]["type"], "message_end");
    }
}
