//! Sandboxed Lua runtime hosting kage plugins.
//!
//! [`PluginRuntime`] wraps an [`mlua::Lua`] state with a small allowlist
//! over the standard library. Every function that touches the host
//! filesystem, spawns processes, or loads native shared libraries is
//! removed before any plugin code runs. Plugins that need filesystem or
//! network access must go through the `kage` table, which routes through
//! the same guards as built-in tools.
//!
//! The runtime is host-driven: nothing runs unless the host calls
//! [`PluginRuntime::eval`] or one of the typed dispatch helpers. A
//! plugin cannot start a thread. It can queue callbacks with
//! `kage.schedule`, `kage.defer` and `kage.timer`, which run on the
//! owner thread between host calls (see the `schedule` module), and
//! every coroutine it creates is covered by the instruction watchdog
//! (see `guard_coroutines`). Before any plugin code, `build` installs
//! the `kage.api` primitives and evaluates the embedded Lua stdlib
//! (see the `stdlib` module). The trusted user config (`init.lua`)
//! runs after every plugin, in its own environment (see the `user`
//! module).
//!
//! See `crates/kage-plugin/src/runtime.rs` source for the exact list of
//! removed bindings.
//!
//! # Sandbox scope
//!
//! Each plugin is evaluated in its own `_ENV` (see
//! [`PluginRuntime::eval_plugin`]): the standard library and the base
//! `kage` API surface, down to every `kage.*` sub-table, as a private
//! per-plugin snapshot, so a plugin can mutate them without poisoning
//! other plugins or the host, and the
//! obvious escapes back to the real globals
//! (`_G`, `load`, `require`, `package`, `debug`, `rawset`) are removed.
//! This is the substrate the opt-in capability tier builds on: elevated
//! APIs attach to one plugin's environment, not the shared one. The
//! tier gates the documented elevated surfaces, not every way a plugin
//! can touch your session - run only plugins you trust.

pub(crate) use std::collections::{BTreeMap, HashMap};
pub(crate) use std::path::PathBuf;
pub(crate) use std::sync::{Arc, Mutex};

use kage_core::sync::lock;

pub(crate) use mlua::{Lua, RegistryKey, Table};

pub(crate) use crate::acp::{self, SharedAcpAgents, shared_acp_agents};
pub(crate) use crate::api::{self, SharedHostLog, default_host_log};
pub(crate) use crate::autocmd::{self, SharedAutocmds};
pub(crate) use crate::autocomplete::{
    self, LuaAutocompleteProvider, RegisteredAutocompleteProviders,
    registered_autocomplete_providers,
};
pub(crate) use crate::block_renderers::{
    self, LuaBlockRenderer, SharedBlockRenderers, shared_block_renderers,
};
pub(crate) use crate::bridge::{self, BridgeStep, SharedBridge, shared_bridge};
pub(crate) use crate::capabilities::{self, CapabilityRegistry, CurrentPlugin};
pub(crate) use crate::commands::{self, LuaCommand, RegisteredCommands, registered_commands};
pub(crate) use crate::crypto;
pub(crate) use crate::env;
pub(crate) use crate::error::PluginError;
pub(crate) use crate::events;
pub(crate) use crate::exec;
pub(crate) use crate::fs as plugin_fs;
pub(crate) use crate::highlight::{self, SharedHighlights, SharedThemeResolver};
pub(crate) use crate::host::LuaHost;
pub(crate) use crate::http;
pub(crate) use crate::keymap::{self, Keymaps};
pub(crate) use crate::lifecycle::{
    self, SharedCompactRequest, SharedUsage, shared_compact_request, shared_usage,
};
pub(crate) use crate::mcp::{
    self, SharedMcpRestart, SharedMcpServers, shared_mcp_restart, shared_mcp_servers,
};
pub(crate) use crate::messages::{
    self, PendingMessage, SharedPendingMessages, shared_pending_messages,
};
pub(crate) use crate::options::{self, Options, SharedOptions};
pub(crate) use crate::providers::{self, LuaProvider, RegisteredProviders, registered_providers};
pub(crate) use crate::schedule;
pub(crate) use crate::session_write::{
    self, SharedSessionEntries, SharedSwitchRequest, SwitchTarget,
};
pub(crate) use crate::sessions::{
    self, PendingSessionOp, SharedForkRequest, SharedSessionList, SharedSessionOps,
    shared_fork_request, shared_session_list, shared_session_ops,
};
pub(crate) use crate::slots::{self, Slots};
pub(crate) use crate::status::{self, SharedStatus, shared_status};
pub(crate) use crate::stdlib;
pub(crate) use crate::store;
pub(crate) use crate::terminal_input::{self, RegisteredTerminalHooks, registered_terminal_hooks};
pub(crate) use crate::theme;
pub(crate) use crate::tools::{self, RegisteredTools, registered_tools};
pub(crate) use crate::ui;
pub(crate) use crate::watchdog;
pub(crate) use crate::widgets::{self, LuaWidget, RegisteredWidgets, registered_widgets};

