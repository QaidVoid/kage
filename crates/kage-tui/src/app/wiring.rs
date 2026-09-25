//! App construction and host-wiring setters.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    /// Construct an app that pushes prompts into `requests`. The
    /// receiver side is owned by the host's worker driver.
    #[must_use]
    pub fn new(buffer: SharedBuffer, requests: Sender<RunRequest>) -> Self {
        let (attach_tx, attach_rx) = std::sync::mpsc::channel();
        Self {
            buffer,
            input: InputState::new(),
            requests,
            model_choices: Vec::new(),
            picker: None,
            picker_kind: None,
            session_scope_all: false,
            settings_overlay: None,
            session_tree: None,
            help_overlay: None,
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
            plugin_usage: None,
            plugin_compact_request: None,
            plugin_session_list: None,
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
            pending_login: None,
            attach_tx,
            attach_rx,
            plugin_overlay: None,
            active_dialog: None,
            pending_tree_delete: None,
            engine_rx: None,
            active_session: None,
            approval_panel: None,
            pending_permission: None,
            permission_queue: std::collections::VecDeque::new(),
            run_started: None,
            key_labels: KeyLabels::default(),
            start_info: None,
            pending: Vec::new(),
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

    /// Whether a run is in flight, from the engine's last reported
    /// session state. `false` when no usage snapshot is registered.
    pub(crate) fn is_run_in_flight(&self) -> bool {
        self.session_usage.as_ref().is_some_and(|u| lock(u).working)
    }

    /// Register the shared toast queue. While set, App-internal
    /// `notify(...)` calls and external sinks holding a clone of
    /// the same handle push into a top-right overlay. Without it
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

    /// Ask the engine to cancel the in-flight run.
    pub(crate) fn trip_cancel(&mut self) {
        let _ = self.send_request(RunRequest::Cancel);
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
            let spec = self.leaked_plugin_spec(cmd);
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
    }

    /// Return the previously-leaked spec for `cmd` when an equal
    /// command was registered in an earlier call, so a hot reload of
    /// unchanged plugins reuses the old `&'static` storage. Otherwise
    /// leak a fresh spec and remember it.
    fn leaked_plugin_spec(&mut self, cmd: &PluginCommand) -> &'static CommandSpec {
        let reused = self
            .plugin_commands_leaked
            .iter()
            .find(|(known, _)| known == cmd)
            .map(|(_, spec)| *spec);
        if let Some(spec) = reused {
            return spec;
        }
        let name_static: &'static str = Box::leak(cmd.name.clone().into_boxed_str());
        let desc_static: &'static str =
            Box::leak(format!("{}  [plugin]", cmd.description).into_boxed_str());
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
        self.plugin_commands_leaked.push((cmd.clone(), spec));
        spec
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

    /// Register the status-bar widgets supplied by plugins. Marks the
    /// plugin text caches dirty so the next frame renders them at
    /// once instead of waiting for the refresh tick.
    pub fn set_plugin_widgets(&mut self, widgets: Vec<Arc<kage_plugin::LuaWidget>>) {
        self.plugin_widgets = widgets;
        self.plugin_texts_dirty = true;
    }

    /// Wire the shared status map populated by `kage.set_status` /
    /// `kage.clear_status`. Without this, those Lua calls still
    /// succeed inside the runtime but the host status bar never paints
    /// the values.
    pub fn set_plugin_status(&mut self, status: kage_plugin::SharedStatus) {
        self.plugin_status = Some(status);
    }

    /// Wire the shared usage snapshot read by `kage.context_usage()`.
    /// Without this, plugins always see `nil`.
    pub fn set_plugin_usage(&mut self, usage: kage_plugin::SharedUsage) {
        self.plugin_usage = Some(usage);
    }

    /// Wire the shared pending-compact slot populated by
    /// `kage.compact(prompt?)`. Without this, plugins can still call
    /// the API but the host never dispatches the requested compaction.
    pub fn set_plugin_compact_request(&mut self, request: kage_plugin::SharedCompactRequest) {
        self.plugin_compact_request = Some(request);
    }

    /// Wire the shared session list `kage.session.list()` reads from.
    /// Without this, plugins always see an empty list.
    pub fn set_plugin_session_list(&mut self, list: kage_plugin::SharedSessionList) {
        self.plugin_session_list = Some(list);
    }

    /// Wire the shared pending-fork slot populated by
    /// `kage.session.fork(at?)`. Without this, plugins can call the
    /// API but the host never performs the fork.
    pub fn set_plugin_fork_request(&mut self, request: kage_plugin::SharedForkRequest) {
        self.plugin_fork_request = Some(request);
    }

    /// Wire the shared reseat slot populated by the `session_write`
    /// `kage.session.switch` / `fork_to`. Without this, a granted
    /// plugin can call the API but the host never reseats.
    pub fn set_plugin_switch_request(&mut self, request: kage_plugin::SharedSwitchRequest) {
        self.plugin_switch_request = Some(request);
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

    /// Wire the host hook [`App::run_login_flow`] uses to run an
    /// interactive credential login in the real terminal. `:login`
    /// is a no-op error without it.
    pub fn set_login_runner(&mut self, runner: LoginRunner) {
        self.login_runner = Some(runner);
    }

    /// Consume a pending `:login` (if any) and run its flow. Returns
    /// whether anything ran so the caller repaints.
    pub(crate) fn consume_pending_login(&mut self, tui: &mut Tui) -> bool {
        let Some(request) = self.pending_login.take() else {
            return false;
        };
        self.run_login_flow(tui, request);
        true
    }

    /// Suspend the TUI, run the host login hook for the request (a
    /// picker or a named provider), resume, and ask the worker to
    /// rebuild the provider registry on success.
    pub(crate) fn run_login_flow(&mut self, tui: &mut Tui, request: PendingLogin) {
        let Some(runner) = self.login_runner.clone() else {
            return;
        };
        let provider = match request {
            PendingLogin::Picker => None,
            PendingLogin::Provider(name) => Some(name),
        };
        tui.suspend();
        let saved = runner(provider.as_deref());
        tui.resume();
        if saved {
            self.notify("credentials updated");
            let _ = self.send_request(RunRequest::RefreshProviders);
        } else {
            self.notify("login cancelled");
        }
    }

    /// Snapshot the slot specs for one frame and report the frame's
    /// width and editor mode to the plugin runtime.
    pub(crate) fn slot_frame(&self, width: u16) -> kage_plugin::SlotSpecs {
        let Some(slots) = &self.slots else {
            return kage_plugin::SlotSpecs::default();
        };
        slots.report(width, mode_label(self.input.mode()));
        slots.specs()
    }

    /// The footer hint: the pending keys of a mapping sequence, else
    /// the keys of the open panel or overlay, else what the next keys
    /// do in the current state. Kept short so it fits at 80 columns
    /// next to the session facts.
    pub(crate) fn footer_hint(&mut self) -> String {
        let keys = self.sequencer.pending();
        if !keys.is_empty() {
            return format!("{} ...", kage_core::keymap::display_keys(keys));
        }
        if let Some(panel) = &self.approval_panel {
            return panel.hint();
        }
        if self.slash_palette.is_some() {
            return ["tab to complete", "enter to run", "esc to close"].join(HINT_SEP);
        }
        if self.help_overlay.is_some() {
            return ["up/down to scroll", "esc to close"].join(HINT_SEP);
        }
        if self.modal_open() {
            return String::new();
        }
        let now = Instant::now();
        let note = self
            .escalation
            .filter(|(_, until)| *until > now && !self.input.has_draft());
        match note {
            Some((keys::Escalation::QuitArmed, _)) => return "ctrl+c again to quit".to_owned(),
            Some((keys::Escalation::DraftCleared, _)) => {
                return "draft cleared, up restores it".to_owned();
            }
            None => {}
        }
        let working = self.is_working();
        let draft = !self.input.text().is_empty();
        if self.input.shell_armed() {
            return if draft {
                "enter to run the command"
            } else {
                "backspace to leave shell mode"
            }
            .to_owned();
        }
        let label =
            |app: &mut Self, action, what| app.key_label(action).map(|key| format!("{key} {what}"));
        let queue = label(self, "QueuePrompt", "to queue").filter(|_| working);
        let queues = label(self, "QueuePrompt", "queues").filter(|_| working);
        let (queue, queues) = (queue.as_deref(), queues.as_deref());
        let mut parts: Vec<&str> = Vec::new();
        if self.input.is_modeless() {
            match (working, draft) {
                (true, false) => parts.extend(queue.into_iter().chain(["esc to interrupt"])),
                (true, true) => {
                    parts.push("enter steers");
                    parts.extend(queues);
                    parts.push("esc clears");
                }
                (false, true) => parts.extend(["enter to send", "shift+enter for a newline"]),
                (false, false) if self.search_pattern.is_some() => {
                    parts.extend(["esc to clear the search", "? for shortcuts"]);
                }
                (false, false) => parts.extend(["? for shortcuts", "/ for commands"]),
            }
            return parts.join(HINT_SEP);
        }
        let help = label(self, "OpenHelp", "for shortcuts");
        let commands = label(self, "BeginCommand", "for commands");
        match self.input.mode() {
            Mode::Normal => {
                match (working, draft) {
                    (_, true) => parts.push("ctrl+c to clear"),
                    (true, false) => parts.push("ctrl+c to interrupt"),
                    (false, false) => {}
                }
                parts.push("i to type");
                parts.extend(help.as_deref());
                if !working && !draft {
                    parts.extend(commands.as_deref());
                }
            }
            Mode::Insert => match (working, draft) {
                (true, false) => parts.extend(queue.into_iter().chain(["ctrl+c to interrupt"])),
                (true, true) => {
                    parts.push("enter steers");
                    parts.extend(queues);
                    parts.push("ctrl+c clears");
                }
                (false, true) => parts.extend(["enter to send", "esc for normal mode"]),
                (false, false) => parts.push("esc for normal mode"),
            },
            Mode::Visual => parts.push("esc to leave visual mode"),
        }
        parts.join(HINT_SEP)
    }

    /// The key that runs `action` in the current editing state, as
    /// `ctrl+p`. A mapping from `init.lua` or `config.toml` wins over
    /// the defaults. Cached per keymap generation and editing state.
    pub(crate) fn key_label(&mut self, action: &'static str) -> Option<String> {
        let keymap = lock(&self.keymap);
        let state = self.edit_state();
        let key = (keymap.generation(), state);
        if self.key_labels.key != Some(key) {
            self.key_labels = KeyLabels {
                key: Some(key),
                labels: Vec::new(),
            };
        }
        if let Some((_, label)) = self.key_labels.labels.iter().find(|(a, _)| *a == action) {
            return label.clone();
        }
        let modes = state.modes();
        let entries = keymap.entries();
        let runs = |e: &&kage_core::keymap::Entry<'_>| {
            modes.contains(&e.mode)
                && matches!(e.mapping.rhs, Rhs::Action { name, .. } if name == action)
        };
        let label = entries
            .iter()
            .filter(runs)
            .find(|e| e.mapping.user_owned())
            .or_else(|| entries.iter().find(runs))
            .map(|e| e.lhs.iter().map(key_chord).collect::<String>());
        self.key_labels.labels.push((action, label.clone()));
        label
    }

    /// The working row text while a run is in flight: what kage is
    /// doing, the run's elapsed time and, when the next key would
    /// reach the editor with an empty draft, the key that interrupts
    /// the run. At `width` columns what kage is doing is cut first, so
    /// the time and the key stay.
    pub(crate) fn activity_label(&self, buffer: &crate::Buffer, width: u16) -> Option<String> {
        let started = self.run_started?;
        let approving = self.pending_permission.is_some();
        let doing = if approving {
            "Waiting for your approval".to_owned()
        } else {
            current_work(buffer)
        };
        let elapsed = started.elapsed();
        let elapsed = if elapsed.as_secs() < 60 {
            format!("{}s", elapsed.as_secs())
        } else {
            view::tool_view::format_elapsed(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        };
        let tail = if approving || !self.input.text().is_empty() {
            format!(" ({elapsed})")
        } else {
            let key = if self.input.is_modeless() {
                "esc"
            } else {
                "ctrl+c"
            };
            format!(" ({elapsed}, {key} to interrupt)")
        };
        let room = usize::from(width).saturating_sub(ACTIVITY_INDENT + tail.len());
        let doing = view::truncate_to_width(&doing, room, "...");
        Some(format!("{doing}{tail}"))
    }

    /// The active model's id: the engine's last report, else the
    /// model the host started with.
    pub(crate) fn model_id(&self, usage: Option<&crate::usage::SessionUsage>) -> Option<String> {
        match usage.map(|u| u.model.as_str()).filter(|m| !m.is_empty()) {
            Some(id) => Some(id.to_owned()),
            None => self.status_model.as_ref().map(|m| lock(m).clone()),
        }
    }

    /// The model picker's label for the model `id`, else the id.
    pub(crate) fn model_label(&self, id: Option<&str>) -> Option<String> {
        let id = id?;
        let label = self
            .model_choices
            .iter()
            .find(|item| item.value == id)
            .map_or(id, |item| item.label.as_str());
        Some(label.to_owned())
    }

    /// Keys for the start card's change hints, while it shows.
    pub(crate) fn start_keys(&mut self) -> view::StartKeys {
        view::StartKeys {
            model: self.key_label("OpenModelPicker"),
            thinking: self.key_label("CycleThinkingLevel"),
            sessions: self.key_label("OpenSessionPicker"),
        }
    }

    pub(crate) fn refresh_plugin_widget_texts(&mut self, width: u16) {
        self.plugin_widget_texts = self
            .plugin_widgets
            .iter()
            .map(|w| w.render(width))
            .collect();
        self.plugin_status_cache.clear();
        if let Some(status) = self.plugin_status.as_ref() {
            let map = lock(status);
            self.plugin_status_cache
                .extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(usage_slot) = self.plugin_usage.as_ref()
            && let Some(snap) = self.session_usage_snapshot()
        {
            let mut slot = lock(usage_slot);
            *slot = serde_json::json!({
                "model": snap.model,
                "input_tokens": snap.input_tokens,
                "output_tokens": snap.output_tokens,
                "cache_read_tokens": snap.cache_read_tokens,
                "cache_write_tokens": snap.cache_write_tokens,
                "current_context": snap.current_context,
                "context_window": snap.context_window,
                "working": snap.working,
            });
        }
    }

    /// Refresh the plugin text caches only when something observable
    /// can have changed: the coarse tick elapsed, the width moved, a
    /// (re)registration marked them dirty, or the plugin runtime
    /// reported fresh output.
    pub(crate) fn refresh_plugin_widget_texts_if_due(&mut self, width: u16) {
        const PLUGIN_TEXT_INTERVAL: Duration = Duration::from_millis(500);
        let due = self.plugin_texts_dirty
            || self.plugin_texts_width != width
            || self
                .plugin_texts_refreshed_at
                .is_none_or(|t| t.elapsed() >= PLUGIN_TEXT_INTERVAL);
        if !due {
            return;
        }
        self.plugin_texts_dirty = false;
        self.plugin_texts_width = width;
        self.plugin_texts_refreshed_at = Some(Instant::now());
        self.refresh_plugin_widget_texts(width);
    }

    /// Register the flags the plugin runtime sets when any retained
    /// output changed (`redraw`) and when block renderer output changed
    /// (`blocks`).
    pub fn set_plugin_redraw(
        &mut self,
        redraw: Arc<std::sync::atomic::AtomicBool>,
        blocks: Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.plugin_redraw = Some((redraw, blocks));
    }

    /// Whether plugin output changed since the last check. Marks the
    /// text caches for refresh, and re-measures blocks when block
    /// renderer output changed.
    pub(crate) fn take_plugin_redraw(&mut self) -> bool {
        use std::sync::atomic::Ordering;
        let Some((redraw, blocks)) = &self.plugin_redraw else {
            return false;
        };
        if blocks.swap(false, Ordering::Relaxed) {
            lock(&self.buffer).invalidate_all_heights();
        }
        let fresh = redraw.swap(false, Ordering::Relaxed);
        if fresh {
            self.plugin_texts_dirty = true;
        }
        fresh
    }

    /// Drain any pending `kage.compact()` request and forward it as
    /// [`RunRequest::CompactNow`] to the worker. The optional prompt
    /// is currently advisory; a future compaction hook will receive it.
    pub(crate) fn drain_plugin_compact_request(&mut self) {
        let Some(slot) = self.plugin_compact_request.as_ref() else {
            return;
        };
        let pending = lock(slot).take();
        if pending.is_some() {
            let _ = self.send_request(RunRequest::CompactNow);
        }
    }

    /// Drain any pending `kage.session.fork()` request and forward it
    /// as [`RunRequest::ForkSession`] to the worker. The worker copies
    /// the current session through entry `at` into a fresh session
    /// file.
    pub(crate) fn drain_plugin_fork_request(&mut self) {
        let Some(slot) = self.plugin_fork_request.as_ref() else {
            return;
        };
        let pending = lock(slot).take();
        if let Some(at) = pending {
            let _ = self.send_request(RunRequest::ForkSession { at });
        }
    }

    /// Drain any pending `session_write` reseat and relay it as
    /// [`RunRequest::SwitchSession`] so the worker applies it on the
    /// same path as a user-initiated resume/fork.
    pub(crate) fn drain_plugin_switch_request(&mut self) {
        let Some(slot) = self.plugin_switch_request.as_ref() else {
            return;
        };
        let pending = lock(slot).take();
        if let Some(target) = pending {
            let _ = self.send_request(RunRequest::SwitchSession(target));
        }
    }

    /// Recompile the palette when the highlight table changed since the
    /// last compile. Returns whether it did.
    pub(crate) fn refresh_highlights(&mut self) -> bool {
        let Some(shared) = self.highlights.as_ref() else {
            return false;
        };
        let hl = {
            let hl = lock(shared);
            if hl.generation() == self.highlights_generation {
                return false;
            }
            hl.clone()
        };
        self.highlights_generation = hl.generation();
        crate::theme::set_current(crate::theme::Theme::from_groups(&hl));
        lock(&self.buffer).invalidate_all_heights();
        true
    }

    /// Drain one pending blocking [`PluginDialog`] and open its
    /// overlay. Skipped while another overlay (picker or an earlier
    /// plugin dialog) is up: the worker stays parked and the request
    /// is taken on a later tick once the screen is free (the bridge is
    /// single-slot, so at most one is queued). An empty item list
    /// resolves immediately to "cancelled" rather than opening a dead
    /// picker.
    pub(crate) fn drain_plugin_dialog(&mut self) -> bool {
        if self.picker.is_some() || self.plugin_overlay.is_some() {
            return false;
        }
        let Some(rx) = self.dialog_rx.as_ref() else {
            return false;
        };
        let Ok(dialog) = rx.try_recv() else {
            return false;
        };
        match dialog {
            PluginDialog::Select {
                title,
                items,
                reply,
            } => {
                if items.is_empty() {
                    let _ = reply.send(None);
                    return false;
                }
                let picks = items
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| PickItem {
                        value: idx.to_string(),
                        label: item.label.clone(),
                        badge: None,
                        group: None,
                        right: None,
                    })
                    .collect();
                self.plugin_overlay = Some(Box::new(OverlayPicker::new(title, picks)));
                self.active_dialog = Some(PluginDialogState::Select { reply, items });
            }
            PluginDialog::Confirm {
                title,
                message,
                reply,
            } => {
                self.plugin_overlay = Some(Box::new(crate::overlay::ConfirmOverlay::new(
                    title, message,
                )));
                self.active_dialog = Some(PluginDialogState::Confirm { reply });
            }
            PluginDialog::Input {
                title,
                placeholder,
                reply,
            } => {
                let mut overlay = crate::overlay::InputOverlay::new(title);
                if let Some(hint) = placeholder {
                    overlay = overlay.with_placeholder(hint);
                }
                self.plugin_overlay = Some(Box::new(overlay));
                self.active_dialog = Some(PluginDialogState::Input { reply });
            }
            PluginDialog::Editor {
                title,
                prefill,
                reply,
            } => {
                let mut overlay = crate::overlay::EditorOverlay::new(title);
                if let Some(text) = prefill {
                    overlay = overlay.with_prefill(text);
                }
                self.plugin_overlay = Some(Box::new(overlay));
                self.active_dialog = Some(PluginDialogState::Editor { reply });
            }
        }
        true
    }

    /// Apply the newest pending [`PluginRefresh`] snapshot, if any.
    /// Drained between event polls; if the worker pushed more than one
    /// between ticks only the latest is applied. Returns `true` when a
    /// snapshot was applied so the caller can force a repaint.
    pub(crate) fn drain_plugin_refresh(&mut self) -> bool {
        let Some(rx) = self.plugin_refresh_rx.as_ref() else {
            return false;
        };
        let mut latest = None;
        while let Ok(snapshot) = rx.try_recv() {
            latest = Some(snapshot);
        }
        let Some(snapshot) = latest else {
            return false;
        };
        self.set_plugin_commands(snapshot.commands);
        self.set_plugin_widgets(snapshot.widgets);
        self.set_plugin_autocomplete(snapshot.autocomplete);
        lock(&self.buffer).invalidate_all_heights();
        if !snapshot.models.is_empty() {
            self.model_choices = snapshot.models;
        }
        true
    }

    /// Refresh the session-list snapshot read by `kage.session.list`.
    /// Builds `[{id, value}]` entries from the registered
    /// [`SessionLister`]; called once per redraw.
    pub(crate) fn refresh_plugin_session_list(&mut self) {
        let Some(slot) = self.plugin_session_list.as_ref() else {
            return;
        };
        let Some(lister) = self.session_lister.as_ref() else {
            return;
        };
        let items = lister(true);
        let entries: Vec<serde_json::Value> = items
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.label,
                    "value": p.value,
                })
            })
            .collect();
        let mut s = lock(slot);
        *s = entries;
    }
}

