//! App construction and host-wiring setters.

use super::*;

impl App {
    /// Construct an app that pushes prompts into `requests`. The
    /// receiver side is owned by the host's worker driver.
    #[must_use]
    pub fn new(buffer: SharedBuffer, requests: Sender<RunRequest>) -> Self {
        let (attach_tx, attach_rx) = std::sync::mpsc::channel();
        Self {
            root_buffer: Arc::clone(&buffer),
            buffer,
            focus: None,
            input: InputState::new(),
            requests,
            model_choices: Vec::new(),
            picker: None,
            picker_kind: None,
            session_scope_all: false,
            settings_overlay: None,
            session_tree: None,
            help_overlay: None,
            agents_overlay: None,
            session_tree_source: None,
            session_lister: None,
            cmdline: None,
            slash_palette: None,
            status_model: None,
            status_session_id: None,
            plugin_commands: Vec::new(),
            plugin_command_aliases: Vec::new(),
            plugin_command_overrides: Vec::new(),
            plugin_command_specs: Vec::new(),
            plugin_commands_leaked: Vec::new(),
            mcp_servers: Vec::new(),
            mcp_command_specs: Vec::new(),
            keymap: kage_plugin::SharedKeymap::default(),
            sequencer: Sequencer::new(Duration::from_secs(1)),
            plugin_widgets: Vec::new(),
            plugin_widget_texts: Vec::new(),
            plugin_texts_refreshed_at: None,
            plugin_texts_width: 0,
            plugin_texts_dirty: false,
            plugin_redraw: None,
            plugin_status: None,
            plugin_status_cache: Vec::new(),
            draw_snapshot: None,
            draw_snapshot_version: 0,
            color_depth: crate::theme::ColorDepth::TrueColor,
            plugin_usage: None,
            plugin_compact_request: None,
            plugin_session_list: None,
            plugin_sessions_stale: true,
            plugin_fork_request: None,
            plugin_switch_request: None,
            highlights: None,
            highlights_generation: 0,
            options: kage_plugin::SharedOptions::default(),
            option_setter: None,
            autocomplete_providers: Vec::new(),
            input_completion: None,
            completion_workdir: None,
            themes_dir: None,
            terminal_hooks: None,
            slots: None,
            search_line: None,
            search_pattern: None,
            search_origin: None,
            search_match_set: Vec::new(),
            search_match_version: 0,
            search_match_pattern: String::new(),
            mouse_drag_anchor: None,
            context_menu: None,
            pending_mouse_capture: None,
            screen_selection: None,
            captured_rows: std::collections::BTreeMap::new(),
            last_cursor_style: None,
            session_usage: None,
            toasts: None,
            dialog_rx: None,
            plugin_refresh_rx: None,
            login_runner: None,
            mcp_login_runner: None,
            pending_login: None,
            attach_tx,
            attach_rx,
            plugin_overlay: None,
            active_dialog: None,
            pending_tree_delete: None,
            engine_rx: None,
            active_session: None,
            agents: kage_core::protocol::AgentTree::default(),
            agent_buffers: std::collections::HashMap::new(),
            agent_loader: None,
            drafts: std::collections::HashMap::new(),
            approval_panel: None,
            pending_permission: None,
            permission_queue: std::collections::VecDeque::new(),
            run_started: None,
            key_labels: chrome::KeyLabels::default(),
            start_info: None,
            pending: Vec::new(),
            pinned_hits: Vec::new(),
            escalation: None,
        }
    }

    /// Hand the App a shared session-usage snapshot. The footer, the
    /// input rule and the working row read the model, token totals,
    /// context fill and working flag from it. Without one they show
    /// none of those.
    pub fn set_session_usage(&mut self, usage: crate::usage::SharedSessionUsage) {
        self.session_usage = Some(usage);
    }

