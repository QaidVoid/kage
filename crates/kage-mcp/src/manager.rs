//! Own every configured MCP server and keep its tools registered.
//!
//! [`McpManager::spawn_all`] launches each enabled `[mcp.servers.*]`
//! entry, [`McpManager::register_into`] discovers and registers their
//! tools, and [`McpManager::refresh_into`] re-lists any server that
//! pushed a `notifications/tools/list_changed`, swapping its adapters
//! in place (stale tools are unregistered, not left dangling).
//!
//! [`McpManager::refresh_into`] also notices a server whose transport
//! has died: it unregisters that server's tools, evicts the handle,
//! and reports a [`McpError::Crashed`] failure, while the launch spec
//! is retained so [`McpManager::restart`] can respawn it later.
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
/// respawned by `restart`, including after an eviction), the live
/// handle (`None` once the server has been evicted as dead), and the
/// tool names it currently contributes.
struct Managed {
    spec: McpServer,
    handle: Option<McpServerHandle>,
    registered: Vec<String>,
}

/// Owns the spawned MCP servers and mediates their tools into a
/// [`ToolRegistry`].
#[derive(Default)]
pub struct McpManager {
    servers: Vec<(String, Managed)>,
    /// Filesystem roots advertised to every server (the host workdir),
    /// retained so a `restart` re-advertises the same roots.
    roots: Vec<std::path::PathBuf>,
    /// Host handler for server-initiated requests (sampling, ...),
    /// retained so a `restart` re-injects it.
    handler: Option<Arc<dyn crate::ServerRequestHandler>>,
}

impl McpManager {
    /// Spawn every enabled server in `cfg` (sorted by name for a
    /// deterministic registration order). Disabled entries are
    /// skipped. Spawn/handshake failures are collected as
    /// `(server_name, error)` and returned alongside the manager so
    /// the caller can surface them without losing the servers that
    /// did come up. `roots` are the filesystem roots advertised to
    /// every server (typically the host workdir); `handler` answers
    /// server-initiated requests such as sampling.
    #[must_use]
    pub fn spawn_all(
        cfg: &McpConfig,
        roots: Vec<std::path::PathBuf>,
        handler: Option<Arc<dyn crate::ServerRequestHandler>>,
    ) -> (Self, Vec<(String, McpError)>) {
        let mut servers = Vec::new();
        let mut errors = Vec::new();
        for (name, spec) in &cfg.servers {
            if spec.disabled {
                continue;
            }
            match McpServerHandle::spawn(name.clone(), spec, &roots, handler.clone()) {
                Ok(handle) => servers.push((
                    name.clone(),
                    Managed {
                        spec: spec.clone(),
                        handle: Some(handle),
                        registered: Vec::new(),
                    },
                )),
                Err(e) => errors.push((name.clone(), e)),
            }
        }
        (
            Self {
                servers,
                roots,
                handler,
            },
            errors,
        )
    }

    /// Whether no server is live (evicted ones do not count).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of live servers (evicted ones do not count).
    #[must_use]
    pub fn len(&self) -> usize {
        self.servers
            .iter()
            .filter(|(_, m)| m.handle.is_some())
            .count()
    }

