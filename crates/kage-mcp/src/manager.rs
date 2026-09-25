//! Own every configured MCP server and keep its tools registered.
//!
//! [`McpManager::spawn_all`] launches each enabled `[mcp.servers.*]`
//! entry, [`McpManager::register_into`] discovers and registers their
//! tools, and [`McpManager::refresh_into`] re-lists any server that
//! pushed a `notifications/tools/list_changed`, swapping its adapters
//! in place (stale tools are unregistered, not left dangling).
//!
//! The manager also caches each server's resources, resource templates
//! and prompts, listed only when the server advertises the capability
//! and reloaded when it announces a change, so [`McpManager::catalog`]
//! never touches the network.
//!
//! [`McpManager::refresh_into`] also notices a server whose transport
//! has died: it unregisters that server's tools, evicts the handle,
//! and reports a [`McpError::Crashed`] failure, while the launch spec
//! is retained so [`McpManager::restart`] can respawn it later.
//!
//! A server that fails to spawn or list tools does not abort the
//! agent: the failure is collected and returned to the caller to
//! surface, while the rest of the servers continue. A server that
//! failed to spawn is kept like an evicted one, so `restart` can bring
//! it up later and permission gates still know its name. The manager
//! owns the [`McpServerHandle`]s, so dropping it kills every child.

use std::sync::Arc;

use kage_core::config::{McpConfig, McpServer};
use kage_core::protocol::{
    McpPrompt, McpResource, McpResourceTemplate, McpServerInfo, McpServerStatus,
};
use kage_tools::ToolRegistry;

use crate::server::{McpConnection, McpError, McpServerHandle};
use crate::tools::tools_from_connection;

/// One configured server: its launch spec (kept so it can be
/// respawned by `restart`, including after an eviction), the live
/// handle (`None` when it failed to spawn or was evicted as dead), the
/// last error of a server that is not live, the tool names it
/// currently contributes, and its cached catalog lists.
struct Managed {
    spec: McpServer,
    handle: Option<McpServerHandle>,
    error: Option<String>,
    registered: Vec<String>,
    resources: Vec<McpResource>,
    templates: Vec<McpResourceTemplate>,
    prompts: Vec<McpPrompt>,
}

impl Managed {
    fn new(spec: McpServer, handle: Option<McpServerHandle>, error: Option<String>) -> Self {
        Self {
            spec,
            handle,
            error,
            registered: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
            prompts: Vec::new(),
        }
    }

    fn connection(&self) -> Option<Arc<McpConnection>> {
        self.handle.as_ref().map(|h| Arc::clone(h.connection()))
    }

    /// Reload the cached resources and templates. A failure keeps the
    /// previous lists.
    fn load_resources(&mut self) -> Result<(), McpError> {
        let Some(conn) = self.connection() else {
            return Ok(());
        };
        let resources = conn.list_resources()?;
        self.templates = conn.list_resource_templates()?;
        self.resources = resources;
        Ok(())
    }

    /// Reload the cached prompts. A failure keeps the previous list.
    fn load_prompts(&mut self) -> Result<(), McpError> {
        let Some(conn) = self.connection() else {
            return Ok(());
        };
        self.prompts = conn.list_prompts()?;
        Ok(())
    }

