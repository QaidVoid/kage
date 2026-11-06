//! Reusable client-side ACP connection over an already opened transport.
//!
//! The caller owns process management and must continuously drain [`Incoming`],
//! including while a blocking handshake or prompt is in progress. Incoming
//! messages retain their raw JSON, including unknown extensions. Requests are
//! never executed automatically. The underlying inbound queue is bounded, so
//! failing to drain it can block response delivery. Dropping [`Incoming`]
//! makes the transport reader disconnect when the next inbound message arrives.
//! Different sessions can issue prompts concurrently. The caller must queue
//! overlapping prompts for the same session.
//!
//! Cancellation is an ACP `session/cancel` notification, not a local abort.
//! A prompt waits for the agent's response even after cancellation, so the
//! caller knows when it is safe to start the next sequential prompt. A silent
//! or nonconforming agent can therefore leave it blocked indefinitely.

#[cfg(unix)]
use kage_core::CancelFlag;
#[cfg(unix)]
use kage_jsonrpc::unix_peer::{CommittedRequest, UnixPeer};
use std::collections::HashSet;
use std::io::{BufRead, Write};
#[cfg(unix)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvError, RecvTimeoutError};
use std::time::{Duration, Instant};

use kage_jsonrpc::{Inbound, OrderedInbound, Peer, RpcError, connect_ordered};

pub use kage_jsonrpc::ResponseBarrier;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::acp::{
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PROTOCOL_VERSION,
    PromptRequest, PromptResponse,
};
#[cfg(unix)]
use crate::acp::{
    LoadSessionRequest, LoadSessionResponse, ResumeSessionRequest, ResumeSessionResponse,
};

/// A reusable ACP client. Blocking operations require the incoming stream to
/// be drained concurrently. Prompts for different sessions may run in parallel;
/// the host must queue prompts for the same session.
pub struct Connection {
    peer: Peer,
    initialized: bool,
    active_prompts: Mutex<HashSet<String>>,
}

struct ActivePrompt<'a> {
    sessions: &'a Mutex<HashSet<String>>,
    session_id: &'a str,
}

impl Drop for ActivePrompt<'_> {
    fn drop(&mut self) {
        // Recovering here ensures a panic cannot permanently strand a session.
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(self.session_id);
    }
}

/// The peer's incoming notifications and requests, in arrival order.
pub struct Incoming {
    peer: IncomingPeer,
    receiver: Receiver<OrderedInbound>,
}

#[derive(Clone)]
enum IncomingPeer {
    Generic(Peer),
    #[cfg(unix)]
    Unix(UnixPeer),
}

/// A peer notification or a request that the caller must answer.
pub enum Message {
    /// A notification, including `session/update`.
    Notification {
        /// ACP method name.
        method: String,
        /// Unmodified JSON parameters.
        params: Value,
    },
    /// A request, including `session/request_permission`.
    Request(Request),
}

/// A single-use reply handle for a peer request.
///
/// Dropping this handle does not send a response. The caller must respond to
/// every request, including unsupported methods, to avoid leaving the peer
/// waiting indefinitely.
pub struct Request {
    /// ACP method name.
    pub method: String,
    /// Unmodified JSON parameters.
    pub params: Value,
    id: Value,
    answered: bool,
    peer: IncomingPeer,
}

impl Request {
    /// Answer this request exactly once with a JSON result or JSON-RPC error.
    ///
    /// # Errors
    /// Returns an error if the response cannot be written.
    pub fn respond(self, result: Result<Value, RpcError>) -> Result<(), RpcError> {
        if self.answered {
            return Err(RpcError::internal("request already answered"));
        }
        match self.peer {
            IncomingPeer::Generic(peer) => peer.respond(&self.id, result),
            #[cfg(unix)]
            IncomingPeer::Unix(peer) => peer.respond(&self.id, result, None, None),
        }
    }

    /// Respond over a Unix connection with an absolute deadline and cancellation.
    /// On write failure the handle is retained for retry. A partial frame closes
    /// the transport, making a subsequent retry fail. If the caller abandons a
    /// request, it should shut down the connection to unblock the peer. This
    /// rejects requests from the generic blocking connection.
    ///
    /// # Errors
    /// Returns an error for a generic connection or if the write fails.
    #[cfg(unix)]
    pub fn respond_bounded(
        &mut self,
        result: Result<Value, RpcError>,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> Result<(), RpcError> {
        if self.answered {
            return Err(RpcError::internal("request already answered"));
        }
        match &self.peer {
            IncomingPeer::Unix(peer) => {
                peer.respond(&self.id, result, deadline, cancel)?;
                self.answered = true;
                Ok(())
            }
            IncomingPeer::Generic(_) => Err(RpcError::internal(
                "bounded response requires Unix connection",
            )),
        }
    }
}

impl Incoming {
    fn convert(&self, message: Inbound) -> Message {
        match message {
            Inbound::Notification { method, params } => Message::Notification { method, params },
            Inbound::Request { id, method, params } => Message::Request(Request {
                id,
                method,
                params,
                answered: false,
                peer: self.peer.clone(),
            }),
        }
    }

    /// Wait for the next message, or return an error when the transport closes.
    ///
    /// # Errors
    /// Returns [`RecvError`] after the incoming stream closes.
    pub fn recv(&self) -> Result<Message, RecvError> {
        loop {
            match self.receiver.recv()? {
                OrderedInbound::Message(message) => return Ok(self.convert(message)),
                OrderedInbound::Barrier(marker) => marker.acknowledge(),
            }
        }
    }

    /// Wait up to `timeout` for the next message.
    ///
    /// # Errors
    /// Returns a timeout or disconnection error.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Message, RecvTimeoutError> {
        let start = Instant::now();
        loop {
            match self
                .receiver
                .recv_timeout(timeout.saturating_sub(start.elapsed()))?
            {
                OrderedInbound::Message(message) => return Ok(self.convert(message)),
                OrderedInbound::Barrier(marker) => marker.acknowledge(),
            }
        }
    }
}

/// Cloneable handle for requesting cancellation from another thread.
#[derive(Clone)]
pub struct Canceller {
    peer: Peer,
}

impl Canceller {
    /// Send `session/cancel` for a session. This does not guarantee that the
    /// agent stops, and the active prompt still needs to receive its response.
    /// The host must coordinate cancellation with prompt completion and the
    /// next prompt, so a late notification cannot cancel a later turn.
    ///
    /// # Errors
    /// Returns an error if the notification cannot be written.
    pub fn cancel(&self, session_id: &str) -> Result<(), RpcError> {
        self.peer.notify(
            "session/cancel",
            serde_json::json!({"sessionId": session_id}),
        )
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(format!("encode: {e}")))
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError::internal(format!("decode: {e}")))
}

impl Connection {
    /// Start the transport reader and return the connection and its incoming
    /// stream. Dropping these objects does not interrupt a blocked reader. The
    /// transport owner must arrange for the supplied reader to reach EOF.
    #[must_use]
    pub fn new<R: BufRead + Send + 'static, W: Write + Send + 'static>(
        reader: R,
        writer: W,
    ) -> (Self, Incoming) {
        let (peer, receiver, _reader_thread) = connect_ordered(reader, writer);
        (
            Self {
                peer: peer.clone(),
                initialized: false,
                active_prompts: Mutex::new(HashSet::new()),
            },
            Incoming {
                peer: IncomingPeer::Generic(peer),
                receiver,
            },
        )
    }

