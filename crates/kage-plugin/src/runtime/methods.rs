//! `PluginRuntime` inherent methods: eval, dispatch, registration snapshots, reload.

use std::sync::atomic::AtomicBool;

use kage_core::options::{OptionSource, OptionValue};
use kage_core::sync::lock;

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

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
            enabled: Vec::new(),
            plugin_config: BTreeMap::new(),
            state_dir: None,
            script_budget: watchdog::BUDGET,
            defaults: stdlib::DEFAULTS,
            user_dir: None,
            options: SharedOptions::default(),
            themes: None,
            keybindings: kage_core::config::KeybindingsConfig::default(),
        }
    }

    /// Run `f` against the Lua state on its owner thread and wait for
    /// the result. The state is never reachable from any other thread;
    /// this is the escape hatch for hosts and tests that need raw Lua.
    /// `f` must not call back into this runtime, or the owner thread
    /// would wait on itself.
    pub fn with_lua<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Lua) -> R + Send + 'static,
    ) -> Result<R, PluginError> {
        self.host.call(f)
    }

    /// Flag the owner thread sets when retained render output (a
    /// widget, a header or footer row, a block renderer) changed, or
    /// when a render that found the owner busy can now be computed.
    /// Render calls return retained output at once and recompute in
    /// the background, so a host that wants fresh output promptly polls
    /// this on its tick with `swap(false, ..)` and redraws when it was
    /// set. Ignoring it is safe: output then refreshes on the host's own
    /// redraw cadence.
    #[must_use]
    pub fn redraw_flag(&self) -> Arc<AtomicBool> {
        self.host.redraw_flag()
    }

    /// Flag set when block renderer output changed, so the host should
    /// measure plugin blocks again. Hosts `swap(false)` it on their tick.
    #[must_use]
    pub fn blocks_flag(&self) -> Arc<AtomicBool> {
        self.host.blocks_flag()
    }

    /// Cloneable handle to the host log sink.
    #[must_use]
    pub fn sink(&self) -> SharedHostLog {
        Arc::clone(&self.sink)
    }

    /// Whether the loader should evaluate the plugin with file stem
    /// `stem`. An empty allowlist (the default) enables every plugin; a
    /// non-empty one enables only the plugins it names, so a user who
    /// lists `[plugins] enabled = ["trusted"]` loads nothing else.
    #[must_use]
    pub fn is_plugin_enabled(&self, stem: &str) -> bool {
        self.eval.is_enabled(stem)
    }

    /// Execute a chunk of Lua source against the shared globals.
    ///
    /// Used by the host for one-off evaluation and by tests. Plugin
    /// files are loaded through [`eval_plugin`](Self::eval_plugin)
    /// instead, so their top-level definitions stay private.
    pub fn eval(&self, source: &str) -> Result<mlua::Value, PluginError> {
        let source = source.to_owned();
        let budget = self.eval.script_budget;
        self.host.call(move |lua| {
            watchdog::run(lua, budget, || lua.load(&source).eval::<mlua::Value>())
        })?
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
        let eval = Arc::clone(&self.eval);
        let name = name.to_owned();
        let source = source.to_owned();
        self.host
            .call(move |lua| eval.eval_plugin(lua, &name, &source))?
    }

    /// Fire every handler subscribed to `event_name` with `payload`.
    ///
    /// Synchronous: it returns once every handler ran. Callers read the
    /// handlers' side effects right after (queued messages, session
    /// ops, compact or fork requests, host-log lines) and surface the
    /// returned error, so a fire-and-forget dispatch would race them.
    /// Dispatches from one thread run in call order. With no
    /// subscriber it returns at once without touching the owner thread.
    pub fn dispatch_event(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<(), PluginError> {
        if self.handler_count(event_name) == 0 {
            return Ok(());
        }
        let (name, payload) = (event_name.to_owned(), payload.clone());
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.call(move |lua| {
            watchdog::run(lua, budget, || {
                events::dispatch(lua, &name, &payload, &sink)
            })
        })?
    }

    /// Queue `event_name` for dispatch and return without waiting.
    ///
    /// For events that originate in the host UI, where nothing reads the
    /// handlers' side effects right away. Handler errors go to the host
    /// log. Jobs keep submission order, so a later [`Self::dispatch_event`]
    /// runs after this one. With no subscriber nothing is queued.
    pub fn notify_event(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<(), PluginError> {
        if self.handler_count(event_name) == 0 {
            return Ok(());
        }
        let (name, payload) = (event_name.to_owned(), payload.clone());
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.submit(move |lua| {
            let result = watchdog::run(lua, budget, || {
                events::dispatch(lua, &name, &payload, &sink)
            });
            if let Err(err) = result {
                lock(&sink).log(
                    crate::api::LogLevel::Error,
                    &format!("{name} dispatch: {err}"),
                );
            }
        })
    }

    /// Chain every handler subscribed to `event_name` and return the
    /// payload after the last handler ran. See [`events::dispatch_transform`]
    /// for the chaining semantics.
    pub fn dispatch_transform(
        &self,
        event_name: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        if self.handler_count(event_name) == 0 {
            return Ok(payload);
        }
        let name = event_name.to_owned();
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.call(move |lua| {
            watchdog::run(lua, budget, || {
                events::dispatch_transform(lua, &name, payload, &sink)
            })
        })?
    }

    /// Poll handlers subscribed to `event_name`; return `true` as soon as
    /// one returns a truthy value. See [`events::dispatch_predicate`].
    pub fn dispatch_predicate(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
    ) -> Result<bool, PluginError> {
        if self.handler_count(event_name) == 0 {
            return Ok(false);
        }
        let (name, payload) = (event_name.to_owned(), payload.clone());
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.call(move |lua| {
            watchdog::run(lua, budget, || {
                events::dispatch_predicate(lua, &name, &payload, &sink)
            })
        })?
    }

    /// Consult handlers subscribed to a session-op event. The first
    /// handler that returns a cancel or patch decision short-circuits the
    /// chain. See [`events::dispatch_session_op`].
    pub fn dispatch_session_op(
        &self,
        event_name: &str,
        target: &str,
    ) -> Result<events::SessionOpDecision, PluginError> {
        if self.handler_count(event_name) == 0 {
            return Ok(events::SessionOpDecision::Proceed);
        }
        let (name, target) = (event_name.to_owned(), target.to_owned());
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.call(move |lua| {
            watchdog::run(lua, budget, || {
                events::dispatch_session_op(lua, &name, &target, &sink)
            })
        })?
    }

    /// Fire every `resources_discover` handler and collect the aggregated
    /// directory paths. See [`events::dispatch_resources_discover`].
    pub fn discover_resources(&self) -> Result<events::DiscoveryEntries, PluginError> {
        if self.handler_count("resources_discover") == 0 {
            return Ok(events::DiscoveryEntries::default());
        }
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.call(move |lua| {
            watchdog::run(lua, budget, || {
                events::dispatch_resources_discover(lua, &sink)
            })
        })?
    }

    /// Number of handlers subscribed to `event_name`. Reads counts kept
    /// outside Lua, so it answers at once even while the owner thread
    /// runs a long job.
    #[must_use]
    pub fn handler_count(&self, event_name: &str) -> usize {
        lock(&self.autocmds).count(event_name)
    }

    /// Cloneable handle to the option store. The host drains its
    /// queued changes with `take_changes` and applies them.
    #[must_use]
    pub fn options(&self) -> SharedOptions {
        Arc::clone(&self.options.store)
    }

    /// Set option `name` from the host UI, with source `runtime`.
    ///
    /// The value is validated here, so a bad name or value fails at
    /// once. The set itself is queued on the owner thread, where it
    /// fires `option_set` in order with sets made from Lua. The change
    /// reaches the store when that job runs.
    pub fn set_option(&self, name: &str, value: OptionValue) -> Result<(), PluginError> {
        let value = self.options.check(name, value)?;
        let (options, name) = (self.options.clone(), name.to_owned());
        let (sink, budget) = (self.sink(), self.eval.script_budget);
        self.host.submit(move |lua| {
            let result = watchdog::run(lua, budget, || {
                options.set(lua, &name, value, OptionSource::Runtime)
            });
            if let Err(err) = result {
                lock(&sink).log(
                    crate::api::LogLevel::Error,
                    &format!("option `{name}`: {err}"),
                );
            }
        })
    }

    /// Snapshot the tools registered by plugins so far. Each call returns
    /// a fresh `Vec`; the underlying `Arc<dyn Tool>` entries are shared
    /// with the runtime's internal registry.
    #[must_use]
    pub fn registered_tools(&self) -> Vec<Arc<dyn kage_tools::Tool>> {
        lock(&self.tools).clone()
    }

    /// Snapshot the tool overrides registered by plugins via
    /// `kage.override_tool`. The host applies these after built-ins
    /// and `register_tool` entries; an override that names a tool not
    /// present at apply time logs a warning instead of crashing.
    #[must_use]
    pub fn registered_tool_overrides(&self) -> Vec<Arc<dyn kage_tools::Tool>> {
        lock(&self.tool_overrides).clone()
    }

    /// Snapshot the slash commands registered by plugins so far.
    #[must_use]
    pub fn registered_commands(&self) -> Vec<Arc<LuaCommand>> {
        lock(&self.commands).clone()
    }

    /// Snapshot the commands plugins registered via
    /// `kage.override_command`. The host lets these shadow a built-in
    /// of the same name and dispatches them ahead of it.
    #[must_use]
    pub fn registered_command_overrides(&self) -> Vec<Arc<LuaCommand>> {
        lock(&self.command_overrides).clone()
    }

    /// Cloneable handle to the keymap table every layer writes. The
    /// host resolves keys against it without a round trip to Lua.
    #[must_use]
    pub fn keymap(&self) -> keymap::SharedKeymap {
        Arc::clone(&self.eval.keymaps.table)
    }

    /// Fetch the Lua function a mapping with [`kage_core::keymap::Rhs::Lua`]
    /// runs. The host calls it through [`Self::bridge_call`] with no
    /// arguments, so it may open `kage.ui.*` dialogs. Fails when the
    /// mapping was replaced or a reload dropped it.
    pub fn keymap_handler(&self, id: u64) -> Result<mlua::Function, PluginError> {
        Ok(self.host.call(move |lua| keymap::handler(lua, id))??)
    }

    /// Snapshot the providers registered by plugins so far.
    #[must_use]
    pub fn registered_providers(&self) -> Vec<Arc<LuaProvider>> {
        lock(&self.providers).clone()
    }

    /// Snapshot the ACP agents plugins declared via
    /// `kage.acp.add_agent`. The host merges these with
    /// `[acp.agents.*]` from config.
    #[must_use]
    pub fn registered_acp_agents(&self) -> Vec<(String, kage_core::config::AcpAgent)> {
        lock(&self.acp_agents)
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Snapshot the MCP servers plugins declared via
    /// `kage.mcp.add_server`. The host merges these with
    /// `[mcp.servers.*]` from config.
    #[must_use]
    pub fn registered_mcp_servers(&self) -> Vec<(String, kage_core::config::McpServer)> {
        lock(&self.mcp_servers)
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
        std::mem::take(&mut *lock(&self.mcp_restart))
    }

    /// Consult the plugin's `kage.on_acp_permission` handler for an
    /// upstream agent's tool-call ask. `Some(true)` allow,
    /// `Some(false)` explicit deny, `None` no handler (host default).
    /// A watchdog overrun denies, like any other handler error.
    #[must_use]
    pub fn acp_permission(&self, payload: &serde_json::Value) -> Option<bool> {
        let payload = payload.clone();
        let budget = self.eval.script_budget;
        self.host
            .call(move |lua| {
                watchdog::run(lua, budget, || {
                    Ok::<_, PluginError>(acp::decide(lua, &payload))
                })
            })
            .and_then(|decision| decision)
            .unwrap_or(Some(false))
    }

    /// Snapshot the status-bar widgets registered by plugins so far.
    /// Each call returns a fresh `Vec`; the underlying [`LuaWidget`]s
    /// are reference-counted and share their Lua handler across clones.
    #[must_use]
    pub fn registered_widgets(&self) -> Vec<Arc<LuaWidget>> {
        lock(&self.widgets).clone()
    }

    /// Snapshot the transient status map populated by
    /// `kage.set_status`. Entries are returned in key-sorted order so
    /// the status bar paints deterministically across redraws.
    #[must_use]
    pub fn status_snapshot(&self) -> Vec<(String, String)> {
        lock(&self.status)
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
        let mut slot = lock(&self.usage);
        *slot = usage;
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
        lock(&self.compact_request).take()
    }

    /// Cloneable handle to the session-list snapshot.
    #[must_use]
    pub fn shared_session_list(&self) -> SharedSessionList {
        Arc::clone(&self.session_list)
    }

    /// Replace the current session list. The host typically refreshes
    /// this from its session lister on a redraw cadence.
    pub fn set_session_list(&self, entries: Vec<serde_json::Value>) {
        let mut slot = lock(&self.session_list);
        *slot = entries;
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
        lock(&self.fork_request).take()
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
        let mut slot = lock(&self.session_entries);
        *slot = entries;
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
        lock(&self.switch_request).take()
    }

    /// Cloneable handle to the highlight table. The host recompiles
    /// its palette when the table's generation moves.
    #[must_use]
    pub fn highlights(&self) -> SharedHighlights {
        Arc::clone(&self.options.highlights)
    }

    /// Snapshot the renderer a plugin installed via
    /// `kage.ui.set_header`, if any. The host calls
    /// [`LuaChrome::render`] on it once per redraw to paint the top
    /// chrome row; `None` means paint the built-in status bar.
    #[must_use]
    pub fn header_chrome(&self) -> Option<Arc<LuaChrome>> {
        lock(&self.header).clone()
    }

    /// Snapshot the renderer a plugin installed via
    /// `kage.ui.set_footer`, if any. The host calls
    /// [`LuaChrome::render`] on it once per redraw to paint the bottom
    /// chrome row; `None` means paint the built-in modeline.
    #[must_use]
    pub fn footer_chrome(&self) -> Option<Arc<LuaChrome>> {
        lock(&self.footer).clone()
    }

    /// Snapshot the custom block renderers plugins installed via
    /// `kage.register_block_renderer`. The host registers each into
    /// the TUI block-renderer registry; an empty result means every
    /// custom block uses the built-in card.
    #[must_use]
    pub fn registered_block_renderers(&self) -> Vec<Arc<LuaBlockRenderer>> {
        lock(&self.block_renderers).values().cloned().collect()
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
        lock(&self.autocomplete).clone()
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
        lock(&self.terminal_hooks).clone()
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
        std::mem::take(&mut *lock(&self.pending_messages))
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
        std::mem::take(&mut *lock(&self.session_ops))
    }

    /// Run `func` inside a fresh plugin coroutine with `args` as its
    /// positional arguments. Returns [`BridgeStep::Done`] if it ran to
    /// completion, or [`BridgeStep::Suspended`] if it called a blocking
    /// API (`kage._suspend`); in the latter case the coroutine is
    /// parked until [`Self::bridge_resume`] / [`Self::bridge_cancel`] /
    /// [`Self::bridge_abort`].
    ///
    /// Every step runs on the owner thread; the owner is free while the
    /// coroutine is parked. Fails with [`PluginError::BridgeBusy`] if
    /// another coroutine is already parked.
    pub fn bridge_call(
        &self,
        func: &mlua::Function,
        args: &[serde_json::Value],
    ) -> Result<BridgeStep, PluginError> {
        let func = func.clone();
        let args = args.to_vec();
        let bridge = Arc::clone(&self.bridge);
        let budget = self.eval.script_budget;
        self.host.call(move |lua| {
            let mut slot = lock(&bridge);
            if slot.is_some() {
                return Err(PluginError::BridgeBusy);
            }
            let thread = lua.create_thread(func)?;
            watchdog::install_on_thread(&thread)?;
            let resume_args = bridge::args_to_multi(lua, &args)?;
            watchdog::run(lua, budget, || bridge::step(thread, resume_args, &mut slot))
        })?
    }

    /// Resume the parked coroutine, delivering `result` as the return
    /// value of the blocking call that suspended it. Returns the next
    /// step (done or suspended again).
    pub fn bridge_resume(&self, result: &serde_json::Value) -> Result<BridgeStep, PluginError> {
        let result = result.clone();
        self.bridge_step(move |lua| bridge::args_to_multi(lua, std::slice::from_ref(&result)))
    }

    /// Resume the parked coroutine signalling the host action was
    /// cancelled. The blocking call returns `nil` to the plugin (the
    /// `kage.ui.*` dialog contract for "user dismissed").
    pub fn bridge_cancel(&self) -> Result<BridgeStep, PluginError> {
        self.bridge_step(|_| Ok(mlua::MultiValue::new()))
    }

    fn bridge_step(
        &self,
        resume_args: impl FnOnce(&Lua) -> Result<mlua::MultiValue, PluginError> + Send + 'static,
    ) -> Result<BridgeStep, PluginError> {
        let bridge = Arc::clone(&self.bridge);
        let budget = self.eval.script_budget;
        self.host.call(move |lua| {
            let mut slot = lock(&bridge);
            let thread = slot.take().ok_or(PluginError::BridgeIdle)?;
            let resume_args = resume_args(lua)?;
            watchdog::run(lua, budget, || bridge::step(thread, resume_args, &mut slot))
        })?
    }

    /// Abandon the parked coroutine without resuming it (hard cancel,
    /// e.g. the run was aborted while a dialog was open). Returns
    /// `true` if a coroutine was actually dropped. Idempotent.
    #[must_use = "the boolean reports whether a coroutine was dropped; \
                  discard with `let _ =` if only the side effect matters"]
    pub fn bridge_abort(&self) -> bool {
        let bridge = Arc::clone(&self.bridge);
        self.host
            .call(move |_| lock(&bridge).take().is_some())
            .unwrap_or(false)
    }

    /// `true` while a bridged coroutine is parked awaiting a host
    /// action.
    #[must_use]
    pub fn bridge_is_suspended(&self) -> bool {
        lock(&self.bridge).is_some()
    }

    /// Reload with `dir` as the plugins directory. Same as
    /// [`Self::reload_all`] with `Some(dir)`.
    pub fn reload_dir(
        &self,
        dir: &std::path::Path,
    ) -> Result<crate::loader::LoadReport, PluginError> {
        self.reload_all(Some(dir))
    }

    /// Drop every registration that came from Lua (autocmds and groups,
    /// pending schedule, defer and timer callbacks, keymaps, tools,
    /// commands, providers, ACP/MCP declarations), then rerun the
    /// full load: `_defaults.lua`, every `*.lua` file in `plugins_dir`,
    /// the `[keybindings]` table, and the trusted `init.lua` when a
    /// user dir is configured.
    /// Designed for hot reload between turns: a stale plugin snapshot
    /// does not survive after this call.
    ///
    /// The Lua side of the reload (clearing handlers, dropping plugin
    /// environments, evaluating every file) runs as one job on the
    /// owner thread, so no other plugin call observes a half-loaded
    /// plugin set.
    ///
    /// Tools, commands, and providers that the host has already handed
    /// to other registries via [`Self::registered_tools`] etc. continue
    /// to exist; this method only clears the runtime's own snapshot.
    /// The host is responsible for re-publishing the new snapshot.
    pub fn reload_all(
        &self,
        plugins_dir: Option<&std::path::Path>,
    ) -> Result<crate::loader::LoadReport, PluginError> {
        lock(&self.tools).clear();
        lock(&self.tool_overrides).clear();
        lock(&self.widgets).clear();
        lock(&self.status).clear();
        lock(&self.commands).clear();
        lock(&self.command_overrides).clear();
        lock(&self.providers).clear();
        lock(&self.acp_agents).clear();
        lock(&self.mcp_servers).clear();
        lock(&self.mcp_restart).clear();
        lock(&self.pending_messages).clear();
        lock(&self.session_ops).clear();
        *lock(&self.compact_request) = None;
        *lock(&self.fork_request) = None;
        *lock(&self.switch_request) = None;
        *lock(&self.header) = None;
        *lock(&self.footer) = None;
        lock(&self.block_renderers).clear();
        lock(&self.autocomplete).clear();
        lock(&self.terminal_hooks).clear();
        let eval = Arc::clone(&self.eval);
        let bridge = Arc::clone(&self.bridge);
        let dir = plugins_dir.map(std::path::Path::to_path_buf);
        self.host.call(move |lua| {
            schedule::clear(lua)?;
            autocmd::clear(lua)?;
            acp::clear_permission_handler(lua)?;
            eval.keymaps.clear(lua)?;
            *lock(&bridge) = None;
            eval.reset(lua);
            crate::loader::load_on(lua, dir.as_deref(), &eval)
        })?
    }
}
