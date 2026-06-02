//! Bidirectional JSON-RPC 2.0 peer over newline-delimited stdio.
//!
//! Both MCP and ACP speak JSON-RPC over stdio where each message is a
//! single line of JSON terminated by `\n`, and both are symmetric: a
//! party answers the other's requests and issues its own. So the
//! transport is a *peer*, not a one-way server. This crate is the single
//! shared implementation both `kage-mcp` and `kage-acp` re-export, so a
//! bugfix to the framing or id-routing lands once rather than drifting
//! between two near-identical copies.
//!
//! Concurrency follows the workspace rule: `std::thread` plus
//! `std::sync::mpsc`, no async. A reader thread parses each line and
//! either routes a response to the waiting [`Peer::request`] caller or
//! forwards a peer-initiated request/notification to the [`Inbound`]
//! channel the owner drains on its own thread. That split lets a
//! long-running inbound request (a prompt turn) issue its own outgoing
//! requests without deadlocking the reader.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// A JSON-RPC error object (`code` / `message`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("jsonrpc error {code}: {message}")]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
}

impl RpcError {
    /// Build an error with an arbitrary code.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `-32603` internal error, the catch-all for handler failures.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    /// `-32601` method not found.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("method not found: {method}"))
    }

    fn from_value(value: &serde_json::Value) -> Self {
        let code = value
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-32603);
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error")
            .to_owned();
        Self { code, message }
    }
}

/// A peer-initiated message handed to the owner via [`Inbound`].
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A request expecting a response; reply via [`Peer::respond`]
    /// with this exact `id`.
    Request {
        /// JSON-RPC id to echo on the response (number or string).
        id: serde_json::Value,
        /// Method name.
        method: String,
        /// Params object (`Null` when absent).
        params: serde_json::Value,
    },
    /// A notification: no reply.
    Notification {
        /// Method name.
        method: String,
        /// Params object (`Null` when absent).
        params: serde_json::Value,
    },
}

type Pending = Arc<Mutex<HashMap<i64, mpsc::Sender<Result<serde_json::Value, RpcError>>>>>;

/// The outgoing half of a JSON-RPC connection. Cloneable; every clone
/// shares the same writer and pending-response table.
#[derive(Clone)]
pub struct Peer {
    writer: Arc<Mutex<dyn Write + Send>>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
}

impl Peer {
    fn write(&self, value: &serde_json::Value) -> Result<(), RpcError> {
        let mut guard = self.writer.lock().expect("jsonrpc writer mutex poisoned");
        let mut line =
            serde_json::to_vec(value).map_err(|e| RpcError::internal(format!("encode: {e}")))?;
        line.push(b'\n');
        guard
            .write_all(&line)
            .and_then(|()| guard.flush())
            .map_err(|e| RpcError::internal(format!("write: {e}")))
    }

    /// Send a notification (no id, no reply).
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] if the value cannot be encoded or the
    /// write fails.
    pub fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), RpcError> {
        let mut obj = serde_json::Map::with_capacity(3);
        obj.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
        obj.insert("method".to_owned(), serde_json::Value::from(method));
        obj.insert("params".to_owned(), params);
        self.write(&serde_json::Value::Object(obj))
    }

    /// Reply to a peer request previously delivered as
    /// [`Inbound::Request`].
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] if the write fails.
    pub fn respond(
        &self,
        id: &serde_json::Value,
        outcome: Result<serde_json::Value, RpcError>,
    ) -> Result<(), RpcError> {
        let msg = match outcome {
            Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": e.code, "message": e.message},
            }),
        };
        self.write(&msg)
    }

    /// Send a request and block until the peer responds, the
    /// connection drops, or `should_cancel` returns `true`.
    ///
    /// # Errors
    ///
    /// Returns the peer's [`RpcError`], or a synthetic one when the
    /// connection closed or the call was cancelled.
    pub fn request_cancellable(
        &self,
        method: &str,
        params: serde_json::Value,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .expect("jsonrpc pending mutex poisoned")
            .insert(id, tx);
        let mut obj = serde_json::Map::with_capacity(4);
        obj.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
        obj.insert("id".to_owned(), serde_json::Value::from(id));
        obj.insert("method".to_owned(), serde_json::Value::from(method));
        obj.insert("params".to_owned(), params);
        if let Err(e) = self.write(&serde_json::Value::Object(obj)) {
            self.pending
                .lock()
                .expect("jsonrpc pending mutex poisoned")
                .remove(&id);
            return Err(e);
        }
        loop {
            if should_cancel() {
                self.pending
                    .lock()
                    .expect("jsonrpc pending mutex poisoned")
                    .remove(&id);
                return Err(RpcError::new(-32800, "request cancelled"));
            }
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(outcome) => return outcome,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RpcError::internal("connection closed"));
                }
            }
        }
    }

    /// Send a request and block until the peer responds or the
    /// connection drops.
    ///
    /// # Errors
    ///
    /// Returns the peer's [`RpcError`], or a synthetic one when the
    /// connection closed.
    pub fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.request_cancellable(method, params, &|| false)
    }
}