    /// Hand the App the loader that reads a finished agent's stored
    /// transcript, so the agents overlay can open the agents of a
    /// resumed session.
    pub fn set_agent_loader(&mut self, loader: AgentLoader) {
        self.agent_loader = Some(loader);
    }

    /// Whether a run of the session on screen is in flight. See
    /// [`Self::session_running`].
    pub(crate) fn is_run_in_flight(&self) -> bool {
        self.session_running(self.focus)
    }

    /// Whether a run of `session` is in flight or waiting to start: the
    /// main session for `None`, from the engine's last reported state
    /// (`false` without a usage snapshot), else the agent's state.
    pub(crate) fn session_running(&self, session: Option<kage_core::SessionId>) -> bool {
        use kage_core::protocol::AgentState;
        match session {
            None => self.session_usage.as_ref().is_some_and(|u| lock(u).working),
            Some(session) => self
                .agents
                .get(session)
                .is_some_and(|n| matches!(n.state, AgentState::Queued | AgentState::Running)),
        }
    }

    /// The session whose agents the pinned list and the working row
    /// count: the agent on screen, else the main session.
    pub(crate) fn view_root(&self) -> Option<kage_core::SessionId> {
        self.focus.or(self.active_session)
    }

    /// Whether the agent on screen came from a resumed session's
    /// history, so no engine runs it and it cannot be messaged.
    pub(crate) fn focused_read_only(&self) -> bool {
        self.focus
            .and_then(|session| self.agents.get(session))
            .is_some_and(|node| node.restored)
    }

    /// The name of the agent on screen. `None` in the main view.
    pub(crate) fn focused_agent(&self) -> Option<&str> {
        self.agents.get(self.focus?).map(|n| n.agent.as_str())
    }

    /// Register the shared toast queue. While set, App-internal
    /// `notify(...)` calls and external sinks holding a clone of
    /// the same handle push into the toast strip. Without it
    /// `notify(...)` silently drops the message - toasts are
    /// decorative, never load-bearing.
    pub fn set_toasts(&mut self, toasts: SharedToasts) {
        self.toasts = Some(toasts);
    }

    /// Snapshot live (non-expired) toasts for one frame, dropping
    /// expired entries in the process. Returns an empty vector when
    /// no toast queue is registered.
    pub(crate) fn live_toasts(&self) -> Vec<Toast> {
        let Some(handle) = &self.toasts else {
            return Vec::new();
        };
        let now = Instant::now();
        let _ = toast::prune_expired(handle, now);
        lock(handle).iter().cloned().collect()
    }

    /// Earliest deadline at which a live toast will expire, used by
    /// the event loop to wake up just in time to repaint without
    /// waiting for an unrelated key event.
    pub(crate) fn next_toast_deadline(&self) -> Option<Instant> {
        let handle = self.toasts.as_ref()?;
        let q = lock(handle);
        q.iter().map(|t| t.expires_at).min()
    }

    /// Ask the engine to cancel the run of the session on screen, with
    /// the agents under it.
    pub(crate) fn trip_cancel(&mut self) {
        let _ = self.send_request(RunRequest::Cancel {
            session: self.focus,
        });
    }

    /// Snapshot the session-usage handle, returning `None` when the
    /// host has not registered one.
    pub(crate) fn session_usage_snapshot(&self) -> Option<crate::usage::SessionUsage> {
        self.session_usage.as_ref().map(|h| lock(h).clone())
    }