    /// Reload tools, resources and prompts, returning every failure. A
    /// failing catalog list leaves the tools registered.
    fn load_all(&mut self, reg: &mut ToolRegistry) -> Vec<McpError> {
        [
            McpManager::reload(self, reg),
            self.load_resources(),
            self.load_prompts(),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect()
    }

    /// Forget the cached catalog of a server that is no longer live or
    /// is about to be replaced.
    fn clear_catalog(&mut self) {
        self.resources.clear();
        self.templates.clear();
        self.prompts.clear();
    }

    fn info(&self, name: &str) -> McpServerInfo {
        let status = if self.handle.is_some() {
            McpServerStatus::Connected
        } else {
            McpServerStatus::Failed {
                error: self.error.clone().unwrap_or_default(),
            }
        };
        McpServerInfo {
            name: name.to_owned(),
            status,
            tools: u32::try_from(self.registered.len()).unwrap_or(u32::MAX),
            resources: self.resources.clone(),
            templates: self.templates.clone(),
            prompts: self.prompts.clone(),
        }
    }
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
    /// did come up. A failed server stays in the manager without a
    /// handle, so [`Self::restart`] can retry it. `roots` are the
    /// filesystem roots advertised to every server (typically the host
    /// workdir); `handler` answers server-initiated requests such as
    /// sampling.
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
            let (handle, error) =
                match McpServerHandle::spawn(name.clone(), spec, &roots, handler.clone()) {
                    Ok(handle) => (Some(handle), None),
                    Err(e) => {
                        let detail = e.to_string();
                        errors.push((name.clone(), e));
                        (None, Some(detail))
                    }
                };
            servers.push((name.clone(), Managed::new(spec.clone(), handle, error)));
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

    /// Whether no server is live (failed and evicted ones do not count).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of live servers (failed and evicted ones do not count).
    #[must_use]
    pub fn len(&self) -> usize {
        self.servers
            .iter()
            .filter(|(_, m)| m.handle.is_some())
            .count()
    }

    /// Names of the servers whose tools must be gated: every
    /// configured, enabled server, live, failed or evicted, in
    /// registration order. A server that comes up later through
    /// [`Self::restart`] is therefore already known to a gate built
    /// from this list.
    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.servers.iter().map(|(n, _)| n.as_str())
    }