    /// Initialize the connection with protocol version 1. Advertise only
    /// capabilities the caller can handle. Discard the connection on failure,
    /// including a missing or failed barrier, because the agent may have initialized.
    ///
    /// The result and barrier are returned separately, including for a valid
    /// agent error response or a result decode error. A malformed JSON-RPC
    /// response, local failure, or disconnect may have no barrier. Before
    /// publishing completion or using the initialized connection, wait for the
    /// barrier on a separate thread. The incoming consumer must finish handling
    /// each message, including any asynchronously dispatched work, before
    /// receiving the next. It must continue receiving to acknowledge the marker.
    /// A missing barrier means ordering cannot be established. A queued marker may remain
    /// unacknowledged even after disconnect, so use [`ResponseBarrier::wait_timeout`]
    /// to bound that wait. Do not wait from an incoming message handler.
    ///
    /// The timeout covers only the response wait after the request is written.
    /// Encoding, acquiring the writer lock, writing, and barrier acknowledgement
    /// are not bounded. The generic writer can block indefinitely. A timed-out
    /// request is abandoned and a late response is discarded.
    pub fn initialize(
        &mut self,
        request: &InitializeRequest,
        timeout: Duration,
    ) -> (
        Result<InitializeResponse, RpcError>,
        Option<ResponseBarrier>,
    ) {
        if self.initialized || request.protocol_version != PROTOCOL_VERSION {
            return (
                Err(RpcError::internal(
                    "invalid initialize state or protocol version",
                )),
                None,
            );
        }
        let (result, barrier) = self.call_ordered_timeout("initialize", request, timeout);
        let result = result
            .and_then(decode)
            .and_then(|response: InitializeResponse| {
                if response.protocol_version == PROTOCOL_VERSION {
                    Ok(response)
                } else {
                    Err(RpcError::internal("unsupported ACP protocol version"))
                }
            });
        if result.is_ok() {
            self.initialized = true;
        }
        (result, barrier)
    }

    /// Create a session on the initialized connection. After a timeout, the
    /// agent may still create a session whose id is unavailable to the caller.
    ///
    /// The response and barrier follow the same ordering and timeout rules as
    /// [`Self::initialize`]. Wait for the barrier before exposing or using the
    /// session. A missing or failed barrier leaves ordering uncertain. Do not
    /// wait for it from an incoming message handler.
    pub fn new_session(
        &self,
        request: &NewSessionRequest,
        timeout: Duration,
    ) -> (
        Result<NewSessionResponse, RpcError>,
        Option<ResponseBarrier>,
    ) {
        if let Err(error) = self.ensure_initialized() {
            return (Err(error), None);
        }
        let (result, barrier) = self.call_ordered_timeout("session/new", request, timeout);
        (result.and_then(decode), barrier)
    }

    fn call_ordered_timeout<T: Serialize>(
        &self,
        method: &str,
        request: &T,
        timeout: Duration,
    ) -> (Result<Value, RpcError>, Option<ResponseBarrier>) {
        match encode(request) {
            Ok(params) => self.peer.request_ordered_timeout(method, params, timeout),
            Err(error) => (Err(error), None),
        }
    }

    /// Send a prompt and wait for its result. Use the session id returned by
    /// [`Self::new_session`] to continue the same conversation.
    ///
    /// # Errors
    /// Returns an error if not initialized, another prompt is active for this
    /// session, the request fails, or the result cannot be decoded.
    pub fn prompt(&self, request: &PromptRequest) -> Result<PromptResponse, RpcError> {
        self.ensure_initialized()?;
        let mut sessions = self
            .active_prompts
            .lock()
            .map_err(|_| RpcError::internal("active prompt state poisoned"))?;
        if !sessions.insert(request.session_id.clone()) {
            return Err(RpcError::internal("prompt already active for session"));
        }
        drop(sessions);
        let _active = ActivePrompt {
            sessions: &self.active_prompts,
            session_id: &request.session_id,
        };
        decode(self.peer.request("session/prompt", encode(request)?)?)
    }

    /// Send a prompt and return its response and an optional ordering barrier.
    ///
    /// If a response arrives, including an error response, the barrier is
    /// present unless the incoming receiver was dropped before it was queued.
    /// Wait for it on a thread separate from the incoming consumer
    /// before publishing prompt completion. The single incoming consumer must
    /// finish handling each message before calling `recv` or `recv_timeout`
    /// again. Dispatching work asynchronously requires the consumer to wait for
    /// that work itself. A dropped queued marker causes the barrier to fail.
    /// A live receiver can retain a marker after disconnect, so use
    /// [`ResponseBarrier::wait_timeout`] to bound that wait. Local errors and
    /// disconnects before a response have no barrier. The caller must continue
    /// to serialize prompts for each session, including any barrier wait.
    pub fn prompt_ordered(
        &self,
        request: &PromptRequest,
    ) -> (Result<PromptResponse, RpcError>, Option<ResponseBarrier>) {
        let run = || -> Result<_, RpcError> {
            self.ensure_initialized()?;
            let params = encode(request)?;
            let mut sessions = self
                .active_prompts
                .lock()
                .map_err(|_| RpcError::internal("active prompt state poisoned"))?;
            if !sessions.insert(request.session_id.clone()) {
                return Err(RpcError::internal("prompt already active for session"));
            }
            drop(sessions);
            let _active = ActivePrompt {
                sessions: &self.active_prompts,
                session_id: &request.session_id,
            };
            let (result, barrier) = self.peer.request_ordered("session/prompt", params);
            Ok((result.and_then(decode), barrier))
        };
        run().unwrap_or_else(|error| (Err(error), None))
    }

    /// Obtain a cancellation handle usable while a prompt is blocked.
    #[must_use]
    pub fn canceller(&self) -> Canceller {
        Canceller {
            peer: self.peer.clone(),
        }
    }

