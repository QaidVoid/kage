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

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use mlua::{Lua, RegistryKey, Table};

use crate::acp::{self, SharedAcpAgents, shared_acp_agents};
use crate::api::{self, SharedHostLog, default_host_log};
use crate::autocomplete::{
    self, LuaAutocompleteProvider, RegisteredAutocompleteProviders,
    registered_autocomplete_providers,
};
use crate::block_renderers::{
    self, LuaBlockRenderer, SharedBlockRenderers, shared_block_renderers,
};
use crate::bridge::{self, BridgeStep, SharedBridge, shared_bridge};
use crate::capabilities::{self, CurrentPlugin};
use crate::chrome::{self, LuaChrome, SharedChrome, shared_chrome};
use crate::commands::{self, LuaCommand, RegisteredCommands, registered_commands};
use crate::error::PluginError;
use crate::events;
use crate::exec;
use crate::fs as plugin_fs;
use crate::http;
use crate::keybindings::{self, RegisteredKeybindings, registered_keybindings};
use crate::lifecycle::{
    self, SharedCompactRequest, SharedUsage, shared_compact_request, shared_usage,
};
use crate::mcp::{
    self, SharedMcpRestart, SharedMcpServers, shared_mcp_restart, shared_mcp_servers,
};
use crate::messages::{self, PendingMessage, SharedPendingMessages, shared_pending_messages};
use crate::providers::{self, LuaProvider, RegisteredProviders, registered_providers};
use crate::session_write::{self, SharedSessionEntries, SharedSwitchRequest, SwitchTarget};
use crate::sessions::{
    self, PendingSessionOp, SharedForkRequest, SharedSessionList, SharedSessionOps,
    shared_fork_request, shared_session_list, shared_session_ops,
};
use crate::status::{self, SharedStatus, shared_status};
use crate::terminal_input::{self, RegisteredTerminalHooks, registered_terminal_hooks};
use crate::theme::{
    self, SharedThemeRequest, SharedThemeState, shared_theme_request, shared_theme_state,
};
use crate::tools::{self, RegisteredTools, registered_tools};
use crate::ui;
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
            capabilities: BTreeMap::new(),
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

    /// Execute a chunk of Lua source against the shared globals.
    ///
    /// Used by the host for one-off evaluation and by tests. Plugin
    /// files are loaded through [`eval_plugin`](Self::eval_plugin)
    /// instead, so their top-level definitions stay private.
    pub fn eval(&self, source: &str) -> Result<mlua::Value, PluginError> {
        let lua = self.lock_lua();
        Ok(lua.load(source).eval::<mlua::Value>()?)
    }

    /// Evaluate a plugin source chunk in its own `_ENV`.
    ///
    /// Top-level definitions land in a per-plugin environment instead
    /// of the shared globals, so two plugins cannot see or overwrite
    /// each other. Reads fall through to the shared, sandboxed
    /// standard library and the base `kage` API; `kage` is a per-plugin
    /// proxy the capability tier later extends. The host loader calls
    /// this for every `*.lua` file with the file stem as `name`; the
    /// environment is created once per name and reused.
    pub fn eval_plugin(&self, name: &str, source: &str) -> Result<mlua::Value, PluginError> {
        let lua = self.lock_lua();
        let env = plugin_env(&lua, name, &self.plugin_envs)?;
        if let Ok(mut cur) = self.current_plugin.lock() {
            *cur = Some(name.to_owned());
        }
        let result = lua
            .load(source)
            .set_name(name)
            .set_environment(env)
            .eval::<mlua::Value>();
        if let Ok(mut cur) = self.current_plugin.lock() {
            *cur = None;
        }
        Ok(result?)
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

    /// Fire every `resources_discover` handler and collect the aggregated
    /// directory paths. See [`events::dispatch_resources_discover`].
    pub fn discover_resources(&self) -> Result<events::DiscoveryEntries, PluginError> {
        let lua = self.lock_lua();
        events::dispatch_resources_discover(&lua, &self.sink)
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

    /// Snapshot the commands plugins registered via
    /// `kage.override_command`. The host lets these shadow a built-in
    /// of the same name and dispatches them ahead of it.
    #[must_use]
    pub fn registered_command_overrides(&self) -> Vec<Arc<LuaCommand>> {
        self.command_overrides
            .lock()
            .expect("plugin command overrides mutex poisoned")
            .clone()
    }

    /// Snapshot the keybindings registered by plugins so far. Each
    /// entry pairs a canonical chord with a bridged handler; the host
    /// matches chords against terminal key events.
    #[must_use]
    pub fn registered_keybindings(&self) -> Vec<Arc<crate::keybindings::LuaKeybinding>> {
        self.keybindings
            .lock()
            .expect("plugin keybindings mutex poisoned")
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

    /// Snapshot the ACP agents plugins declared via
    /// `kage.acp.add_agent`. The host merges these with
    /// `[acp.agents.*]` from config.
    #[must_use]
    pub fn registered_acp_agents(&self) -> Vec<(String, kage_core::config::AcpAgent)> {
        self.acp_agents
            .lock()
            .expect("plugin acp agents mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Snapshot the MCP servers plugins declared via
    /// `kage.mcp.add_server`. The host merges these with
    /// `[mcp.servers.*]` from config.
    #[must_use]
    pub fn registered_mcp_servers(&self) -> Vec<(String, kage_core::config::McpServer)> {
        self.mcp_servers
            .lock()
            .expect("plugin mcp servers mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Drain the MCP server names a plugin asked to restart via
    /// `kage.mcp.restart`. The host applies each against the live
    /// manager between turns; an unknown name surfaces as an error
    /// there rather than failing silently.
    #[must_use]
    pub fn take_mcp_restarts(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .mcp_restart
                .lock()
                .expect("plugin mcp restart mutex poisoned"),
        )
    }

    /// Consult the plugin's `kage.on_acp_permission` handler for an
    /// upstream agent's tool-call ask. `Some(true)` allow,
    /// `Some(false)` explicit deny, `None` no handler (host default).
    #[must_use]
    pub fn acp_permission(&self, payload: &serde_json::Value) -> Option<bool> {
        acp::decide(&self.lock_lua(), payload)
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

    /// Cloneable handle to the session-entries snapshot the
    /// `session_write` `kage.session.entries` reads.
    #[must_use]
    pub fn shared_session_entries(&self) -> SharedSessionEntries {
        Arc::clone(&self.session_entries)
    }

    /// Replace the session-entries snapshot. The host refreshes this
    /// from the active session file on its redraw / between-turn
    /// cadence, like [`set_session_list`](Self::set_session_list).
    pub fn set_session_entries(&self, entries: Vec<serde_json::Value>) {
        if let Ok(mut slot) = self.session_entries.lock() {
            *slot = entries;
        }
    }

    /// Cloneable handle to the pending `session_write` reseat slot, so
    /// the host can drain it on its event-loop cadence the same way it
    /// drains [`shared_fork_request`](Self::shared_fork_request).
    #[must_use]
    pub fn shared_switch_request(&self) -> SharedSwitchRequest {
        Arc::clone(&self.switch_request)
    }

    /// Drain the pending `session_write` reseat request. The host
    /// applies it between turns - resuming the named session, or
    /// landing on the fork a `fork_to` just queued - after consulting
    /// the `session_before_switch` veto.
    #[must_use]
    pub fn take_switch_request(&self) -> Option<SwitchTarget> {
        self.switch_request
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Cloneable handle to the theme snapshot. The host overwrites
    /// `current` / `available` on its redraw cadence so
    /// `kage.theme.current()` and `kage.theme.list()` stay fresh.
    #[must_use]
    pub fn shared_theme_state(&self) -> SharedThemeState {
        Arc::clone(&self.theme_state)
    }

    /// Cloneable handle to the pending theme-switch slot, for a host
    /// that drains it on its own (UI) thread rather than via
    /// [`Self::take_theme_request`].
    #[must_use]
    pub fn shared_theme_request(&self) -> SharedThemeRequest {
        Arc::clone(&self.theme_request)
    }

    /// Drain a pending `kage.theme.set` request. `Some(name)` means
    /// the host should validate `name` and switch to it.
    #[must_use]
    pub fn take_theme_request(&self) -> Option<String> {
        self.theme_request
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Snapshot the renderer a plugin installed via
    /// `kage.ui.set_header`, if any. The host calls
    /// [`LuaChrome::render`] on it once per redraw to paint the top
    /// chrome row; `None` means paint the built-in status bar.
    #[must_use]
    pub fn header_chrome(&self) -> Option<Arc<LuaChrome>> {
        self.header
            .lock()
            .expect("plugin header chrome mutex poisoned")
            .clone()
    }

    /// Snapshot the renderer a plugin installed via
    /// `kage.ui.set_footer`, if any. The host calls
    /// [`LuaChrome::render`] on it once per redraw to paint the bottom
    /// chrome row; `None` means paint the built-in modeline.
    #[must_use]
    pub fn footer_chrome(&self) -> Option<Arc<LuaChrome>> {
        self.footer
            .lock()
            .expect("plugin footer chrome mutex poisoned")
            .clone()
    }

    /// Snapshot the custom block renderers plugins installed via
    /// `kage.register_block_renderer`. The host registers each into
    /// the TUI block-renderer registry; an empty result means every
    /// custom block uses the built-in card.
    #[must_use]
    pub fn registered_block_renderers(&self) -> Vec<Arc<LuaBlockRenderer>> {
        self.block_renderers
            .lock()
            .expect("plugin block renderers mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Cloneable handle to the header-chrome slot, for a host that
    /// snapshots it per redraw (so a `kage.ui.set_header` call made
    /// after startup, e.g. from a command, takes effect) rather than
    /// reading a one-time [`Self::header_chrome`].
    #[must_use]
    pub fn shared_header(&self) -> SharedChrome {
        Arc::clone(&self.header)
    }

    /// Cloneable handle to the footer-chrome slot. See
    /// [`Self::shared_header`].
    #[must_use]
    pub fn shared_footer(&self) -> SharedChrome {
        Arc::clone(&self.footer)
    }

    /// Snapshot the autocomplete providers registered via
    /// `kage.add_autocomplete_provider`, in registration order. The
    /// host consults them in reverse order (last registered first) and
    /// calls [`LuaAutocompleteProvider::complete`] on each as the
    /// prompt input changes.
    #[must_use]
    pub fn registered_autocomplete_providers(&self) -> Vec<Arc<LuaAutocompleteProvider>> {
        self.autocomplete
            .lock()
            .expect("plugin autocomplete mutex poisoned")
            .clone()
    }

    /// Cloneable handle to the raw terminal-input hook list from
    /// `kage.on_terminal_input`. The host snapshots it before each
    /// keystroke so a runtime `off` or late registration is honored.
    #[must_use]
    pub fn shared_terminal_hooks(&self) -> RegisteredTerminalHooks {
        Arc::clone(&self.terminal_hooks)
    }

    /// Snapshot the active terminal-input hooks, in registration
    /// order.
    #[must_use]
    pub fn registered_terminal_hooks(&self) -> Vec<Arc<crate::terminal_input::LuaTerminalHook>> {
        self.terminal_hooks
            .lock()
            .expect("plugin terminal hooks mutex poisoned")
            .clone()
    }

    /// Cloneable handle to the queue of plugin-supplied messages.
    /// Hosts that want to sample the queue without consuming it (for
    /// diagnostics) hold onto this; production drain goes through
    /// [`Self::take_pending_messages`].
    #[must_use]
    pub fn shared_pending_messages(&self) -> SharedPendingMessages {
        Arc::clone(&self.pending_messages)
    }

    /// Drain every queued message. Called by the host between turns;
    /// returns the entries in submission order so a `send_message`
    /// chain reads naturally.
    #[must_use]
    pub fn take_pending_messages(&self) -> Vec<PendingMessage> {
        self.pending_messages
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    /// Cloneable handle to the queue of plugin-requested session
    /// writes. The host drains this through
    /// [`Self::take_pending_session_ops`] between turns and applies
    /// each entry to its session writer.
    #[must_use]
    pub fn shared_session_ops(&self) -> SharedSessionOps {
        Arc::clone(&self.session_ops)
    }

    /// Drain every queued session op (`append_entry` / `set_label`)
    /// in submission order. Empty when no plugin wrote anything since
    /// the last drain.
    #[must_use]
    pub fn take_pending_session_ops(&self) -> Vec<PendingSessionOp> {
        self.session_ops
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    /// Run `func` inside a fresh plugin coroutine with `args` as its
    /// positional arguments. Returns [`BridgeStep::Done`] if it ran to
    /// completion, or [`BridgeStep::Suspended`] if it called a blocking
    /// API (`kage._suspend`); in the latter case the coroutine is
    /// parked until [`Self::bridge_resume`] / [`Self::bridge_cancel`] /
    /// [`Self::bridge_abort`].
    ///
    /// Fails with [`PluginError::BridgeBusy`] if another coroutine is
    /// already parked. The caller must not hold [`Self::lock_lua`] when
    /// calling this; the bridge takes the lock itself.
    pub fn bridge_call(
        &self,
        func: &mlua::Function,
        args: &[serde_json::Value],
    ) -> Result<BridgeStep, PluginError> {
        let mut slot = self.bridge.lock().expect("plugin bridge mutex poisoned");
        if slot.is_some() {
            return Err(PluginError::BridgeBusy);
        }
        let lua = self.lock_lua();
        let thread = lua.create_thread(func.clone())?;
        let resume_args = bridge::args_to_multi(&lua, args)?;
        bridge::step(thread, resume_args, &mut slot)
    }

    /// Resume the parked coroutine, delivering `result` as the return
    /// value of the blocking call that suspended it. Returns the next
    /// step (done or suspended again).
    pub fn bridge_resume(&self, result: &serde_json::Value) -> Result<BridgeStep, PluginError> {
        let mut slot = self.bridge.lock().expect("plugin bridge mutex poisoned");
        let thread = slot.take().ok_or(PluginError::BridgeIdle)?;
        let lua = self.lock_lua();
        let resume_args = bridge::args_to_multi(&lua, std::slice::from_ref(result))?;
        bridge::step(thread, resume_args, &mut slot)
    }

    /// Resume the parked coroutine signalling the host action was
    /// cancelled. The blocking call returns `nil` to the plugin (the
    /// PE.B dialog contract for "user dismissed").
    pub fn bridge_cancel(&self) -> Result<BridgeStep, PluginError> {
        let mut slot = self.bridge.lock().expect("plugin bridge mutex poisoned");
        let thread = slot.take().ok_or(PluginError::BridgeIdle)?;
        let _lua = self.lock_lua();
        bridge::step(thread, mlua::MultiValue::new(), &mut slot)
    }

    /// Abandon the parked coroutine without resuming it (hard cancel,
    /// e.g. the run was aborted while a dialog was open). Returns
    /// `true` if a coroutine was actually dropped. Idempotent.
    #[must_use = "the boolean reports whether a coroutine was dropped; \
                  discard with `let _ =` if only the side effect matters"]
    pub fn bridge_abort(&self) -> bool {
        self.bridge
            .lock()
            .expect("plugin bridge mutex poisoned")
            .take()
            .is_some()
    }

    /// `true` while a bridged coroutine is parked awaiting a host
    /// action.
    #[must_use]
    pub fn bridge_is_suspended(&self) -> bool {
        self.bridge.lock().is_ok_and(|slot| slot.is_some())
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
        self.command_overrides
            .lock()
            .expect("plugin command overrides mutex poisoned")
            .clear();
        self.providers
            .lock()
            .expect("plugin providers mutex poisoned")
            .clear();
        self.keybindings
            .lock()
            .expect("plugin keybindings mutex poisoned")
            .clear();
        if let Ok(mut q) = self.pending_messages.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.session_ops.lock() {
            q.clear();
        }
        if let Ok(mut parked) = self.bridge.lock() {
            *parked = None;
        }
        if let Ok(mut slot) = self.theme_request.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.header.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.footer.lock() {
            *slot = None;
        }
        if let Ok(mut map) = self.block_renderers.lock() {
            map.clear();
        }
        self.autocomplete
            .lock()
            .expect("plugin autocomplete mutex poisoned")
            .clear();
        self.terminal_hooks
            .lock()
            .expect("plugin terminal hooks mutex poisoned")
            .clear();
        // Drop every per-plugin environment so a reload is a clean
        // slate: stale plugin globals do not survive, and a capability
        // revoked in config is no longer attached to the old proxy.
        // Lock lua before plugin_envs to match `eval_plugin`'s order.
        {
            let lua = self.lock_lua();
            let mut envs = self
                .plugin_envs
                .lock()
                .expect("plugin env map mutex poisoned");
            for (_, key) in envs.drain() {
                let _ = lua.remove_registry_value(key);
            }
        }
        if let Ok(mut cur) = self.current_plugin.lock() {
            *cur = None;
        }
        crate::loader::load_dir(dir, self)
    }
}

/// Builder for [`PluginRuntime`]. Lets the host inject a custom host-log
/// sink, a config snapshot, and the workdir that gates `kage.fs.*`.
pub struct PluginRuntimeBuilder {
    sink: SharedHostLog,
    config: serde_json::Value,
    workdir: PathBuf,
    capabilities: BTreeMap<String, Vec<String>>,
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

    /// Set the per-plugin capability grants (from
    /// `[plugins.capabilities]`), keyed by plugin file stem. Unknown
    /// capability names are rejected by [`build`](Self::build).
    #[must_use]
    pub fn capabilities(mut self, capabilities: BTreeMap<String, Vec<String>>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Finalize the runtime: build the Lua state, apply sandbox removals,
    /// install the `kage` API table, wire `kage.on`,
    /// `kage.register_tool`, `kage.register_command`,
    /// `kage.register_provider`, and `kage.fs.*`.
    #[allow(clippy::too_many_lines)]
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
        let command_override_registry = registered_commands();
        let provider_registry = registered_providers();
        let widget_registry = registered_widgets();
        let status_map = shared_status();
        let acp_agents = shared_acp_agents();
        let mcp_servers = shared_mcp_servers();
        let mcp_restart = shared_mcp_restart();
        let usage_snapshot = shared_usage();
        let compact_slot = shared_compact_request();
        let session_list_slot = shared_session_list();
        let fork_slot = shared_fork_request();
        let session_ops_slot = shared_session_ops();
        let pending_messages_slot = shared_pending_messages();
        let bridge_slot = shared_bridge();
        let keybinding_registry = registered_keybindings();
        let theme_state_slot = shared_theme_state();
        let theme_request_slot = shared_theme_request();
        let header_slot = shared_chrome();
        let footer_slot = shared_chrome();
        let block_renderer_map = shared_block_renderers();
        let autocomplete_registry = registered_autocomplete_providers();
        let terminal_hook_registry = registered_terminal_hooks();
        let plugin_envs: Arc<Mutex<HashMap<String, RegistryKey>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let current_plugin: CurrentPlugin = Arc::new(Mutex::new(None));
        let grants = Arc::new(capabilities::parse_grants(&self.capabilities)?);
        let cap_registry = capabilities::capability_registry();
        let session_entries = session_write::shared_session_entries();
        let switch_request = session_write::shared_switch_request();
        session_write::register(
            &cap_registry,
            Arc::clone(&session_entries),
            Arc::clone(&switch_request),
        );
        exec::register(&cap_registry, self.workdir.clone());
        {
            let lua_guard = shared_lua.lock().expect("plugin lua mutex poisoned");
            bridge::install_suspend(&lua_guard)?;
            capabilities::install_request_capabilities(
                &lua_guard,
                Arc::clone(&current_plugin),
                Arc::clone(&grants),
                Arc::clone(&plugin_envs),
                Arc::clone(&cap_registry),
            )?;
            ui::install_ui(&lua_guard)?;
            keybindings::install_register_keybinding(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&keybinding_registry),
            )?;
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
            commands::install_override_command(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&command_override_registry),
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
            acp::install_acp(&lua_guard, Arc::clone(&acp_agents))?;
            mcp::install_mcp(
                &lua_guard,
                Arc::clone(&mcp_servers),
                Arc::clone(&mcp_restart),
            )?;
            lifecycle::install_lifecycle(
                &lua_guard,
                Arc::clone(&usage_snapshot),
                Arc::clone(&compact_slot),
            )?;
            sessions::install_sessions(
                &lua_guard,
                Arc::clone(&session_list_slot),
                Arc::clone(&fork_slot),
                Arc::clone(&session_ops_slot),
            )?;
            messages::install_send_message(&lua_guard, Arc::clone(&pending_messages_slot))?;
            theme::install_theme(
                &lua_guard,
                Arc::clone(&theme_state_slot),
                Arc::clone(&theme_request_slot),
            )?;
            chrome::install_chrome(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&header_slot),
                Arc::clone(&footer_slot),
            )?;
            block_renderers::install_block_renderers(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&block_renderer_map),
            )?;
            autocomplete::install_add_autocomplete_provider(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&autocomplete_registry),
            )?;
            terminal_input::install_on_terminal_input(
                &lua_guard,
                Arc::clone(&shared_lua),
                self.sink.clone(),
                Arc::clone(&terminal_hook_registry),
            )?;
        }
        Ok(PluginRuntime {
            lua: shared_lua,
            sink: self.sink,
            tools: tool_registry,
            tool_overrides: tool_override_registry,
            commands: command_registry,
            command_overrides: command_override_registry,
            providers: provider_registry,
            widgets: widget_registry,
            status: status_map,
            acp_agents,
            mcp_servers,
            mcp_restart,
            usage: usage_snapshot,
            compact_request: compact_slot,
            session_list: session_list_slot,
            fork_request: fork_slot,
            session_ops: session_ops_slot,
            pending_messages: pending_messages_slot,
            bridge: bridge_slot,
            keybindings: keybinding_registry,
            theme_state: theme_state_slot,
            theme_request: theme_request_slot,
            header: header_slot,
            footer: footer_slot,
            block_renderers: block_renderer_map,
            autocomplete: autocomplete_registry,
            terminal_hooks: terminal_hook_registry,
            plugin_envs,
            current_plugin,
            session_entries,
            switch_request,
        })
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

    #[test]
    fn eval_plugin_isolates_globals_between_plugins() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval_plugin("a", "shared = 'from-a'").unwrap();
        // A second plugin must not see plugin a's top-level global.
        let v = rt.eval_plugin("b", "return shared").unwrap();
        assert!(v.is_nil(), "plugin b saw plugin a's global: {v:?}");
        // Nor does it leak into the shared globals the host evals on.
        assert!(rt.eval("return shared").unwrap().is_nil());
    }

    #[test]
    fn eval_plugin_reuses_one_env_per_name() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval_plugin("p", "counter = 1").unwrap();
        let v = rt
            .eval_plugin("p", "counter = counter + 1; return counter")
            .unwrap();
        assert_eq!(v.as_integer(), Some(2), "same name must reuse its env");
        let other = rt.eval_plugin("q", "return counter").unwrap();
        assert!(other.is_nil(), "a different plugin must get a fresh env");
    }

    #[test]
    fn eval_plugin_closes_global_escapes() {
        let rt = PluginRuntime::new().unwrap();
        let v = rt
            .eval_plugin(
                "esc",
                "return load == nil and loadstring == nil and require == nil \
                 and package == nil and debug == nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true), "escape globals still reachable");
        // `_G` must be the plugin's own env, so writes through it cannot
        // reach the real globals the host evaluates against.
        rt.eval_plugin("esc2", "_G.leaked = 42").unwrap();
        assert!(rt.eval("return leaked").unwrap().is_nil());
    }

    #[test]
    fn eval_plugin_still_reaches_base_kage_and_stdlib() {
        let rt = PluginRuntime::new().unwrap();
        let len = rt.eval_plugin("std", "return string.len('abcd')").unwrap();
        assert_eq!(
            len.as_integer(),
            Some(4),
            "stdlib unreachable in plugin env"
        );
        rt.eval_plugin(
            "reg",
            "kage.register_command({ name='z', description='', handler=function() end })",
        )
        .unwrap();
        let cmds = rt.registered_commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name(), "z");
    }

    #[test]
    fn eval_plugin_event_handlers_dispatch_with_plugin_env() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval_plugin(
            "ev",
            "hits = 0; kage.on('agent_start', function() hits = hits + 1 end)",
        )
        .unwrap();
        rt.dispatch_event("agent_start", &serde_json::json!({}))
            .unwrap();
        rt.dispatch_event("agent_start", &serde_json::json!({}))
            .unwrap();
        // The handler closes over plugin `ev`'s env, so its mutations
        // land there and survive across dispatches and re-evals.
        let v = rt.eval_plugin("ev", "return hits").unwrap();
        assert_eq!(v.as_integer(), Some(2));
    }
}