/// A Lua VM with the dangerous standard-library bindings stripped and
/// the `kage` API table installed.
///
/// The Lua state lives on a dedicated owner thread; this struct is a
/// handle that sends it jobs. Methods that need Lua wait for the owner
/// thread to answer, while render surfaces read retained output and
/// never wait on a busy owner (see [`PluginRuntime::redraw_flag`]).
///
/// Known limitation: a long Lua tool or a Lua provider stream occupies
/// the owner thread for its whole duration. Render output stays on
/// screen meanwhile, but plugin commands, keybindings, and event
/// dispatch queue behind it.
pub struct PluginRuntime {
    pub(crate) host: LuaHost,
    pub(crate) eval: Arc<EvalState>,
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
    /// Slot specs and the UI state their Lua components read.
    slots: Slots,
    block_renderers: SharedBlockRenderers,
    autocomplete: RegisteredAutocompleteProviders,
    terminal_hooks: RegisteredTerminalHooks,
    /// Autocmd metadata, read without a round trip to the owner thread.
    autocmds: SharedAutocmds,
    /// Option store, highlight table and the theme resolver.
    options: Options,
    /// Host-maintained snapshot of the current session's entry
    /// metadata, read by `session_write`'s `kage.session.entries`.
    session_entries: SharedSessionEntries,
    /// Pending `session_write` reseat request (`switch`/`fork_to`),
    /// drained by the host.
    switch_request: SharedSwitchRequest,
}

/// Plugin evaluation settings and per-plugin environments, shared with
/// the owner thread so loading and hot reload run there as one job.
pub(crate) struct EvalState {
    sink: SharedHostLog,
    /// Per-plugin `_ENV` tables, keyed by plugin name, held in the Lua
    /// registry. Each plugin re-evaluates against its own environment
    /// so plugins cannot see or clobber one another; granted
    /// capabilities are attached onto a plugin's own proxy here.
    plugin_envs: Arc<Mutex<HashMap<String, RegistryKey>>>,
    /// Name of the plugin currently being evaluated, so
    /// `kage.request_capabilities` knows who is asking.
    current_plugin: CurrentPlugin,
    /// Load allowlist by plugin file stem. When non-empty, the loader
    /// evaluates only the listed plugins and skips the rest; empty means
    /// load every discovered plugin.
    enabled: Vec<String>,
    /// Per-plugin settings by file stem, surfaced to the named plugin
    /// through `kage.plugin_config()`. Each plugin sees only its slice.
    plugin_config: BTreeMap<String, serde_json::Value>,
    /// Store directory backing `kage.store`. When set, each plugin gets a
    /// private `<state_dir>/<stem>.json` persisted across runs; when
    /// `None`, `kage.store` raises so misconfiguration is not silent.
    state_dir: Option<PathBuf>,
    /// VM instructions one host-driven plugin entry may execute before
    /// the watchdog aborts it. See [`crate::watchdog`].
    pub(crate) script_budget: u64,
    /// Source of `_defaults.lua`, evaluated before plugins on every load.
    defaults: &'static str,
    /// Trusted user config directory holding `init.lua` and `lua/`.
    /// `None` skips the user phase.
    pub(crate) user_dir: Option<PathBuf>,
    /// Capability installers, attached in full to the user environment.
    pub(crate) capabilities: CapabilityRegistry,
    /// The keymap table and what setting a mapping needs.
    pub(crate) keymaps: Keymaps,
    /// `[keybindings]` from `config.toml`, applied after the plugins on
    /// every load.
    pub(crate) keybindings: kage_core::config::KeybindingsConfig,
    /// Option store and highlight table, for the `color_scheme` fired
    /// at the end of every load.
    pub(crate) options: Options,
}