    fn ensure_initialized(&self) -> Result<(), RpcError> {
        if self.initialized {
            Ok(())
        } else {
            Err(RpcError::internal("connection not initialized"))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::thread;

    use crate::acp::{ClientCapabilities, ContentBlock, StopReason};
    use serde_json::json;

    fn read(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn send(writer: &mut UnixStream, value: Value) {
        writeln!(writer, "{value}").unwrap();
    }

    fn pair() -> (Connection, Incoming, BufReader<UnixStream>, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        let (conn, incoming) = Connection::new(BufReader::new(client.try_clone().unwrap()), client);
        (
            conn,
            incoming,
            BufReader::new(server.try_clone().unwrap()),
            server,
        )
    }

    fn init(conn: &mut Connection) {
        conn.initialize(
            &InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities: ClientCapabilities::default(),
                client_info: None,
            },
            Duration::from_secs(2),
        )
        .0
        .unwrap();
    }

    fn serve_init(reader: &mut BufReader<UnixStream>, writer: &mut UnixStream) {
        let msg = read(reader);
        assert_eq!(msg["method"], "initialize");
        send(
            writer,
            json!({"jsonrpc":"2.0", "id":msg["id"], "result":{"protocolVersion":1,"agentCapabilities":{}}}),
        );
    }

    #[test]
    fn initialization_and_session_barriers_follow_processing() {
        for method in ["initialize", "session/new"] {
            let (mut conn, incoming, mut reader, mut writer) = pair();
            conn.initialized = method == "session/new";
            let (release, released) = std::sync::mpsc::channel::<()>();
            let server = thread::spawn(move || {
                let request = read(&mut reader);
                assert_eq!(request["method"], method);
                send(
                    &mut writer,
                    json!({"method":"example/update","params":{"n":1}}),
                );
                let result = if method == "initialize" {
                    json!({"protocolVersion":1,"agentCapabilities":{}})
                } else {
                    json!({"sessionId":"s"})
                };
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                );
                released.recv().unwrap();
            });
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                let (ok, barrier) = if method == "initialize" {
                    let (result, barrier) = conn.initialize(
                        &InitializeRequest {
                            protocol_version: PROTOCOL_VERSION,
                            client_capabilities: ClientCapabilities::default(),
                            client_info: None,
                        },
                        Duration::from_secs(2),
                    );
                    (result.is_ok(), barrier)
                } else {
                    let (result, barrier) = conn.new_session(
                        &NewSessionRequest {
                            cwd: "/tmp".into(),
                            mcp_servers: vec![],
                            meta: None,
                        },
                        Duration::from_secs(2),
                    );
                    (result.is_ok(), barrier)
                };
                result_tx.send((ok, barrier)).unwrap();
            });
            assert!(
                matches!(incoming.recv().unwrap(), Message::Notification { params, .. } if params["n"] == 1)
            );
            let (ok, barrier) = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(ok);
            let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
            let waiter = thread::spawn(move || barrier_tx.send(barrier.unwrap().wait()).unwrap());
            assert!(matches!(
                barrier_rx.recv_timeout(Duration::from_millis(30)),
                Err(RecvTimeoutError::Timeout)
            ));
            assert!(matches!(
                incoming.recv_timeout(Duration::from_millis(30)),
                Err(RecvTimeoutError::Timeout)
            ));
            barrier_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            release.send(()).unwrap();
            waiter.join().unwrap();
            worker.join().unwrap();
            server.join().unwrap();
        }
    }

    #[test]
    fn ordered_handshake_errors_retain_barriers_and_timeouts_do_not() {
        for method in ["initialize", "session/new"] {
            let (mut conn, incoming, mut reader, mut writer) = pair();
            conn.initialized = method == "session/new";
            let (release, released) = std::sync::mpsc::channel::<()>();
            let server = thread::spawn(move || {
                let req = read(&mut reader);
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32001,"message":"failed"}}),
                );
                let req = read(&mut reader);
                released.recv().unwrap();
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":req["id"],"result":{}}),
                );
            });
            let call = |conn: &mut Connection, timeout| {
                if method == "initialize" {
                    let (r, b) = conn.initialize(
                        &InitializeRequest {
                            protocol_version: PROTOCOL_VERSION,
                            client_capabilities: ClientCapabilities::default(),
                            client_info: None,
                        },
                        timeout,
                    );
                    (r.map(|_| ()), b)
                } else {
                    let (r, b) = conn.new_session(
                        &NewSessionRequest {
                            cwd: "/tmp".into(),
                            mcp_servers: vec![],
                            meta: None,
                        },
                        timeout,
                    );
                    (r.map(|_| ()), b)
                }
            };
            let (result, barrier) = call(&mut conn, Duration::from_secs(2));
            assert_eq!(result.unwrap_err().code, -32001);
            assert!(matches!(
                incoming.recv_timeout(Duration::from_millis(10)),
                Err(RecvTimeoutError::Timeout)
            ));
            barrier
                .unwrap()
                .wait_timeout(Duration::from_secs(2))
                .unwrap();
            let (result, barrier) = call(&mut conn, Duration::from_millis(20));
            assert!(result.unwrap_err().message.contains("timed out"));
            assert!(barrier.is_none());
            release.send(()).unwrap();
            server.join().unwrap();
        }
    }

    #[test]
    fn generic_response_timeout_does_not_bound_write() {
        struct GatedWriter {
            stream: UnixStream,
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Write for GatedWriter {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                self.stream.write(data)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.stream.flush()
            }
        }
        let (client, server) = UnixStream::pair().unwrap();
        let (entered, in_write) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (mut conn, _incoming) = Connection::new(
            BufReader::new(client.try_clone().unwrap()),
            GatedWriter {
                stream: client,
                entered,
                release: released,
            },
        );
        let (done, completion) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let (result, barrier) = conn.initialize(
                &InitializeRequest {
                    protocol_version: PROTOCOL_VERSION,
                    client_capabilities: ClientCapabilities::default(),
                    client_info: None,
                },
                Duration::from_millis(30),
            );
            done.send((result, barrier)).unwrap();
        });
        in_write.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            completion.recv_timeout(Duration::from_millis(80)),
            Err(RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        let mut reader = BufReader::new(server);
        assert_eq!(read(&mut reader)["method"], "initialize");
        let (result, barrier) = completion.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.unwrap_err().message.contains("timed out"));
        assert!(barrier.is_none());
        worker.join().unwrap();
    }

    #[test]
    fn ordered_prompt_waits_for_consumer_after_error_response() {
        let (mut conn, incoming, mut reader, mut writer) = pair();
        conn.initialized = true;
        let (release, released) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let prompt = read(&mut reader);
            send(
                &mut writer,
                json!({"method":"session/update", "params":{"n":1}}),
            );
            send(
                &mut writer,
                json!({"jsonrpc":"2.0","id":prompt["id"], "error":{"code":-32001,"message":"failed"}}),
            );
            released.recv().unwrap();
        });
        let (done, completed) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let (result, barrier) = conn.prompt_ordered(&PromptRequest {
                session_id: "s".into(),
                prompt: vec![ContentBlock::text("hello")],
                delivery: None,
            });
            assert_eq!(result.unwrap_err().code, -32001);
            done.send("response").unwrap();
            barrier.unwrap().wait().unwrap();
            done.send("barrier").unwrap();
        });
        assert!(
            matches!(incoming.recv().unwrap(), Message::Notification { params, .. } if params["n"] == 1)
        );
        assert_eq!(
            completed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "response"
        );
        assert!(completed.recv_timeout(Duration::from_millis(30)).is_err());
        assert!(matches!(
            incoming.recv_timeout(Duration::from_millis(30)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert_eq!(
            completed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "barrier"
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn sequential_prompts_keep_updates_and_permission_requests() {
        let (mut conn, incoming, mut reader, mut writer) = pair();
        let server = thread::spawn(move || {
            serve_init(&mut reader, &mut writer);
            let msg = read(&mut reader);
            assert_eq!(msg["method"], "session/new");
            send(
                &mut writer,
                json!({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":"s1", "update":{"sessionUpdate":"extension", "extra":42}}}),
            );
            send(
                &mut writer,
                json!({"jsonrpc":"2.0", "id":msg["id"], "result":{"sessionId":"s1", "configOptions":[{"type":"future", "data":true}]}}),
            );
            for n in 1..=2 {
                let msg = read(&mut reader);
                assert_eq!(msg["method"], "session/prompt");
                assert_eq!(msg["params"]["sessionId"], "s1");
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":"s1", "update":{"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":format!("turn {n}")}}}}),
                );
                if n == 1 {
                    send(
                        &mut writer,
                        json!({"jsonrpc":"2.0", "id":"permission", "method":"session/request_permission", "params":{"sessionId":"s1", "extra":"preserved"}}),
                    );
                    let reply = read(&mut reader);
                    assert_eq!(reply["id"], "permission");
                    assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
                }
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0", "id":msg["id"], "result":{"stopReason":"end_turn"}}),
                );
            }
        });
        let drain = thread::spawn(move || {
            let mut updates = Vec::new();
            while let Ok(message) = incoming.recv() {
                match message {
                    Message::Notification { method, params } => {
                        assert_eq!(method, "session/update");
                        updates.push(params);
                    }
                    Message::Request(request) => {
                        assert_eq!(request.method, "session/request_permission");
                        assert_eq!(request.params["extra"], "preserved");
                        request
                            .respond(Ok(json!({"outcome":{"outcome":"cancelled"}})))
                            .unwrap();
                    }
                }
            }
            updates
        });
        init(&mut conn);
        let session = conn
            .new_session(
                &NewSessionRequest {
                    cwd: "/tmp".into(),
                    mcp_servers: vec![],
                    meta: None,
                },
                Duration::from_secs(2),
            )
            .0
            .unwrap();
        assert_eq!(session.session_id, "s1");
        assert!(session.config_options.is_empty());
        for _ in 0..2 {
            let result = conn
                .prompt(&PromptRequest {
                    session_id: session.session_id.clone(),
                    prompt: vec![ContentBlock::text("hello")],
                    delivery: None,
                })
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::EndTurn);
        }
        server.join().unwrap();
        let updates = drain.join().unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0]["update"]["extra"], 42);
        assert_eq!(updates[1]["update"]["content"]["text"], "turn 1");
        assert_eq!(updates[2]["update"]["content"]["text"], "turn 2");
    }

    #[test]
    fn handshake_can_receive_a_request_before_its_response() {
        let (mut conn, incoming, mut reader, mut writer) = pair();
        let server = thread::spawn(move || {
            let init = read(&mut reader);
            send(
                &mut writer,
                json!({"jsonrpc":"2.0", "id":7, "method":"example/request", "params":{"key":"value"}}),
            );
            let reply = read(&mut reader);
            assert_eq!(reply["id"], 7);
            assert_eq!(reply["error"]["code"], -32601);
            send(
                &mut writer,
                json!({"jsonrpc":"2.0", "id":init["id"], "result":{"protocolVersion":1,"agentCapabilities":{}}}),
            );
        });
        let drain = thread::spawn(move || {
            let Message::Request(request) = incoming.recv().unwrap() else {
                panic!("expected request")
            };
            assert_eq!(request.params["key"], "value");
            let error = RpcError::method_not_found(&request.method);
            request.respond(Err(error)).unwrap();
        });
        init(&mut conn);
        server.join().unwrap();
        drain.join().unwrap();
    }

    #[test]
    fn concurrent_sessions_and_duplicate_prompt() {
        use std::sync::{Arc, mpsc};

        let (mut conn, incoming, mut reader, mut writer) = pair();
        let (requests_tx, requests_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            serve_init(&mut reader, &mut writer);
            let first = read(&mut reader);
            let second = read(&mut reader);
            requests_tx.send((first.clone(), second.clone())).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            for request in [second, first] {
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0", "id":request["id"], "result":{"stopReason":"end_turn"}}),
                );
            }
        });
        init(&mut conn);
        let conn = Arc::new(conn);
        let workers: Vec<_> = ["s1", "s2"]
            .into_iter()
            .map(|session| {
                let conn = Arc::clone(&conn);
                thread::spawn(move || {
                    conn.prompt(&PromptRequest {
                        session_id: session.into(),
                        prompt: vec![ContentBlock::text("hello")],
                        delivery: None,
                    })
                })
            })
            .collect();
        let (first, second) = requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_ne!(first["params"]["sessionId"], second["params"]["sessionId"]);
        let error = conn
            .prompt(&PromptRequest {
                session_id: "s1".into(),
                prompt: vec![ContentBlock::text("duplicate")],
                delivery: None,
            })
            .unwrap_err();
        assert!(error.message.contains("already active"));
        release_tx.send(()).unwrap();
        for worker in workers {
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
        }
        server.join().unwrap();
        drop(incoming);
    }

    #[test]
    fn disconnect_settles_all_active_prompts() {
        use std::sync::{Arc, mpsc};

        let (mut conn, incoming, mut reader, mut writer) = pair();
        let (requests_tx, requests_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            serve_init(&mut reader, &mut writer);
            read(&mut reader);
            read(&mut reader);
            requests_tx.send(()).unwrap();
        });
        init(&mut conn);
        let conn = Arc::new(conn);
        let workers: Vec<_> = ["s1", "s2"]
            .into_iter()
            .map(|session| {
                let conn = Arc::clone(&conn);
                thread::spawn(move || {
                    conn.prompt(&PromptRequest {
                        session_id: session.into(),
                        prompt: vec![ContentBlock::text("hello")],
                        delivery: None,
                    })
                })
            })
            .collect();
        requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        server.join().unwrap();
        for worker in workers {
            assert!(
                worker
                    .join()
                    .unwrap()
                    .unwrap_err()
                    .message
                    .contains("connection closed")
            );
        }
        drop(incoming);
    }

    #[test]
    fn cancellation_notifies_and_waits_for_prompt_result() {
        let (mut conn, incoming, mut reader, mut writer) = pair();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            serve_init(&mut reader, &mut writer);
            let prompt = read(&mut reader);
            assert_eq!(prompt["method"], "session/prompt");
            started_tx.send(()).unwrap();
            let cancel = read(&mut reader);
            assert_eq!(cancel["method"], "session/cancel");
            assert_eq!(cancel["params"]["sessionId"], "s1");
            send(
                &mut writer,
                json!({"jsonrpc":"2.0", "id":prompt["id"], "result":{"stopReason":"cancelled"}}),
            );
        });
        init(&mut conn);
        let cancel = conn.canceller();
        let worker = thread::spawn(move || {
            conn.prompt(&PromptRequest {
                session_id: "s1".into(),
                prompt: vec![ContentBlock::text("hello")],
                delivery: None,
            })
            .unwrap()
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        cancel.cancel("s1").unwrap();
        assert_eq!(worker.join().unwrap().stop_reason, StopReason::Cancelled);
        server.join().unwrap();
        drop(incoming);
    }
}

