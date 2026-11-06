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
//! Concurrency follows the workspace rule: `std::thread` plus blocking
//! channels, no async. A reader thread parses each line and
//! either routes a response to the waiting [`Peer::request`] caller or
//! forwards a peer-initiated request/notification to the [`Inbound`]
//! channel the owner drains on its own thread. That split lets a
//! long-running inbound request (a prompt turn) issue its own outgoing
//! requests without deadlocking the reader.
//!
//! A request abandoned on a cancel or at its deadline can tell the other
//! side: [`connect_with`] takes a [`CancelNotice`] that builds the
//! protocol's cancel notification.
//!
//! Malformed inbound lines get spec error replies (`-32700` for
//! unparseable or oversized input, `-32600` for a structurally invalid
//! request) instead of being dropped silently, and a single line may
//! not exceed `MAX_LINE` bytes. A response matching a pending request must
//! contain `"jsonrpc": "2.0"`. An invalid matched response fails that request
//! and ends the reader, while unknown and late responses remain silent. Objects
//! containing a `method` are classified as requests or notifications first.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, select_biased};
use kage_core::CancelFlag;
use kage_core::sync::lock;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// Interruptible, deadline-aware writes to Unix pipes.
#[cfg(unix)]
pub mod unix_pipe;

/// Ordered JSON-RPC peer over an interruptible Unix pipe writer.
#[cfg(unix)]
pub mod unix_peer;

/// Cap on a single inbound line, mirroring the HTTP transport's body
/// cap: one newline-delimited message cannot exhaust memory. A longer
/// line is answered with a -32700 error and the connection closes.
const MAX_LINE: u64 = 8 * 1024 * 1024;

/// Inbound messages the reader thread may queue for the owner before
/// `send` blocks it, so a chatty peer cannot grow the queue without
/// bound. A full queue blocks the reader before it can read and route
/// later responses. The owner must drain it concurrently, including while
/// waiting for requests. Deadline and cancellation APIs bound the wait for a
/// reply, but cannot interrupt a blocked writer.
const INBOUND_CAP: usize = 256;

/// A JSON-RPC error object (`code` / `message`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("jsonrpc error {code}: {message}")]
pub struct RpcError {
    /// JSON-RPC error code. Remote codes outside `i64` map to `-32603`.
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

