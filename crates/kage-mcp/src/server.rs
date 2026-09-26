//! Spawn and handshake one external MCP server over stdio or HTTP.
//!
//! [`McpServerHandle::spawn`] connects the server described by a
//! `[mcp.servers.<name>]` config block, wires its transport into the
//! shared [`kage_jsonrpc`] peer, performs the MCP `initialize`
//! handshake, and then keeps the connection live for tool discovery
//! and calls. Dropping a stdio handle kills the child so a crashed kage
//! never leaves orphaned server processes.
//!
//! The handshake is intentionally split from process spawning:
//! [`McpConnection::initialize`] works over any reader/writer so it
//! can be tested with in-process pipes, and the process plumbing in
//! [`McpServerHandle::spawn`] stays a thin shell on top.

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use kage_core::CancelFlag;
use kage_core::config::McpServer;

use kage_jsonrpc::{CancelNotice, Inbound, Peer, RpcError, connect_with};

use crate::oauth::TokenSource;

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

/// The `notifications/cancelled` notice MCP expects when kage abandons
/// a request. MCP forbids cancelling `initialize`, so that one gets none.
pub(crate) fn cancel_notice() -> CancelNotice {
    Arc::new(|id, method| {
        (method != "initialize").then(|| {
            (
                "notifications/cancelled".to_owned(),
                serde_json::json!({"requestId": id, "reason": "cancelled by client"}),
            )
        })
    })
}

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
    /// The HTTP server answered 401: kage has no token for it, or the
    /// token was refused and could not be refreshed.
    #[error("server `{server}` needs authorization{}", login_hint(*.login, .server))]
    Unauthorized {
        /// Server name for context.
        server: String,
        /// Whether kage sends a stored token, so a login is the fix. It
        /// does not when a configured `authorization` header wins.
        login: bool,
    },
}

fn login_hint(login: bool, server: &str) -> String {
    if login {
        format!(": run kage mcp login {server}, or /mcp in the TUI")
    } else {
        String::new()
    }
}

impl McpError {
    /// Tag a JSON-RPC failure with the server name. The HTTP
    /// transport's unauthorized codes become [`McpError::Unauthorized`].
    fn rpc(server: &str, source: RpcError) -> Self {
        if matches!(
            source.code,
            crate::http::UNAUTHORIZED | crate::http::REFUSED
        ) {
            Self::Unauthorized {
                server: server.to_owned(),
                login: source.code == crate::http::UNAUTHORIZED,
            }
        } else {
            Self::Rpc {
                server: server.to_owned(),
                source,
            }
        }
    }
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

/// The "list changed" notices a server sent since the last take, one
/// flag per list.
#[derive(Default)]
struct ListsChanged {
    tools: AtomicBool,
    resources: AtomicBool,
    prompts: AtomicBool,
}

/// Callback that emits the `notifications/progress` params of one token.
type ProgressEmit = Box<dyn Fn(serde_json::Value) + Send>;

/// Open progress tokens, each with the callback of the call that owns it.
type ProgressRoutes = Arc<Mutex<HashMap<String, ProgressEmit>>>;

/// One registered progress token. Its callback runs on the drain thread,
/// under the routes lock, until the ticket is dropped. Dropping takes the
/// same lock, so no update is emitted once the drop returns.
pub(crate) struct ProgressTicket {
    pub(crate) token: String,
    routes: ProgressRoutes,
}

impl Drop for ProgressTicket {
    fn drop(&mut self) {
        kage_core::sync::lock(&self.routes).remove(&self.token);
    }
}

/// A live, initialized MCP connection (transport + drained
/// notifications), independent of how the peer was created.
pub struct McpConnection {
    server: String,
    peer: Peer,
    changed: Arc<ListsChanged>,
    progress: ProgressRoutes,
    next_progress: AtomicU64,
    drain: JoinHandle<()>,
    protocol_version: String,
    capabilities: serde_json::Value,
    refused: AtomicBool,
}

impl McpConnection {
    /// Drive the MCP `initialize` / `notifications/initialized`
    /// handshake on an already-connected `peer`, then spawn a thread
    /// that drains server-initiated traffic: the tools, resources and
    /// prompts `list_changed` notifications each flip their own flag,
    /// `notifications/progress` runs the callback of the call that
    /// registered its token, `roots/list`
    /// requests are answered from `roots` (advertised as a client
    /// capability), `ping` gets an empty result, and any other server
    /// request is answered with `method not found` so a server that
    /// asks for an unsupported feature (sampling, elicitation) is not
    /// left hanging. The server's `capabilities` are recorded.
    ///
    /// `roots` are the filesystem roots exposed to the server (the host
    /// workdir); each is sent as a `file://` URI.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] if `initialize` fails or the
    /// connection drops, [`McpError::Unauthorized`] if an HTTP server
    /// refuses kage's token, and [`McpError::Protocol`] if the response
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
            .map_err(|source| McpError::rpc(&server, source))?;
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
        let capabilities = result
            .get("capabilities")
            .filter(|c| c.is_object())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        peer.notify("notifications/initialized", serde_json::json!({}))
            .map_err(|source| McpError::rpc(&server, source))?;