/// ACP client using an interruptible Unix pipe writer.
///
/// The incoming stream must be drained continuously, including during calls and
/// barrier waits. Its single consumer must finish handling each message before
/// receiving the next. The caller must serialize each session until the prompt
/// response's barrier is acknowledged, including after local timeout or cancel.
/// A local timeout does not establish that the agent has stopped a prompt.
/// Deadlines do not bound encoding, scheduling, or incoming handling. At most
/// 256 active or permanently reserved sessions are tracked on one connection,
/// with session IDs limited to 4096 bytes.
#[cfg(unix)]
pub struct UnixConnection {
    peer: UnixPeer,
    initialized: bool,
    can_load: bool,
    can_resume: bool,
    active_prompts: Arc<Mutex<HashSet<String>>>,
}

/// A clonable handle for interrupting writes and settling pending requests.
///
/// Shutdown does not interrupt the separately owned reader.
#[cfg(unix)]
#[derive(Clone)]
pub struct UnixShutdown {
    peer: UnixPeer,
}

#[cfg(unix)]
impl UnixShutdown {
    /// Interrupt writes and settle pending requests without joining the reader.
    pub fn shutdown(&self) {
        self.peer.shutdown();
    }
}

/// A committed prompt. Dropping it permanently reserves its session on this
/// connection because the turn may still be active. Other sessions are not
/// affected. It never sends `session/cancel` automatically.
#[cfg(unix)]
pub struct UnixPrompt {
    request: Option<CommittedRequest>,
    peer: UnixPeer,
    session_id: String,
    settled: bool,
    cancel_state: Arc<Mutex<PromptCancelState>>,
}

