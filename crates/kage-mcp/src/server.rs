//! Spawn and handshake one external MCP server over stdio.
//!
//! [`McpServerHandle::spawn`] launches the child described by a
//! `[mcp.servers.<name>]` config block, wires its stdin/stdout into
//! the shared [`kage_jsonrpc`] peer, performs the MCP `initialize`
//! handshake, and then keeps the connection live for tool discovery
//! and calls. Dropping the handle kills the child so a crashed kage
//! never leaves orphaned server processes.
//!
//! The handshake is intentionally split from process spawning:
//! [`McpConnection::initialize`] works over any reader/writer so it
//! can be tested with in-process pipes, and the process plumbing in
//! [`McpServerHandle::spawn`] stays a thin shell on top.

use std::io::BufReader;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use kage_core::config::McpServer;

use kage_jsonrpc::{Inbound, Peer, RpcError, connect};

/// Protocol revision kage advertises in `initialize`. The server
/// replies with the revision it wants to speak; kage records it
/// (see [`McpConnection::protocol_version`]) and keeps working with
/// the server's choice rather than hard-failing on a mismatch.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long the `initialize` handshake waits before giving up on a
/// silent server.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client-issued request (e.g. `tools/list`) waits before
/// giving up on a silent server. Long-running `tools/call` is exempt:
/// it goes through [`Self::request_cancellable`] instead.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A failure spawning or talking to an MCP server.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The child process could not be spawned.
    #[error("spawn `{command}`: {source}")]
    Spawn {
        /// The command that failed to launch.
        command: String,
        /// The underlying OS error.
        source: std::io::Error,
    },
    /// The child was spawned but its stdio pipes were unavailable.
    #[error("server `{0}` exposed no stdio pipes")]
    NoStdio(String),
    /// A JSON-RPC call returned an error or the connection dropped.
    #[error("server `{server}`: {source}")]
    Rpc {
        /// Server name for context.
        server: String,
        /// The transport-level error.
        source: RpcError,
    },
    /// The server's response did not match the protocol shape.
    #[error("server `{server}` protocol error: {detail}")]
    Protocol {
        /// Server name for context.
        server: String,
        /// What was wrong.
        detail: String,
    },
    /// An operation named a server the manager does not know.
    #[error("no mcp server named `{0}`")]
    Unknown(String),
    /// The server's process or transport terminated unexpectedly.
    #[error("server `{server}` crashed: {detail}")]
    Crashed {
        /// Server name for context.
        server: String,
        /// What was observed (exit status, closed transport, ...).
        detail: String,
    },
    /// The server's transport could not be established over HTTP.
    #[error("server `{server}` http transport: {detail}")]
    Http {
        /// Server name for context.
        server: String,
        /// What went wrong opening or driving the HTTP transport.
        detail: String,
    },
    /// The server config is invalid (e.g. neither or both of
    /// `command` / `url` set).
    #[error("server `{server}` config: {detail}")]
    Config {
        /// Server name for context.
        server: String,
        /// What is wrong with the configuration.
        detail: String,
    },
}

/// Host-supplied handler for server-initiated MCP requests the client
/// chooses to support (e.g. `sampling/createMessage`).
///
/// `kage-mcp` cannot reach the host's LLM provider on its own, so the
/// CLI injects this. It runs on the connection's drain thread, so it may
/// block (a sampling call streams a full completion). Returning `None`
/// declines the request (the client answers method-not-found); a
/// `Some(Ok)` / `Some(Err)` is sent back to the server verbatim.
pub trait ServerRequestHandler: Send + Sync {
    /// Handle one server request, or return `None` when the method is
    /// not supported.
    fn handle(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, RpcError>>;

    /// Extra client capabilities to advertise at `initialize`, merged
    /// into the `capabilities` object (e.g. `{"sampling": {}}`). The
    /// default advertises nothing.
    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({})
    }
}

/// A live, initialized MCP connection (transport + drained
/// notifications), independent of how the peer was created.
pub struct McpConnection {
    server: String,
    peer: Peer,
    tools_changed: Arc<AtomicBool>,
    drain: JoinHandle<()>,
    protocol_version: String,
}

