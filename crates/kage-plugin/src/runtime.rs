//! Sandboxed Lua runtime hosting kage plugins.
//!
//! [`PluginRuntime`] wraps an [`mlua::Lua`] state with a small allowlist
//! over the standard library. Every function that touches the host
//! filesystem, spawns processes, or loads native shared libraries is
//! removed before any plugin code runs. Plugins that need filesystem or
//! network access must go through the `kage` table (added in later tasks),
//! which routes through the same guards as built-in tools.
//!
//! The runtime is host-driven: nothing runs unless the host calls
//! [`PluginRuntime::eval`] or one of the typed dispatch helpers added
//! later. A plugin cannot start a thread or schedule a callback on its
//! own.
//!
//! See `crates/kage-plugin/src/runtime.rs` source for the exact list of
//! removed bindings.
//!
//! # Sandbox scope
//!
//! Each plugin is evaluated in its own `_ENV` (see
//! [`PluginRuntime::eval_plugin`]): the standard library and the base
//! `kage` API are shared read-only, but a plugin's own globals are
//! private to it, and the obvious escapes back to the real globals
//! (`_G`, `load`, `require`, `package`, `debug`) are removed. This is
//! the substrate the opt-in capability tier builds on: elevated APIs
//! attach to one plugin's environment, not the shared one. Until that
//! tier lands the sandbox still guards against accidental, not
//! adversarial, access - run only plugins you trust.

pub(crate) use std::collections::{BTreeMap, HashMap};
pub(crate) use std::path::PathBuf;
pub(crate) use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) use mlua::{Lua, RegistryKey, Table};

pub(crate) use crate::acp::{self, SharedAcpAgents, shared_acp_agents};
pub(crate) use crate::api::{self, SharedHostLog, default_host_log};
pub(crate) use crate::autocomplete::{
    self, LuaAutocompleteProvider, RegisteredAutocompleteProviders,
    registered_autocomplete_providers,
};
pub(crate) use crate::block_renderers::{
    self, LuaBlockRenderer, SharedBlockRenderers, shared_block_renderers,
};
pub(crate) use crate::bridge::{self, BridgeStep, SharedBridge, shared_bridge};
pub(crate) use crate::capabilities::{self, CurrentPlugin};
pub(crate) use crate::chrome::{self, LuaChrome, SharedChrome, shared_chrome};
pub(crate) use crate::commands::{self, LuaCommand, RegisteredCommands, registered_commands};
pub(crate) use crate::env;
pub(crate) use crate::error::PluginError;
pub(crate) use crate::events;
pub(crate) use crate::exec;
pub(crate) use crate::fs as plugin_fs;
pub(crate) use crate::http;
pub(crate) use crate::keybindings::{self, RegisteredKeybindings, registered_keybindings};
pub(crate) use crate::lifecycle::{
    self, SharedCompactRequest, SharedUsage, shared_compact_request, shared_usage,
};
pub(crate) use crate::mcp::{
    self, SharedMcpRestart, SharedMcpServers, shared_mcp_restart, shared_mcp_servers,
};
pub(crate) use crate::messages::{
    self, PendingMessage, SharedPendingMessages, shared_pending_messages,
};
pub(crate) use crate::providers::{self, LuaProvider, RegisteredProviders, registered_providers};
pub(crate) use crate::session_write::{
    self, SharedSessionEntries, SharedSwitchRequest, SwitchTarget,
};
pub(crate) use crate::sessions::{
    self, PendingSessionOp, SharedForkRequest, SharedSessionList, SharedSessionOps,
    shared_fork_request, shared_session_list, shared_session_ops,
};
pub(crate) use crate::status::{self, SharedStatus, shared_status};
pub(crate) use crate::store;
pub(crate) use crate::terminal_input::{self, RegisteredTerminalHooks, registered_terminal_hooks};
pub(crate) use crate::theme::{
    self, SharedThemeRequest, SharedThemeState, shared_theme_request, shared_theme_state,
};
pub(crate) use crate::tools::{self, RegisteredTools, registered_tools};
pub(crate) use crate::ui;
pub(crate) use crate::widgets::{self, LuaWidget, RegisteredWidgets, registered_widgets};

