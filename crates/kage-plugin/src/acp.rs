//! `kage.acp.add_agent` / `kage.on_acp_permission`: declarative ACP
//! client config from plugins, plus a pure-policy permission hook.
//!
//! `kage.acp.add_agent({...})` mirrors `[acp.agents.<name>]` in
//! `config.toml` so a plugin can declare upstream ACP agents at
//! runtime (the `nvim-lspconfig` analogy: plugins *configure*, core
//! spawns). `kage.on_acp_permission(fn)` registers ONE synchronous
//! decision callback the host consults when an upstream agent asks to
//! run a tool. It must return a boolean and must NOT open a dialog
//! (no coroutine suspend): it is policy, not UI. No handler, or a
//! non-boolean / erroring handler, denies - kage never auto-approves
//! an upstream agent's tools.
//!
//! Declaring an agent names a command the host will spawn, so
//! `add_agent` requires the `exec` capability (see [`register`]);
//! `on_acp_permission` stays on the base surface.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use kage_core::config::AcpAgent;
use kage_core::sync::lock;
use mlua::{Lua, Table, Value};

use crate::api::json_to_lua;
use crate::capabilities::{Capability, CapabilityRegistry};
use crate::error::PluginError;

/// Shared map of plugin-declared ACP agents. The host merges these
/// with `[acp.agents.*]` from config.
pub type SharedAcpAgents = Arc<Mutex<BTreeMap<String, AcpAgent>>>;

