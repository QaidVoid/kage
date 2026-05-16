//! Own every configured MCP server and keep its tools registered.
//!
//! [`McpManager::spawn_all`] launches each enabled `[mcp.servers.*]`
//! entry, [`McpManager::register_into`] discovers and registers their
//! tools, and [`McpManager::refresh_into`] re-lists any server that
//! pushed a `notifications/tools/list_changed`, swapping its adapters
//! in place (stale tools are unregistered, not left dangling).
//!
//! A server that fails to spawn or list tools does not abort the
//! agent: the failure is collected and returned to the caller to
//! surface, while the rest of the servers continue. The manager owns
//! the [`McpServerHandle`]s, so dropping it kills every child.

use std::sync::Arc;

use kage_core::config::{McpConfig, McpServer};
use kage_tools::ToolRegistry;

use crate::server::{McpConnection, McpError, McpServerHandle};
use crate::tools::tools_from_connection;

/// One spawned server: its launch spec (kept so it can be
/// respawned on `restart`), the live handle, and the tool names it
/// currently contributes.
struct Managed {
    spec: McpServer,
    handle: McpServerHandle,
    registered: Vec<String>,
}

/// Owns the spawned MCP servers and mediates their tools into a
/// [`ToolRegistry`].
#[derive(Default)]
pub struct McpManager {
    servers: Vec<(String, Managed)>,
}

impl McpManager {
    /// Spawn every enabled server in `cfg` (sorted by name for a
    /// deterministic registration order). Disabled entries are
    /// skipped. Spawn/handshake failures are collected as
    /// `(server_name, error)` and returned alongside the manager so
    /// the caller can surface them without losing the servers that
    /// did come up.
    #[must_use]
    pub fn spawn_all(cfg: &McpConfig) -> (Self, Vec<(String, McpError)>) {
        let mut servers = Vec::new();
        let mut errors = Vec::new();
        for (name, spec) in &cfg.servers {
            if spec.disabled {
                continue;
            }
            match McpServerHandle::spawn(name.clone(), spec) {
                Ok(handle) => servers.push((
                    name.clone(),
                    Managed {
                        spec: spec.clone(),
                        handle,
                        registered: Vec::new(),
                    },
                )),
                Err(e) => errors.push((name.clone(), e)),
            }
        }
        (Self { servers }, errors)
    }

    /// Whether no server is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Number of live servers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// Names of the live servers, in registration order.
    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.servers.iter().map(|(n, _)| n.as_str())
    }

    /// Discover and register every server's tools. Returns the
    /// per-server discovery failures; a failing server simply
    /// contributes no tools.
    pub fn register_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            if let Err(e) = Self::reload(managed, reg) {
                errors.push((name.clone(), e));
            }
        }
        errors
    }

    /// Re-list only the servers that announced a tool-list change
    /// since the last call, swapping their adapters in place. Returns
    /// per-server failures.
    pub fn refresh_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            if managed.handle.connection().take_tools_changed() {
                if let Err(e) = Self::reload(managed, reg) {
                    errors.push((name.clone(), e));
                }
            }
        }
        errors
    }

    /// Restart one server by name: spawn a fresh process from its
    /// original spec, and only on success swap it in (killing the old
    /// child) and re-register its tools. A failed respawn leaves the
    /// old server running and untouched, so `restart` never causes
    /// downtime on its own failure.
    ///
    /// # Errors
    ///
    /// [`McpError::Unknown`] if no server has that name, or the spawn
    /// / discovery error from bringing the replacement up.
    pub fn restart(&mut self, name: &str, reg: &mut ToolRegistry) -> Result<(), McpError> {
        let managed = self
            .servers
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m)
            .ok_or_else(|| McpError::Unknown(name.to_owned()))?;
        let fresh = McpServerHandle::spawn(name.to_owned(), &managed.spec)?;
        for stale in managed.registered.drain(..) {
            reg.unregister(&stale);
        }
        managed.handle = fresh;
        reload_connection(managed.handle.connection(), &mut managed.registered, reg)
    }

    /// Drop this server's previously registered tools and register
    /// its current set, updating the tracked names.
    fn reload(managed: &mut Managed, reg: &mut ToolRegistry) -> Result<(), McpError> {
        reload_connection(managed.handle.connection(), &mut managed.registered, reg)
    }
}

/// Connection-level reload: list `conn`'s tools, unregister the names
/// in `registered`, register the current set, and update `registered`
/// to match. Factored out of [`McpManager::reload`] so it can be
/// exercised without spawning a process.
fn reload_connection(
    conn: &Arc<McpConnection>,
    registered: &mut Vec<String>,
    reg: &mut ToolRegistry,
) -> Result<(), McpError> {
    let tools = tools_from_connection(conn)?;
    for stale in registered.drain(..) {
        reg.unregister(&stale);
    }
    for tool in tools {
        registered.push(tool.name().to_owned());
        reg.register(tool);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;
    use crate::server::PROTOCOL_VERSION;
    use crate::transport::{Inbound, connect};

    /// A server whose `tools/list` returns `old` on the first call
    /// and `new` on every call after, so a reload must swap them.
    fn flipping_server() -> Arc<McpConnection> {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder = srv_peer.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            for msg in srv_in {
                let Inbound::Request { id, method, .. } = msg else {
                    continue;
                };
                let outcome = match method.as_str() {
                    "initialize" => Ok(serde_json::json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "serverInfo": { "name": "x", "version": "0" },
                    })),
                    "tools/list" => {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        let tool = if n == 0 { "old" } else { "new" };
                        Ok(serde_json::json!({
                            "tools": [{ "name": tool, "inputSchema": {} }]
                        }))
                    }
                    other => Err(crate::transport::RpcError::method_not_found(other)),
                };
                let _ = responder.respond(&id, outcome);
            }
        });
        Arc::new(McpConnection::initialize("x", cli_peer, cli_in).unwrap())
    }

    #[test]
    fn reload_swaps_stale_tools_for_current_ones() {
        let conn = flipping_server();
        let mut reg = ToolRegistry::new();
        let mut registered = Vec::new();

        reload_connection(&conn, &mut registered, &mut reg).unwrap();
        assert_eq!(registered, ["x__old"]);
        assert!(reg.get("x__old").is_some());

        reload_connection(&conn, &mut registered, &mut reg).unwrap();
        assert_eq!(registered, ["x__new"]);
        assert!(reg.get("x__new").is_some());
        assert!(reg.get("x__old").is_none(), "stale tool was unregistered");
    }

    #[test]
    fn spawn_all_skips_disabled_and_reports_spawn_failures() {
        use kage_core::config::McpServer;

        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "off".to_owned(),
            McpServer {
                command: "definitely-not-a-real-binary-xyz".to_owned(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                disabled: true,
            },
        );
        cfg.servers.insert(
            "broken".to_owned(),
            McpServer {
                command: "definitely-not-a-real-binary-xyz".to_owned(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                disabled: false,
            },
        );
        let (mgr, errors) = McpManager::spawn_all(&cfg);
        assert!(mgr.is_empty(), "disabled skipped, broken failed to spawn");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "broken");
    }

    #[test]
    fn restart_unknown_server_errors() {
        let (mut mgr, _e) = McpManager::spawn_all(&McpConfig::default());
        let mut reg = ToolRegistry::new();
        let err = mgr.restart("ghost", &mut reg).unwrap_err();
        assert!(
            matches!(&err, crate::server::McpError::Unknown(n) if n == "ghost"),
            "{err}"
        );
    }
}