    /// The spawn or crash error of a server that is not live, or `None`
    /// for a live or unknown server.
    #[must_use]
    pub fn error(&self, name: &str) -> Option<&str> {
        self.servers
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, m)| m.error.as_deref())
    }

    /// Every configured, enabled server with its status, tool count and
    /// cached resources, templates and prompts, in registration order. A
    /// server that is not live reports its error and empty lists. Never
    /// touches the network.
    #[must_use]
    pub fn catalog(&self) -> Vec<McpServerInfo> {
        self.servers.iter().map(|(n, m)| m.info(n)).collect()
    }

    /// The live connections, by server name, for the calls that read
    /// resources and fetch prompts.
    #[must_use]
    pub fn clients(&self) -> Vec<(String, Arc<McpConnection>)> {
        self.servers
            .iter()
            .filter_map(|(n, m)| Some((n.clone(), m.connection()?)))
            .collect()
    }

    /// Manage an already initialized connection as server `name`, for
    /// hosts and tests that run a server in process. The server has no
    /// launch spec, so [`Self::restart`] reports a config error for it.
    /// Call [`Self::register_into`] afterwards to list its tools and
    /// catalog.
    pub fn adopt(&mut self, name: &str, conn: Arc<McpConnection>) {
        let spec = McpServer {
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
        };
        let handle = McpServerHandle::from_connection(conn);
        self.servers
            .push((name.to_owned(), Managed::new(spec, Some(handle), None)));
    }

    /// Discover and register every live server's tools, and list the
    /// resources, templates and prompts of servers that advertise them.
    /// Returns the per-server failures; a failing tool list simply
    /// contributes no tools, and a failing catalog list leaves the
    /// tools registered.
    pub fn register_into(&mut self, reg: &mut ToolRegistry) -> Vec<(String, McpError)> {
        let mut errors = Vec::new();
        for (name, managed) in &mut self.servers {
            if managed.handle.is_none() {
                continue;
            }
            errors.extend(managed.load_all(reg).into_iter().map(|e| (name.clone(), e)));
        }
        errors
    }

    /// Re-list only the lists each server announced a change for since
    /// the last call: tools (swapping their adapters in place),
    /// resources with their templates, or prompts. A server whose
    /// transport has died is evicted instead: its tools are
    /// unregistered, its catalog is cleared and a [`McpError::Crashed`]
    /// failure is reported (the launch spec is kept for a later
    /// `restart`). Returns per-server failures.
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
                managed.clear_catalog();
                let crash = McpError::Crashed {
                    server: name.clone(),
                    detail,
                };
                managed.error = Some(crash.to_string());
                errors.push((name.clone(), crash));
                continue;
            }
            if conn.take_tools_changed()
                && let Err(e) = Self::reload(managed, reg)
            {
                errors.push((name.clone(), e));
            }
            if conn.take_resources_changed()
                && let Err(e) = managed.load_resources()
            {
                errors.push((name.clone(), e));
            }
            if conn.take_prompts_changed()
                && let Err(e) = managed.load_prompts()
            {
                errors.push((name.clone(), e));
            }
        }
        errors
    }

    /// Restart one server by name: spawn a fresh process from its
    /// original spec, and only on success swap it in (killing the old
    /// child, if any), re-register its tools and reload its catalog.
    /// The name is looked up across every entry, including servers
    /// that failed to spawn or were evicted as dead, so `restart` can
    /// bring them up from the retained spec. A failed respawn leaves a live server untouched,
    /// so `restart` never causes downtime on its own failure, and
    /// records the new error for a server that is not live.
    ///
    /// # Errors
    ///
    /// [`McpError::Unknown`] if no server has that name, the spawn
    /// error, or the first discovery error from bringing the
    /// replacement up (the server stays live).
    pub fn restart(&mut self, name: &str, reg: &mut ToolRegistry) -> Result<(), McpError> {
        let roots = self.roots.clone();
        let handler = self.handler.clone();
        let managed = self
            .servers
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m)
            .ok_or_else(|| McpError::Unknown(name.to_owned()))?;
        let fresh = match McpServerHandle::spawn(name.to_owned(), &managed.spec, &roots, handler) {
            Ok(fresh) => fresh,
            Err(e) => {
                if managed.handle.is_none() {
                    managed.error = Some(e.to_string());
                }
                return Err(e);
            }
        };
        for stale in managed.registered.drain(..) {
            reg.unregister(&stale);
        }
        managed.handle = Some(fresh);
        managed.error = None;
        managed.clear_catalog();
        managed.load_all(reg).into_iter().next().map_or(Ok(()), Err)
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
    fn a_failed_spawn_is_kept_for_gates_and_restart() {
        let mut cfg = McpConfig::default();
        let spec = McpServer {
            command: Some("definitely-not-a-real-binary-xyz".to_owned()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
        };
        cfg.servers.insert("broken".to_owned(), spec.clone());
        cfg.servers.insert(
            "off".to_owned(),
            McpServer {
                disabled: true,
                ..spec
            },
        );
        let (mut mgr, errors) = McpManager::spawn_all(&cfg, vec![], None);
        assert_eq!(errors.len(), 1);
        assert_eq!(mgr.server_names().collect::<Vec<_>>(), ["broken"]);
        assert_eq!(mgr.len(), 0);
        assert!(mgr.is_empty());
        let spawn_error = mgr.error("broken").expect("the spawn error is kept");
        assert!(spawn_error.contains("definitely-not-a-real-binary-xyz"));

        let mut reg = ToolRegistry::new();
        let err = mgr.restart("broken", &mut reg).unwrap_err();
        assert!(
            matches!(&err, McpError::Spawn { command, .. } if command == "definitely-not-a-real-binary-xyz"),
            "restart must retry the kept spec: {err}"
        );
        assert!(mgr.error("off").is_none());
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
        mgr.adopt("x", Arc::clone(&conn));
        mgr.servers[0].1.spec = spec;
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
        assert!(mgr.error("x").is_some_and(|e| e.contains("crashed")));
        assert_eq!(mgr.server_names().collect::<Vec<_>>(), ["x"]);
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

    /// A server with one tool, one resource, one template and one
    /// prompt, advertising `resources` and `prompts`. `resources/list`
    /// fails while `fail_resources` is set.
    fn catalog_server(
        fail_resources: Arc<AtomicBool>,
    ) -> (
        Arc<McpConnection>,
        kage_jsonrpc::Peer,
        crate::catalog::tests::Seen,
    ) {
        let caps = serde_json::json!({ "tools": {}, "resources": {}, "prompts": {} });
        crate::catalog::tests::scripted("srv", caps, move |method, _| {
            Ok(match method {
                "tools/list" => {
                    serde_json::json!({ "tools": [{ "name": "t", "inputSchema": {} }] })
                }
                "resources/list" if fail_resources.load(Ordering::SeqCst) => {
                    return Err(kage_jsonrpc::RpcError::internal("boom"));
                }
                "resources/list" => {
                    serde_json::json!({ "resources": [{ "uri": "test://r", "name": "R" }] })
                }
                "resources/templates/list" => serde_json::json!({
                    "resourceTemplates": [{ "uriTemplate": "test://r/{id}", "name": "T" }]
                }),
                "prompts/list" => serde_json::json!({ "prompts": [{ "name": "p" }] }),
                other => return Err(kage_jsonrpc::RpcError::method_not_found(other)),
            })
        })
    }

    fn methods(seen: &crate::catalog::tests::Seen) -> Vec<String> {
        seen.lock().unwrap().drain(..).map(|(m, _)| m).collect()
    }

    #[test]
    fn catalog_reports_live_counts_and_failed_errors() {
        let mut cfg = McpConfig::default();
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
        let (mut mgr, _errors) = McpManager::spawn_all(&cfg, vec![], None);
        let (conn, _srv, _seen) = catalog_server(Arc::new(AtomicBool::new(false)));
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());

        let catalog = mgr.catalog();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].name, "broken");
        let McpServerStatus::Failed { error } = &catalog[0].status else {
            panic!("broken must be failed: {:?}", catalog[0].status);
        };
        assert!(
            error.contains("definitely-not-a-real-binary-xyz"),
            "{error}"
        );
        assert_eq!(catalog[0].tools, 0);
        assert!(catalog[0].resources.is_empty());

        let live = &catalog[1];
        assert_eq!(live.name, "srv");
        assert_eq!(live.status, McpServerStatus::Connected);
        assert_eq!(live.tools, 1);
        assert_eq!(live.resources[0].uri, "test://r");
        assert_eq!(live.templates[0].uri_template, "test://r/{id}");
        assert_eq!(live.prompts[0].name, "p");

        let clients = mgr.clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].0, "srv");
        assert_eq!(clients[0].1.name(), "srv");
    }

    #[test]
    fn a_failing_catalog_list_leaves_tools_registered() {
        let fail = Arc::new(AtomicBool::new(true));
        let (conn, _srv, _seen) = catalog_server(Arc::clone(&fail));
        let mut mgr = McpManager::default();
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        let errors = mgr.register_into(&mut reg);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "srv");
        assert!(errors[0].1.to_string().contains("boom"), "{}", errors[0].1);
        assert!(reg.get("srv__t").is_some());
        let info = &mgr.catalog()[0];
        assert_eq!(info.tools, 1);
        assert!(info.resources.is_empty());
        assert_eq!(info.prompts.len(), 1, "prompts still load");
    }

    #[test]
    fn a_resources_list_changed_notice_reloads_resources_only() {
        let (conn, srv, seen) = catalog_server(Arc::new(AtomicBool::new(false)));
        let mut mgr = McpManager::default();
        mgr.adopt("srv", conn);
        let mut reg = ToolRegistry::new();
        assert!(mgr.register_into(&mut reg).is_empty());
        methods(&seen);

        assert!(mgr.refresh_into(&mut reg).is_empty());
        assert!(methods(&seen).is_empty(), "no notice, no request");

        srv.notify(
            "notifications/resources/list_changed",
            serde_json::json!({}),
        )
        .unwrap();
        srv.request("ping", serde_json::json!({}))
            .expect("the drain thread handled the notice before the ping");
        assert!(mgr.refresh_into(&mut reg).is_empty());
        assert_eq!(
            methods(&seen),
            ["resources/list", "resources/templates/list"]
        );
        assert_eq!(mgr.catalog()[0].resources.len(), 1);
    }
}
