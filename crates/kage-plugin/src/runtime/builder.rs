//! `PluginRuntimeBuilder`: configuration setters and `build`.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

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

    /// Set the load allowlist (from `[plugins] enabled`), keyed by
    /// plugin file stem. Empty (the default) loads every discovered
    /// plugin; non-empty loads only the named plugins.
    #[must_use]
    pub fn enabled(mut self, enabled: Vec<String>) -> Self {
        self.enabled = enabled;
        self
    }

    /// Set per-plugin settings (from `[plugins.config.<stem>]`), keyed
    /// by plugin file stem. Each plugin reads only its own slice through
    /// `kage.plugin_config()`.
    #[must_use]
    pub fn plugin_config(mut self, plugin_config: BTreeMap<String, serde_json::Value>) -> Self {
        self.plugin_config = plugin_config;
        self
    }

    /// Set the store directory backing `kage.store`. Each plugin persists to
    /// `<dir>/<stem>.json`. Unset, `kage.store` raises rather than
    /// silently dropping writes.
    #[must_use]
    pub fn state_dir(mut self, state_dir: Option<PathBuf>) -> Self {
        self.state_dir = state_dir;
        self
    }

    /// Set the VM instruction budget one host-driven plugin entry may
    /// execute before the watchdog aborts it. Defaults to
    /// [`crate::watchdog::BUDGET`].
    #[must_use]
    pub fn script_budget(mut self, script_budget: u64) -> Self {
        self.script_budget = script_budget;
        self
    }

    /// Set the trusted user config directory. Every load then ends by
    /// evaluating `<dir>/init.lua` in the user environment, with
    /// `require` confined to `<dir>/lua/`. Unset (the default), no user
    /// config is loaded.
    #[must_use]
    pub fn user_dir(mut self, user_dir: Option<PathBuf>) -> Self {
        self.user_dir = user_dir;
        self
    }

    /// Share `options` with the runtime. The host seeds it from config
    /// before [`build`](Self::build), so every Lua layer sees the
    /// configured values and `init.lua` can override them. Defaults to
    /// a store holding the registry defaults.
    #[must_use]
    pub fn options(mut self, options: SharedOptions) -> Self {
        self.options = options;
        self
    }

    /// Set the source of theme names the `theme` option accepts.
    /// Unset, any non-empty name is accepted.
    #[must_use]
    pub fn theme_names(mut self, theme_names: ThemeNames) -> Self {
        self.theme_names = Some(theme_names);
        self
    }

    /// Set the `[keybindings]` table from `config.toml`. Its `bindings`
    /// map in mode `g` after the plugins and before `init.lua` on every
    /// load, and every entry that cannot be applied, including keys
    /// written directly under the table, lands in
    /// [`crate::LoadReport::keymap_errors`].
    #[must_use]
    pub fn keybindings(mut self, keybindings: kage_core::config::KeybindingsConfig) -> Self {
        self.keybindings = keybindings;
        self
    }

    /// Replace the embedded `_defaults.lua` source.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn defaults(mut self, defaults: &'static str) -> Self {
        self.defaults = defaults;
        self
    }

    /// Finalize the runtime: build the Lua state, apply sandbox removals,
    /// install the `kage` API table with its `kage.api` primitives, wire
    /// `kage.register_tool`, `kage.register_command`,
    /// `kage.register_provider`, `kage.fs.*`, `kage.opt`, `kage.action`, and
    /// `kage.schedule`, `kage.defer` and `kage.timer`, evaluate the embedded stdlib
    /// (which defines `kage.on`), freeze the shared tables, then hand
    /// the state to its owner thread.
    #[allow(clippy::too_many_lines)]
    pub fn build(self) -> Result<PluginRuntime, PluginError> {
        let lua = Lua::new();
        apply_sandbox(&lua)?;
        watchdog::install(&lua)?;
        api::install(&lua, self.sink.clone(), self.config)?;
        plugin_fs::install_fs(&lua, self.workdir.clone())?;
        http::install_http(&lua)?;
        store::install_base(&lua)?;
        let (host, owner) = LuaHost::new();
        let weak_host = host.downgrade();
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
        let autocmds = autocmd::install(&lua, self.sink.clone(), Arc::clone(&current_plugin))?;
        schedule::install(
            &lua,
            self.sink.clone(),
            Arc::clone(&current_plugin),
            self.script_budget,
        )?;
        let keymaps = Keymaps::new(
            Arc::clone(&self.options),
            Arc::clone(&current_plugin),
            self.sink.clone(),
        );
        keymap::install(&lua, &keymaps)?;
        let options = Options {
            store: self.options,
            themes: self.theme_names,
            sink: self.sink.clone(),
        };
        options::install(&lua, &options)?;
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
        env::register(&cap_registry);
        http::register(&cap_registry);
        bridge::install_suspend(&lua)?;
        capabilities::install_request_capabilities(
            &lua,
            Arc::clone(&current_plugin),
            Arc::clone(&grants),
            Arc::clone(&plugin_envs),
            Arc::clone(&cap_registry),
        )?;
        ui::install_ui(&lua)?;
        tools::install_register_tool(&lua, weak_host.clone(), self.sink.clone(), &tool_registry)?;
        tools::install_override_tool(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &tool_override_registry,
        )?;
        commands::install_register_command(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &command_registry,
        )?;
        commands::install_override_command(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &command_override_registry,
        )?;
        providers::install_register_provider(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &provider_registry,
        )?;
        widgets::install_register_widget(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &widget_registry,
        )?;
        status::install_status(&lua, Arc::clone(&status_map))?;
        acp::install_acp(&lua, Arc::clone(&acp_agents))?;
        mcp::install_mcp(&lua, Arc::clone(&mcp_servers), Arc::clone(&mcp_restart))?;
        lifecycle::install_lifecycle(&lua, Arc::clone(&usage_snapshot), Arc::clone(&compact_slot))?;
        sessions::install_sessions(
            &lua,
            Arc::clone(&session_list_slot),
            Arc::clone(&fork_slot),
            Arc::clone(&session_ops_slot),
        )?;
        messages::install_send_message(&lua, Arc::clone(&pending_messages_slot))?;
        theme::install_theme(
            &lua,
            Arc::clone(&theme_state_slot),
            Arc::clone(&theme_request_slot),
        )?;
        chrome::install_chrome(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &header_slot,
            &footer_slot,
        )?;
        block_renderers::install_block_renderers(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &block_renderer_map,
        )?;
        autocomplete::install_add_autocomplete_provider(
            &lua,
            weak_host.clone(),
            self.sink.clone(),
            &autocomplete_registry,
        )?;
        terminal_input::install_on_terminal_input(
            &lua,
            weak_host,
            self.sink.clone(),
            &terminal_hook_registry,
        )?;
        stdlib::install(&lua)?;
        // Last: lock down the shared tables. Everything above runs
        // build-time writes through plain `Table::set`, which would
        // trip the read-only `__newindex` guards.
        freeze_shared_tables(&lua)?;
        owner.spawn(lua)?;
        let eval = Arc::new(EvalState {
            sink: self.sink.clone(),
            plugin_envs,
            current_plugin,
            enabled: self.enabled,
            plugin_config: self.plugin_config,
            state_dir: self.state_dir,
            script_budget: self.script_budget,
            defaults: self.defaults,
            user_dir: self.user_dir,
            capabilities: cap_registry,
            keymaps,
            keybindings: self.keybindings,
        });
        Ok(PluginRuntime {
            host,
            eval,
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
            theme_state: theme_state_slot,
            theme_request: theme_request_slot,
            header: header_slot,
            footer: footer_slot,
            block_renderers: block_renderer_map,
            autocomplete: autocomplete_registry,
            terminal_hooks: terminal_hook_registry,
            autocmds,
            options,
            session_entries,
            switch_request,
        })
    }
}