    /// Register the plugin commands the host wants exposed in the
    /// palette and on the `:` line. Names that collide with built-in
    /// specs are dropped; the host should log a warning at
    /// registration time.
    ///
    /// Builds one [`CommandSpec`] per plugin command, leaking the
    /// owned name, description, and per-arg schema into `&'static`
    /// storage so plugin commands participate in the same completion
    /// engine the builtins use. A command equal (by name, aliases,
    /// override flag, description, and arg schema) to one registered
    /// in a previous call reuses that call's leaked spec, so repeated
    /// hot reloads of an unchanged plugin set do not grow the leak;
    /// it grows only when a reload actually changes the command set.
    /// MCP prompt commands are rebuilt so plugin names win over them.
    pub fn set_plugin_commands(&mut self, mut commands: Vec<PluginCommand>) {
        // A regular plugin command (or any of its aliases) may not
        // shadow a builtin; an `override_command` is allowed to and
        // is dispatched ahead of the builtin.
        commands.retain(|c| {
            c.is_override
                || (crate::command::find_builtin_command(&c.name).is_none()
                    && c.aliases
                        .iter()
                        .all(|a| crate::command::find_builtin_command(a).is_none()))
        });
        self.plugin_command_specs.clear();
        self.plugin_command_aliases.clear();
        self.plugin_command_overrides.clear();
        for cmd in &commands {
            let spec = self.leaked_spec(PluginCommand {
                description: format!("{}  [plugin]", cmd.description),
                ..cmd.clone()
            });
            self.plugin_command_specs.push(spec);
            for alias in &cmd.aliases {
                self.plugin_command_aliases
                    .push((alias.clone(), cmd.name.clone()));
            }
            if cmd.is_override {
                self.plugin_command_overrides.push(cmd.name.clone());
            }
        }
        self.plugin_commands = commands
            .into_iter()
            .map(|c| (c.name, c.description))
            .collect();
        self.set_mcp_prompts();
    }

