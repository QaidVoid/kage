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
//! The sandbox prevents accidental filesystem and process access from
//! plugin code; it is not a security boundary against actively malicious
//! Lua. Only run plugins you trust.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use mlua::Lua;

use crate::api::{self, SharedHostLog, default_host_log};
use crate::commands::{self, LuaCommand, RegisteredCommands, registered_commands};
use crate::error::PluginError;
use crate::events;
use crate::fs as plugin_fs;
use crate::http;
use crate::lifecycle::{
    self, SharedCompactRequest, SharedUsage, shared_compact_request, shared_usage,
};
use crate::providers::{self, LuaProvider, RegisteredProviders, registered_providers};
use crate::sessions::{
    self, SharedForkRequest, SharedSessionList, shared_fork_request, shared_session_list,
};
use crate::status::{self, SharedStatus, shared_status};
use crate::tools::{self, RegisteredTools, registered_tools};
use crate::widgets::{self, LuaWidget, RegisteredWidgets, registered_widgets};

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
    providers: RegisteredProviders,
    widgets: RegisteredWidgets,
    status: SharedStatus,
    usage: SharedUsage,
    compact_request: SharedCompactRequest,
    session_list: SharedSessionList,
    fork_request: SharedForkRequest,
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRuntime").finish_non_exhaustive()
    }
}

impl PluginRuntime {
    /// Build a runtime with default host log + empty config. Equivalent to
    /// `PluginRuntime::builder().build()`.
    pub fn new() -> Result<Self, PluginError> {
        Self::builder().build()
    }

    /// Begin configuring a runtime. The returned builder picks a default
    /// host log and empty config; either can be replaced before `build`.
    #[must_use]
    pub fn builder() -> PluginRuntimeBuilder {
        PluginRuntimeBuilder {
            sink: default_host_log(),
            config: serde_json::Value::Object(serde_json::Map::new()),
            workdir: PathBuf::from("."),
        }
    }

