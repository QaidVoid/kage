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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use kage_core::config::AcpAgent;
use mlua::{Lua, Table, Value};

use crate::api::json_to_lua;
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

/// Install `kage.acp.add_agent({...})` and
/// `kage.on_acp_permission(fn)` on the running Lua state.
pub fn install_acp(lua: &Lua, agents: SharedAcpAgents) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let acp = lua.create_table()?;

    acp.set(
        "add_agent",
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
        })?,
    )?;
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

    #[test]
    fn add_agent_records_into_shared_map() {
        let lua = lua_with_kage();
        let agents = shared_acp_agents();
        install_acp(&lua, Arc::clone(&agents)).unwrap();
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
        install_acp(&lua, shared_acp_agents()).unwrap();
        assert!(
            lua.load(r#"kage.acp.add_agent({ name = "x" })"#)
                .exec()
                .is_err()
        );
    }

    #[test]
    fn permission_handler_allow_deny_and_absent() {
        let lua = lua_with_kage();
        install_acp(&lua, shared_acp_agents()).unwrap();
        let payload = serde_json::json!({"tool": "bash"});

        assert_eq!(decide(&lua, &payload), None, "no handler => None");

        lua.load("kage.on_acp_permission(function(req) return req.tool == 'bash' end)")
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
}