    /// Whether the request failed because the connection closed, before
    /// or while it waited for an answer.
    #[must_use]
    pub fn is_connection_closed(&self) -> bool {
        self.code == -32603 && self.message == CLOSED
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

type Reply = Result<serde_json::Value, RpcError>;
type OrderedReply = (Reply, Option<ResponseBarrier>);

enum PendingReply {
    Delivered(OrderedReply),
    LocalError(RpcError),
}

impl PendingReply {
    fn into_legacy(self) -> OrderedReply {
        match self {
            Self::Delivered(reply) => reply,
            Self::LocalError(error) => (Err(error), None),
        }
    }
}

struct PendingRequest {
    tx: Sender<PendingReply>,
    ordered: bool,
}

#[derive(Default)]
struct PendingState {
    requests: HashMap<i64, PendingRequest>,
    closed: bool,
}

type Pending = Arc<Mutex<PendingState>>;

/// A response ordering acknowledgement. Wait on a different thread from the
/// inbound consumer, which must keep draining its queue.
pub struct ResponseBarrier(mpsc::Receiver<()>);

impl ResponseBarrier {
    /// Wait until the consumer acknowledges all preceding inbound messages.
    /// This can block indefinitely, including after disconnect if the marker
    /// is still held by the consumer. Use [`Self::wait_timeout`] to bound it.
    ///
    /// # Errors
    /// Returns an error if the marker was dropped without acknowledgement.
    pub fn wait(self) -> Result<(), mpsc::RecvError> {
        self.0.recv()
    }

    /// Wait for acknowledgement or local cancellation.
    ///
    /// Returns `Ok(true)` on acknowledgement and `Ok(false)` on cancellation.
    /// Cancellation does not acknowledge the marker or affect the connection.
    ///
    /// # Errors
    /// Returns an error if the marker was dropped without acknowledgement.
    pub fn wait_cancellable(self, cancel: &CancelFlag) -> Result<bool, mpsc::RecvError> {
        self.wait_cancellable_until(cancel, || false)
    }

    /// Wait for acknowledgement, cancellation, or the stop predicate.
    ///
    /// Returns `Ok(false)` if cancelled or stopped. The predicate is checked
    /// at least once per 10 ms, subject to scheduling. Neither condition
    /// acknowledges the marker. The caller can distinguish them by checking
    /// its cancellation flag and stop state.
    ///
    /// # Errors
    /// Returns an error if the marker was dropped without acknowledgement.
    pub fn wait_cancellable_until(
        self,
        cancel: &CancelFlag,
        stopped: impl Fn() -> bool,
    ) -> Result<bool, mpsc::RecvError> {
        loop {
            if cancel.is_cancelled() || stopped() {
                return Ok(false);
            }
            match self.0.recv_timeout(Duration::from_millis(10)) {
                Ok(()) => return Ok(!cancel.is_cancelled() && !stopped()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(mpsc::RecvError),
            }
        }
    }

    /// Wait at most `timeout` for acknowledgement.
    ///
    /// # Errors
    /// Returns a timeout or disconnection error.
    pub fn wait_timeout(self, timeout: Duration) -> Result<(), mpsc::RecvTimeoutError> {
        self.0.recv_timeout(timeout)
    }
}

/// An item in an ordering-aware inbound stream.
pub enum OrderedInbound {
    /// A peer request or notification.
    Message(Inbound),
    /// A marker after all messages preceding a response on the wire.
    Barrier(BarrierMarker),
}

/// A marker that the consumer acknowledges after handling preceding messages.
pub struct BarrierMarker(mpsc::Sender<()>);

impl BarrierMarker {
    /// Acknowledge this marker. Dropping it instead causes the waiter to fail.
    pub fn acknowledge(self) {
        let _ = self.0.send(());
    }
}

enum InboundSender {
    Plain(mpsc::SyncSender<Inbound>),
    Ordered(mpsc::SyncSender<OrderedInbound>),
}

impl InboundSender {
    fn send(&self, message: Inbound) -> bool {
        match self {
            Self::Plain(tx) => tx.send(message).is_ok(),
            Self::Ordered(tx) => tx.send(OrderedInbound::Message(message)).is_ok(),
        }
    }
}

/// Builds the notification a [`Peer`] sends when one of its requests is
/// abandoned on a cancel or at its deadline. It receives the request id and
/// method and returns the notification's method and params, or `None`
/// to send nothing for that request.
pub type CancelNotice = Arc<dyn Fn(i64, &str) -> Option<(String, serde_json::Value)> + Send + Sync>;

/// The outgoing half of a JSON-RPC connection. Cloneable; every clone
/// shares the same writer and pending-response table.
///
/// Outgoing requests and notifications omit `params` when passed
/// [`serde_json::Value::Null`]. Other values are sent unchanged without
/// validation, including nulls nested inside objects or arrays.
#[derive(Clone)]
pub struct Peer {
    writer: Arc<Mutex<dyn Write + Send>>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
    cancel_notice: Option<CancelNotice>,
    ordered: bool,
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
        if !params.is_null() {
            obj.insert("params".to_owned(), params);
        }
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
    /// connection drops, or `cancel` is cancelled. A flag that is
    /// already cancelled still sends the request, then abandons it.
    ///
    /// A cancelled request that was still pending sends the
    /// connection's [`CancelNotice`], if any, on a best-effort basis.
    ///
    /// # Errors
    ///
    /// Returns the peer's [`RpcError`], or a synthetic one when the
    /// connection closed or the call was cancelled.
    pub fn request_cancellable(
        &self,
        method: &str,
        params: serde_json::Value,
        cancel: &CancelFlag,
    ) -> Result<serde_json::Value, RpcError> {
        let (id, rx) = self.start(method, params, false)?;
        let watch = cancel.watch();
        select_biased! {
            recv(watch.receiver()) -> _ => Err(self.abandon(id, method)),
            recv(rx) -> reply => reply.map_or_else(|_| Err(closed()), |reply| reply.into_legacy().0),
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
        let (_, rx) = self.start(method, params, false)?;
        rx.recv()
            .map_or_else(|_| Err(closed()), |reply| reply.into_legacy().0)
    }

    /// Send a request and give up after `timeout` without an answer.
    ///
    /// A silent peer surfaces as [`RpcError::timed_out`] instead of
    /// blocking the caller forever. The request is abandoned like a
    /// cancelled one, [`CancelNotice`] included, and a late reply is
    /// dropped because the pending entry is gone.
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
        let (id, rx) = self.start(method, params, false)?;
        match rx.recv_timeout(timeout) {
            Ok(reply) => reply.into_legacy().0,
            Err(RecvTimeoutError::Timeout) => {
                self.abandon(id, method);
                Err(RpcError::timed_out(method))
            }
            Err(RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }

    /// Send a request whose response carries an ordering barrier.
    ///
    /// This requires a peer created by [`connect_ordered`]. The response,
    /// including a JSON-RPC error response, is returned separately from its
    /// barrier so the caller can decide where to wait. If the inbound receiver
    /// is dropped before the marker is queued, the response is returned without
    /// a barrier. A missing barrier also occurs when no response was delivered.
    pub fn request_ordered(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> (Result<serde_json::Value, RpcError>, Option<ResponseBarrier>) {
        if !self.ordered {
            return (
                Err(RpcError::internal("ordered inbound stream required")),
                None,
            );
        }
        match self.start(method, params, true) {
            Ok((_, rx)) => rx
                .recv()
                .map_or_else(|_| (Err(closed()), None), PendingReply::into_legacy),
            Err(error) => (Err(error), None),
        }
    }

    /// Send an ordered request and wait at most `timeout` for its response.
    ///
    /// The timeout does not interrupt writes or bound a subsequent barrier wait.
    /// If the request times out, a late response is discarded. Use
    /// [`ResponseBarrier::wait_timeout`] to bound acknowledgement separately.
    pub fn request_ordered_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> (Result<serde_json::Value, RpcError>, Option<ResponseBarrier>) {
        if !self.ordered {
            return (
                Err(RpcError::internal("ordered inbound stream required")),
                None,
            );
        }
        match self.start(method, params, true) {
            Ok((id, rx)) => match rx.recv_timeout(timeout) {
                Ok(reply) => reply.into_legacy(),
                Err(RecvTimeoutError::Timeout) => {
                    self.abandon(id, method);
                    (Err(RpcError::timed_out(method)), None)
                }
                Err(RecvTimeoutError::Disconnected) => (Err(closed()), None),
            },
            Err(error) => (Err(error), None),
        }
    }

    /// Register a pending entry, write the request, and return its id and
    /// the channel its reply arrives on.
    fn start(
        &self,
        method: &str,
        params: serde_json::Value,
        ordered: bool,
    ) -> Result<(i64, Receiver<PendingReply>), RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = crossbeam_channel::bounded(1);
        {
            let mut pending = lock(&self.pending);
            if pending.closed {
                return Err(closed());
            }
            pending.requests.insert(id, PendingRequest { tx, ordered });
        }
        let mut obj = serde_json::Map::with_capacity(4);
        obj.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
        obj.insert("id".to_owned(), serde_json::Value::from(id));
        obj.insert("method".to_owned(), serde_json::Value::from(method));
        if !params.is_null() {
            obj.insert("params".to_owned(), params);
        }
        if let Err(e) = self.write(&serde_json::Value::Object(obj)) {
            lock(&self.pending).requests.remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    /// Give up on request `id` and return the -32800 error. The
    /// [`CancelNotice`] goes out only when the entry was still pending: a
    /// missing one means the reader already answered or closed it.
    fn abandon(&self, id: i64, method: &str) -> RpcError {
        let pending = lock(&self.pending).requests.remove(&id).is_some();
        if pending
            && let Some((notice, params)) = self
                .cancel_notice
                .as_ref()
                .and_then(|build| build(id, method))
        {
            let _ = self.notify(&notice, params);
        }
        RpcError::new(-32800, "request cancelled")
    }
}

fn close_pending(pending: &Pending) {
    let mut pending = lock(pending);
    pending.closed = true;
    for (_, entry) in pending.requests.drain() {
        let _ = entry.tx.send(PendingReply::LocalError(closed()));
    }
}

/// The error a request gets when the connection closes under it.
const CLOSED: &str = "connection closed";

fn closed() -> RpcError {
    RpcError::internal(CLOSED)
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
    max_frame: Option<usize>,
}

struct LimitedFrame {
    bytes: Vec<u8>,
    cap: usize,
}

impl Write for LimitedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "outbound frame exceeds size cap",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ReaderPeer {
    fn respond(
        &self,
        id: &serde_json::Value,
        outcome: Result<serde_json::Value, RpcError>,
    ) -> Result<(), RpcError> {
        let writer = self.writer.upgrade().ok_or_else(closed)?;
        let mut line = if let Some(cap) = self.max_frame {
            let mut line = LimitedFrame {
                bytes: Vec::new(),
                cap: cap - 1,
            };
            let encode = (|| -> Result<(), Box<dyn std::error::Error>> {
                line.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":")?;
                serde_json::to_writer(&mut line, id)?;
                match outcome {
                    Ok(value) => {
                        line.write_all(b",\"result\":")?;
                        serde_json::to_writer(&mut line, &value)?;
                    }
                    Err(error) => {
                        line.write_all(b",\"error\":{\"code\":")?;
                        serde_json::to_writer(&mut line, &error.code)?;
                        line.write_all(b",\"message\":")?;
                        serde_json::to_writer(&mut line, &error.message)?;
                        line.write_all(b"}")?;
                    }
                }
                line.write_all(b"}")?;
                Ok(())
            })();
            encode.map_err(|e| RpcError::internal(format!("encode: {e}")))?;
            line.bytes
        } else {
            serde_json::to_vec(&response_message(id, outcome))
                .map_err(|e| RpcError::internal(format!("encode: {e}")))?
        };
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
/// the receiver ends. Abandoned requests send no notice; use
/// [`connect_with`] for that.
#[must_use]
pub fn connect<R, W>(reader: R, writer: W) -> (Peer, mpsc::Receiver<Inbound>, JoinHandle<()>)
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    connect_with(reader, writer, None)
}

/// [`connect`] with an optional [`CancelNotice`] that the [`Peer`] sends
/// whenever a request is abandoned on a cancel or at its deadline.
#[must_use]
pub fn connect_with<R, W>(
    reader: R,
    writer: W,
    cancel_notice: Option<CancelNotice>,
) -> (Peer, mpsc::Receiver<Inbound>, JoinHandle<()>)
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(INBOUND_CAP);
    let (peer, handle) = connect_inner(
        reader,
        writer,
        cancel_notice,
        InboundSender::Plain(tx),
        false,
        MAX_LINE,
        None,
        None,
    );
    (peer, rx, handle)
}

/// Start a connection with an ordering-aware inbound stream. The consumer must
/// acknowledge barriers only after handling every preceding message, and must
/// continuously drain the stream while requests are outstanding. A full
/// stream blocks the reader and prevents routing later responses. Ordered
/// request and barrier waits can otherwise block indefinitely. This constructor
/// does not send cancellation notices for abandoned requests.
#[must_use]
pub fn connect_ordered<R, W>(
    reader: R,
    writer: W,
) -> (Peer, mpsc::Receiver<OrderedInbound>, JoinHandle<()>)
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(INBOUND_CAP);
    let (peer, handle) = connect_inner(
        reader,
        writer,
        None,
        InboundSender::Ordered(tx),
        true,
        MAX_LINE,
        None,
        None,
    );
    (peer, rx, handle)
}

#[allow(clippy::too_many_arguments)]
fn connect_inner<R, W>(
    reader: R,
    writer: W,
    cancel_notice: Option<CancelNotice>,
    in_tx: InboundSender,
    ordered: bool,
    max_line: u64,
    max_frame: Option<usize>,
    on_reader_close: Option<Box<dyn FnOnce(bool) + Send>>,
) -> (Peer, JoinHandle<()>)
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let writer: Arc<Mutex<dyn Write + Send>> = Arc::new(Mutex::new(writer));
    let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
    let peer = Peer {
        writer: Arc::clone(&writer),
        pending: Arc::clone(&pending),
        next_id: Arc::new(AtomicI64::new(1)),
        cancel_notice,
        ordered,
    };
    let reader_peer = ReaderPeer {
        writer: Arc::downgrade(&writer),
        max_frame,
    };
    let handle = thread::spawn(move || {
        let mut reader = reader;
        let mut clean_eof = false;
        loop {
            let mut line = String::new();
            let n = match reader.by_ref().take(max_line + 1).read_line(&mut line) {
                // A line that is not valid UTF-8 can never parse, so
                // it gets the over-cap treatment: report once, then
                // drop the connection. Clean EOF and genuine I/O
                // errors stay silent.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    let _ = reply_error(
                        &reader_peer,
                        &serde_json::Value::Null,
                        -32700,
                        "line is not valid UTF-8",
                    );
                    break;
                }
                Ok(0) => {
                    clean_eof = true;
                    break;
                }
                Err(_) => break,
                Ok(n) => n,
            };
            if u64::try_from(n).unwrap_or(u64::MAX) > max_line {
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
            if !line.ends_with('\n') {
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
        if let Some(on_reader_close) = on_reader_close {
            on_reader_close(clean_eof);
        }
        close_pending(&pending);
    });
    (peer, handle)
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
fn route_line(peer: &ReaderPeer, pending: &Pending, in_tx: &InboundSender, line: &str) -> bool {
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
            in_tx.send(inbound)
        }
        Some(_) => reply_error(
            peer,
            &id,
            -32600,
            "invalid request: method must be a string",
        )
        .is_ok(),
        None => match obj.get("id") {
            // A response-shaped message: an id member and no method.
            // The spec forbids replying to a response, so an id that
            // matches nothing pending, a string id, or one beyond
            // `i64` range stays silent.
            Some(id) => {
                if let Some(id) = id.as_i64() {
                    return route_response(pending, in_tx, id, obj);
                }
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
fn route_response(
    pending: &Pending,
    in_tx: &InboundSender,
    id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let ordered = lock(pending).requests.get(&id).map(|entry| entry.ordered);
    if let Some(ordered) = ordered {
        if obj.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
            return reject_invalid_response(
                pending,
                id,
                "invalid response: jsonrpc must be \"2.0\"",
            );
        }
        let outcome = match (obj.get("error"), obj.get("result")) {
            (Some(err), None)
                if err.get("code").is_some_and(|code| {
                    code.as_i64().is_some()
                        || code.as_u64().is_some()
                        || code.as_f64().is_some_and(|number| number.fract() == 0.0)
                }) && err
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .is_some() =>
            {
                Err(RpcError::from_value(err))
            }
            (None, Some(result)) => Ok(result.clone()),
            _ => {
                return reject_invalid_response(
                    pending,
                    id,
                    "invalid response: expected result or error",
                );
            }
        };
        let barrier = if ordered {
            let (tx, rx) = mpsc::channel();
            if let InboundSender::Ordered(in_tx) = in_tx {
                if in_tx
                    .send(OrderedInbound::Barrier(BarrierMarker(tx)))
                    .is_err()
                {
                    if let Some(entry) = lock(pending).requests.remove(&id) {
                        let _ = entry.tx.send(PendingReply::Delivered((outcome, None)));
                    }
                    return false;
                }
                Some(ResponseBarrier(rx))
            } else {
                return false;
            }
        } else {
            None
        };
        if let Some(entry) = lock(pending).requests.remove(&id) {
            let _ = entry.tx.send(PendingReply::Delivered((outcome, barrier)));
        }
    }
    true
}

fn reject_invalid_response(pending: &Pending, id: i64, message: &str) -> bool {
    let Some(entry) = lock(pending).requests.remove(&id) else {
        return true;
    };
    let _ = entry
        .tx
        .send(PendingReply::LocalError(RpcError::new(-32600, message)));
    false
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;
    use crate::testing::pair;

    #[test]
    fn response_versions_are_checked_only_for_pending_ids() {
        for version in [
            None,
            Some(serde_json::json!(null)),
            Some(serde_json::json!(2)),
            Some(serde_json::json!("1.0")),
            Some(serde_json::json!({})),
        ] {
            let (peer, mut input, mut output, reader) = recorded(None);
            let (id, reply) = peer
                .start("matched", serde_json::Value::Null, false)
                .unwrap();
            let (_, other) = peer.start("other", serde_json::Value::Null, false).unwrap();
            next_line(&mut output);
            next_line(&mut output);
            let mut response = serde_json::json!({"id": id, "result": true});
            if let Some(version) = version {
                response["jsonrpc"] = version;
            }
            writeln!(input, "{response}").unwrap();
            let (result, barrier) = reply
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .into_legacy();
            assert_eq!(
                result.unwrap_err(),
                RpcError::new(-32600, "invalid response: jsonrpc must be \"2.0\"")
            );
            assert!(barrier.is_none());
            reader.join().unwrap();
            assert_eq!(other.recv().unwrap().into_legacy().0.unwrap_err(), closed());
            drop(peer);
            assert!(written(output).is_empty());
        }
    }

    #[test]
    fn unknown_and_abandoned_bad_versions_are_silent_and_do_not_close() {
        let (peer, mut input, mut output, reader) = recorded(None);
        let (late, _) = peer.start("late", serde_json::Value::Null, false).unwrap();
        next_line(&mut output);
        peer.abandon(late, "late");
        let (id, reply) = peer.start("live", serde_json::Value::Null, false).unwrap();
        next_line(&mut output);
        writeln!(input, "{}", serde_json::json!({"id":late,"result":0})).unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":false,"id":999,"result":0})
        )
        .unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"result":42})
        )
        .unwrap();
        assert_eq!(
            reply
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .into_legacy()
                .0
                .unwrap(),
            42
        );
        drop(input);
        reader.join().unwrap();
        drop(peer);
        assert!(written(output).is_empty());
    }

    #[test]
    fn invalid_response_does_not_close_on_concurrent_abandonment() {
        let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        lock(&pending)
            .requests
            .insert(7, PendingRequest { tx, ordered: true });
        assert!(lock(&pending).requests.contains_key(&7));
        lock(&pending).requests.remove(&7);
        assert!(reject_invalid_response(&pending, 7, "invalid"));
        assert!(rx.recv().is_err());

        let (tx, rx) = crossbeam_channel::bounded(1);
        lock(&pending)
            .requests
            .insert(8, PendingRequest { tx, ordered: true });
        assert!(!reject_invalid_response(&pending, 8, "invalid"));
        let (result, barrier) = rx.recv().unwrap().into_legacy();
        assert_eq!(result.unwrap_err().code, -32600);
        assert!(barrier.is_none());
    }

    #[test]
    fn unsigned_error_code_outside_i64_does_not_close_other_requests() {
        let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (other, _other_rx) = crossbeam_channel::bounded(1);
        lock(&pending)
            .requests
            .insert(1, PendingRequest { tx, ordered: false });
        lock(&pending).requests.insert(
            2,
            PendingRequest {
                tx: other,
                ordered: false,
            },
        );
        let (in_tx, _in_rx) = mpsc::sync_channel(1);
        let response = serde_json::json!({"jsonrpc": "2.0", "error": {"code": u64::MAX, "message": "failure"}});
        assert!(route_response(
            &pending,
            &InboundSender::Plain(in_tx),
            1,
            response.as_object().unwrap()
        ));
        let (result, barrier) = rx.recv().unwrap().into_legacy();
        assert_eq!(result.unwrap_err(), RpcError::internal("failure"));
        assert!(barrier.is_none());
        assert!(lock(&pending).requests.contains_key(&2));
    }

    #[test]
    fn ordered_peer_error_survives_dropped_inbound_receiver() {
        let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        lock(&pending)
            .requests
            .insert(1, PendingRequest { tx, ordered: true });
        let (in_tx, in_rx) = mpsc::sync_channel(1);
        drop(in_rx);
        let value = serde_json::json!({"jsonrpc": "2.0", "error": {"code": -32042, "message": "peer failure"}});
        assert!(!route_response(
            &pending,
            &InboundSender::Ordered(in_tx),
            1,
            value.as_object().unwrap()
        ));
        let (result, barrier) = rx.recv().unwrap().into_legacy();
        assert_eq!(result.unwrap_err(), RpcError::new(-32042, "peer failure"));
        assert!(barrier.is_none());
    }

    #[test]
    fn requests_after_reader_exit_are_rejected() {
        let (peer, _incoming, reader) =
            connect(std::io::Cursor::new(Vec::<u8>::new()), Vec::<u8>::new());
        reader.join().unwrap();
        assert_eq!(
            peer.request("late", serde_json::Value::Null).unwrap_err(),
            closed()
        );
        assert_eq!(
            peer.request_timeout("late", serde_json::Value::Null, Duration::from_secs(1))
                .unwrap_err(),
            closed()
        );
        assert_eq!(
            peer.request_ordered("late", serde_json::Value::Null)
                .0
                .unwrap_err(),
            RpcError::internal("ordered inbound stream required")
        );
    }

    #[cfg(unix)]
    #[test]
    fn barrier_wait_can_time_out_after_disconnect_with_marker_queued() {
        use std::os::unix::net::UnixStream;
        let (client, server) = UnixStream::pair().unwrap();
        let (peer, incoming, reader) =
            connect_ordered(BufReader::new(client.try_clone().unwrap()), client);
        let requester =
            thread::spawn(move || peer.request_ordered("test", serde_json::Value::Null));
        let mut input = BufReader::new(server.try_clone().unwrap());
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        let id = serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"].clone();
        let mut output = server;
        writeln!(
            output,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": 1})
        )
        .unwrap();
        let (result, barrier) = requester.join().unwrap();
        assert_eq!(result.unwrap(), 1);
        drop(output);
        drop(input);
        reader.join().unwrap();
        assert_eq!(
            barrier.unwrap().wait_timeout(Duration::from_millis(10)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(incoming);
    }

    #[cfg(unix)]
    #[test]
    fn full_inbound_queue_can_delay_response_but_ordered_request_times_out() {
        use std::os::unix::net::UnixStream;
        let (client, server) = UnixStream::pair().unwrap();
        let (peer, incoming, reader) =
            connect_ordered(BufReader::new(client.try_clone().unwrap()), client);
        let requester = thread::spawn(move || {
            peer.request_ordered_timeout(
                "test",
                serde_json::Value::Null,
                Duration::from_millis(200),
            )
        });
        let mut input = BufReader::new(server.try_clone().unwrap());
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        let id = serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"].clone();
        let mut output = server;
        for _ in 0..=INBOUND_CAP {
            writeln!(output, "{}", serde_json::json!({"method": "update"})).unwrap();
        }
        writeln!(
            output,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": 1})
        )
        .unwrap();
        let (result, barrier) = requester.join().unwrap();
        assert_eq!(result.unwrap_err(), RpcError::timed_out("test"));
        assert!(barrier.is_none());
        drop(incoming);
        drop(output);
        drop(input);
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ordered_responses_follow_messages_and_match_concurrent_ids() {
        use std::os::unix::net::UnixStream;
        let (client, server) = UnixStream::pair().unwrap();
        let (peer, incoming, reader) =
            connect_ordered(BufReader::new(client.try_clone().unwrap()), client);
        let mut input = BufReader::new(server.try_clone().unwrap());
        let mut output = server;
        let (tx, rx) = mpsc::channel();
        for method in ["first", "second"] {
            let peer = peer.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                tx.send((
                    method,
                    peer.request_ordered(method, serde_json::Value::Null),
                ))
                .unwrap();
            });
        }
        let mut ids = HashMap::new();
        for _ in 0..2 {
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            ids.insert(
                value["method"].as_str().unwrap().to_owned(),
                value["id"].clone(),
            );
        }
        writeln!(
            output,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "method":"update", "params": 1})
        )
        .unwrap();
        writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0", "id":ids["second"], "error":{"code": -1, "message":"bad"}})).unwrap();
        writeln!(
            output,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "method":"update", "params": 2})
        )
        .unwrap();
        writeln!(
            output,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id":ids["first"], "result":42})
        )
        .unwrap();
        let mut replies = HashMap::new();
        for _ in 0..2 {
            let (name, reply) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            replies.insert(name, reply);
        }
        let (result, barrier) = replies.remove("second").unwrap();
        assert_eq!(result.unwrap_err().code, -1);
        let second = barrier.unwrap();
        let (result, barrier) = replies.remove("first").unwrap();
        assert_eq!(result.unwrap(), 42);
        let first = barrier.unwrap();
        assert!(
            matches!(incoming.recv().unwrap(), OrderedInbound::Message(Inbound::Notification { params, .. }) if params == 1)
        );
        let OrderedInbound::Barrier(marker) = incoming.recv().unwrap() else {
            panic!("expected marker")
        };
        marker.acknowledge();
        second.wait().unwrap();
        assert!(
            matches!(incoming.recv().unwrap(), OrderedInbound::Message(Inbound::Notification { params, .. }) if params == 2)
        );
        let OrderedInbound::Barrier(marker) = incoming.recv().unwrap() else {
            panic!("expected marker")
        };
        drop(marker);
        assert!(first.wait().is_err());
        drop(output);
        drop(input);
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ordered_request_disconnects_and_dropped_consumer_fails_barrier() {
        use std::os::unix::net::UnixStream;
        let (client, server) = UnixStream::pair().unwrap();
        let (peer, incoming, reader) =
            connect_ordered(BufReader::new(client.try_clone().unwrap()), client);
        let mut input = BufReader::new(server.try_clone().unwrap());
        let mut output = server;
        let request = thread::spawn(move || peer.request_ordered("x", serde_json::Value::Null));
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        writeln!(
            output,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id":value["id"], "result":true})
        )
        .unwrap();
        let (result, barrier) = request.join().unwrap();
        assert_eq!(result.unwrap(), true);
        drop(incoming);
        assert!(barrier.unwrap().wait().is_err());
        drop(output);
        drop(input);
        reader.join().unwrap();

        let (client, server) = UnixStream::pair().unwrap();
        let (peer, _incoming, reader) =
            connect_ordered(BufReader::new(client.try_clone().unwrap()), client);
        let mut input = BufReader::new(server);
        let request = thread::spawn(move || peer.request_ordered("x", serde_json::Value::Null));
        line.clear();
        input.read_line(&mut line).unwrap();
        drop(input);
        let (result, barrier) = request.join().unwrap();
        assert!(result.is_err());
        assert!(barrier.is_none());
        reader.join().unwrap();
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

    fn cancelled() -> CancelFlag {
        let cancel = CancelFlag::new();
        cancel.cancel();
        cancel
    }

    #[test]
    fn cancellable_request_returns_cancelled() {
        let ((a_peer, _a_in), (_b_peer, _b_in)) = pair();
        let err = a_peer
            .request_cancellable("x", serde_json::Value::Null, &cancelled())
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

    fn test_notice() -> CancelNotice {
        Arc::new(|id, method| {
            Some((
                "test/cancelled".to_owned(),
                serde_json::json!({"requestId": id, "method": method}),
            ))
        })
    }

    /// A peer whose outgoing lines land in the returned reader. The
    /// input writer stays with the caller so the connection stays open.
    fn recorded(
        notice: Option<CancelNotice>,
    ) -> (
        Peer,
        std::io::PipeWriter,
        BufReader<std::io::PipeReader>,
        JoinHandle<()>,
    ) {
        let (in_r, in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (peer, _inbound, handle) = connect_with(BufReader::new(in_r), out_w, notice);
        (peer, in_w, BufReader::new(out_r), handle)
    }

    /// Every line written until the last [`Peer`] clone is dropped.
    fn written(out: BufReader<std::io::PipeReader>) -> Vec<serde_json::Value> {
        out.lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn outbound_params_and_null_result_wire_frames() {
        let (peer, mut input, mut output, reader) = recorded(None);
        for params in [
            serde_json::Value::Null,
            serde_json::json!({"nested": null}),
            serde_json::json!([null, {"nested": null}]),
            serde_json::json!(42),
        ] {
            peer.notify("notice", params.clone()).unwrap();
            let notice = next_line(&mut output);
            let mut expected = serde_json::json!({"jsonrpc": "2.0", "method": "notice"});
            if !params.is_null() {
                expected["params"] = params.clone();
            }
            assert_eq!(notice, expected);

            let caller = peer.clone();
            let request_params = params.clone();
            let request = thread::spawn(move || caller.request("call", request_params));
            let frame = next_line(&mut output);
            let id = frame["id"].clone();
            let mut expected = serde_json::json!({"jsonrpc": "2.0", "method": "call", "id": id});
            if !params.is_null() {
                expected["params"] = params;
            }
            assert_eq!(frame, expected);
            writeln!(
                input,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})
            )
            .unwrap();
            assert_eq!(request.join().unwrap().unwrap(), serde_json::Value::Null);
        }
        peer.respond(&serde_json::json!(7), Ok(serde_json::Value::Null))
            .unwrap();
        assert_eq!(
            next_line(&mut output),
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": null})
        );
        drop(input);
        reader.join().unwrap();
    }

    #[test]
    fn cancelled_request_sends_one_notice() {
        let (peer, _in_w, out, _h) = recorded(Some(test_notice()));
        let err = peer
            .request_cancellable("tools/call", serde_json::Value::Null, &cancelled())
            .unwrap_err();
        assert_eq!(err.code, -32800);
        drop(peer);
        let lines = written(out);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1]["method"], "test/cancelled");
        assert_eq!(lines[1]["params"]["requestId"], lines[0]["id"]);
        assert_eq!(lines[1]["params"]["method"], "tools/call");
        assert!(lines[1].get("id").is_none(), "a notice is a notification");
    }

    #[test]
    fn timed_out_request_sends_one_notice() {
        let (peer, _in_w, out, _h) = recorded(Some(test_notice()));
        let err = peer
            .request_timeout("slow", serde_json::Value::Null, Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.code, -32000);
        drop(peer);
        let lines = written(out);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1]["params"]["requestId"], lines[0]["id"]);
        assert_eq!(lines[1]["params"]["method"], "slow");
    }

    /// Read the next line the peer wrote.
    fn next_line(out: &mut BufReader<std::io::PipeReader>) -> serde_json::Value {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn closed_connection_sends_no_notice() {
        let (peer, in_w, mut out, handle) = recorded(Some(test_notice()));
        let call = {
            let peer = peer.clone();
            thread::spawn(move || {
                peer.request_cancellable("x", serde_json::Value::Null, &CancelFlag::new())
            })
        };
        assert_eq!(next_line(&mut out)["method"], "x");
        drop(in_w);
        handle.join().unwrap();
        let err = call.join().unwrap().unwrap_err();
        assert_eq!(err.code, -32603);
        assert!(err.message.contains("connection closed"), "{err}");
        drop(peer);
        assert!(written(out).is_empty());
    }

    #[test]
    fn request_cancelled_while_blocked_sends_one_notice() {
        let (peer, _in_w, mut out, _h) = recorded(Some(test_notice()));
        let cancel = CancelFlag::new();
        let call = {
            let peer = peer.clone();
            let cancel = cancel.clone();
            thread::spawn(move || {
                peer.request_cancellable("slow", serde_json::Value::Null, &cancel)
            })
        };
        let request = next_line(&mut out);
        cancel.cancel();
        assert_eq!(call.join().unwrap().unwrap_err().code, -32800);
        drop(peer);
        let lines = written(out);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["method"], "test/cancelled");
        assert_eq!(lines[0]["params"]["requestId"], request["id"]);
    }

    #[test]
    fn peer_without_notice_sends_nothing_on_cancel() {
        let (peer, _in_w, out, _h) = recorded(None);
        peer.request_cancellable("x", serde_json::Value::Null, &cancelled())
            .unwrap_err();
        drop(peer);
        assert_eq!(written(out).len(), 1);
    }

    #[test]
    fn notice_builder_may_decline() {
        let decline: CancelNotice = Arc::new(|_, _| None);
        let (peer, _in_w, out, _h) = recorded(Some(decline));
        peer.request_cancellable("x", serde_json::Value::Null, &cancelled())
            .unwrap_err();
        drop(peer);
        assert_eq!(written(out).len(), 1);
    }

    #[test]
    fn pending_request_fails_when_connection_closes() {
        let (peer, in_w, mut out, _h) = recorded(None);
        let waiter = thread::spawn(move || peer.request("x", serde_json::Value::Null));
        assert_eq!(next_line(&mut out)["method"], "x");
        drop(in_w);
        assert_eq!(waiter.join().unwrap().unwrap_err().code, -32603);
    }

    /// A request made after the peer hung up fails at once, even though
    /// writing to it still succeeds, instead of waiting for no one.
    #[test]
    fn a_request_after_the_connection_closed_fails_at_once() {
        let (peer, in_w, _out, handle) = recorded(None);
        drop(in_w);
        handle.join().unwrap();
        let err = peer.request("x", serde_json::Value::Null).unwrap_err();
        assert_eq!(err.code, -32603);
    }

    #[test]
    fn method_not_found_message_includes_method() {
        let err = RpcError::method_not_found("tools/call");
        assert_eq!(err.code, -32601);
        assert!(err.message.contains("tools/call"));
    }

    #[test]
    fn parse_error_gets_a_32700_reply() {
        let (_peer, mut in_w, mut out, _h) = recorded(None);
        in_w.write_all(b"not json\n").unwrap();
        let mut reply = String::new();
        out.read_line(&mut reply).unwrap();
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
        match inbound.recv_timeout(Duration::from_secs(5)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected disconnect, got {other:?}"),
        }
    }

    /// A line the parser can never accept gets the over-cap
    /// treatment: one `-32700` reply, then the connection ends, so
    /// the peer sees the reason instead of a bare close.
    #[test]
    fn a_non_utf8_line_replies_32700_and_ends_the_stream() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (_peer, inbound, _h) = connect(BufReader::new(in_r), out_w);
        in_w.write_all(b"{\"jsonrpc\":\"2.0\",\xff\n").unwrap();
        let mut reply = String::new();
        BufReader::new(out_r).read_line(&mut reply).unwrap();
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("UTF-8"), "{reply}");
        match inbound.recv_timeout(Duration::from_secs(5)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected disconnect, got {other:?}"),
        }
    }

    /// A response object carrying neither member must reach the
    /// waiting caller as an error, not as a successful `null`.
    #[test]
    fn a_response_with_neither_result_nor_error_is_an_error() {
        let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        lock(&pending)
            .requests
            .insert(7, PendingRequest { tx, ordered: false });
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7}"#).unwrap();
        let (in_tx, _in_rx) = mpsc::sync_channel(1);
        route_response(&pending, &InboundSender::Plain(in_tx), 7, &obj);
        let err = rx.recv().unwrap().into_legacy().0.unwrap_err();
        assert_eq!(err.code, -32600);
        assert!(err.message.contains("result or error"), "{err}");
    }

    /// The control for the test above: a legitimate `{"result": null}`
    /// still succeeds, so the fix does not turn nulls into errors.
    #[test]
    fn an_explicit_null_result_still_succeeds() {
        let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        lock(&pending)
            .requests
            .insert(7, PendingRequest { tx, ordered: false });
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"result":null}"#).unwrap();
        let (in_tx, _in_rx) = mpsc::sync_channel(1);
        route_response(&pending, &InboundSender::Plain(in_tx), 7, &obj);
        assert_eq!(
            rx.recv().unwrap().into_legacy().0.unwrap(),
            serde_json::Value::Null
        );
    }

    /// A near-cap inbound line must survive: the reader must not
    /// truncate, split or drop a message just under the cap.
    #[test]
    fn a_giant_just_under_the_cap_line_arrives_whole() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (peer, inbound, _handle) = connect(BufReader::new(in_r), out_w);
        let mut out = BufReader::new(out_r);
        let pad = usize::try_from(MAX_LINE / 2).unwrap();
        let raw = format!(
            "{{\"id\":77,\"method\":\"giant\",\"params\":{{\"pad\":\"{}\"}}}}\n",
            "p".repeat(pad),
        );
        assert!(
            (raw.len() as u64) <= MAX_LINE,
            "fixture stays under the cap"
        );
        in_w.write_all(raw.as_bytes()).unwrap();

        let call = {
            let peer = peer.clone();
            thread::spawn(move || peer.request("reply", serde_json::Value::Null))
        };
        let request = next_line(&mut out);
        assert_eq!(request["method"], "reply");
        in_w.write_all(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":42}}\n",
                request["id"]
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(call.join().unwrap().unwrap(), serde_json::json!(42));

        match inbound.recv_timeout(Duration::from_secs(5)) {
            Ok(Inbound::Request { id, method, params }) => {
                assert_eq!(id, serde_json::json!(77));
                assert_eq!(method, "giant");
                assert_eq!(
                    params["pad"].as_str().map(str::len),
                    Some(pad),
                    "no truncation"
                );
            }
            other => panic!("expected the giant request whole, got {other:?}"),
        }
    }

    #[test]
    fn batch_and_scalar_get_32600() {
        let (_peer, mut in_w, mut out, _h) = recorded(None);
        for raw in ["[1, 2]\n", "42\n", "{\"method\":42,\"id\":7}\n"] {
            in_w.write_all(raw.as_bytes()).unwrap();
        }
        let mut reply = String::new();
        out.read_line(&mut reply).unwrap();
        assert!(
            reply.contains("-32600") && reply.contains("expected one JSON-RPC message object"),
            "{reply}"
        );
        reply.clear();
        out.read_line(&mut reply).unwrap();
        assert!(reply.contains("-32600"), "{reply}");
        reply.clear();
        out.read_line(&mut reply).unwrap();
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
        let ((client, inbox), (peer, inbound)) = pair();
        let server = thread::spawn(move || {
            for message in inbound {
                if let Inbound::Request { id, .. } = message {
                    let _ = peer.respond(&id, Err(RpcError::method_not_found("bogus/method")));
                }
            }
        });
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
        let (_peer, mut in_w, mut out, _h) = recorded(None);
        in_w.write_all(b"{\"id\":999,\"result\":1}\n").unwrap();
        in_w.write_all(b"boom\n").unwrap();
        let mut reply = String::new();
        out.read_line(&mut reply).unwrap();
        // The first reply must belong to `boom`: the response-shaped
        // line was dropped, not answered with -32600.
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("parse error"), "{reply}");
    }

    /// A response with a string id has an id member, so it is a
    /// response: dropped, never answered, loop alive.
    #[test]
    fn string_id_response_stays_silent() {
        let (_peer, mut in_w, mut out, _h) = recorded(None);
        in_w.write_all(b"{\"id\":\"abc\",\"result\":1}\n").unwrap();
        in_w.write_all(b"boom\n").unwrap();
        let mut reply = String::new();
        out.read_line(&mut reply).unwrap();
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("parse error"), "{reply}");
    }

    /// An id beyond `i64` range cannot match the pending table, so it
    /// is dropped like any other unknown id instead of drawing a
    /// reply.
    #[test]
    fn id_beyond_i64_range_response_stays_silent() {
        let (_peer, mut in_w, mut out, _h) = recorded(None);
        in_w.write_all(b"{\"id\":18446744073709551615,\"result\":1}\n")
            .unwrap();
        in_w.write_all(b"boom\n").unwrap();
        let mut reply = String::new();
        out.read_line(&mut reply).unwrap();
        assert!(reply.contains("-32700"), "{reply}");
        assert!(reply.contains("parse error"), "{reply}");
    }

    /// A message with neither a method nor an id member is not a
    /// response, so it keeps its -32600 reply.
    #[test]
    fn idless_response_gets_a_32600_reply() {
        let (_peer, mut in_w, mut out, _h) = recorded(None);
        in_w.write_all(b"{\"result\":1}\n").unwrap();
        let mut reply = String::new();
        out.read_line(&mut reply).unwrap();
        assert!(reply.contains("-32600"), "{reply}");
        assert!(reply.contains("missing method or response id"), "{reply}");
    }

    /// Peers that assign string request ids get their answers under
    /// the same string: the inbound id is carried raw and echoed
    /// verbatim.
    #[test]
    fn string_id_requests_reach_the_owner_and_echo_the_string_id() {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let (peer, inbound, _h) = connect(BufReader::new(in_r), out_w);
        in_w.write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":\"agent-1\",\"method\":\"ping\",\"params\":{}}\n",
        )
        .unwrap();
        match inbound.recv_timeout(Duration::from_secs(5)).unwrap() {
            Inbound::Request { id, method, .. } => {
                assert_eq!(method, "ping");
                assert_eq!(id, serde_json::json!("agent-1"));
                peer.respond(&id, Ok(serde_json::json!({"pong": true})))
                    .unwrap();
            }
            Inbound::Notification { .. } => panic!("expected a request"),
        }
        let mut out = BufReader::new(out_r);
        let reply = next_line(&mut out);
        assert_eq!(reply["id"], serde_json::json!("agent-1"));
        assert_eq!(reply["result"]["pong"], true);
    }

    /// The reader parks on the full inbound queue instead of queueing
    /// every line the peer sends, and no queued message is lost.
    #[test]
    fn the_inbound_queue_is_bounded_and_keeps_every_message() {
        let line = "{\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n";
        let total = INBOUND_CAP + 44;
        let (_peer, inbound, handle) =
            connect(std::io::Cursor::new(line.repeat(total)), std::io::sink());
        // At EOF an unbounded channel would have let the reader parse
        // everything and exit; bounded, it waits on the blocked send.
        thread::sleep(Duration::from_millis(500));
        assert!(!handle.is_finished());
        let got = std::iter::from_fn(|| inbound.recv_timeout(Duration::from_secs(5)).ok()).count();
        assert_eq!(got, total);
        handle.join().unwrap();
    }
}
