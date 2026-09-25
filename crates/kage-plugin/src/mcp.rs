//! `kage.mcp.add_server` / `list_servers` / `restart`: declarative
//! MCP client config from plugins, plus a restart request.
//!
//! `kage.mcp.add_server({...})` mirrors `[mcp.servers.<name>]` in
//! `config.toml` so a plugin can declare external MCP tool servers at
//! runtime (the `nvim-lspconfig` analogy: plugins *configure*, core
//! spawns). `kage.mcp.list_servers()` returns the declared names.
//! `kage.mcp.restart(name)` enqueues a restart the host drains and
//! applies against the live manager (the plugin layer does not own
//! the process handles, so it requests rather than acts).
//!
//! Declaring a server names a command the host will spawn, so
//! `add_server` and `restart` require the `exec` capability (see
//! [`register`]); `list_servers` stays on the base surface.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use kage_core::config::McpServer;
use kage_core::sync::lock;
use mlua::{Lua, Table, Value};

use crate::capabilities::{Capability, CapabilityRegistry};
use crate::error::PluginError;

/// Shared map of plugin-declared MCP servers. The host merges these
/// with `[mcp.servers.*]` from config.
pub type SharedMcpServers = Arc<Mutex<BTreeMap<String, McpServer>>>;

/// Construct an empty plugin-server map.
#[must_use]
pub fn shared_mcp_servers() -> SharedMcpServers {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// FIFO of server names a plugin asked to restart, drained by the
/// host between turns and applied against the live MCP manager.
pub type SharedMcpRestart = Arc<Mutex<Vec<String>>>;

/// Construct an empty restart queue.
#[must_use]
pub fn shared_mcp_restart() -> SharedMcpRestart {
    Arc::new(Mutex::new(Vec::new()))
}

/// Install the base `kage.mcp` surface: `list_servers`.
///
/// # Errors
///
/// Returns [`PluginError`] if the `kage` global is missing or the
/// table cannot be populated.
pub fn install_mcp(lua: &Lua, servers: SharedMcpServers) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let mcp = lua.create_table()?;
    mcp.set("list_servers", list_servers_fn(lua, servers)?)?;
    kage.set("mcp", mcp)?;
    Ok(())
}

/// Register the `exec`-capability installer that attaches
/// `kage.mcp.add_server` and `kage.mcp.restart` onto a granted
/// plugin's `kage` proxy, shadowing the base `kage.mcp` table (whose
/// `list_servers` stays reachable through the shadow's `__index`).
pub(crate) fn register(
    registry: &CapabilityRegistry,
    servers: SharedMcpServers,
    restart: SharedMcpRestart,
) {
    let mut reg = lock(registry);
    reg.entry(Capability::Exec)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
            let kage: Table = lua.globals().get("kage")?;
            let base: Table = kage.get("mcp")?;
            let pmcp = lua.create_table()?;
            let mt = lua.create_table()?;
            mt.set("__index", base)?;
            mt.set("__metatable", false)?;
            pmcp.set_metatable(Some(mt))?;
            pmcp.set("add_server", add_server_fn(lua, Arc::clone(&servers))?)?;
            pmcp.set("restart", restart_fn(lua, Arc::clone(&restart))?)?;
            pkage.set("mcp", pmcp)?;
            Ok(())
        }));
}

fn list_servers_fn(lua: &Lua, servers: SharedMcpServers) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, ()| {
        let names = lua.create_table()?;
        let guard = servers
            .lock()
            .map_err(|_| mlua::Error::external("plugin mcp servers map poisoned"))?;
        for (i, name) in guard.keys().enumerate() {
            names.set(i + 1, name.clone())?;
        }
        Ok(names)
    })
}

fn add_server_fn(lua: &Lua, servers: SharedMcpServers) -> mlua::Result<mlua::Function> {
    lua.create_function(move |_lua, spec: Table| {
        let name: String = spec.get("name")?;
        let command: String = spec.get("command")?;
        if name.is_empty() || command.is_empty() {
            return Err(mlua::Error::external(
                "kage.mcp.add_server: `name` and `command` are required",
            ));
        }
        let args: Vec<String> = match spec.get::<Value>("args")? {
            Value::Nil => Vec::new(),
            Value::Table(t) => t
                .sequence_values::<String>()
                .collect::<Result<_, _>>()
                .map_err(|_| {
                    mlua::Error::external("kage.mcp.add_server: `args` must be a string array")
                })?,
            _ => {
                return Err(mlua::Error::external(
                    "kage.mcp.add_server: `args` must be a string array",
                ));
            }
        };
        let mut env = BTreeMap::new();
        if let Value::Table(t) = spec.get::<Value>("env")? {
            for pair in t.pairs::<String, String>() {
                let (k, v) = pair.map_err(|_| {
                    mlua::Error::external("kage.mcp.add_server: `env` must be a string map")
                })?;
                env.insert(k, v);
            }
        }
        let disabled = matches!(spec.get::<Value>("disabled")?, Value::Boolean(true));
        servers
            .lock()
            .map_err(|_| mlua::Error::external("plugin mcp servers map poisoned"))?
            .insert(
                name,
                McpServer {
                    command: Some(command),
                    args,
                    env,
                    url: None,
                    headers: BTreeMap::new(),
                    disabled,
                    oauth: None,
                },
            );
        Ok(())
    })
}

