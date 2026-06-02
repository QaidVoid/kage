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

    /// Set the directory backing `kage.store`. Each plugin persists to
    /// `<dir>/<stem>.json`. Unset, `kage.store` raises rather than
    /// silently dropping writes.
    #[must_use]
    pub fn state_dir(mut self, state_dir: Option<PathBuf>) -> Self {
        self.state_dir = state_dir;
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
        store::install_base(&lua)?;
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
        env::register(&cap_registry);
        http::register(&cap_registry);
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
            enabled: self.enabled,
            plugin_config: self.plugin_config,
            state_dir: self.state_dir,
        })
    }
}