/// Separator between the parts of a footer hint.
const HINT_SEP: &str = " \u{B7} ";

/// Columns the `activity` component paints before its text.
const ACTIVITY_INDENT: usize = 2;

/// Labels from [`App::key_label`], valid for one keymap generation and
/// editing state.
#[derive(Debug, Default)]
pub(crate) struct KeyLabels {
    key: Option<(u64, EditState)>,
    labels: Vec<(&'static str, Option<String>)>,
}

/// One key in hint form: `<C-p>` reads `ctrl+p`, `<S-Tab>` reads
/// `shift+tab`, and a plain character stays itself.
fn key_chord(key: &kage_core::keymap::Key) -> String {
    let vim = key.to_string();
    let Some(mut inner) = vim.strip_prefix('<').and_then(|v| v.strip_suffix('>')) else {
        return vim;
    };
    let mut out = String::new();
    loop {
        let (prefix, rest) = match inner.split_at_checked(2) {
            Some(("C-", rest)) => ("ctrl+", rest),
            Some(("M-", rest)) => ("alt+", rest),
            Some(("S-", rest)) => ("shift+", rest),
            Some(("D-", rest)) => ("super+", rest),
            _ => break,
        };
        out.push_str(prefix);
        inner = rest;
    }
    match inner {
        "CR" => out.push_str("enter"),
        "BS" => out.push_str("backspace"),
        name => out.push_str(&name.to_lowercase()),
    }
    out
}

/// What the current run is doing, from the newest blocks back to the
/// prompt that started it: running a tool, thinking, or just working.
fn current_work(buffer: &crate::Buffer) -> String {
    use crate::view::tool_view::{ToolPhase, describe};
    for block in buffer.blocks().iter().rev() {
        match block {
            crate::Block::User { .. } => break,
            crate::Block::ToolCall {
                name,
                input,
                phase: ToolPhase::Running,
                ..
            } => {
                let label = describe(name, input);
                return format!("{} {}", label.verb_live, label.target)
                    .trim_end()
                    .to_owned();
            }
            crate::Block::Thinking { live: true, .. } => return "Thinking".to_owned(),
            _ => {}
        }
    }
    "Working".to_owned()
}