fn restart_fn(lua: &Lua, restart: SharedMcpRestart) -> mlua::Result<mlua::Function> {
    lua.create_function(move |_lua, name: String| {
        if name.is_empty() {
            return Err(mlua::Error::external(
                "kage.mcp.restart: a server `name` is required",
            ));
        }
        restart
            .lock()
            .map_err(|_| mlua::Error::external("plugin mcp restart queue poisoned"))?
            .push(name);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install the base surface and run the `exec` installer against
    /// the global `kage` table, which stands in for a granted
    /// plugin's proxy.
    fn install_for_test(lua: &Lua, servers: &SharedMcpServers, restart: &SharedMcpRestart) {
        install_mcp(lua, Arc::clone(servers)).unwrap();
        let registry = crate::capabilities::capability_registry();
        register(&registry, Arc::clone(servers), Arc::clone(restart));
        let kage: Table = lua.globals().get("kage").unwrap();
        for installer in registry
            .lock()
            .unwrap()
            .get(&Capability::Exec)
            .expect("exec installer registered")
        {
            installer(lua, &kage).unwrap();
        }
    }

    fn lua_with_kage() -> Lua {
        let lua = Lua::new();
        lua.globals()
            .set("kage", lua.create_table().unwrap())
            .unwrap();
        lua
    }

    #[test]
    fn add_server_records_into_shared_map() {
        let lua = lua_with_kage();
        let servers = shared_mcp_servers();
        install_for_test(&lua, &servers, &shared_mcp_restart());
        lua.load(
            r#"kage.mcp.add_server({
                name = "fs",
                command = "npx",
                args = { "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
                env = { TOKEN = "k" },
                disabled = true,
            })"#,
        )
        .exec()
        .unwrap();
        let map = servers.lock().unwrap();
        let s = map.get("fs").expect("server recorded");
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(
            s.args,
            ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        );
        assert_eq!(s.env.get("TOKEN").map(String::as_str), Some("k"));
        assert!(s.disabled);
    }

    #[test]
    fn add_server_rejects_missing_command() {
        let lua = lua_with_kage();
        install_for_test(&lua, &shared_mcp_servers(), &shared_mcp_restart());
        assert!(
            lua.load(r#"kage.mcp.add_server({ name = "x" })"#)
                .exec()
                .is_err()
        );
    }

    #[test]
    fn list_servers_returns_declared_names() {
        let lua = lua_with_kage();
        let servers = shared_mcp_servers();
        install_for_test(&lua, &servers, &shared_mcp_restart());
        lua.load(
            r#"
            kage.mcp.add_server({ name = "b", command = "x" })
            kage.mcp.add_server({ name = "a", command = "y" })
            "#,
        )
        .exec()
        .unwrap();
        let names: Vec<String> = lua
            .load("kage.mcp.list_servers()")
            .eval::<mlua::Table>()
            .unwrap()
            .sequence_values::<String>()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(names, ["a", "b"], "sorted by BTreeMap key order");
    }

    #[test]
    fn list_servers_stays_on_base_surface() {
        let lua = lua_with_kage();
        let servers = shared_mcp_servers();
        install_mcp(&lua, Arc::clone(&servers)).unwrap();
        let names: Vec<String> = lua
            .load("return kage.mcp.list_servers()")
            .eval::<mlua::Table>()
            .unwrap()
            .sequence_values::<String>()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn restart_enqueues_name_and_rejects_empty() {
        let lua = lua_with_kage();
        let queue = shared_mcp_restart();
        install_for_test(&lua, &shared_mcp_servers(), &queue);
        lua.load(r#"kage.mcp.restart("fs")"#).exec().unwrap();
        assert_eq!(queue.lock().unwrap().as_slice(), ["fs"]);
        assert!(lua.load(r#"kage.mcp.restart("")"#).exec().is_err());
    }
}