/// Shared, mutex-guarded handle to the Lua state. Plugin-defined tools
/// hold one of these so they can call back into Lua from the host's tool
/// dispatch path.
pub type SharedLua = Arc<Mutex<Lua>>;

/// A Lua VM with the dangerous standard-library bindings stripped and
/// the `kage` API table installed.
pub struct PluginRuntime {
    lua: SharedLua,
    sink: SharedHostLog,
    tools: RegisteredTools,
    tool_overrides: RegisteredTools,
    commands: RegisteredCommands,
    command_overrides: RegisteredCommands,
    providers: RegisteredProviders,
    widgets: RegisteredWidgets,
    status: SharedStatus,
    acp_agents: SharedAcpAgents,
    mcp_servers: SharedMcpServers,
    mcp_restart: SharedMcpRestart,
    usage: SharedUsage,
    compact_request: SharedCompactRequest,
    session_list: SharedSessionList,
    fork_request: SharedForkRequest,
    session_ops: SharedSessionOps,
    pending_messages: SharedPendingMessages,
    bridge: SharedBridge,
    keybindings: RegisteredKeybindings,
    theme_state: SharedThemeState,
    theme_request: SharedThemeRequest,
    header: SharedChrome,
    footer: SharedChrome,
    block_renderers: SharedBlockRenderers,
    autocomplete: RegisteredAutocompleteProviders,
    terminal_hooks: RegisteredTerminalHooks,
    /// Per-plugin `_ENV` tables, keyed by plugin name, held in the Lua
    /// registry. Each plugin re-evaluates against its own environment
    /// so plugins cannot see or clobber one another; granted
    /// capabilities are attached onto a plugin's own proxy here.
    plugin_envs: Arc<Mutex<HashMap<String, RegistryKey>>>,
    /// Name of the plugin currently being evaluated, so
    /// `kage.request_capabilities` knows who is asking.
    current_plugin: CurrentPlugin,
    /// Host-maintained snapshot of the current session's entry
    /// metadata, read by `session_write`'s `kage.session.entries`.
    session_entries: SharedSessionEntries,
    /// Pending `session_write` reseat request (`switch`/`fork_to`),
    /// drained by the host.
    switch_request: SharedSwitchRequest,
    /// Load allowlist by plugin file stem. When non-empty, the loader
    /// evaluates only the listed plugins and skips the rest; empty means
    /// load every discovered plugin.
    enabled: Vec<String>,
    /// Per-plugin settings by file stem, surfaced to the named plugin
    /// through `kage.plugin_config()`. Each plugin sees only its slice.
    plugin_config: BTreeMap<String, serde_json::Value>,
    /// Directory backing `kage.store`. When set, each plugin gets a
    /// private `<state_dir>/<stem>.json` persisted across runs; when
    /// `None`, `kage.store` raises so misconfiguration is not silent.
    state_dir: Option<PathBuf>,
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRuntime").finish_non_exhaustive()
    }
}

/// Builder for [`PluginRuntime`]. Lets the host inject a custom host-log
/// sink, a config snapshot, and the workdir that gates `kage.fs.*`.
pub struct PluginRuntimeBuilder {
    sink: SharedHostLog,
    config: serde_json::Value,
    workdir: PathBuf,
    capabilities: BTreeMap<String, Vec<String>>,
    enabled: Vec<String>,
    plugin_config: BTreeMap<String, serde_json::Value>,
    state_dir: Option<PathBuf>,
}