/// Construct an empty plugin-agent map.
#[must_use]
pub fn shared_acp_agents() -> SharedAcpAgents {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// Registry key holding the single `on_acp_permission` handler.
const PERMISSION_KEY: &str = "kage.acp.permission_handler";

/// Install the base `kage.acp` surface: `on_acp_permission`.
pub fn install_acp(lua: &Lua) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let acp = lua.create_table()?;
    kage.set("acp", acp)?;
    kage.set(
        "on_acp_permission",
        lua.create_function(|lua, handler: mlua::Function| {
            lua.set_named_registry_value(PERMISSION_KEY, handler)?;
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Register the `exec`-capability installer that attaches
/// `kage.acp.add_agent` onto a granted plugin's `kage` proxy,
/// shadowing the base `kage.acp` table (whose `on_acp_permission`
/// stays reachable through the shadow's `__index`).
pub(crate) fn register(registry: &CapabilityRegistry, agents: SharedAcpAgents) {
    let mut reg = lock(registry);
    reg.entry(Capability::Exec)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
            let kage: Table = lua.globals().get("kage")?;
            let base: Table = kage.get("acp")?;
            let pacp = lua.create_table()?;
            let mt = lua.create_table()?;
            mt.set("__index", base)?;
            mt.set("__metatable", false)?;
            pacp.set_metatable(Some(mt))?;
            pacp.set("add_agent", add_agent_fn(lua, Arc::clone(&agents))?)?;
            pkage.set("acp", pacp)?;
            Ok(())
        }));
}

fn add_agent_fn(lua: &Lua, agents: SharedAcpAgents) -> mlua::Result<mlua::Function> {
    lua.create_function(move |_lua, spec: Table| {
        let name: String = spec.get("name")?;
        let command: String = spec.get("command")?;
        if name.is_empty() || command.is_empty() {
            return Err(mlua::Error::external(
                "kage.acp.add_agent: `name` and `command` are required",
            ));
        }
        let args: Vec<String> = match spec.get::<Value>("args")? {
            Value::Nil => Vec::new(),
            Value::Table(t) => t
                .sequence_values::<String>()
                .collect::<Result<_, _>>()
                .map_err(|_| {
                    mlua::Error::external("kage.acp.add_agent: `args` must be a string array")
                })?,
            _ => {
                return Err(mlua::Error::external(
                    "kage.acp.add_agent: `args` must be a string array",
                ));
            }
        };
        let mut env = BTreeMap::new();
        if let Value::Table(t) = spec.get::<Value>("env")? {
            for pair in t.pairs::<String, String>() {
                let (k, v) = pair.map_err(|_| {
                    mlua::Error::external("kage.acp.add_agent: `env` must be a string map")
                })?;
                env.insert(k, v);
            }
        }
        agents
            .lock()
            .map_err(|_| mlua::Error::external("plugin acp agents map poisoned"))?
            .insert(name, AcpAgent { command, args, env });
        Ok(())
    })
}

/// Consult the registered permission handler.
///
/// `Some(true)` = allow, `Some(false)` = explicit deny, `None` = no
/// handler registered (the host applies its own default, which is
/// deny). A handler that errors or returns a non-boolean denies.
#[must_use]
pub fn decide(lua: &Lua, payload: &serde_json::Value) -> Option<bool> {
    let handler: mlua::Function = lua.named_registry_value(PERMISSION_KEY).ok()?;
    let arg = json_to_lua(lua, payload).ok()?;
    match handler.call::<Value>(arg) {
        Ok(Value::Boolean(b)) => Some(b),
        Ok(_) | Err(_) => Some(false),
    }
}

/// Drop the registered `on_acp_permission` handler so a stale
/// plugin's policy callback cannot survive a hot reload.
///
/// # Errors
///
/// Returns [`PluginError`] if the registry value cannot be written.
pub(crate) fn clear_permission_handler(lua: &Lua) -> Result<(), PluginError> {
    lua.set_named_registry_value(PERMISSION_KEY, Value::Nil)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_kage() -> Lua {
        let lua = Lua::new();
        lua.globals()
            .set("kage", lua.create_table().unwrap())
            .unwrap();
        lua
    }

    /// Install the base surface and run the `exec` installer against
    /// the global `kage` table, which stands in for a granted
    /// plugin's proxy.
    fn install_for_test(lua: &Lua, agents: &SharedAcpAgents) {
        install_acp(lua).unwrap();
        let registry = crate::capabilities::capability_registry();
        register(&registry, Arc::clone(agents));
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

    #[test]
    fn add_agent_records_into_shared_map() {
        let lua = lua_with_kage();
        let agents = shared_acp_agents();
        install_for_test(&lua, &agents);
        lua.load(
            r#"kage.acp.add_agent({
                name = "claude-code",
                command = "npx",
                args = { "-y", "@zed-industries/claude-code-acp" },
                env = { ANTHROPIC_API_KEY = "k" },
            })"#,
        )
        .exec()
        .unwrap();
        let map = agents.lock().unwrap();
        let a = map.get("claude-code").expect("agent recorded");
        assert_eq!(a.command, "npx");
        assert_eq!(a.args, ["-y", "@zed-industries/claude-code-acp"]);
        assert_eq!(
            a.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("k")
        );
    }

    #[test]
    fn add_agent_rejects_missing_command() {
        let lua = lua_with_kage();
        install_for_test(&lua, &shared_acp_agents());
        assert!(
            lua.load(r#"kage.acp.add_agent({ name = "x" })"#)
                .exec()
                .is_err()
        );
    }

    #[test]
    fn add_agent_stays_off_base_surface() {
        let lua = lua_with_kage();
        install_acp(&lua).unwrap();
        let absent: bool = lua.load("return kage.acp.add_agent == nil").eval().unwrap();
        assert!(absent, "add_agent must not resolve without the grant");
    }

    #[test]
    fn permission_handler_allow_deny_and_absent() {
        let lua = lua_with_kage();
        install_acp(&lua).unwrap();
        let payload = serde_json::json!({"tool": "shell"});

        assert_eq!(decide(&lua, &payload), None, "no handler => None");

        lua.load("kage.on_acp_permission(function(req) return req.tool == 'shell' end)")
            .exec()
            .unwrap();
        assert_eq!(decide(&lua, &payload), Some(true));
        assert_eq!(
            decide(&lua, &serde_json::json!({"tool": "rm"})),
            Some(false)
        );

        lua.load("kage.on_acp_permission(function() return 'nope' end)")
            .exec()
            .unwrap();
        assert_eq!(decide(&lua, &payload), Some(false), "non-bool => deny");

        lua.load("kage.on_acp_permission(function() error('boom') end)")
            .exec()
            .unwrap();
        assert_eq!(decide(&lua, &payload), Some(false), "error => deny");
    }

    #[test]
    fn clear_permission_handler_removes_decision() {
        let lua = lua_with_kage();
        install_acp(&lua).unwrap();
        lua.load("kage.on_acp_permission(function() return true end)")
            .exec()
            .unwrap();
        assert_eq!(decide(&lua, &serde_json::json!({})), Some(true));
        clear_permission_handler(&lua).unwrap();
        assert_eq!(decide(&lua, &serde_json::json!({})), None);
    }
}