#[cfg(unix)]
struct PromptCancelState {
    live: bool,
    settled: bool,
    in_flight: usize,
    session_id: String,
    active: Arc<Mutex<HashSet<String>>>,
}

#[cfg(unix)]
impl PromptCancelState {
    fn release_if_settled(&self) {
        if self.settled && self.in_flight == 0 {
            self.active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.session_id);
        }
    }
}

#[cfg(unix)]
impl Drop for UnixPrompt {
    fn drop(&mut self) {
        let mut state = self
            .cancel_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.live = false;
        state.settled = self.settled;
        state.release_if_settled();
    }
}

#[cfg(unix)]
impl UnixPrompt {
    /// Obtain a cancellation handle usable from another thread while waiting.
    #[must_use]
    pub fn canceller(&self) -> UnixPromptCanceller {
        UnixPromptCanceller {
            peer: self.peer.clone(),
            session_id: self.session_id.clone(),
            state: Arc::clone(&self.cancel_state),
        }
    }

    /// Wait for the response and its ordering barrier while incoming is drained.
    /// The session remains reserved until the barrier is acknowledged. A local
    /// timeout, cancellation, missing barrier, or failed barrier permanently
    /// reserves this session because completion cannot be established safely.
    /// The optional response deadline covers the response wait. The barrier
    /// timeout starts after the response arrives. Cancellation is observed during
    /// response wait, not while waiting for the barrier.
    ///
    /// # Errors
    /// Returns the agent error, a transport error, or a barrier failure.
    pub fn wait(
        mut self,
        response_deadline: Option<Instant>,
        barrier_timeout: Duration,
        cancel: Option<&CancelFlag>,
    ) -> Result<PromptResponse, RpcError> {
        let (result, barrier) = self
            .request
            .take()
            .expect("prompt request present")
            .wait(response_deadline, cancel);
        let Some(barrier) = barrier else {
            return match result {
                Err(error) => Err(error),
                Ok(_) => Err(RpcError::internal("response ordering barrier unavailable")),
            };
        };
        let acknowledged = barrier.wait_timeout(barrier_timeout).is_ok();
        if !acknowledged {
            return Err(RpcError::internal(
                "response ordering barrier failed or timed out",
            ));
        }
        match result {
            Ok(value) => {
                let decoded = decode(value);
                self.settled = decoded.is_ok();
                decoded
            }
            Err(error) => {
                self.settled = true;
                Err(error)
            }
        }
    }
}

/// Cloneable cancellation handle for exactly one committed prompt.
/// It cannot start a cancellation after the prompt has settled or been abandoned.
/// The session remains reserved while any earlier cancellation write is in flight,
/// so that write cannot affect a subsequent turn.
#[cfg(unix)]
#[derive(Clone)]
pub struct UnixPromptCanceller {
    peer: UnixPeer,
    session_id: String,
    state: Arc<Mutex<PromptCancelState>>,
}

#[cfg(unix)]
impl UnixPromptCanceller {
    /// Send a cancellation notification while the prompt is still active.
    /// A concurrent completion does not wait for the write, but the session
    /// remains reserved until all in-flight cancel writes have finished.
    /// The absolute deadline and flag cover state admission and the write, but
    /// not JSON encoding or thread scheduling.
    ///
    /// # Errors
    /// Returns an error if the prompt is no longer active or the write fails.
    pub fn cancel(&self, deadline: Instant, cancel: Option<&CancelFlag>) -> Result<(), RpcError> {
        if self.cancel_if_active(deadline, cancel)? {
            Ok(())
        } else {
            Err(RpcError::internal("prompt is no longer active"))
        }
    }

    /// Send a cancellation if the prompt is still active.
    ///
    /// Returns `Ok(false)` without attempting a write if the prompt has already
    /// settled or been abandoned. `Ok(true)` means the notification was sent.
    /// The session remains reserved until any admitted cancellation write ends.
    ///
    /// # Errors
    /// Returns an error if admission or the write fails. A write error does not
    /// establish that no bytes were sent.
    pub fn cancel_if_active(
        &self,
        deadline: Instant,
        cancel: Option<&CancelFlag>,
    ) -> Result<bool, RpcError> {
        let mut state = loop {
            if cancel.is_some_and(CancelFlag::is_cancelled) {
                return Err(RpcError::new(-32800, "cancel notification cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(RpcError::internal("cancel notification timed out"));
            }
            match self.state.try_lock() {
                Ok(state) => break state,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(RpcError::internal("prompt cancellation state poisoned"));
                }
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                ),
            }
        };
        if !state.live {
            return Ok(false);
        }
        state.in_flight += 1;
        drop(state);
        let result = self.peer.notify(
            "session/cancel",
            serde_json::json!({"sessionId": self.session_id}),
            Some(deadline),
            cancel,
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight -= 1;
        state.release_if_settled();
        result.map(|()| true)
    }
}

#[cfg(unix)]
impl UnixConnection {
    /// Start a connection with exclusive ownership of a writable pipe or FIFO.
    /// The descriptor's open file description must not be used elsewhere.
    /// Shutdown does not interrupt the reader. The owner must arrange EOF or
    /// otherwise interrupt it, and keep draining incoming to unblock the queue.
    /// See `kage_jsonrpc::unix_pipe::PipeWriter` for SIGPIPE and ownership rules.
    ///
    /// # Errors
    /// Returns an error if the descriptor is unsuitable or cannot be configured.
    pub fn new<R: BufRead + Send + 'static>(
        reader: R,
        writer: std::os::fd::OwnedFd,
    ) -> std::io::Result<(Self, Incoming)> {
        let (peer, receiver, _reader_thread) =
            kage_jsonrpc::unix_peer::connect_ordered(reader, writer)?;
        Ok((
            Self {
                peer: peer.clone(),
                initialized: false,
                can_load: false,
                can_resume: false,
                active_prompts: Arc::new(Mutex::new(HashSet::new())),
            },
            Incoming {
                peer: IncomingPeer::Unix(peer),
                receiver,
            },
        ))
    }

    /// Obtain a handle that can shut down this connection from another thread.
    #[must_use]
    pub fn shutdown_handle(&self) -> UnixShutdown {
        UnixShutdown {
            peer: self.peer.clone(),
        }
    }

    /// Interrupt writes and settle pending requests. This does not interrupt the reader.
    pub fn shutdown(&self) {
        self.peer.shutdown();
    }