impl McpConnection {
    /// Drive the MCP `initialize` / `notifications/initialized`
    /// handshake on an already-connected `peer`, then spawn a thread
    /// that drains server-initiated traffic: `tools/list_changed`
    /// notifications flip an internal flag, `roots/list` requests are
    /// answered from `roots` (advertised as a client capability), and
    /// any other server request is answered with `method not found` so
    /// a server that asks for an unsupported feature (sampling,
    /// elicitation) is not left hanging.
    ///
    /// `roots` are the filesystem roots exposed to the server (the host
    /// workdir); each is sent as a `file://` URI.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] if `initialize` fails or the
    /// connection drops, and [`McpError::Protocol`] if the response
    /// is not a JSON object.
    pub fn initialize(
        server: impl Into<String>,
        peer: Peer,
        inbound: std::sync::mpsc::Receiver<Inbound>,
        roots: &[std::path::PathBuf],
        handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Result<Self, McpError> {
        Self::initialize_with_timeout(server, peer, inbound, roots, handler, INITIALIZE_TIMEOUT)
    }

    /// [`Self::initialize`] with an explicit handshake deadline; tests
    /// use a short one against a silent server.
    pub fn initialize_with_timeout(
        server: impl Into<String>,
        peer: Peer,
        inbound: std::sync::mpsc::Receiver<Inbound>,
        roots: &[std::path::PathBuf],
        handler: Option<Arc<dyn ServerRequestHandler>>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let server = server.into();
        let roots_result = Self::roots_list_result(roots);
        let mut capabilities = serde_json::Map::new();
        capabilities.insert(
            "roots".to_owned(),
            serde_json::json!({ "listChanged": false }),
        );
        if let Some(h) = &handler
            && let serde_json::Value::Object(extra) = h.capabilities()
        {
            capabilities.extend(extra);
        }
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": serde_json::Value::Object(capabilities),
            "clientInfo": { "name": "kage", "version": env!("CARGO_PKG_VERSION") },
        });
        let result = peer
            .request_timeout("initialize", params, timeout)
            .map_err(|source| McpError::Rpc {
                server: server.clone(),
                source,
            })?;
        if !result.is_object() {
            return Err(McpError::Protocol {
                server: server.clone(),
                detail: "initialize result was not an object".to_owned(),
            });
        }
        let protocol_version = match result.get("protocolVersion").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v.to_owned(),
            _ => {
                return Err(McpError::Protocol {
                    server: server.clone(),
                    detail: "initialize result missing `protocolVersion` string".to_owned(),
                });
            }
        };
        peer.notify("notifications/initialized", serde_json::json!({}))
            .map_err(|source| McpError::Rpc {
                server: server.clone(),
                source,
            })?;

        let tools_changed = Arc::new(AtomicBool::new(false));
        let drain = {
            let flag = Arc::clone(&tools_changed);
            let peer = peer.clone();
            std::thread::spawn(move || {
                for msg in inbound {
                    match msg {
                        Inbound::Notification { method, .. }
                            if method == "notifications/tools/list_changed" =>
                        {
                            flag.store(true, Ordering::SeqCst);
                        }
                        Inbound::Notification { .. } => {}
                        Inbound::Request { id, method, .. } if method == "roots/list" => {
                            let _ = peer.respond(&id, Ok(roots_result.clone()));
                        }
                        Inbound::Request { id, method, params } => {
                            // Offer the request to the host handler
                            // (sampling, etc.); fall back to
                            // method-not-found when unsupported.
                            let outcome = handler
                                .as_ref()
                                .and_then(|h| h.handle(&method, &params))
                                .unwrap_or_else(|| {
                                    Err(RpcError::method_not_found("client capability"))
                                });
                            let _ = peer.respond(&id, outcome);
                        }
                    }
                }
            })
        };

        Ok(Self {
            server,
            peer,
            tools_changed,
            drain,
            protocol_version,
        })
    }

    /// The configured server name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.server
    }

    /// The protocol revision the server answered `initialize` with.
    #[must_use]
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// Whether the server's side of the transport has closed. The
    /// drain thread ends exactly when the inbound channel closes
    /// (reader EOF on stdio or HTTP alike), so this is a cheap,
    /// transport-independent liveness probe; a `true` here means the
    /// next request would fail.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.drain.is_finished()
    }

    /// Build the `roots/list` result advertised to the server: one
    /// entry per host root as a `file://` URI named by its last path
    /// component.
    fn roots_list_result(roots: &[std::path::PathBuf]) -> serde_json::Value {
        let entries: Vec<serde_json::Value> = roots
            .iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("root")
                    .to_owned();
                serde_json::json!({
                    "uri": format!("file://{}", path.display()),
                    "name": name,
                })
            })
            .collect();
        serde_json::json!({ "roots": entries })
    }

    /// The underlying peer, for issuing `tools/list` / `tools/call`.
    #[must_use]
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Take the "server announced its tool list changed" flag,
    /// resetting it to `false`. The result must be used: discarding
    /// it both loses the signal and clears the flag.
    #[must_use]
    pub fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::SeqCst)
    }

    /// Issue a request to the server, tagging failures with the
    /// server name.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error or dropped
    /// connection.
    pub fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        self.peer
            .request_timeout(method, params, REQUEST_TIMEOUT)
            .map_err(|source| McpError::Rpc {
                server: self.server.clone(),
                source,
            })
    }

    /// Like [`Self::request`] but abandons the call when
    /// `should_cancel` trips, so a long-running `tools/call` honors
    /// the agent loop's cancel flag.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error, a dropped
    /// connection, or cancellation.
    pub fn request_cancellable(
        &self,
        method: &str,
        params: serde_json::Value,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<serde_json::Value, McpError> {
        self.peer
            .request_cancellable(method, params, should_cancel)
            .map_err(|source| McpError::Rpc {
                server: self.server.clone(),
                source,
            })
    }
}