    /// Lock the underlying Lua state. Held only as long as the returned
    /// guard is alive; the Tool dispatch path uses this same lock so
    /// plugin-defined tools serialize against runtime calls.
    pub fn lock_lua(&self) -> MutexGuard<'_, Lua> {
        self.lua.lock().expect("plugin lua mutex poisoned")
    }

    /// Cloneable handle to the shared Lua state, for tool implementations
    /// that need to live independently of the runtime borrow.
    #[must_use]
    pub fn shared_lua(&self) -> SharedLua {
        Arc::clone(&self.lua)
    }

    /// Cloneable handle to the host log sink.
    #[must_use]
    pub fn sink(&self) -> SharedHostLog {
        Arc::clone(&self.sink)
    }

    /// Execute a chunk of Lua source against this runtime. Returns the
    /// chunk's return value as a Lua [`mlua::Value`].
    pub fn eval(&self, source: &str) -> Result<mlua::Value, PluginError> {
        let lua = self.lock_lua();
        Ok(lua.load(source).eval::<mlua::Value>()?)
    }

    /// Fire every handler subscribed to `event_name` with `payload`.
    pub fn dispatch_event(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<(), PluginError> {
        let lua = self.lock_lua();
        events::dispatch(&lua, event_name, payload, &self.sink)
    }

    /// Chain every handler subscribed to `event_name` and return the
    /// payload after the last handler ran. See [`events::dispatch_transform`]
    /// for the chaining semantics.
    pub fn dispatch_transform(
        &self,
        event_name: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let lua = self.lock_lua();
        events::dispatch_transform(&lua, event_name, payload, &self.sink)
    }

    /// Poll handlers subscribed to `event_name`; return `true` as soon as
    /// one returns a truthy value. See [`events::dispatch_predicate`].
    pub fn dispatch_predicate(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<bool, PluginError> {
        let lua = self.lock_lua();
        events::dispatch_predicate(&lua, event_name, payload, &self.sink)
    }

    /// Consult handlers subscribed to a session-op event. The first
    /// handler that returns a cancel or patch decision short-circuits the
    /// chain. See [`events::dispatch_session_op`].
    pub fn dispatch_session_op(
        &self,
        event_name: &str,
        target: &str,
    ) -> Result<events::SessionOpDecision, PluginError> {
        let lua = self.lock_lua();
        events::dispatch_session_op(&lua, event_name, target, &self.sink)
    }

    /// Number of handlers subscribed to `event_name`.
    #[must_use]
    pub fn handler_count(&self, event_name: &str) -> usize {
        let lua = self.lock_lua();
        events::handler_count(&lua, event_name)
    }

    /// Snapshot the tools registered by plugins so far. Each call returns
    /// a fresh `Vec`; the underlying `Arc<dyn Tool>` entries are shared
    /// with the runtime's internal registry.
    #[must_use]
    pub fn registered_tools(&self) -> Vec<Arc<dyn kage_tools::Tool>> {
        self.tools
            .lock()
            .expect("plugin tools mutex poisoned")
            .clone()
    }

    /// Snapshot the tool overrides registered by plugins via
    /// `kage.override_tool`. The host applies these after built-ins
    /// and `register_tool` entries; an override that names a tool not
    /// present at apply time logs a warning instead of crashing.
    #[must_use]
    pub fn registered_tool_overrides(&self) -> Vec<Arc<dyn kage_tools::Tool>> {
        self.tool_overrides
            .lock()
            .expect("plugin tool overrides mutex poisoned")
            .clone()
    }

    /// Snapshot the slash commands registered by plugins so far.
    #[must_use]
    pub fn registered_commands(&self) -> Vec<Arc<LuaCommand>> {
        self.commands
            .lock()
            .expect("plugin commands mutex poisoned")
            .clone()
    }

    /// Snapshot the providers registered by plugins so far.
    #[must_use]
    pub fn registered_providers(&self) -> Vec<Arc<LuaProvider>> {
        self.providers
            .lock()
            .expect("plugin providers mutex poisoned")
            .clone()
    }

    /// Snapshot the status-bar widgets registered by plugins so far.
    /// Each call returns a fresh `Vec`; the underlying [`LuaWidget`]s
    /// are reference-counted and share their Lua handler across clones.
    #[must_use]
    pub fn registered_widgets(&self) -> Vec<Arc<LuaWidget>> {
        self.widgets
            .lock()
            .expect("plugin widgets mutex poisoned")
            .clone()
    }

    /// Snapshot the transient status map populated by
    /// `kage.set_status`. Entries are returned in key-sorted order so
    /// the status bar paints deterministically across redraws.
    #[must_use]
    pub fn status_snapshot(&self) -> Vec<(String, String)> {
        self.status
            .lock()
            .expect("plugin status mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Cloneable handle to the live status map. Hosts that want to
    /// re-snapshot every redraw without going through
    /// [`Self::status_snapshot`] (one allocation each) can hold this.
    #[must_use]
    pub fn shared_status(&self) -> SharedStatus {
        Arc::clone(&self.status)
    }

    /// Cloneable handle to the per-turn usage snapshot. The host
    /// updates the inner value after every assistant turn; plugins
    /// read it via `kage.context_usage()`.
    #[must_use]
    pub fn shared_usage(&self) -> SharedUsage {
        Arc::clone(&self.usage)
    }

    /// Replace the current usage snapshot. Convenience wrapper around
    /// locking [`Self::shared_usage`] and assigning.
    pub fn set_usage(&self, usage: serde_json::Value) {
        if let Ok(mut slot) = self.usage.lock() {
            *slot = usage;
        }
    }

    /// Cloneable handle to the pending-compact slot.
    #[must_use]
    pub fn shared_compact_request(&self) -> SharedCompactRequest {
        Arc::clone(&self.compact_request)
    }

    /// Drain the pending compact request if any. The host calls this
    /// between turns; `Some(prompt)` means a plugin asked for a
    /// compaction and the host should run one.
    #[must_use]
    pub fn take_compact_request(&self) -> Option<String> {
        self.compact_request
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Cloneable handle to the session-list snapshot.
    #[must_use]
    pub fn shared_session_list(&self) -> SharedSessionList {
        Arc::clone(&self.session_list)
    }

    /// Replace the current session list. The host typically refreshes
    /// this from its session lister on a redraw cadence.
    pub fn set_session_list(&self, entries: Vec<serde_json::Value>) {
        if let Ok(mut slot) = self.session_list.lock() {
            *slot = entries;
        }
    }

    /// Cloneable handle to the pending-fork slot.
    #[must_use]
    pub fn shared_fork_request(&self) -> SharedForkRequest {
        Arc::clone(&self.fork_request)
    }

    /// Drain the pending fork request. `Some(at)` means the plugin
    /// asked for a fork at entry `at` (empty string == "latest"); the
    /// host should run a fork and create a new session file.
    #[must_use]
    pub fn take_fork_request(&self) -> Option<String> {
        self.fork_request
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Drop every registration that came from plugins (event handlers,
    /// tools, commands, providers) and replay every `*.lua` file in
    /// `dir`. Designed for hot reload between turns: a stale plugin
    /// snapshot does not survive after this call.
    ///
    /// Tools, commands, and providers that the host has already handed
    /// to other registries via [`Self::registered_tools`] etc. continue
    /// to exist; this method only clears the runtime's own snapshot.
    /// The host is responsible for re-publishing the new snapshot.
    pub fn reload_dir(
        &self,
        dir: &std::path::Path,
    ) -> Result<crate::loader::LoadReport, PluginError> {
        {
            let lua = self.lock_lua();
            let handlers: mlua::Table = lua.named_registry_value("kage._handlers")?;
            handlers.clear()?;
        }
        self.tools
            .lock()
            .expect("plugin tools mutex poisoned")
            .clear();
        self.tool_overrides
            .lock()
            .expect("plugin tool overrides mutex poisoned")
            .clear();
        self.widgets
            .lock()
            .expect("plugin widgets mutex poisoned")
            .clear();
        self.status
            .lock()
            .expect("plugin status mutex poisoned")
            .clear();
        self.commands
            .lock()
            .expect("plugin commands mutex poisoned")
            .clear();
        self.providers
            .lock()
            .expect("plugin providers mutex poisoned")
            .clear();
        crate::loader::load_dir(dir, self)
    }
}

/// Builder for [`PluginRuntime`]. Lets the host inject a custom host-log
/// sink, a config snapshot, and the workdir that gates `kage.fs.*`.
pub struct PluginRuntimeBuilder {
    sink: SharedHostLog,
    config: serde_json::Value,
    workdir: PathBuf,
}

impl std::fmt::Debug for PluginRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRuntimeBuilder")
            .field("config", &self.config)
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl PluginRuntimeBuilder {
    /// Replace the default [`crate::api::HostLog`] sink.
    #[must_use]
    pub fn sink(mut self, sink: SharedHostLog) -> Self {
        self.sink = sink;
        self
    }

    /// Replace the value returned by `kage.config()` in plugins.
    #[must_use]
    pub fn config(mut self, config: serde_json::Value) -> Self {
        self.config = config;
        self
    }

    /// Set the workdir that `kage.fs.*` helpers anchor at. All paths the
    /// plugin passes are resolved through `kage_tools::resolve_under` with
    /// this root.
    #[must_use]
    pub fn workdir(mut self, workdir: PathBuf) -> Self {
        self.workdir = workdir;
        self
    }

    /// Finalize the runtime: build the Lua state, apply sandbox removals,
    /// install the `kage` API table, wire `kage.on`,
    /// `kage.register_tool`, `kage.register_command`,
    /// `kage.register_provider`, and `kage.fs.*`.
    pub fn build(self) -> Result<PluginRuntime, PluginError> {
        let lua = Lua::new();
        apply_sandbox(&lua)?;
        api::install(&lua, self.sink.clone(), self.config)?;
        events::install_subscriptions(&lua)?;
        plugin_fs::install_fs(&lua, self.workdir.clone())?;
        http::install_http(&lua)?;
        let shared_lua: SharedLua = Arc::new(Mutex::new(lua));
        let tool_registry = registered_tools();
        let tool_override_registry = registered_tools();
        let command_registry = registered_commands();
        let provider_registry = registered_providers();
        let widget_registry = registered_widgets();
        let status_map = shared_status();
        let usage_snapshot = shared_usage();
        let compact_slot = shared_compact_request();
        let session_list_slot = shared_session_list();
        let fork_slot = shared_fork_request();
        {
            let lua_guard = shared_lua.lock().expect("plugin lua mutex poisoned");
            tools::install_register_tool(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&tool_registry),
            )?;
            tools::install_override_tool(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&tool_override_registry),
            )?;
            commands::install_register_command(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&command_registry),
            )?;
            providers::install_register_provider(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&provider_registry),
            )?;
            widgets::install_register_widget(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&widget_registry),
            )?;
            status::install_status(&lua_guard, Arc::clone(&status_map))?;
            lifecycle::install_lifecycle(
                &lua_guard,
                Arc::clone(&usage_snapshot),
                Arc::clone(&compact_slot),
            )?;
            sessions::install_sessions(
                &lua_guard,
                Arc::clone(&session_list_slot),
                Arc::clone(&fork_slot),
            )?;
        }
        Ok(PluginRuntime {
            lua: shared_lua,
            sink: self.sink,
            tools: tool_registry,
            tool_overrides: tool_override_registry,
            commands: command_registry,
            providers: provider_registry,
            widgets: widget_registry,
            status: status_map,
            usage: usage_snapshot,
            compact_request: compact_slot,
            session_list: session_list_slot,
            fork_request: fork_slot,
        })
    }
}

/// Pairs of `(table_path, key)` removed from the standard library on
/// runtime construction. `table_path` is dot-separated starting from the
/// globals table; an empty path means "drop the global by `key`".
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
];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_strips_dangerous_globals() {
        let rt = PluginRuntime::new().unwrap();
        for (path, key) in SANDBOX_REMOVALS {
            let chunk = if path.is_empty() {
                format!("return {key} == nil")
            } else {
                format!("return {path} == nil or {path}.{key} == nil")
            };
            let lua = rt.lock_lua();
            let v: bool = lua.load(&chunk).eval().unwrap_or(false);
            assert!(v, "sandbox failed to remove {path}.{key}");
        }
    }

    #[test]
    fn benign_library_functions_still_work() {
        let rt = PluginRuntime::new().unwrap();
        let lua = rt.lock_lua();
        let v: i64 = lua.load("return string.len('hello')").eval().unwrap();
        assert_eq!(v, 5);
        let v: f64 = lua.load("return math.sqrt(81)").eval().unwrap();
        assert!((v - 9.0).abs() < 1e-9);
    }

    #[test]
    fn os_execute_call_errors_after_sandboxing() {
        let rt = PluginRuntime::new().unwrap();
        let lua = rt.lock_lua();
        let res: Result<mlua::Value, _> = lua.load("return os.execute('echo hi')").eval();
        assert!(res.is_err());
    }

    #[test]
    fn dofile_and_loadfile_are_unreachable() {
        let rt = PluginRuntime::new().unwrap();
        let lua = rt.lock_lua();
        for chunk in ["dofile('/etc/passwd')", "loadfile('/etc/passwd')"] {
            let res: Result<mlua::Value, _> = lua.load(chunk).eval();
            assert!(res.is_err(), "expected error from {chunk}");
        }
    }

    #[test]
    fn eval_returns_lua_values() {
        let rt = PluginRuntime::new().unwrap();
        let v: mlua::Value = rt.eval("return 21 * 2").unwrap();
        assert_eq!(v.as_integer(), Some(42));
    }

    #[test]
    fn reload_dir_clears_prior_registrations() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.lua"),
            "kage.register_command({ name='a', description='', handler=function() end })",
        )
        .unwrap();
        let rt = PluginRuntime::new().unwrap();
        rt.reload_dir(dir.path()).unwrap();
        assert_eq!(rt.registered_commands().len(), 1);

        // Replace the plugin with one that registers a different command.
        fs::write(
            dir.path().join("a.lua"),
            "kage.register_command({ name='b', description='', handler=function() end })",
        )
        .unwrap();
        rt.reload_dir(dir.path()).unwrap();
        let cmds = rt.registered_commands();
        assert_eq!(cmds.len(), 1, "old registration should not survive");
        assert_eq!(cmds[0].name(), "b");
    }
}