    /// Return the previously-leaked spec for `cmd` when an equal
    /// command was registered in an earlier call, so a hot reload of
    /// unchanged plugins reuses the old `&'static` storage. Otherwise
    /// leak a fresh spec and remember it. The description is used as
    /// given, tag included.
    fn leaked_spec(&mut self, cmd: PluginCommand) -> &'static CommandSpec {
        let reused = self
            .plugin_commands_leaked
            .iter()
            .find(|(known, _)| *known == cmd)
            .map(|(_, spec)| *spec);
        if let Some(spec) = reused {
            return spec;
        }
        let name_static: &'static str = Box::leak(cmd.name.clone().into_boxed_str());
        let desc_static: &'static str = Box::leak(cmd.description.clone().into_boxed_str());
        let args_owned: Vec<ArgSpec> = cmd.args.iter().map(leak_argspec).collect();
        let args_static: &'static [ArgSpec] = Box::leak(args_owned.into_boxed_slice());
        let aliases_static: &'static [&'static str] = Box::leak(
            cmd.aliases
                .iter()
                .map(|a| &*Box::leak(a.clone().into_boxed_str()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let spec: &'static CommandSpec = Box::leak(Box::new(CommandSpec {
            name: name_static,
            aliases: aliases_static,
            description: desc_static,
            category: CommandCategory::Both,
            args: args_static,
            subcommands: &[],
        }));
        self.plugin_commands_leaked.push((cmd, spec));
        spec
    }

    /// Take the main session's MCP catalog from an `McpServers`
    /// snapshot and rebuild the prompt commands from it.
    pub(crate) fn set_mcp_servers(&mut self, servers: Vec<kage_core::protocol::McpServerInfo>) {
        self.mcp_servers = servers;
        self.set_mcp_prompts();
    }

    /// Rebuild [`Self::mcp_command_specs`]: one `server:prompt` command
    /// per prompt of a live server, whose argument hint lists the
    /// prompt's arguments and whose description ends in `[mcp]`. The
    /// argument is required when the prompt has a required one.
    /// Names a builtin or plugin command takes are skipped, so the
    /// palette shows only the command that runs.
    fn set_mcp_prompts(&mut self) {
        let mut commands = Vec::new();
        for server in &self.mcp_servers {
            if server.status != kage_core::protocol::McpServerStatus::Connected {
                continue;
            }
            for prompt in &server.prompts {
                let name = format!("{}:{}", server.name, prompt.name);
                let taken = crate::command::find_builtin_command(&name).is_some()
                    || self.plugin_commands.iter().any(|(n, _)| *n == name)
                    || self.plugin_command_aliases.iter().any(|(a, _)| *a == name);
                if taken {
                    continue;
                }
                let hint = prompt
                    .arguments
                    .iter()
                    .map(|a| {
                        if a.required {
                            format!("<{}>", a.name)
                        } else {
                            format!("[{}]", a.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let args = if hint.is_empty() {
                    Vec::new()
                } else {
                    vec![OwnedArgSpec::Text {
                        name: "arguments".to_owned(),
                        optional: prompt.arguments.iter().all(|a| !a.required),
                        hint,
                    }]
                };
                let description = match prompt.description.as_deref().and_then(|d| d.lines().next())
                {
                    Some(text) => format!("{text}  [mcp]"),
                    None => "[mcp]".to_owned(),
                };
                commands.push(PluginCommand {
                    name,
                    aliases: Vec::new(),
                    is_override: false,
                    description,
                    args,
                });
            }
        }
        self.mcp_command_specs = commands
            .into_iter()
            .map(|cmd| self.leaked_spec(cmd))
            .collect();
    }

    /// Hand the App a shared handle on the active `provider:model`
    /// string so the status bar reflects model switches in real time.
    pub fn set_status_model(&mut self, model: Arc<Mutex<String>>) {
        self.status_model = Some(model);
    }

    /// Set the short session-id pill shown on the right of the status
    /// bar.
    pub fn set_status_session_id(&mut self, short_id: String) {
        self.status_session_id = Some(short_id);
    }

    /// Replace the model list shown when the user opens the in-TUI
    /// picker. The host computes this from its provider registry +
    /// catalog.
    pub fn set_model_choices(&mut self, choices: Vec<PickItem>) {
        self.model_choices = choices;
    }

    /// Seed the prompt history with persisted entries (oldest first).
    /// Truncated to [`crate::input::HISTORY_MAX`] keeping the most
    /// recent.
    pub fn set_history(&mut self, entries: Vec<String>) {
        self.input.set_history(entries);
    }

    /// Register the closure that produces the session picker's items
    /// at the moment of opening. Without this, `Ctrl+S` is a no-op.
    pub fn set_session_lister(&mut self, lister: SessionLister) {
        self.session_lister = Some(lister);
    }

    /// Set what the start card lists: recent sessions, startup notices
    /// and the permission summary. Later session changes list the
    /// sessions again through the session lister.
    pub fn set_start_info(&mut self, mut info: view::StartInfo) {
        info.sessions.truncate(view::START_SESSIONS);
        self.start_info = Some(info);
    }

    /// Run on a session change: drop the old conversation's search
    /// and list the start card's recent sessions again.
    pub(crate) fn on_session_changed(&mut self) {
        self.search_pattern = None;
        let (Some(info), Some(lister)) = (self.start_info.as_mut(), &self.session_lister) else {
            return;
        };
        info.sessions = lister(false);
        info.sessions.truncate(view::START_SESSIONS);
    }

    /// Register the closure that produces the `:tree` session forest
    /// at open time. Without this, `:tree` reports it is unavailable.
    pub fn set_session_tree_source(&mut self, source: SessionTreeSource) {
        self.session_tree_source = Some(source);
    }
    /// Wire the shared state of the plugin runtime `rt`: the status-bar
    /// widgets, the autocomplete providers, the maps behind
    /// `kage.set_status` and `kage.context_usage()`, the slots
    /// `kage.compact`, `kage.session.list`, `kage.session.fork` and the
    /// `session_write` reseat fill, the highlight table, the UI slots,
    /// the terminal-input hooks, the redraw flags and the keymap.
    /// Without it plugins can call those APIs but the App never shows or
    /// acts on the results.
    pub fn attach_plugins(&mut self, rt: &kage_plugin::PluginRuntime) {
        self.set_plugin_widgets(rt.registered_widgets());
        self.set_plugin_autocomplete(rt.registered_autocomplete_providers());
        self.plugin_status = Some(rt.shared_status());
        self.plugin_usage = Some(rt.shared_usage());
        self.plugin_compact_request = Some(rt.shared_compact_request());
        self.set_plugin_session_list(rt.shared_session_list());
        self.plugin_fork_request = Some(rt.shared_fork_request());
        self.plugin_switch_request = Some(rt.shared_switch_request());
        self.set_highlights(rt.highlights());
        self.set_slots(rt.slots());
        self.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
        self.plugin_redraw = Some((rt.redraw_flag(), rt.blocks_flag()));
        self.set_keymap(rt.keymap());
    }

    /// Register the status-bar widgets supplied by plugins. Marks the
    /// plugin text caches dirty so the next frame renders them at
    /// once instead of waiting for the refresh tick.
    pub fn set_plugin_widgets(&mut self, widgets: Vec<Arc<kage_plugin::LuaWidget>>) {
        self.plugin_widgets = widgets;
        self.plugin_texts_dirty = true;
    }

    /// Wire the shared session list `kage.session.list()` reads from.
    /// Without this, plugins always see an empty list.
    pub fn set_plugin_session_list(&mut self, list: kage_plugin::SharedSessionList) {
        self.plugin_session_list = Some(list);
    }

    /// Wire the plugin runtime's highlight table and compile the
    /// palette from it. From then on the palette follows the table:
    /// theme switches, `kage.api.hl_set` and theme files all land there.
    /// Without this the palette stays the default one.
    pub fn set_highlights(&mut self, highlights: kage_plugin::SharedHighlights) {
        self.highlights = Some(highlights);
        self.highlights_generation = 0;
        self.refresh_highlights();
    }

    /// Wire the slot specs `kage.ui.set_slot`, `set_header` and
    /// `set_footer` fill. Without this every slot paints its default
    /// spec.
    pub fn set_slots(&mut self, slots: kage_plugin::Slots) {
        self.slots = Some(slots);
    }

    /// Wire the autocomplete provider stack from
    /// `kage.add_autocomplete_provider`. Without this the Lua calls
    /// still register providers in the runtime but the input never
    /// queries them. Providers run synchronously inside the plugin
    /// runtime's Lua mutex on each prompt-input change.
    pub fn set_plugin_autocomplete(
        &mut self,
        providers: Vec<Arc<kage_plugin::LuaAutocompleteProvider>>,
    ) {
        self.autocomplete_providers = providers;
    }

    /// Wire the raw terminal-input hook list from
    /// `kage.on_terminal_input`. Without this the Lua calls still
    /// register hooks in the runtime but no key is ever offered to
    /// them. Hooks run synchronously inside the plugin runtime's Lua
    /// mutex, before every modal layer, on each keystroke.
    pub fn set_plugin_terminal_hooks(&mut self, hooks: kage_plugin::RegisteredTerminalHooks) {
        self.terminal_hooks = Some(hooks);
    }

    /// Apply the configured editor model at startup (and live from
    /// the settings dialog). `true` selects non-modal editing.
    pub fn set_editor_modeless(&mut self, on: bool) {
        self.input.set_modeless(on);
    }

    /// Set the workdir the built-in `@file` autocomplete lists under.
    /// Without this the `@file` fallback is disabled; plugin providers
    /// still function.
    pub fn set_workdir(&mut self, workdir: std::path::PathBuf) {
        self.completion_workdir = Some(workdir);
    }

    /// Point theme resolution at the user theme directory
    /// (`~/.config/kage/themes`). Without this only bundled themes
    /// resolve; with it, `<name>.toml` files there become selectable
    /// everywhere a bundled name is.
    pub fn set_themes_dir(&mut self, dir: std::path::PathBuf) {
        self.themes_dir = Some(dir);
    }

    /// Share the option store with the plugin runtime and apply its
    /// current values: mouse, editor, input bounds and the key sequence
    /// timeout. Changes queued before this call are covered by those
    /// values and dropped. `setter` routes later sets through the
    /// runtime. The theme reaches the palette through
    /// [`Self::set_highlights`].
    pub fn set_options(
        &mut self,
        options: kage_plugin::SharedOptions,
        setter: Option<OptionSetter>,
    ) {
        self.options = options;
        self.option_setter = setter;
        let current: Vec<(&str, Option<OptionValue>)> = {
            let mut store = lock(&self.options);
            store.take_changes();
            ["mouse", "editor", "input_min_lines", "timeoutlen"]
                .into_iter()
                .map(|name| (name, store.get(name).cloned()))
                .collect()
        };
        for (name, value) in current {
            if let Some(value) = value {
                self.apply_option(name, &value, false);
            }
        }
    }

    /// Share the keymap table the plugin runtime fills. Until this is
    /// called the table is empty, so only the editor grammar handles
    /// keys.
    pub fn set_keymap(&mut self, keymap: kage_plugin::SharedKeymap) {
        self.keymap = keymap;
        self.sequencer.clear();
    }

    /// Wire the channel the worker pushes blocking [`PluginDialog`]
    /// requests onto. Without this, `kage.ui.select` has nowhere to
    /// surface and the worker's send fails, which it treats as a
    /// cancel (the plugin call returns `nil`).
    pub fn set_plugin_dialog(&mut self, rx: std::sync::mpsc::Receiver<PluginDialog>) {
        self.dialog_rx = Some(rx);
    }

    /// Wire the channel the worker pushes a fresh [`PluginRefresh`]
    /// snapshot onto after a plugin hot reload. Without it, a reload
    /// leaves the `:` palette and status widgets serving the
    /// pre-reload registration until the app restarts.
    pub fn set_plugin_refresh(&mut self, rx: std::sync::mpsc::Receiver<PluginRefresh>) {
        self.plugin_refresh_rx = Some(rx);
    }

    /// Wire the host hook `App::run_login_flow` uses to run an
    /// interactive credential login in the real terminal. `:login`
    /// is a no-op error without it.
    pub fn set_login_runner(&mut self, runner: LoginRunner) {
        self.login_runner = Some(runner);
    }

    /// Wire the host hook `App::run_login_flow` uses to log in to an
    /// MCP server in the real terminal. `/mcp login` is an error
    /// without it.
    pub fn set_mcp_login_runner(&mut self, runner: McpLoginRunner) {
        self.mcp_login_runner = Some(runner);
    }

    /// Consume a pending login (if any) and run its flow with the
    /// terminal suspended. Returns whether anything ran so the caller
    /// repaints.
    pub(crate) fn consume_pending_login(&mut self, tui: &mut Tui) -> bool {
        let Some(request) = self.pending_login.take() else {
            return false;
        };
        tui.suspend();
        self.run_login_flow(request);
        tui.resume();
        true
    }

    /// Run the host login hook for the request while the terminal is
    /// suspended. A saved provider credential rebuilds the provider
    /// registry, and an MCP login restarts its server.
    pub(crate) fn run_login_flow(&mut self, request: PendingLogin) {
        let provider = match request {
            PendingLogin::Picker => None,
            PendingLogin::Provider(name) => Some(name),
            PendingLogin::Mcp(server) => {
                let Some(runner) = self.mcp_login_runner.clone() else {
                    return;
                };
                match runner(&server) {
                    Ok(()) => {
                        self.notify(format!("mcp {server}: logged in, reconnecting"));
                        let _ = self.send_request(RunRequest::RestartMcp(server));
                    }
                    Err(e) => self.push_error(format!("mcp login {server}: {e}")),
                }
                return;
            }
        };
        let Some(runner) = self.login_runner.clone() else {
            return;
        };
        if runner(provider.as_deref()) {
            self.notify("credentials updated");
            let _ = self.send_request(RunRequest::RefreshProviders);
        } else {
            self.notify("login cancelled");
        }
    }
}
