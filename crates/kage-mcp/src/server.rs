//! Spawn and handshake one external MCP server over stdio.
//!
//! [`McpServerHandle::spawn`] launches the child described by a
//! `[mcp.servers.<name>]` config block, wires its stdin/stdout into
//! the [`crate::transport`] peer, performs the MCP `initialize`
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

use kage_core::config::McpServer;

use crate::transport::{Inbound, Peer, RpcError, connect};

/// Protocol revision kage advertises in `initialize`. Servers that
/// speak a different revision still respond with their own; we log
/// the negotiated value but do not hard-fail on a mismatch, matching
/// how the reference clients behave.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

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
}

/// A live, initialized MCP connection (transport + drained
/// notifications), independent of how the peer was created.
pub struct McpConnection {
    server: String,
    peer: Peer,
    tools_changed: Arc<AtomicBool>,
    _drain: JoinHandle<()>,
}

impl McpConnection {
    /// Drive the MCP `initialize` / `notifications/initialized`
    /// handshake on an already-connected `peer`, then spawn a thread
    /// that drains server-initiated traffic: `tools/list_changed`
    /// notifications flip an internal flag, and any other server
    /// request is answered with `method not found` so a server that
    /// asks for sampling/elicitation is not left hanging.
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
    ) -> Result<Self, McpError> {
        let server = server.into();
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "kage", "version": env!("CARGO_PKG_VERSION") },
        });
        let result = peer
            .request("initialize", params)
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
                        Inbound::Request { id, .. } => {
                            let _ = peer
                                .respond(&id, Err(RpcError::method_not_found("client capability")));
                        }
                    }
                }
            })
        };

        Ok(Self {
            server,
            peer,
            tools_changed,
            _drain: drain,
        })
    }

    /// The configured server name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.server
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
            .request(method, params)
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
/// backing it. Dropping this kills the child.
pub struct McpServerHandle {
    conn: Arc<McpConnection>,
    child: Child,
}

impl McpServerHandle {
    /// Spawn the server described by `cfg`, wire stdio into the
    /// transport, and run the `initialize` handshake.
    ///
    /// `stderr` is inherited so a server's diagnostics reach the
    /// user's terminal rather than being silently swallowed.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Spawn`] when the process cannot start,
    /// [`McpError::NoStdio`] when its pipes are missing, and the
    /// handshake errors from [`McpConnection::initialize`].
    pub fn spawn(name: impl Into<String>, cfg: &McpServer) -> Result<Self, McpError> {
        let name = name.into();
        let mut command = Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|source| McpError::Spawn {
            command: cfg.command.clone(),
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
        let conn = Arc::new(McpConnection::initialize(name, peer, inbound)?);
        Ok(Self { conn, child })
    }

    /// The live connection, shareable into tool adapters that must
    /// outlive individual calls but not the child.
    #[must_use]
    pub fn connection(&self) -> &Arc<McpConnection> {
        &self.conn
    }
}

impl Drop for McpServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::thread;

    use super::*;

    /// Wire two transport peers back to back and run a minimal MCP
    /// server on one side: a thread drains the server inbound for the
    /// whole test, answering `initialize` and rejecting anything else,
    /// while ignoring notifications (the `initialized` one).
    fn stub_server() -> (McpConnection, Peer) {
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
        let conn = McpConnection::initialize("stub", cli_peer, cli_in).unwrap();
        (conn, srv_peer)
    }

    #[test]
    fn initialize_completes_handshake() {
        let (conn, _srv) = stub_server();
        assert_eq!(conn.name(), "stub");
        assert!(!conn.take_tools_changed());
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
    fn unknown_server_request_is_answered_not_hung() {
        let (_conn, srv) = stub_server();
        let err = srv
            .request("sampling/createMessage", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
