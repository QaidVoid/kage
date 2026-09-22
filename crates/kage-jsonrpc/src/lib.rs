//! Bidirectional JSON-RPC 2.0 peer over newline-delimited stdio.
//!
//! Layering: leaf crate alongside `kage-core`, which it depends on for
//! the shared lock helpers and nothing else.
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
//!
//! Malformed inbound lines get spec error replies (`-32700` for
//! unparseable or oversized input, `-32600` for a structurally invalid
//! request) instead of being dropped silently, and a single line may
//! not exceed `MAX_LINE` bytes.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kage_core::sync::lock;

/// Cap on a single inbound line, mirroring the HTTP transport's body
/// cap: one newline-delimited message cannot exhaust memory. A longer
/// line is answered with a -32700 error and the connection closes.
const MAX_LINE: u64 = 8 * 1024 * 1024;

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

    /// `-32000` request exceeded its deadline without an answer.
    #[must_use]
    pub fn timed_out(method: &str) -> Self {
        Self::new(-32000, format!("request timed out: {method}"))
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
        let mut guard = lock(&self.writer);
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
        self.write(&response_message(id, outcome))
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
        lock(&self.pending).insert(id, tx);
        let mut obj = serde_json::Map::with_capacity(4);
        obj.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
        obj.insert("id".to_owned(), serde_json::Value::from(id));
        obj.insert("method".to_owned(), serde_json::Value::from(method));
        obj.insert("params".to_owned(), params);
        if let Err(e) = self.write(&serde_json::Value::Object(obj)) {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        loop {
            if should_cancel() {
                lock(&self.pending).remove(&id);
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

    /// Send a request and give up after `timeout` without an answer.
    ///
    /// Unlike [`Self::request_cancellable`] the poll closure is a
    /// deadline, so a silent peer surfaces as [`RpcError::timed_out`]
    /// instead of blocking the caller forever. Late replies arriving
    /// after the deadline are dropped: the pending entry is gone.
    ///
    /// # Errors
    ///
    /// Returns the peer's [`RpcError`], [`RpcError::timed_out`] when
    /// the deadline passes, or a synthetic one when the connection
    /// closed.
    pub fn request_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, RpcError> {
        let deadline = Instant::now() + timeout;
        self.request_cancellable(method, params, &|| Instant::now() >= deadline)
            .map_err(|e| {
                // -32800 here can only mean the deadline tripped: this
                // closure is the only cancel source in this call.
                if e.code == -32800 {
                    RpcError::timed_out(method)
                } else {
                    e
                }
            })
    }
}

/// A JSON-RPC response message shared by both reply paths.
fn response_message(
    id: &serde_json::Value,
    outcome: Result<serde_json::Value, RpcError>,
) -> serde_json::Value {
    match outcome {
        Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": e.code, "message": e.message},
        }),
    }
}

/// The reader thread's weakened view of the connection.
///
/// It must not hold the write half open: peers notice shutdown through
/// the writer closing, so a reader-held [`Peer`] would pin the stream
/// and deadlock every teardown that waits for a serve loop to end.
/// Error replies for malformed lines are therefore best-effort: they
/// work only while the owner's [`Peer`] is alive.
struct ReaderPeer {
    writer: Weak<Mutex<dyn Write + Send>>,
}

impl ReaderPeer {
    fn respond(
        &self,
        id: &serde_json::Value,
        outcome: Result<serde_json::Value, RpcError>,
    ) -> Result<(), RpcError> {
        let writer = self
            .writer
            .upgrade()
            .ok_or_else(|| RpcError::internal("connection closed"))?;
        let mut line = serde_json::to_vec(&response_message(id, outcome))
            .map_err(|e| RpcError::internal(format!("encode: {e}")))?;
        line.push(b'\n');
        let mut guard = lock(&writer);
        guard
            .write_all(&line)
            .and_then(|()| guard.flush())
            .map_err(|e| RpcError::internal(format!("write: {e}")))
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
        writer: Arc::clone(&writer),
        pending: Arc::clone(&pending),
        next_id: Arc::new(AtomicI64::new(1)),
    };
    let (in_tx, in_rx) = mpsc::channel();
    let reader_peer = ReaderPeer {
        writer: Arc::downgrade(&writer),
    };
    let handle = thread::spawn(move || {
        let mut reader = reader;
        loop {
            let mut line = String::new();
            let n = match reader.by_ref().take(MAX_LINE + 1).read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if u64::try_from(n).unwrap_or(u64::MAX) > MAX_LINE {
                // Same policy as the HTTP transport on an over-cap
                // body: report once, then drop the connection.
                let _ = reply_error(
                    &reader_peer,
                    &serde_json::Value::Null,
                    -32700,
                    "line exceeds size cap",
                );
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !route_line(&reader_peer, &pending, &in_tx, trimmed) {
                break;
            }
        }
        for (_, tx) in lock(&pending).drain() {
            let _ = tx.send(Err(RpcError::internal("connection closed")));
        }
    });
    (peer, in_rx, handle)
}

/// Best-effort error reply for a malformed inbound line.
fn reply_error(
    peer: &ReaderPeer,
    id: &serde_json::Value,
    code: i64,
    message: impl Into<String>,
) -> Result<(), RpcError> {
    peer.respond(id, Err(RpcError::new(code, message)))
}

/// Parse one already-capped inbound line and route it: a response to
/// the pending table, a request/notification to the owner, or a spec
/// error reply. Returns false when the reader loop must stop because
/// the writer or the owner is gone.
fn route_line(
    peer: &ReaderPeer,
    pending: &Pending,
    in_tx: &mpsc::Sender<Inbound>,
    line: &str,
) -> bool {
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(e) => {
            return reply_error(
                peer,
                &serde_json::Value::Null,
                -32700,
                format!("parse error: {e}"),
            )
            .is_ok();
        }
    };
    let Some(obj) = value.as_object() else {
        return reply_error(
            peer,
            &serde_json::Value::Null,
            -32600,
            "invalid request: expected one JSON-RPC message object",
        )
        .is_ok();
    };
    let id = obj.get("id").cloned().unwrap_or(serde_json::Value::Null);
    match obj.get("method") {
        Some(serde_json::Value::String(method)) => {
            let params = obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let inbound = if id.is_null() {
                Inbound::Notification {
                    method: method.clone(),
                    params,
                }
            } else {
                Inbound::Request {
                    id,
                    method: method.clone(),
                    params,
                }
            };
            in_tx.send(inbound).is_ok()
        }
        Some(_) => reply_error(
            peer,
            &id,
            -32600,
            "invalid request: method must be a string",
        )
        .is_ok(),
        None => match obj.get("id").and_then(serde_json::Value::as_i64) {
            Some(id) => {
                route_response(pending, id, obj);
                true
            }
            None => reply_error(
                peer,
                &id,
                -32600,
                "invalid request: missing method or response id",
            )
            .is_ok(),
        },
    }
}

/// Route a response-shaped object by i64 id. Unknown or late ids are
/// dropped: the spec forbids replying to a response.
fn route_response(pending: &Pending, id: i64, obj: &serde_json::Map<String, serde_json::Value>) {
    if let Some(tx) = lock(pending).remove(&id) {
        let outcome = obj.get("error").map_or_else(
            || {
                Ok(obj
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null))
            },
            |err| Err(RpcError::from_value(err)),
        );
        let _ = tx.send(outcome);
    }
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
    fn request_timeout_gives_up_when_the_peer_never_answers() {
        let ((a_peer, _a_in), (_b_peer, _b_in)) = pair();
        let start = std::time::Instant::now();
        let err = a_peer
            .request_timeout(
                "initialize",
                serde_json::Value::Null,
                Duration::from_millis(100),
            )
            .unwrap_err();
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("initialize"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "deadline must bound the wait, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn request_timeout_still_accepts_a_timely_reply() {
        let ((a_peer, _a_in), (b_peer, b_in)) = pair();
        thread::spawn(move || {
            if let Inbound::Request { id, .. } = b_in.recv().unwrap() {
                let _ = b_peer.respond(&id, Ok(serde_json::json!({"ok": true})));
            }
        });
        let res = a_peer
            .request_timeout("ping", serde_json::Value::Null, Duration::from_secs(5))
            .unwrap();
        assert_eq!(res["ok"], true);
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

    #[test]
    fn parse_error_gets_a_32700_reply() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (_peer, _inbound, _h) = connect(BufReader::new(in_r), out_w);
        in_w.write_all(b"not json\n").unwrap();
        let mut reply = String::new();
        BufReader::new(out_r).read_line(&mut reply).unwrap();
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("parse error"), "{reply}");
    }

    #[test]
    fn oversized_line_replies_32700_and_ends_the_stream() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (_peer, inbound, _h) = connect(BufReader::new(in_r), out_w);
        let len = usize::try_from(MAX_LINE + 16).unwrap();
        // The peer stops reading at the cap, so the tail of this write
        // surfaces as EPIPE; that failure is the signal, not a bug.
        let _ = in_w.write_all(&vec![b'x'; len]);
        let _ = in_w.write_all(b"\n");
        let mut reply = String::new();
        BufReader::new(out_r).read_line(&mut reply).unwrap();
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("size cap"), "{reply}");
        match inbound.recv_timeout(Duration::from_secs(1)) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected disconnect, got {other:?}"),
        }
    }

    #[test]
    fn batch_and_scalar_get_32600() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (_peer, _inbound, _h) = connect(BufReader::new(in_r), out_w);
        for raw in ["[1, 2]\n", "42\n", "{\"method\":42,\"id\":7}\n"] {
            in_w.write_all(raw.as_bytes()).unwrap();
        }
        let mut reader = BufReader::new(out_r);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        assert!(
            reply.contains("-32600") && reply.contains("expected one JSON-RPC message object"),
            "{reply}"
        );
        reply.clear();
        reader.read_line(&mut reply).unwrap();
        assert!(reply.contains("-32600"), "{reply}");
        reply.clear();
        reader.read_line(&mut reply).unwrap();
        assert!(
            reply.contains("-32600")
                && reply.contains("\"id\":7")
                && reply.contains("method must be a string"),
            "{reply}"
        );
    }

    /// Regression: the reader thread must not hold the write half
    /// open. A serve loop ends only when the client's writer closes,
    /// and that close happens on the last owner [`Peer`] drop; a
    /// reader-held Peer clone pinned the stream and deadlocked every
    /// serve-then-join teardown.
    #[test]
    fn dropping_the_client_peer_ends_the_server_loop() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || {
            let (peer, inbound, _h) = connect(BufReader::new(srv_r), srv_w);
            for message in inbound {
                if let Inbound::Request { id, .. } = message {
                    let _ = peer.respond(&id, Err(RpcError::method_not_found("bogus/method")));
                }
            }
        });
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let err = client
            .request("bogus/method", serde_json::Value::Null)
            .unwrap_err();
        assert_eq!(err.code, -32601);
        drop(client);
        drop(inbox);
        server.join().unwrap();
    }

    #[test]
    fn response_shaped_unknown_id_stays_silent() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (_peer, _inbound, _h) = connect(BufReader::new(in_r), out_w);
        in_w.write_all(b"{\"id\":999,\"result\":1}\n").unwrap();
        in_w.write_all(b"boom\n").unwrap();
        let mut reply = String::new();
        BufReader::new(out_r).read_line(&mut reply).unwrap();
        // The first reply must belong to `boom`: the response-shaped
        // line was dropped, not answered with -32600.
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("parse error"), "{reply}");
    }
}