impl EvalState {
    pub(crate) fn sink(&self) -> &SharedHostLog {
        &self.sink
    }

    pub(crate) fn is_enabled(&self, stem: &str) -> bool {
        self.enabled.is_empty() || self.enabled.iter().any(|name| name == stem)
    }

    /// Evaluate `source` as plugin `name` in its own `_ENV`. See
    /// [`PluginRuntime::eval_plugin`].
    pub(crate) fn eval_plugin(
        &self,
        lua: &Lua,
        name: &str,
        source: &str,
    ) -> Result<mlua::Value, PluginError> {
        let env = self.env(lua, name)?;
        self.eval_in(lua, name, name, env, source)
    }

    /// Get or create the `_ENV` of plugin `name`. See [`plugin_env`].
    pub(crate) fn env(&self, lua: &Lua, name: &str) -> mlua::Result<Table> {
        let store_path = self
            .state_dir
            .as_deref()
            .map(|dir| store::store_path(dir, name));
        plugin_env(
            lua,
            name,
            &self.plugin_envs,
            self.plugin_config.get(name),
            store_path,
        )
    }

    /// Evaluate `source` in `env` under the watchdog, with `name` as the
    /// current plugin and `chunk` as the chunk name.
    pub(crate) fn eval_in(
        &self,
        lua: &Lua,
        name: &str,
        chunk: &str,
        env: Table,
        source: &str,
    ) -> Result<mlua::Value, PluginError> {
        *lock(&self.current_plugin) = Some(name.to_owned());
        let result = watchdog::run(lua, self.script_budget, || {
            lua.load(source)
                .set_name(chunk)
                .set_environment(env)
                .eval::<mlua::Value>()
        });
        *lock(&self.current_plugin) = None;
        result
    }

    /// Evaluate `_defaults.lua` in its own environment. Runs before the
    /// plugins on every load, so plugins and user config override it.
    pub(crate) fn eval_defaults(&self, lua: &Lua) -> Result<(), PluginError> {
        self.eval_plugin(lua, stdlib::DEFAULTS_ENV, self.defaults)
            .map(drop)
    }

    /// Drop every per-plugin environment so a reload is a clean slate:
    /// stale plugin globals do not survive, and a capability revoked in
    /// config is no longer attached to the old proxy.
    pub(crate) fn reset(&self, lua: &Lua) {
        for (_, key) in lock(&self.plugin_envs).drain() {
            let _ = lua.remove_registry_value(key);
        }
        *lock(&self.current_plugin) = None;
    }
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRuntime").finish_non_exhaustive()
    }
}

/// Default Lua memory ceiling for the runtime, in bytes.
pub const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// Builder for [`PluginRuntime`]. Lets the host inject a custom host-log
/// sink, a config snapshot, and the workdir that gates `kage.fs.*`.
pub struct PluginRuntimeBuilder {
    sink: SharedHostLog,
    config: serde_json::Value,
    workdir: PathBuf,
    capabilities: BTreeMap<String, Vec<String>>,
    credential_lookup: env::CredentialLookup,
    enabled: Vec<String>,
    plugin_config: BTreeMap<String, serde_json::Value>,
    state_dir: Option<PathBuf>,
    script_budget: u64,
    memory_limit: usize,
    defaults: &'static str,
    user_dir: Option<PathBuf>,
    options: SharedOptions,
    themes: Option<SharedThemeResolver>,
    keybindings: kage_core::config::KeybindingsConfig,
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
    // The whole `io` library. Files go through `kage.fs`.
    ("", "io"),
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
    // workdir; single-file plugins do not need it. The trusted user
    // environment gets a confined `require` instead (see crate::user).
    ("", "require"),
    ("", "package"),
    // Reflection: debug.getregistry reaches the shared handler
    // registry and debug.setupvalue can rewrite another function's
    // `_ENV`, either of which defeats per-plugin isolation.
    ("", "debug"),
    // `rawset` would bypass the `__newindex` guards that make the
    // shared tables read-only once `build` finishes.
    ("", "rawset"),
];

