//! The JSON-RPC 2.0 messages both sides of an ACP connection speak.
//!
//! One [`Frame`] is one message, parsed from or rendered to a JSON
//! value. The framing is deliberately small: newline delimiting,
//! sockets and buffers belong to the transport a host runs the client
//! on, so this module only shapes values and never touches bytes on
//! the way anywhere.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC error object, as carried by a [`Frame::Failure`] and as
/// an agent answers requests it refuses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// The JSON-RPC error code, such as `-32601` for a method the peer
    /// does not implement.
    pub code: i64,
    /// Human-readable explanation.
    pub message: String,
    /// Extra detail, if the peer added any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with a code and message and no data.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// The `-32601` error for a method the peer does not serve.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("method not found: {method}"))
    }

    /// The `-32602` error for params that did not decode.
    #[must_use]
    pub fn invalid_params(detail: impl std::fmt::Display) -> Self {
        Self::new(-32602, format!("invalid params: {detail}"))
    }
}

/// One JSON-RPC 2.0 message, in either direction.
///
/// Ids are the numbers both kage sides assign; a message whose id is
/// anything else does not parse.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// A request that wants an answer, sent with a fresh id.
    Request {
        /// The id the answer repeats.
        id: u64,
        /// The method called.
        method: String,
        /// The call parameters, or `null` when the method takes none.
        params: Value,
    },
    /// A notification, which wants no answer.
    Notification {
        /// The method notified.
        method: String,
        /// The notification parameters, or `null` when it takes none.
        params: Value,
    },
    /// A successful answer to a request.
    Success {
        /// The id of the request answered.
        id: u64,
        /// The result payload.
        result: Value,
    },
    /// A failed answer to a request.
    Failure {
        /// The id of the request answered.
        id: u64,
        /// What went wrong.
        error: RpcError,
    },
}

impl Frame {
    /// The JSON value carrying this frame, with the `jsonrpc` version
    /// field set.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut message = serde_json::Map::new();
        message.insert("jsonrpc".into(), Value::String("2.0".into()));
        match self {
            Self::Request { id, method, params } => {
                message.insert("id".into(), Value::from(*id));
                message.insert("method".into(), Value::String(method.clone()));
                message.insert("params".into(), params.clone());
            }
            Self::Notification { method, params } => {
                message.insert("method".into(), Value::String(method.clone()));
                message.insert("params".into(), params.clone());
            }
            Self::Success { id, result } => {
                message.insert("id".into(), Value::from(*id));
                message.insert("result".into(), result.clone());
            }
            Self::Failure { id, error } => {
                message.insert("id".into(), Value::from(*id));
                message.insert(
                    "error".into(),
                    serde_json::to_value(error).unwrap_or(Value::Null),
                );
            }
        }
        Value::Object(message)
    }

    /// Parses one JSON message. Returns `None` for anything that is
    /// not a JSON-RPC 2.0 request, notification or response.
    #[must_use]
    pub fn parse(message: &Value) -> Option<Self> {
        let object = message.as_object()?;
        let id = object.get("id").and_then(Value::as_u64);
        if let Some(method) = object.get("method").and_then(Value::as_str) {
            let params = object.get("params").cloned().unwrap_or(Value::Null);
            return match id {
                Some(id) => Some(Self::Request {
                    id,
                    method: method.to_owned(),
                    params,
                }),
                None => Some(Self::Notification {
                    method: method.to_owned(),
                    params,
                }),
            };
        }
        let id = id?;
        if let Some(result) = object.get("result") {
            return Some(Self::Success {
                id,
                result: result.clone(),
            });
        }
        if let Some(error) = object.get("error") {
            let error: RpcError = serde_json::from_value(error.clone()).ok()?;
            return Some(Self::Failure { id, error });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_notifications_round_trip() {
        let request = Frame::Request {
            id: 7,
            method: "session/prompt".into(),
            params: serde_json::json!({"sessionId": "s1"}),
        };
        let parsed = Frame::parse(&request.to_value()).unwrap();
        assert_eq!(parsed, request);
        let notification = Frame::Notification {
            method: "session/cancel".into(),
            params: serde_json::json!({"sessionId": "s1"}),
        };
        let value = notification.to_value();
        assert!(value.get("id").is_none());
        assert_eq!(Frame::parse(&value).unwrap(), notification);
    }

    #[test]
    fn responses_round_trip_both_ways() {
        let success = Frame::Success {
            id: 3,
            result: serde_json::json!({"stopReason": "end_turn"}),
        };
        assert_eq!(Frame::parse(&success.to_value()).unwrap(), success);
        let failure = Frame::Failure {
            id: 4,
            error: RpcError::new(-32602, "unknown session s9"),
        };
        assert_eq!(Frame::parse(&failure.to_value()).unwrap(), failure);
    }

    #[test]
    fn malformed_messages_do_not_parse() {
        assert_eq!(Frame::parse(&serde_json::json!({"jsonrpc": "2.0"})), None);
        assert_eq!(Frame::parse(&serde_json::json!({"id": "s1"})), None);
        assert_eq!(
            Frame::parse(&serde_json::json!({"id": 1, "error": {"code": -1}})),
            None
        );
        let string_id = serde_json::json!({"id": "a", "result": null});
        assert_eq!(Frame::parse(&string_id), None);
    }

    #[test]
    fn missing_params_parse_as_null() {
        let bare = serde_json::json!({"jsonrpc": "2.0", "method": "ping"});
        match Frame::parse(&bare) {
            Some(Frame::Notification { method, params }) => {
                assert_eq!(method, "ping");
                assert_eq!(params, Value::Null);
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }
}