        let changed = Arc::new(ListsChanged::default());
        let progress = ProgressRoutes::default();
        let drain = {
            let changed = Arc::clone(&changed);
            let progress = Arc::clone(&progress);
            let peer = peer.clone();
            std::thread::spawn(move || {
                for msg in inbound {
                    match msg {
                        Inbound::Notification { method, params } => match method.as_str() {
                            "notifications/tools/list_changed" => {
                                changed.tools.store(true, Ordering::SeqCst);
                            }
                            "notifications/resources/list_changed" => {
                                changed.resources.store(true, Ordering::SeqCst);
                            }
                            "notifications/prompts/list_changed" => {
                                changed.prompts.store(true, Ordering::SeqCst);
                            }
                            "notifications/progress" => route_progress(&progress, params),
                            _ => {}
                        },
                        Inbound::Request { id, method, .. } if method == "roots/list" => {
                            let _ = peer.respond(&id, Ok(roots_result.clone()));
                        }
                        Inbound::Request { id, method, .. } if method == "ping" => {
                            let _ = peer.respond(&id, Ok(serde_json::json!({})));
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
            changed,
            progress,
            next_progress: AtomicU64::new(0),
            drain,
            protocol_version,
            capabilities,
            refused: AtomicBool::new(false),
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

    /// The `capabilities` object the server answered `initialize` with
    /// (an empty object when it sent none).
    #[must_use]
    pub fn server_capabilities(&self) -> &serde_json::Value {
        &self.capabilities
    }

    /// Whether the server advertised the capability `name` (for example
    /// `tools`, `resources` or `prompts`).
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.capabilities
            .get(name)
            .is_some_and(|value| !value.is_null())
    }

    /// Register a fresh progress token, `<label>#<n>`, for one call.
    /// `emit` runs on the drain thread with the params of each progress
    /// notification for it until the ticket is dropped, so it must not
    /// block.
    pub(crate) fn track_progress(
        &self,
        label: &str,
        emit: impl Fn(serde_json::Value) + Send + 'static,
    ) -> ProgressTicket {
        let n = self.next_progress.fetch_add(1, Ordering::Relaxed);
        let token = format!("{label}#{n}");
        kage_core::sync::lock(&self.progress).insert(token.clone(), Box::new(emit));
        ProgressTicket {
            token,
            routes: Arc::clone(&self.progress),
        }
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

    /// Whether an HTTP server refused kage's token on a request since the
    /// connection opened, so the manager can take it down.
    #[must_use]
    pub fn is_refused(&self) -> bool {
        self.refused.load(Ordering::SeqCst)
    }

    /// Tag a request failure with the server name, remembering a refused
    /// token.
    fn failure(&self, source: RpcError) -> McpError {
        let error = McpError::rpc(&self.server, source);
        if matches!(error, McpError::Unauthorized { .. }) {
            self.refused.store(true, Ordering::SeqCst);
        }
        error
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
        self.changed.tools.swap(false, Ordering::SeqCst)
    }

    /// Take the "server announced its resource list changed" flag,
    /// resetting it to `false`. Resource templates reload with it.
    #[must_use]
    pub fn take_resources_changed(&self) -> bool {
        self.changed.resources.swap(false, Ordering::SeqCst)
    }

    /// Take the "server announced its prompt list changed" flag,
    /// resetting it to `false`.
    #[must_use]
    pub fn take_prompts_changed(&self) -> bool {
        self.changed.prompts.swap(false, Ordering::SeqCst)
    }

    /// Issue a request to the server, tagging failures with the
    /// server name.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error or dropped
    /// connection, and [`McpError::Unauthorized`] when an HTTP server
    /// refuses kage's token.
    pub fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        self.peer
            .request_timeout(method, params, REQUEST_TIMEOUT)
            .map_err(|source| self.failure(source))
    }

    /// Like [`Self::request`] but without a deadline, abandoning the call
    /// as soon as `cancel` is cancelled, so a long-running `tools/call`
    /// honors the agent loop's cancel flag.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error, a dropped
    /// connection, or cancellation, and [`McpError::Unauthorized`] when
    /// an HTTP server refuses kage's token.
    pub fn request_cancellable(
        &self,
        method: &str,
        params: serde_json::Value,
        cancel: &CancelFlag,
    ) -> Result<serde_json::Value, McpError> {
        self.peer
            .request_cancellable(method, params, cancel)
            .map_err(|source| self.failure(source))
    }
}

/// Emit `notifications/progress` params through the callback of the call
/// that owns their token. Unknown and finished tokens are dropped.
fn route_progress(routes: &ProgressRoutes, params: serde_json::Value) {
    let Some(token) = params.get("progressToken").and_then(|t| t.as_str()) else {
        return;
    };
    if let Some(emit) = kage_core::sync::lock(routes).get(token) {
        emit(params);
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
        Self::spawn_with(name, cfg, roots, handler, None)
    }

    /// [`Self::spawn`] with a bearer token source for an HTTP server.
    /// The transport asks `tokens` only when `cfg` has no
    /// `authorization` header of its own. A stdio server ignores it.
    ///
    /// # Errors
    ///
    /// As [`Self::spawn`], plus [`McpError::Unauthorized`] when the
    /// HTTP server refuses the token (or asks for one there is none of).
    pub fn spawn_with(
        name: impl Into<String>,
        cfg: &McpServer,
        roots: &[std::path::PathBuf],
        handler: Option<Arc<dyn ServerRequestHandler>>,
        tokens: Option<Arc<dyn TokenSource>>,
    ) -> Result<Self, McpError> {
        let name = name.into();
        match (cfg.command.as_deref(), cfg.url.as_deref()) {
            (Some(command), None) => Self::spawn_stdio(name, command, cfg, roots, handler),
            (None, Some(url)) => Self::connect_http(name, url, cfg, roots, handler, tokens),
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
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group, so the kill on drop also reaches
            // grandchildren such as the `node` behind `npx` or `bunx`.
            process.process_group(0);
        }
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
        let (peer, inbound, _reader) =
            connect_with(BufReader::new(stdout), stdin, Some(cancel_notice()));
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
        tokens: Option<Arc<dyn TokenSource>>,
    ) -> Result<Self, McpError> {
        let (peer, inbound, _reader) = crate::http::connect_http(url, &cfg.headers, tokens)
            .map_err(|detail| McpError::Http {
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

    /// Wrap an already initialized connection as a childless handle, so
    /// the manager can adopt an in-process transport.
    pub(crate) fn from_connection(conn: Arc<McpConnection>) -> Self {
        Self { conn, child: None }
    }
}

impl Drop for McpServerHandle {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            kill_process_group(child);
            let _ = child.wait();
        }
    }
}

/// Kill the child and everything it spawned. A launcher like `bunx`
/// leaves its server running when killed alone, and that orphan can
/// reset the terminal modes after kage has already exited.
fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let pgid = nix::unistd::Pid::from_raw(child.id().cast_signed());
        if nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread;

    use kage_jsonrpc::testing::{pair, pair_with};

    use super::*;
    use crate::test_support::{answer_requests, wait_until};

    /// A minimal MCP server that answers `initialize` and rejects every
    /// other request.
    fn stub_server() -> (McpConnection, Peer) {
        stub_server_with_roots(&[])
    }

    fn stub_server_with_roots(roots: &[std::path::PathBuf]) -> (McpConnection, Peer) {
        let ((cli_peer, cli_in), (srv_peer, srv_in)) = pair();
        answer_requests(
            srv_peer.clone(),
            srv_in,
            serde_json::json!({ "tools": {} }),
            |method, _| Err(RpcError::method_not_found(method)),
        );
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
        // Nobody ever drains or answers the server end: it stays mute.
        let ((cli_peer, cli_in), _srv) = pair();
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
        let ((cli_peer, cli_in), (srv_peer, srv_in)) = pair();
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
            oauth: None,
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
        srv.request("ping", serde_json::json!({}))
            .expect("the drain thread handled the notice before the ping");
        assert!(conn.take_tools_changed(), "tools_changed flag should latch");
        assert!(!conn.take_tools_changed(), "flag clears after take");
    }

    #[test]
    fn each_list_changed_notice_sets_only_its_own_flag() {
        let (conn, srv) = stub_server();
        srv.notify(
            "notifications/resources/list_changed",
            serde_json::Value::Null,
        )
        .unwrap();
        srv.notify(
            "notifications/prompts/list_changed",
            serde_json::Value::Null,
        )
        .unwrap();
        srv.request("ping", serde_json::json!({}))
            .expect("the drain thread handled both notices before the ping");
        assert!(conn.take_resources_changed());
        assert!(conn.take_prompts_changed());
        assert!(!conn.take_tools_changed());
        assert!(!conn.take_resources_changed(), "flag clears after take");
        assert!(!conn.take_prompts_changed(), "flag clears after take");
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
    fn server_ping_is_answered_with_an_empty_result() {
        let (_conn, srv) = stub_server();
        let result = srv
            .request("ping", serde_json::json!({}))
            .expect("ping is answered");
        assert_eq!(result, serde_json::json!({}));
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
        let ((cli_peer, cli_in), (srv_peer, srv_in)) = pair();
        let responder = srv_peer.clone();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        thread::spawn(move || {
            for msg in srv_in {
                if let Inbound::Request { id, method, .. } = msg
                    && method == "initialize"
                {
                    let _ = responder.respond(&id, Ok(result));
                    let _ = hold_rx.recv();
                    return;
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
        assert!(
            wait_until(|| conn.is_dead()),
            "connection must report dead once the server exits"
        );
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
    fn initialize_records_server_capabilities() {
        let (conn, _hold) = server_answering(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true }, "prompts": {} },
        }));
        let conn = conn.unwrap();
        assert_eq!(conn.server_capabilities()["tools"]["listChanged"], true);
        assert!(conn.has("tools"));
        assert!(conn.has("prompts"));
        assert!(!conn.has("resources"));
    }

    #[test]
    fn missing_capabilities_record_an_empty_object() {
        let (conn, _hold) = server_answering(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
        }));
        let conn = conn.unwrap();
        assert_eq!(conn.server_capabilities(), &serde_json::json!({}));
        assert!(!conn.has("tools"));
    }

    #[test]
    fn initialize_rejects_a_result_without_a_protocol_version() {
        let (conn, _hold) = server_answering(serde_json::json!({}));
        let err = conn.err().expect("missing protocolVersion must fail");
        assert!(matches!(err, McpError::Protocol { .. }), "got {err:?}");
        assert!(err.to_string().contains("protocolVersion"), "got {err}");
    }

    #[test]
    fn cancelled_call_notifies_the_server() {
        let ((cli_peer, cli_in), (srv_peer, srv_in)) = pair_with(Some(cancel_notice()));
        let (seen_tx, seen) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for msg in srv_in {
                match msg {
                    Inbound::Request { id, method, .. } if method == "initialize" => {
                        let _ = srv_peer.respond(
                            &id,
                            Ok(serde_json::json!({
                                "protocolVersion": PROTOCOL_VERSION,
                                "capabilities": { "tools": {} },
                            })),
                        );
                    }
                    Inbound::Notification { method, .. }
                        if method == "notifications/initialized" => {}
                    other => {
                        let _ = seen_tx.send(other);
                    }
                }
            }
        });
        let conn = McpConnection::initialize("slow", cli_peer, cli_in, &[], None).unwrap();
        let cancel = CancelFlag::new();
        thread::scope(|scope| {
            let call = scope
                .spawn(|| conn.request_cancellable("tools/call", serde_json::json!({}), &cancel));
            let timeout = Duration::from_secs(5);
            let Ok(Inbound::Request { id, method, .. }) = seen.recv_timeout(timeout) else {
                panic!("server must see the call");
            };
            assert_eq!(method, "tools/call");
            cancel.cancel();
            match seen.recv_timeout(timeout) {
                Ok(Inbound::Notification { method, params }) => {
                    assert_eq!(method, "notifications/cancelled");
                    assert_eq!(params["requestId"], id);
                    assert_eq!(params["reason"], "cancelled by client");
                }
                other => panic!("expected a cancel notice, got {other:?}"),
            }
            assert!(call.join().unwrap().is_err());
        });
    }

    #[test]
    fn abandoned_initialize_sends_no_notice() {
        let ((cli_peer, cli_in), (_srv_peer, srv_in)) = pair_with(Some(cancel_notice()));
        let err = McpConnection::initialize_with_timeout(
            "silent",
            cli_peer,
            cli_in,
            &[],
            None,
            Duration::from_millis(50),
        )
        .err()
        .expect("silent server must fail the handshake");
        assert!(err.to_string().contains("timed out"), "got {err}");
        let seen: Vec<Inbound> = srv_in.iter().collect();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            matches!(&seen[0], Inbound::Request { method, .. } if method == "initialize"),
            "{seen:?}"
        );
    }
}