/// Get or create the dedicated `_ENV` table for plugin `name`.
///
/// The table reads through to a private copy of the shared, sandboxed
/// globals (standard library plus the base `kage` API) via an `__index`
/// metatable, but has no `__newindex`, so a plugin's own top-level
/// assignments are `rawset` into this table and stay private to it.
/// The copy covers two levels, so `kage.ui` and every other `kage.*`
/// sub-table is private too. `kage` is a per-plugin proxy over that
/// copy: reads fall through, and the capability tier attaches granted
/// APIs onto this proxy so they are visible only to the grantee. `_G`
/// is bound back to this table so `_G.x = ...` cannot reach the real
/// globals, and every metatable on the way is protected so
/// `getmetatable` cannot walk back to them either. The table is kept in
/// the Lua registry and reused for repeat evals of `name`.
fn plugin_env(
    lua: &Lua,
    name: &str,
    slots: &Mutex<HashMap<String, RegistryKey>>,
    config_slice: Option<&serde_json::Value>,
    store_path: Option<PathBuf>,
) -> mlua::Result<Table> {
    let mut slots = lock(slots);
    if let Some(key) = slots.get(name) {
        return lua.registry_value::<Table>(key);
    }
    let globals = lua.globals();
    let env = lua.create_table()?;
    // Per-plugin snapshot of the shared tables: a fresh copy per env,
    // so a plugin assigning e.g. `string.format` or `kage.ui.notify`
    // poisons only its own view, never another plugin or the host. The
    // snapshot falls back to the shared globals for everything it does
    // not carry (`print`, `pcall`, ...).
    let snapshot = lua.create_table()?;
    for shared in SHARED_TABLES {
        let src: Table = globals.get(*shared)?;
        snapshot.raw_set(*shared, copy_two_levels(lua, &src)?)?;
    }
    snapshot.set_metatable(Some(protected_index(lua, globals)?))?;
    env.set_metatable(Some(protected_index(lua, snapshot.clone())?))?;

    let base_kage: Table = snapshot.get("kage")?;
    let pkage = lua.create_table()?;
    pkage.set_metatable(Some(protected_index(lua, base_kage)?))?;
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

/// Copy `src` and every table directly inside it.
fn copy_two_levels(lua: &Lua, src: &Table) -> mlua::Result<Table> {
    let copy = lua.create_table()?;
    for entry in src.pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = entry?;
        let value = match value {
            mlua::Value::Table(inner) => {
                let inner_copy = lua.create_table()?;
                for entry in inner.pairs::<mlua::Value, mlua::Value>() {
                    let (k, v) = entry?;
                    inner_copy.raw_set(k, v)?;
                }
                mlua::Value::Table(inner_copy)
            }
            other => other,
        };
        copy.raw_set(key, value)?;
    }
    Ok(copy)
}

/// A metatable that reads through to `index` and hides itself from
/// `getmetatable` and `setmetatable`.
fn protected_index(lua: &Lua, index: Table) -> mlua::Result<Table> {
    let mt = lua.create_table()?;
    mt.raw_set("__index", index)?;
    mt.raw_set("__metatable", false)?;
    Ok(mt)
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
    guard_coroutines(lua)
}