impl std::fmt::Debug for PluginRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRuntimeBuilder")
            .field("config", &self.config)
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}
/// Pairs of `(table_path, key)` removed from the standard library on
/// runtime construction. `table_path` is dot-separated starting from the
/// globals table; an empty path means "drop the global by `key`".
///
/// Besides accidental filesystem/process access, this also closes the
/// reflective and dynamic-loading escapes that would let a plugin
/// reach the real globals out from under its per-plugin `_ENV`
/// (`load`/`require`/`package`/`debug`/`string.dump`), which the
/// capability tier relies on for isolation.
pub const SANDBOX_REMOVALS: &[(&str, &str)] = &[
    // Process spawning and shell access.
    ("os", "execute"),
    ("os", "exit"),
    ("os", "remove"),
    ("os", "rename"),
    ("os", "tmpname"),
    ("os", "getenv"),
    ("os", "setlocale"),
    // Process-spawning io helpers; `io.open` will be replaced with a
    // safe wrapper in T6.8.
    ("io", "popen"),
    ("io", "open"),
    ("io", "tmpfile"),
    ("io", "input"),
    ("io", "output"),
    ("io", "lines"),
    // Native code loading.
    ("package", "loadlib"),
    ("package", "cpath"),
    // Bytecode and arbitrary-file loading.
    ("", "dofile"),
    ("", "loadfile"),
    // Dynamic chunk loading: `load`/`loadstring` default the new
    // chunk's `_ENV` to the real globals, and `string.dump` plus
    // bytecode loading sidestep source review.
    ("", "load"),
    ("", "loadstring"),
    ("string", "dump"),
    // Module loading would execute arbitrary files outside the
    // workdir; single-file plugins do not need it. A future scoped
    // capability can re-grant a constrained require.
    ("", "require"),
    ("", "package"),
    // Reflection: debug.getregistry reaches the shared handler
    // registry and debug.setupvalue can rewrite another function's
    // `_ENV`, either of which defeats per-plugin isolation.
    ("", "debug"),
];

/// Get or create the dedicated `_ENV` table for plugin `name`.
///
/// The table reads through to the shared, sandboxed globals (standard
/// library plus the base `kage` API) via an `__index` metatable, but
/// has no `__newindex`, so a plugin's own top-level assignments are
/// `rawset` into this table and stay private to it. `kage` is a
/// per-plugin proxy over the shared base table - reads fall through,
/// and the capability tier attaches granted APIs onto this proxy so
/// they are visible only to the grantee. `_G` is bound back to this
/// table so `_G.x = ...` cannot reach the real globals. The table is
/// kept in the Lua registry and reused for repeat evals of `name`.
fn plugin_env(
    lua: &Lua,
    name: &str,
    slots: &Mutex<HashMap<String, RegistryKey>>,
    config_slice: Option<&serde_json::Value>,
    store_path: Option<PathBuf>,
) -> mlua::Result<Table> {
    let mut slots = slots.lock().expect("plugin env map poisoned");
    if let Some(key) = slots.get(name) {
        return lua.registry_value::<Table>(key);
    }
    let globals = lua.globals();
    let env = lua.create_table()?;
    let env_mt = lua.create_table()?;
    env_mt.set("__index", globals.clone())?;
    env.set_metatable(Some(env_mt))?;

    let base_kage: Table = globals.get("kage")?;
    let pkage = lua.create_table()?;
    let pkage_mt = lua.create_table()?;
    pkage_mt.set("__index", base_kage)?;
    pkage.set_metatable(Some(pkage_mt))?;
    // Override the base `kage.plugin_config()` with one that returns
    // this plugin's own `[plugins.config.<stem>]` slice. An absent slice
    // yields an empty table, matching the base surface.
    let slice = config_slice
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    pkage.set(
        "plugin_config",
        lua.create_function(move |lua, ()| crate::api::json_to_lua(lua, &slice))?,
    )?;
    // Attach a real `kage.store` when the host configured a state dir;
    // otherwise the base stub (which raises) stays in effect.
    if let Some(path) = store_path {
        store::install_for_plugin(lua, &pkage, path)?;
    }
    env.set("kage", pkage)?;

    // `_G` must point at the plugin's own env, not the shared globals,
    // or it would be a trivial isolation escape.
    env.set("_G", env.clone())?;

    let key = lua.create_registry_value(env.clone())?;
    slots.insert(name.to_owned(), key);
    Ok(env)
}

fn apply_sandbox(lua: &Lua) -> Result<(), PluginError> {
    let globals = lua.globals();
    for (path, key) in SANDBOX_REMOVALS {
        if path.is_empty() {
            globals.set(*key, mlua::Value::Nil)?;
            continue;
        }
        let table: mlua::Value = globals.get(*path)?;
        if let mlua::Value::Table(t) = table {
            t.set(*key, mlua::Value::Nil)?;
        }
    }
    Ok(())
}

mod builder;
mod methods;

#[cfg(test)]
mod tests;