/// Start a JSON-RPC connection over `reader`/`writer`.
///
/// Spawns the reader thread and returns the cloneable [`Peer`], an
/// [`Inbound`] receiver the owner drains on its own thread, and the
/// reader's join handle. When the peer disconnects, the reader fails
/// every in-flight [`Peer::request`] and drops the inbound sender so
/// the receiver ends.
#[must_use]
pub fn connect<R, W>(reader: R, writer: W) -> (Peer, mpsc::Receiver<Inbound>, JoinHandle<()>)
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let writer: Arc<Mutex<dyn Write + Send>> = Arc::new(Mutex::new(writer));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let peer = Peer {
        writer,
        pending: Arc::clone(&pending),
        next_id: Arc::new(AtomicI64::new(1)),
    };
    let (in_tx, in_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut reader = reader;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if value.get("method").is_some() {
                let method = value
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let params = value
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let inbound = match value.get("id") {
                    Some(id) if !id.is_null() => Inbound::Request {
                        id: id.clone(),
                        method,
                        params,
                    },
                    _ => Inbound::Notification { method, params },
                };
                if in_tx.send(inbound).is_err() {
                    break;
                }
            } else if let Some(id) = value.get("id").and_then(serde_json::Value::as_i64) {
                if let Some(tx) = pending
                    .lock()
                    .expect("jsonrpc pending mutex poisoned")
                    .remove(&id)
                {
                    let outcome = value.get("error").map_or_else(
                        || {
                            Ok(value
                                .get("result")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null))
                        },
                        |err| Err(RpcError::from_value(err)),
                    );
                    let _ = tx.send(outcome);
                }
            }
        }
        for (_, tx) in pending
            .lock()
            .expect("jsonrpc pending mutex poisoned")
            .drain()
        {
            let _ = tx.send(Err(RpcError::internal("connection closed")));
        }
    });
    (peer, in_rx, handle)
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;

    /// Wire two `connect`ed peers back to back with OS pipes.
    fn pair() -> (
        (Peer, mpsc::Receiver<Inbound>),
        (Peer, mpsc::Receiver<Inbound>),
    ) {
        let (a_r, b_w) = std::io::pipe().unwrap();
        let (b_r, a_w) = std::io::pipe().unwrap();
        let (a_peer, a_in, _a) = connect(BufReader::new(a_r), a_w);
        let (b_peer, b_in, _b) = connect(BufReader::new(b_r), b_w);
        ((a_peer, a_in), (b_peer, b_in))
    }

    #[test]
    fn request_gets_routed_response() {
        let ((a_peer, _a_in), (b_peer, b_in)) = pair();
        let responder = thread::spawn(move || {
            if let Inbound::Request { id, method, params } = b_in.recv().unwrap() {
                assert_eq!(method, "ping");
                assert_eq!(params["n"], 1);
                b_peer
                    .respond(&id, Ok(serde_json::json!({"pong": true})))
                    .unwrap();
            } else {
                panic!("expected a request");
            }
        });
        let res = a_peer.request("ping", serde_json::json!({"n": 1})).unwrap();
        assert_eq!(res["pong"], true);
        responder.join().unwrap();
    }

    #[test]
    fn error_response_propagates() {
        let ((a_peer, _a_in), (b_peer, b_in)) = pair();
        thread::spawn(move || {
            if let Inbound::Request { id, .. } = b_in.recv().unwrap() {
                b_peer
                    .respond(&id, Err(RpcError::method_not_found("nope")))
                    .unwrap();
            }
        });
        let err = a_peer.request("nope", serde_json::Value::Null).unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn notification_arrives_without_id() {
        let ((a_peer, _a_in), (_b_peer, b_in)) = pair();
        a_peer
            .notify("session/cancel", serde_json::json!({"sessionId": "s1"}))
            .unwrap();
        match b_in.recv().unwrap() {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/cancel");
                assert_eq!(params["sessionId"], "s1");
            }
            Inbound::Request { .. } => panic!("notification must have no id"),
        }
    }

    #[test]
    fn cancellable_request_returns_cancelled() {
        let ((a_peer, _a_in), (_b_peer, _b_in)) = pair();
        // The peer never answers; should_cancel trips immediately.
        let err = a_peer
            .request_cancellable("x", serde_json::Value::Null, &|| true)
            .unwrap_err();
        assert_eq!(err.code, -32800);
    }

    #[test]
    fn pending_request_fails_when_connection_closes() {
        let (a_r, b_w) = std::io::pipe().unwrap();
        let (b_r, a_w) = std::io::pipe().unwrap();
        let (a_peer, _a_in, _h) = connect(BufReader::new(a_r), a_w);
        // Close the peer end so the reader hits EOF while a request
        // is outstanding.
        let waiter = thread::spawn(move || a_peer.request("x", serde_json::Value::Null));
        thread::sleep(Duration::from_millis(50));
        drop(b_w);
        drop(b_r);
        assert_eq!(waiter.join().unwrap().unwrap_err().code, -32603);
    }

    #[test]
    fn method_not_found_message_includes_method() {
        let err = RpcError::method_not_found("tools/call");
        assert_eq!(err.code, -32601);
        assert!(err.message.contains("tools/call"));
    }
}