    /// Initialize with one absolute deadline covering dispatch and response.
    /// Discard the connection on failure since the agent may have initialized.
    /// A returned barrier must be handled before publishing completion.
    pub fn initialize(
        &mut self,
        request: &InitializeRequest,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> (
        Result<InitializeResponse, RpcError>,
        Option<ResponseBarrier>,
    ) {
        if self.initialized || request.protocol_version != PROTOCOL_VERSION {
            return (
                Err(RpcError::internal(
                    "invalid initialize state or protocol version",
                )),
                None,
            );
        }
        let (result, barrier) = self.call("initialize", request, deadline, cancel);
        let result: Result<InitializeResponse, RpcError> =
            result
                .and_then(decode)
                .and_then(|response: InitializeResponse| {
                    if response.protocol_version == PROTOCOL_VERSION {
                        Ok(response)
                    } else {
                        Err(RpcError::internal("unsupported ACP protocol version"))
                    }
                });
        if let Ok(response) = &result {
            self.initialized = true;
            self.can_load = response.agent_capabilities.load_session;
            self.can_resume = response
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some();
        }
        (result, barrier)
    }

    /// Create a session with one deadline covering dispatch and response.
    /// A timeout may leave an agent session whose ID is unavailable. A returned
    /// barrier must be handled before publishing completion.
    pub fn new_session(
        &self,
        request: &NewSessionRequest,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> (
        Result<NewSessionResponse, RpcError>,
        Option<ResponseBarrier>,
    ) {
        if !self.initialized {
            return (Err(RpcError::internal("connection not initialized")), None);
        }
        let (result, barrier) = self.call("session/new", request, deadline, cancel);
        (result.and_then(decode), barrier)
    }

    /// Restore a session with upstream history replay when advertised.
    ///
    /// The caller must drain updates and wait for the returned response barrier
    /// before exposing the restored session. Serialize other work for this
    /// session until then. On timeout or uncertain completion, discard the
    /// connection instead of retrying or treating the session as restored.
    pub fn load_session(
        &self,
        request: &LoadSessionRequest,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> (
        Result<LoadSessionResponse, RpcError>,
        Option<ResponseBarrier>,
    ) {
        if !self.initialized || !self.can_load {
            return (Err(RpcError::internal("session/load not advertised")), None);
        }
        let (result, barrier) = self.call("session/load", request, deadline, cancel);
        (result.and_then(decode), barrier)
    }

    /// Restore a session without upstream history replay when advertised.
    ///
    /// The caller must drain updates and wait for the returned response barrier
    /// before exposing the restored session. Serialize other work for this
    /// session until then. On timeout or uncertain completion, discard the
    /// connection instead of retrying or treating the session as restored.
    pub fn resume_session(
        &self,
        request: &ResumeSessionRequest,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> (
        Result<ResumeSessionResponse, RpcError>,
        Option<ResponseBarrier>,
    ) {
        if !self.initialized || !self.can_resume {
            return (
                Err(RpcError::internal("session/resume not advertised")),
                None,
            );
        }
        let (result, barrier) = self.call("session/resume", request, deadline, cancel);
        (result.and_then(decode), barrier)
    }

    fn call<T: Serialize>(
        &self,
        method: &str,
        request: &T,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> (Result<Value, RpcError>, Option<ResponseBarrier>) {
        let handle =
            encode(request).and_then(|params| self.peer.dispatch(method, params, deadline, cancel));
        match handle {
            Ok(handle) => handle.wait(deadline, cancel),
            Err(error) => (Err(error), None),
        }
    }

    /// Commit a prompt before returning a handle that can send cancellation.
    /// Deadline and cancellation cover dispatch. Pass the same absolute deadline
    /// to `wait` to cover the response as well. Only one live handle per session
    /// is accepted. The handle retains the session reservation through its
    /// response barrier and permanently reserves the session if completion is uncertain.
    ///
    /// # Errors
    /// Returns an error if uninitialized, the session has a live prompt, or the
    /// request could not be committed. A partial write closes the transport.
    pub fn dispatch_prompt(
        &self,
        request: &PromptRequest,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> Result<UnixPrompt, RpcError> {
        if !self.initialized {
            return Err(RpcError::internal("connection not initialized"));
        }
        if request.session_id.len() > 4096 {
            return Err(RpcError::internal("session ID exceeds size limit"));
        }
        let params = encode(request)?;
        let mut sessions = self
            .active_prompts
            .lock()
            .map_err(|_| RpcError::internal("active prompt state poisoned"))?;
        if sessions.contains(&request.session_id) {
            return Err(RpcError::internal("prompt already active for session"));
        }
        if sessions.len() >= 256 {
            return Err(RpcError::internal(
                "too many active or unresolved prompt sessions",
            ));
        }
        sessions.insert(request.session_id.clone());
        drop(sessions);
        let active = UnixPrompt {
            request: None,
            peer: self.peer.clone(),
            session_id: request.session_id.clone(),
            settled: true,
            cancel_state: Arc::new(Mutex::new(PromptCancelState {
                live: true,
                settled: false,
                in_flight: 0,
                session_id: request.session_id.clone(),
                active: Arc::clone(&self.active_prompts),
            })),
        };
        let committed = self
            .peer
            .dispatch("session/prompt", params, deadline, cancel)?;
        let mut active = active;
        active.request = Some(committed);
        active.settled = false;
        Ok(active)
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use crate::acp::{ClientCapabilities, ContentBlock};
    use serde_json::json;
    use std::io::{BufReader, Read};
    use std::sync::mpsc;
    use std::thread;

    fn fixture() -> (
        UnixConnection,
        Incoming,
        BufReader<std::io::PipeReader>,
        std::io::PipeWriter,
    ) {
        let (agent_reader, client_writer) = std::io::pipe().unwrap();
        let (client_reader, agent_writer) = std::io::pipe().unwrap();
        let (conn, incoming) =
            UnixConnection::new(BufReader::new(client_reader), client_writer.into()).unwrap();
        (conn, incoming, BufReader::new(agent_reader), agent_writer)
    }

    fn read(reader: &mut BufReader<std::io::PipeReader>) -> Value {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }

    #[allow(clippy::unnecessary_wraps)]
    fn deadline() -> Option<Instant> {
        Some(Instant::now() + Duration::from_secs(3))
    }

    fn prompt(session: &str, text: &str) -> PromptRequest {
        PromptRequest {
            session_id: session.into(),
            prompt: vec![ContentBlock::text(text)],
            delivery: None,
        }
    }

    #[test]
    fn restore_methods_reject_missing_capabilities_after_initialization() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        let (done, wait) = mpsc::channel();
        let server = thread::spawn(move || {
            let init = read(&mut reader);
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":init["id"],"result":{
                    "protocolVersion":1,"agentCapabilities":{}
                }})
            )
            .unwrap();
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let drain = thread::spawn(move || while incoming.recv().is_ok() {});
        let (result, barrier) = conn.initialize(
            &InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities: ClientCapabilities::default(),
                client_info: None,
            },
            deadline(),
            None,
        );
        result.unwrap();
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(3))
            .unwrap();
        let load = LoadSessionRequest {
            session_id: "s".into(),
            cwd: "/tmp".into(),
            mcp_servers: vec![],
            meta: None,
        };
        let resume = ResumeSessionRequest {
            session_id: "s".into(),
            cwd: "/tmp".into(),
            mcp_servers: vec![],
            meta: None,
        };
        let (result, barrier) = conn.load_session(&load, deadline(), None);
        assert!(result.is_err());
        assert!(barrier.is_none());
        let (result, barrier) = conn.resume_session(&resume, deadline(), None);
        assert!(result.is_err());
        assert!(barrier.is_none());
        done.send(()).unwrap();
        server.join().unwrap();
        conn.shutdown();
        drain.join().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn restore_methods_require_advertised_capabilities_and_preserve_barriers() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        let load = LoadSessionRequest {
            session_id: "upstream".into(),
            cwd: "/tmp".into(),
            mcp_servers: vec![],
            meta: None,
        };
        let resume = ResumeSessionRequest {
            session_id: "upstream".into(),
            cwd: "/tmp".into(),
            mcp_servers: vec![],
            meta: None,
        };
        assert!(conn.load_session(&load, deadline(), None).0.is_err());
        let server = thread::spawn(move || {
            let init = read(&mut reader);
            writeln!(writer, "{}", json!({"jsonrpc":"2.0","id":init["id"],"result":{
                "protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}}}
            }})).unwrap();
            let new = read(&mut reader);
            assert_eq!(new["method"], "session/new");
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":new["id"],"result":{
                    "sessionId":"upstream", "configOptions":[{"type":"future"}]
                }})
            )
            .unwrap();
            for method in ["session/load", "session/resume"] {
                let request = read(&mut reader);
                assert_eq!(request["method"], method);
                assert_eq!(request["params"]["sessionId"], "upstream");
                writeln!(
                    writer,
                    "{}",
                    json!({"method":"session/update","params":{"sessionId":"upstream"}})
                )
                .unwrap();
                writeln!(writer, "{}", json!({"jsonrpc":"2.0","id":request["id"],"result":{"configOptions":[{"type":"future"},{"id":"model","name":"Model","type":"select","category":"future_category","currentValue":"m","options":[]}]}})).unwrap();
            }
        });
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let drain = thread::spawn(move || {
            while incoming.recv().is_ok() {
                seen_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        });
        let (result, barrier) = conn.initialize(
            &InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities: ClientCapabilities::default(),
                client_info: None,
            },
            deadline(),
            None,
        );
        result.unwrap();
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(3))
            .unwrap();
        let (result, barrier) = conn.new_session(
            &NewSessionRequest {
                cwd: "/tmp".into(),
                mcp_servers: vec![],
                meta: None,
            },
            deadline(),
            None,
        );
        let session = result.unwrap();
        assert_eq!(session.session_id, "upstream");
        assert!(session.config_options.is_empty());
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(3))
            .unwrap();
        let (result, barrier) = conn.load_session(&load, deadline(), None);
        let options = result.unwrap().config_options;
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].category, None);
        seen_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let (barrier_tx, barrier_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            barrier_tx.send(barrier.unwrap().wait()).unwrap();
        });
        assert!(matches!(
            barrier_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        barrier_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        waiter.join().unwrap();
        let (result, barrier) = conn.resume_session(&resume, deadline(), None);
        assert_eq!(result.unwrap().config_options, options);
        seen_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        release_tx.send(()).unwrap();
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(3))
            .unwrap();
        server.join().unwrap();
        conn.shutdown();
        drain.join().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn handshake_committed_cancel_and_response_barrier() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        let (update_tx, update_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let init = read(&mut reader);
            assert_eq!(init["method"], "initialize");
            writeln!(
                writer,
                "{}",
                json!({"id":7,"method":"extension/request","params":{}})
            )
            .unwrap();
            assert_eq!(read(&mut reader)["id"], 7);
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":init["id"],"result":{"protocolVersion":1,"agentCapabilities":{}}})
            )
            .unwrap();
            let session = read(&mut reader);
            assert_eq!(session["method"], "session/new");
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":session["id"],"result":{"sessionId":"s"}})
            )
            .unwrap();
            let prompt = read(&mut reader);
            assert_eq!(prompt["method"], "session/prompt");
            let cancel = read(&mut reader);
            assert_eq!(cancel["method"], "session/cancel");
            assert_eq!(cancel["params"]["sessionId"], "s");
            writeln!(
                writer,
                "{}",
                json!({"method":"session/update","params":{"n":1}})
            )
            .unwrap();
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"end_turn"}})
            )
            .unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let second = read(&mut reader);
            assert_eq!(second["method"], "session/prompt");
            writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":second["id"],"result":{"stopReason":"end_turn"}})
            )
            .unwrap();
        });
        let (allow_tx, allow_rx) = mpsc::channel();
        let drain = thread::spawn(move || {
            while let Ok(message) = incoming.recv() {
                match message {
                    Message::Request(mut request) => request
                        .respond_bounded(Ok(json!({})), deadline(), None)
                        .unwrap(),
                    Message::Notification { params, .. } => {
                        update_tx.send(params).unwrap();
                        allow_rx.recv().unwrap();
                    }
                }
            }
        });
        let (result, barrier) = conn.initialize(
            &InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities: ClientCapabilities::default(),
                client_info: None,
            },
            deadline(),
            None,
        );
        result.unwrap();
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(3))
            .unwrap();
        let (result, barrier) = conn.new_session(
            &NewSessionRequest {
                cwd: "/tmp".into(),
                mcp_servers: vec![],
                meta: None,
            },
            deadline(),
            None,
        );
        assert_eq!(result.unwrap().session_id, "s");
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(3))
            .unwrap();
        let handle = conn
            .dispatch_prompt(&prompt("s", "hello"), deadline(), None)
            .unwrap();
        let canceller = handle.canceller();
        let conn = Arc::new(conn);
        let worker = thread::spawn(move || handle.wait(deadline(), Duration::from_secs(3), None));
        canceller.cancel(deadline().unwrap(), None).unwrap();
        assert_eq!(
            update_rx.recv_timeout(Duration::from_secs(3)).unwrap()["n"],
            1
        );
        assert!(
            conn.dispatch_prompt(&prompt("s", "duplicate"), deadline(), None)
                .is_err()
        );
        allow_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        release_tx.send(()).unwrap();
        let second = conn
            .dispatch_prompt(&prompt("s", "second"), deadline(), None)
            .unwrap();
        assert!(canceller.cancel(deadline().unwrap(), None).is_err());
        second
            .wait(deadline(), Duration::from_secs(3), None)
            .unwrap();
        server.join().unwrap();
        conn.shutdown();
        drain.join().unwrap();
    }

    #[test]
    fn cancel_if_active_distinguishes_stale_from_write_failure() {
        let (mut conn, _incoming, mut reader, _writer) = fixture();
        conn.initialized = true;
        let first = conn
            .dispatch_prompt(&prompt("s", "first"), deadline(), None)
            .unwrap();
        assert_eq!(read(&mut reader)["method"], "session/prompt");
        let stale = first.canceller();
        drop(first);
        assert!(
            !stale
                .cancel_if_active(Instant::now() + Duration::from_secs(3), None)
                .unwrap()
        );
        let next = conn
            .dispatch_prompt(&prompt("other", "second"), deadline(), None)
            .unwrap();
        assert_eq!(read(&mut reader)["method"], "session/prompt");
        let live = next.canceller();
        conn.shutdown();
        assert!(
            live.cancel_if_active(Instant::now() + Duration::from_secs(3), None)
                .is_err()
        );
    }

    #[test]
    fn blocked_partial_prompt_cancel_closes_transport() {
        let (mut conn, incoming, mut reader, writer) = fixture();
        conn.initialized = true;
        let flag = CancelFlag::default();
        let signal = flag.clone();
        let server = thread::spawn(move || {
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            signal.cancel();
            // Do not drain the remaining pipe until dispatch has observed cancellation.
            (reader, writer)
        });
        let Err(err) = conn.dispatch_prompt(
            &prompt("s", &"x".repeat(2_000_000)),
            deadline(),
            Some(&flag),
        ) else {
            panic!("unexpected commit")
        };
        assert_eq!(err.code, -32800);
        let (_reader, writer) = server.join().unwrap();
        drop(writer);
        conn.shutdown();
        drop(incoming);
    }

    #[test]
    fn eof_and_abandoned_session_are_isolated() {
        let (mut conn, incoming, mut reader, writer) = fixture();
        conn.initialized = true;
        let handle = conn
            .dispatch_prompt(&prompt("s", "first"), deadline(), None)
            .unwrap();
        assert_eq!(read(&mut reader)["method"], "session/prompt");
        drop(handle);
        assert!(
            conn.dispatch_prompt(&prompt("s", "again"), deadline(), None)
                .is_err()
        );
        let other = conn
            .dispatch_prompt(&prompt("other", "ok"), deadline(), None)
            .unwrap();
        assert_eq!(read(&mut reader)["params"]["sessionId"], "other");
        drop(writer);
        assert!(
            other
                .wait(deadline(), Duration::from_secs(3), None)
                .is_err()
        );
        assert!(incoming.recv_timeout(Duration::from_secs(3)).is_err());
    }

    #[test]
    fn bounded_reply_can_be_retried_after_prewrite_cancellation() {
        let (_conn, incoming, mut reader, mut writer) = fixture();
        writeln!(writer, "{}", json!({"id":7,"method":"test","params":{}})).unwrap();
        let Message::Request(mut request) = incoming.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("request expected")
        };
        let flag = CancelFlag::default();
        flag.cancel();
        assert!(
            request
                .respond_bounded(Ok(json!(1)), deadline(), Some(&flag))
                .is_err()
        );
        request
            .respond_bounded(Ok(json!(2)), deadline(), None)
            .unwrap();
        assert!(
            request
                .respond_bounded(Ok(json!(3)), deadline(), None)
                .is_err()
        );
        assert_eq!(read(&mut reader)["result"], 2);
    }

    #[test]
    fn malformed_prompt_result_keeps_session_reserved() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        conn.initialized = true;
        let drain = thread::spawn(move || while incoming.recv().is_ok() {});
        let handle = conn
            .dispatch_prompt(&prompt("s", "first"), deadline(), None)
            .unwrap();
        let request = read(&mut reader);
        writeln!(
            writer,
            "{}",
            json!({"jsonrpc":"2.0","id":request["id"],"result":{}})
        )
        .unwrap();
        assert!(
            handle
                .wait(deadline(), Duration::from_secs(3), None)
                .is_err()
        );
        assert!(
            conn.dispatch_prompt(&prompt("s", "again"), deadline(), None)
                .is_err()
        );
        drop(writer);
        conn.shutdown();
        drain.join().unwrap();
    }

    #[test]
    fn reserved_session_budget_is_bounded() {
        let (mut conn, _incoming, _reader, _writer) = fixture();
        conn.initialized = true;
        {
            let mut sessions = conn.active_prompts.lock().unwrap();
            sessions.extend((0..256).map(|n| format!("s{n}")));
        }
        assert!(
            conn.dispatch_prompt(&prompt("new", "hello"), deadline(), None)
                .err()
                .unwrap()
                .message
                .contains("too many")
        );
        assert!(
            conn.dispatch_prompt(&prompt(&"s".repeat(4097), "hello"), deadline(), None)
                .err()
                .unwrap()
                .message
                .contains("size limit")
        );
        conn.shutdown();
    }

    #[test]
    fn blocked_cancel_does_not_block_prompt_completion_or_release_session() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        conn.initialized = true;
        let conn = Arc::new(conn);
        let drain = thread::spawn(move || while incoming.recv().is_ok() {});
        let handle = conn
            .dispatch_prompt(&prompt("s", "first"), deadline(), None)
            .unwrap();
        let request = read(&mut reader);
        let large_flag = CancelFlag::default();
        let large_worker_flag = large_flag.clone();
        let large_conn = Arc::clone(&conn);
        let large = thread::spawn(move || {
            large_conn
                .dispatch_prompt(
                    &prompt("other", &"x".repeat(2_000_000)),
                    deadline(),
                    Some(&large_worker_flag),
                )
                .err()
                .unwrap()
        });
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        let canceller = handle.canceller();
        let cancel_flag = CancelFlag::default();
        let worker_flag = cancel_flag.clone();
        let clone = canceller.clone();
        let cancellation = thread::spawn(move || {
            clone.cancel(Instant::now() + Duration::from_secs(3), Some(&worker_flag))
        });
        let until = Instant::now() + Duration::from_secs(2);
        while canceller.state.lock().unwrap().in_flight == 0 {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        writeln!(
            writer,
            "{}",
            json!({"jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"end_turn"}})
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            tx.send(handle.wait(deadline(), Duration::from_secs(3), None))
                .unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert!(
            conn.dispatch_prompt(&prompt("s", "second"), deadline(), None)
                .is_err()
        );
        cancel_flag.cancel();
        cancellation.join().unwrap().unwrap_err();
        assert!(!conn.active_prompts.lock().unwrap().contains("s"));
        assert!(
            canceller
                .cancel(Instant::now() + Duration::from_secs(1), None)
                .is_err()
        );
        large_flag.cancel();
        large.join().unwrap();
        drop(writer);
        conn.shutdown();
        drain.join().unwrap();
    }

    #[test]
    fn malformed_jsonrpc_response_does_not_release_session() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        conn.initialized = true;
        let drain = thread::spawn(move || while incoming.recv().is_ok() {});
        let handle = conn
            .dispatch_prompt(&prompt("s", "first"), deadline(), None)
            .unwrap();
        let request = read(&mut reader);
        writeln!(writer, "{}", json!({"jsonrpc":"2.0","id":request["id"]})).unwrap();
        assert!(
            handle
                .wait(deadline(), Duration::from_secs(3), None)
                .is_err()
        );
        assert!(conn.active_prompts.lock().unwrap().contains("s"));
        drop(writer);
        conn.shutdown();
        drain.join().unwrap();
    }

    #[test]
    fn indefinite_response_still_has_bounded_barrier() {
        let (mut conn, incoming, mut reader, mut writer) = fixture();
        conn.initialized = true;
        let handle = conn
            .dispatch_prompt(&prompt("s", "first"), deadline(), None)
            .unwrap();
        let request = read(&mut reader);
        writeln!(writer, "{}", json!({"method":"session/update","params":{}})).unwrap();
        writeln!(
            writer,
            "{}",
            json!({"jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"end_turn"}})
        )
        .unwrap();
        assert!(matches!(
            incoming.recv_timeout(Duration::from_secs(3)),
            Ok(Message::Notification { .. })
        ));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            tx.send(handle.wait(None, Duration::from_millis(30), None))
                .unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        assert!(conn.active_prompts.lock().unwrap().contains("s"));
        drop(writer);
        conn.shutdown();
    }
}