    /// Names of the live servers, in registration order.
    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.servers
            .iter()
            .filter(|(_, m)| m.handle.is_some())
            .map(|(n, _)| n.as_str())
    }

    /// Discover and register every live server's tools. Returns the
    /// per-server discovery failures; a failing server simply
    /// contributes no tools.
    pub fn register_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            if managed.handle.is_none() {
                continue;
            }
            if let Err(e) = Self::reload(managed, reg) {
                errors.push((name.clone(), e));
            }
        }
        errors
    }

    /// Re-list only the servers that announced a tool-list change
    /// since the last call, swapping their adapters in place. A
    /// server whose transport has died is evicted instead: its tools
    /// are unregistered and a [`McpError::Crashed`] failure is
    /// reported (the launch spec is kept for a later `restart`).
    /// Returns per-server failures.
    pub fn refresh_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            let Some(handle) = managed.handle.as_ref() else {
                continue;
            };
            let conn = Arc::clone(handle.connection());
            if conn.is_dead() {
                let detail = managed
                    .handle
                    .as_mut()
                    .and_then(McpServerHandle::exit_status)
                    .unwrap_or_else(|| "connection closed".to_owned());
                for stale in managed.registered.drain(..) {
                    reg.unregister(&stale);
                }
                managed.handle = None;
                errors.push((
                    name.clone(),
                    McpError::Crashed {
                        server: name.clone(),
                        detail,
                    },
                ));
            } else if conn.take_tools_changed() {
                if let Err(e) = Self::reload(managed, reg) {
                    errors.push((name.clone(), e));
                }
            }
        }
        errors
    }

    /// Restart one server by name: spawn a fresh process from its
    /// original spec, and only on success swap it in (killing the old
    /// child, if any) and re-register its tools. The name is looked
    /// up across every entry, including servers evicted as dead, so
    /// `restart` can respawn a crashed server from its retained spec.
    /// A failed respawn leaves the current state untouched, so
    /// `restart` never causes downtime on its own failure.
    ///
    /// # Errors
    ///
    /// [`McpError::Unknown`] if no server has that name, or the spawn
    /// / discovery error from bringing the replacement up.
    pub fn restart(&mut self, name: &str, reg: &mut ToolRegistry) -> Result<(), McpError> {
        let roots = self.roots.clone();
        let handler = self.handler.clone();
        let managed = self
            .servers
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m)
            .ok_or_else(|| McpError::Unknown(name.to_owned()))?;
        let fresh = McpServerHandle::spawn(name.to_owned(), &managed.spec, &roots, handler)?;
        for stale in managed.registered.drain(..) {
            reg.unregister(&stale);
        }
        managed.handle = Some(fresh);
        Self::reload(managed, reg)
    }

    /// Drop this server's previously registered tools and register
    /// its current set, updating the tracked names. The caller must
    /// ensure the handle is live.
    fn reload(managed: &mut Managed, reg: &mut ToolRegistry) -> Result<(), McpError> {
        let conn = Arc::clone(
            managed
                .handle
                .as_ref()
                .expect("reload called on a live server")
                .connection(),
        );
        reload_connection(&conn, &mut managed.registered, reg)
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    use super::*;
    use crate::server::PROTOCOL_VERSION;
    use kage_core::config::McpServer;
    use kage_jsonrpc::{Inbound, connect};

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
                    other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
                };
                let _ = responder.respond(&id, outcome);
            }
        });
        Arc::new(McpConnection::initialize("x", cli_peer, cli_in, &[], None).unwrap())
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
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: None,
                headers: std::collections::BTreeMap::new(),
                disabled: true,
            },
        );
        cfg.servers.insert(
            "broken".to_owned(),
            McpServer {
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                url: None,
                headers: std::collections::BTreeMap::new(),
                disabled: false,
            },
        );
        let (mgr, errors) = McpManager::spawn_all(&cfg, vec![], None);
        assert!(mgr.is_empty(), "disabled skipped, broken failed to spawn");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "broken");
    }

    #[test]
    fn restart_unknown_server_errors() {
        let (mut mgr, _e) = McpManager::spawn_all(&McpConfig::default(), vec![], None);
        let mut reg = ToolRegistry::new();
        let err = mgr.restart("ghost", &mut reg).unwrap_err();
        assert!(
            matches!(&err, crate::server::McpError::Unknown(n) if n == "ghost"),
            "{err}"
        );
    }

    impl McpManager {
        /// Test-only injection: build a `Managed` around an
        /// in-process connection, since `spawn_all` launches real
        /// processes.
        fn inject(&mut self, name: &str, spec: McpServer, conn: Arc<McpConnection>) {
            self.servers.push((
                name.to_owned(),
                Managed {
                    spec,
                    handle: Some(McpServerHandle::from_connection(conn)),
                    registered: Vec::new(),
                },
            ));
        }
    }

    /// A server that answers `initialize` and `tools/list` (one tool
    /// named `t`) and exits once `kill` is set, dropping the
    /// transport so the client sees EOF.
    fn killable_server(kill: Arc<AtomicBool>) -> Arc<McpConnection> {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder = srv_peer.clone();
        thread::spawn(move || {
            loop {
                if kill.load(Ordering::SeqCst) {
                    return;
                }
                match srv_in.recv_timeout(std::time::Duration::from_millis(5)) {
                    Ok(Inbound::Request { id, method, .. }) => {
                        let outcome = match method.as_str() {
                            "initialize" => Ok(serde_json::json!({
                                "protocolVersion": PROTOCOL_VERSION,
                                "capabilities": {},
                            })),
                            "tools/list" => Ok(serde_json::json!({
                                "tools": [{ "name": "t", "inputSchema": {} }],
                            })),
                            other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
                        };
                        let _ = responder.respond(&id, outcome);
                    }
                    Ok(Inbound::Notification { .. })
                    | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Arc::new(McpConnection::initialize("x", cli_peer, cli_in, &[], None).unwrap())
    }

    #[test]
    fn refresh_evicts_a_dead_server_and_restart_uses_its_spec() {
        let kill = Arc::new(AtomicBool::new(false));
        let conn = killable_server(Arc::clone(&kill));
        let spec = McpServer {
            command: Some("definitely-not-a-real-binary-xyz".to_owned()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
        };
        let mut mgr = McpManager::default();
        mgr.inject("x", spec, Arc::clone(&conn));
        let mut reg = ToolRegistry::new();
        assert!(
            mgr.register_into(&mut reg).is_empty(),
            "injected server registers cleanly"
        );
        assert!(reg.get("x__t").is_some());
        assert_eq!(mgr.len(), 1);

        kill.store(true, Ordering::SeqCst);
        let mut dead = false;
        for _ in 0..200 {
            if conn.is_dead() {
                dead = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(dead, "kill switch must close the transport");

        let errors = mgr.refresh_into(&mut reg);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "x");
        assert!(
            matches!(&errors[0].1, McpError::Crashed { server, .. } if server == "x"),
            "{:?}",
            errors[0].1
        );
        assert!(reg.get("x__t").is_none(), "dead server's tools evicted");
        assert!(mgr.is_empty(), "evicted server no longer counts as live");
        assert!(
            mgr.refresh_into(&mut reg).is_empty(),
            "an evicted server is skipped, not re-erroring"
        );

        let err = mgr.restart("x", &mut reg).unwrap_err();
        assert!(
            matches!(&err, McpError::Spawn { command, .. } if command == "definitely-not-a-real-binary-xyz"),
            "spec must survive eviction: {err}"
        );
    }
}