/// An initialized MCP server connection plus the child process
/// backing it, if any. Dropping this kills the child; the HTTP
/// transport owns no child.
pub struct McpServerHandle {
    conn: Arc<McpConnection>,
    child: Option<Child>,
}

impl McpServerHandle {
    /// Connect to the server described by `cfg` and run the
    /// `initialize` handshake. The transport is chosen by the config:
    /// `command` spawns a child and speaks JSON-RPC over its stdio,
    /// `url` opens a remote Streamable HTTP connection. `roots` are the
    /// filesystem roots advertised to the server (the host workdir),
    /// answered on `roots/list`.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Config`] when neither or both transports are
    /// configured, [`McpError::Spawn`] / [`McpError::NoStdio`] for a
    /// stdio child, [`McpError::Http`] for an HTTP connection, and the
    /// handshake errors from [`McpConnection::initialize`].
    pub fn spawn(
        name: impl Into<String>,
        cfg: &McpServer,
        roots: &[std::path::PathBuf],
        handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Result<Self, McpError> {
        let name = name.into();
        match (cfg.command.as_deref(), cfg.url.as_deref()) {
            (Some(command), None) => Self::spawn_stdio(name, command, cfg, roots, handler),
            (None, Some(url)) => Self::connect_http(name, url, cfg, roots, handler),
            (Some(_), Some(_)) => Err(McpError::Config {
                server: name,
                detail: "set exactly one of `command` (stdio) or `url` (http), not both".to_owned(),
            }),
            (None, None) => Err(McpError::Config {
                server: name,
                detail: "set `command` (stdio) or `url` (http)".to_owned(),
            }),
        }
    }

    /// Spawn a stdio child and run the handshake over its pipes.
    fn spawn_stdio(
        name: String,
        command: &str,
        cfg: &McpServer,
        roots: &[std::path::PathBuf],
        handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Result<Self, McpError> {
        let mut process = Command::new(command);
        process
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = process.spawn().map_err(|source| McpError::Spawn {
            command: command.to_owned(),
            source,
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::NoStdio(name.clone()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::NoStdio(name.clone()))?;
        let (peer, inbound, _reader) = connect(BufReader::new(stdout), stdin);
        let conn = Arc::new(McpConnection::initialize(
            name, peer, inbound, roots, handler,
        )?);
        Ok(Self {
            conn,
            child: Some(child),
        })
    }

    /// Open a remote Streamable HTTP connection and run the handshake
    /// over it.
    fn connect_http(
        name: String,
        url: &str,
        cfg: &McpServer,
        roots: &[std::path::PathBuf],
        handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Result<Self, McpError> {
        let (peer, inbound, _reader) =
            crate::http::connect_http(url, &cfg.headers).map_err(|detail| McpError::Http {
                server: name.clone(),
                detail,
            })?;
        let conn = Arc::new(McpConnection::initialize(
            name, peer, inbound, roots, handler,
        )?);
        Ok(Self { conn, child: None })
    }

    /// The live connection, shareable into tool adapters that must
    /// outlive individual calls but not the child.
    #[must_use]
    pub fn connection(&self) -> &Arc<McpConnection> {
        &self.conn
    }

    /// The child's exit status once it has terminated, or `None`
    /// while it is still running and for a transport without a child
    /// process. Used to detail a server evicted as dead.
    #[must_use]
    pub fn exit_status(&mut self) -> Option<String> {
        self.child
            .as_mut()?
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.to_string())
    }

    /// Test-only: wrap a bare connection as a childless handle so
    /// manager tests can inject an in-process transport.
    #[cfg(test)]
    pub(crate) fn from_connection(conn: Arc<McpConnection>) -> Self {
        Self { conn, child: None }
    }
}

impl Drop for McpServerHandle {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::sync::Mutex;
    use std::thread;

    use super::*;

    /// Wire two transport peers back to back and run a minimal MCP
    /// server on one side: a thread drains the server inbound for the
    /// whole test, answering `initialize` and rejecting anything else,
    /// while ignoring notifications (the `initialized` one).
    fn stub_server() -> (McpConnection, Peer) {
        stub_server_with_roots(&[])
    }

    fn stub_server_with_roots(roots: &[std::path::PathBuf]) -> (McpConnection, Peer) {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder = srv_peer.clone();
        thread::spawn(move || {
            for msg in srv_in {
                if let Inbound::Request { id, method, .. } = msg {
                    let outcome = if method == "initialize" {
                        Ok(serde_json::json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "stub", "version": "0" },
                        }))
                    } else {
                        Err(RpcError::method_not_found(&method))
                    };
                    let _ = responder.respond(&id, outcome);
                }
            }
        });
        let conn = McpConnection::initialize("stub", cli_peer, cli_in, roots, None).unwrap();
        (conn, srv_peer)
    }

    #[test]
    fn initialize_completes_handshake() {
        let (conn, _srv) = stub_server();
        assert_eq!(conn.name(), "stub");
        assert!(!conn.take_tools_changed());
    }

    #[test]
    fn initialize_gives_up_on_a_silent_server() {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        // Nobody ever drains `srv_in` or answers: the server side of
        // the pipe stays mute.
        let (_srv_peer, _srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let start = std::time::Instant::now();
        let err = McpConnection::initialize_with_timeout(
            "silent",
            cli_peer,
            cli_in,
            &[],
            None,
            Duration::from_millis(100),
        )
        .err()
        .expect("silent server must fail the handshake");
        assert!(matches!(err, McpError::Rpc { .. }), "got {err:?}");
        assert!(err.to_string().contains("timed out"), "got {err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "deadline must bound the handshake, took {:?}",
            start.elapsed()
        );
    }

    struct EchoHandler;
    impl ServerRequestHandler for EchoHandler {
        fn handle(
            &self,
            method: &str,
            params: &serde_json::Value,
        ) -> Option<Result<serde_json::Value, RpcError>> {
            (method == "test/echo").then(|| Ok(params.clone()))
        }
        fn capabilities(&self) -> serde_json::Value {
            serde_json::json!({ "sampling": {} })
        }
    }

    /// Like `stub_server` but records the `initialize` params and wires
    /// the given handler into the connection.
    fn stub_server_with_handler(
        handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> (McpConnection, Peer, Arc<Mutex<Option<serde_json::Value>>>) {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder = srv_peer.clone();
        let init_params = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&init_params);
        thread::spawn(move || {
            for msg in srv_in {
                if let Inbound::Request { id, method, params } = msg {
                    let outcome = if method == "initialize" {
                        *captured.lock().unwrap() = Some(params);
                        Ok(serde_json::json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "stub", "version": "0" },
                        }))
                    } else {
                        Err(RpcError::method_not_found(&method))
                    };
                    let _ = responder.respond(&id, outcome);
                }
            }
        });
        let conn = McpConnection::initialize("stub", cli_peer, cli_in, &[], handler).unwrap();
        (conn, srv_peer, init_params)
    }

    #[test]
    fn handler_answers_server_request_and_advertises_capability() {
        let (_conn, srv, init_params) = stub_server_with_handler(Some(Arc::new(EchoHandler)));
        // The handler's capabilities are merged into `initialize`.
        let caps = init_params
            .lock()
            .unwrap()
            .clone()
            .expect("initialize seen");
        assert!(caps["capabilities"]["sampling"].is_object());
        assert!(caps["capabilities"]["roots"].is_object());
        // A handled request routes to the handler...
        let echoed = srv
            .request("test/echo", serde_json::json!({ "n": 7 }))
            .expect("echo answered");
        assert_eq!(echoed["n"], 7);
        // ...and an unhandled one still gets method-not-found.
        let err = srv
            .request("test/other", serde_json::Value::Null)
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    fn empty_server() -> McpServer {
        McpServer {
            command: None,
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
        }
    }

    #[test]
    fn spawn_rejects_a_server_with_no_transport() {
        let err = McpServerHandle::spawn("x", &empty_server(), &[], None)
            .err()
            .expect("no transport must error");
        assert!(matches!(err, McpError::Config { .. }), "got {err:?}");
    }

    #[test]
    fn spawn_rejects_both_transports() {
        let cfg = McpServer {
            command: Some("npx".to_owned()),
            url: Some("https://example.com/sse".to_owned()),
            ..empty_server()
        };
        let err = McpServerHandle::spawn("x", &cfg, &[], None)
            .err()
            .expect("both transports must error");
        assert!(matches!(err, McpError::Config { .. }), "got {err:?}");
    }

    #[test]
    fn tools_list_changed_sets_and_clears_flag() {
        let (conn, srv) = stub_server();
        srv.notify("notifications/tools/list_changed", serde_json::Value::Null)
            .unwrap();
        let mut seen = false;
        for _ in 0..50 {
            if conn.take_tools_changed() {
                seen = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(seen, "tools_changed flag should latch");
        assert!(!conn.take_tools_changed(), "flag clears after take");
    }

    #[test]
    fn roots_list_returns_configured_roots() {
        let (_conn, srv) = stub_server_with_roots(&[std::path::PathBuf::from("/work/project")]);
        let result = srv
            .request("roots/list", serde_json::json!({}))
            .expect("roots/list is answered");
        let roots = result["roots"].as_array().expect("roots array");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0]["uri"], "file:///work/project");
        assert_eq!(roots[0]["name"], "project");
    }

    #[test]
    fn unknown_server_request_is_answered_not_hung() {
        let (_conn, srv) = stub_server();
        let err = srv
            .request("sampling/createMessage", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    /// Wire a client connection to a server thread that answers
    /// `initialize` with `result` and then blocks on the returned
    /// channel, holding the transport open until the test drops the
    /// sender (which closes the server side and EOFs the client).
    fn server_answering(
        result: serde_json::Value,
    ) -> (Result<McpConnection, McpError>, std::sync::mpsc::Sender<()>) {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder = srv_peer.clone();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        thread::spawn(move || {
            for msg in srv_in {
                if let Inbound::Request { id, method, .. } = msg {
                    if method == "initialize" {
                        let _ = responder.respond(&id, Ok(result));
                        let _ = hold_rx.recv();
                        return;
                    }
                }
            }
        });
        let conn = McpConnection::initialize("stub", cli_peer, cli_in, &[], None);
        (conn, hold_tx)
    }

    #[test]
    fn is_dead_flips_when_the_transport_closes() {
        let (conn, hold) = server_answering(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
        }));
        let conn = conn.unwrap();
        assert!(!conn.is_dead(), "the server holds the transport open");
        drop(hold);
        let mut dead = false;
        for _ in 0..100 {
            if conn.is_dead() {
                dead = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(dead, "connection must report dead once the server exits");
    }

    #[test]
    fn initialize_records_the_negotiated_protocol_version() {
        let (conn, _hold) = server_answering(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
        }));
        assert_eq!(conn.unwrap().protocol_version(), "2024-11-05");
    }

    #[test]
    fn initialize_rejects_a_result_without_a_protocol_version() {
        let (conn, _hold) = server_answering(serde_json::json!({}));
        let err = conn.err().expect("missing protocolVersion must fail");
        assert!(matches!(err, McpError::Protocol { .. }), "got {err:?}");
        assert!(err.to_string().contains("protocolVersion"), "got {err}");
    }
}