/// Replace `coroutine.create` and `coroutine.wrap` so every coroutine a
/// plugin creates carries the watchdog hook.
///
/// Lua debug hooks are per-thread and are not inherited by coroutines,
/// so with the stock functions a plugin could escape its instruction
/// budget with `coroutine.wrap(function() while true do end end)()`,
/// hanging the owner thread permanently. The replacements install the
/// hook at creation time; every thread a plugin can resume reaches it
/// through `create`/`wrap` (bridged threads are hooked by
/// [`crate::LuaHost`]-level `bridge_call`, the main state by
/// [`watchdog::install`]). `wrap` is rebuilt over the hooked `create`
/// with resume semantics matching the stock function: results without
/// the leading boolean, errors propagated.
fn guard_coroutines(lua: &Lua) -> Result<(), PluginError> {
    let coroutine: Table = lua.globals().get("coroutine")?;
    let create_orig: mlua::Function = coroutine.get("create")?;
    let hooked_create = {
        let create_orig = create_orig.clone();
        lua.create_function(move |_, f: mlua::Function| {
            let thread: mlua::Thread = create_orig.call(f)?;
            watchdog::install_on_thread(&thread).map_err(mlua::Error::external)?;
            Ok(thread)
        })?
    };
    let hooked_wrap = {
        let hooked_create = hooked_create.clone();
        lua.create_function(move |lua, f: mlua::Function| {
            let thread: mlua::Thread = hooked_create.call(f)?;
            lua.create_function(
                move |_, args: mlua::MultiValue| -> mlua::Result<mlua::MultiValue> {
                    thread.resume(args)
                },
            )
        })?
    };
    coroutine.set("create", hooked_create)?;
    coroutine.set("wrap", hooked_wrap)?;
    Ok(())
}

/// Shared tables (standard library plus the base `kage` API) treated as
/// read-only. Two layers: [`plugin_env`] gives each plugin a private
/// two-level copy of these, so a mutating plugin poisons only itself,
/// and [`freeze_shared_tables`] stops further writes into the shared
/// originals and every table nested in them once `build` finishes (new
/// keys raise, metatable protected).
const SHARED_TABLES: &[&str] = &["string", "table", "math", "os", "coroutine", "utf8", "kage"];

/// Guard the shared originals, and every table nested inside them,
/// after all build-time installs: assignments of NEW keys raise, and a
/// protected `__metatable` stops `setmetatable` from swapping a table
/// out. Lua's `__newindex` does not fire for keys a table already has,
/// so plugin isolation does not rely on this layer: plugins only ever
/// hold copies (see [`plugin_env`]). The string metatable is protected
/// too, since its `__index` is the shared `string` table.
fn freeze_shared_tables(lua: &Lua) -> Result<(), PluginError> {
    let globals = lua.globals();
    let mut seen = std::collections::HashSet::new();
    for name in SHARED_TABLES {
        let table: Table = globals.get(*name)?;
        freeze(lua, &table, (*name).to_owned(), &mut seen)?;
    }
    let string_mt: Table = lua.load("return getmetatable('')").eval()?;
    string_mt.raw_set("__metatable", false)?;
    Ok(())
}

fn freeze(
    lua: &Lua,
    table: &Table,
    path: String,
    seen: &mut std::collections::HashSet<*const std::ffi::c_void>,
) -> Result<(), PluginError> {
    if !seen.insert(table.to_pointer()) {
        return Ok(());
    }
    for entry in table.pairs::<mlua::Value, mlua::Value>() {
        if let (mlua::Value::String(key), mlua::Value::Table(inner)) = entry? {
            freeze(lua, &inner, format!("{path}.{}", key.to_str()?), seen)?;
        }
    }
    let mt = lua.create_table()?;
    mt.set(
        "__newindex",
        lua.create_function(
            move |_, _: (mlua::Value, mlua::Value, mlua::Value)| -> mlua::Result<()> {
                Err(mlua::Error::external(format!(
                    "shared table '{path}' is read-only"
                )))
            },
        )?,
    )?;
    mt.set("__metatable", false)?;
    table.set_metatable(Some(mt))?;
    Ok(())
}

mod builder;
mod methods;

#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod tests;
