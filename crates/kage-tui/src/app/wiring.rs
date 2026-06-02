//! App construction and host-wiring setters.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    /// Construct an app that pushes prompts into `requests`. The
    /// receiver side is owned by the host's worker driver.
    #[must_use]
    pub fn new(buffer: SharedBuffer, requests: Sender<RunRequest>) -> Self {
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
            plugin_keybindings: Vec::new(),
            config_keybindings: Vec::new(),
            plugin_widgets: Vec::new(),
            plugin_widget_texts: Vec::new(),
            plugin_status: None,
            plugin_status_cache: Vec::new(),
            plugin_usage: None,
            plugin_compact_request: None,
            plugin_session_list: None,
            plugin_fork_request: None,
            plugin_switch_request: None,
            plugin_theme_state: None,
            plugin_theme_request: None,
            autocomplete_providers: Vec::new(),
            input_completion: None,
            completion_workdir: None,
            themes_dir: None,
            terminal_hooks: None,
            plugin_header: None,
            plugin_footer: None,
            plugin_header_lines: Vec::new(),
            plugin_footer_lines: Vec::new(),
            search_line: None,
            search_pattern: None,
            search_match_set: std::collections::HashSet::new(),
            search_match_version: 0,
            mouse_drag_anchor: None,
            context_menu: None,
            pending_mouse_capture: None,
            screen_selection: None,
            captured_rows: std::collections::BTreeMap::new(),
            last_cursor_style: None,
            session_usage: None,
            cancel_flag: None,
            steering: None,
            toasts: None,
            dialog_rx: None,
            plugin_overlay: None,
            active_dialog: None,
        }
    }

    /// Hand the App a shared session-usage snapshot. While set, the
    /// renderer reserves a one-row modeline below the input card and
    /// paints the snapshot's model + token totals + context-window
    /// fill. Pass `None` (or never call this) to keep the modeline
    /// collapsed.
    pub fn set_session_usage(&mut self, usage: crate::usage::SharedSessionUsage) {
        self.session_usage = Some(usage);
    }

    /// Register the host's cancellation flag so [`InputAction::Cancel`]
    /// and `:cancel` can flip it directly on the event-loop thread,
    /// bypassing the worker request queue. Without this, cancellation
    /// of an in-flight turn does not take effect until the turn ends
    /// naturally because the worker thread is blocked inside the
    /// agent loop and cannot drain its request channel.
    pub fn set_cancel_flag(&mut self, flag: CancelFlag) {
        self.cancel_flag = Some(flag);
    }

    /// Register a steering queue. With one set, a text-only `Submit`
    /// issued while [`Self::is_run_in_flight`] is true pushes into the
    /// queue and shows a toast; the agent loop's `get_steering` hook
    /// drains it at the next turn boundary. Without one, every
    /// `Submit` goes through the normal worker channel.
    pub fn set_steering_queue(&mut self, queue: crate::events::SharedSteering) {
        self.steering = Some(queue);
    }

    /// Whether the host worker is currently inside `run_with_hooks`.
    /// Reads the shared session-usage `working` flag the worker
    /// flips on entry and exit. Returns `false` when no usage
    /// snapshot is registered (the host opted out of the modeline).
    pub(crate) fn is_run_in_flight(&self) -> bool {
        self.session_usage
            .as_ref()
            .and_then(|u| u.lock().ok().map(|g| g.working))
            .unwrap_or(false)
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
    /// expired entries in the process. Returns `None` when no toast
    /// queue is registered or the lock is poisoned.
    pub(crate) fn live_toasts(&self) -> Vec<Toast> {
        let Some(handle) = &self.toasts else {
            return Vec::new();
        };
        let now = Instant::now();
        let _ = toast::prune_expired(handle, now);
        handle
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Earliest deadline at which a live toast will expire, used by
    /// the event loop to wake up just in time to repaint without
    /// waiting for an unrelated key event.
    pub(crate) fn next_toast_deadline(&self) -> Option<Instant> {
        let handle = self.toasts.as_ref()?;
        let q = handle.lock().ok()?;
        q.iter().map(|t| t.expires_at).min()
    }

    /// Trip the registered cancel flag if any, then forward a
    /// `RunRequest::Cancel` to the worker for any extra cleanup that
    /// arm performs (currently it just calls `.cancel()` again, which
    /// is idempotent - the channel send is a fallback for hosts that
    /// have not registered a flag via [`Self::set_cancel_flag`]).
    pub(crate) fn trip_cancel(&mut self) {
        if let Some(flag) = &self.cancel_flag {
            flag.cancel();
        }
        let _ = self.send_request(RunRequest::Cancel);
    }

    /// Whether the host has registered a session-usage handle. Used
    /// by the layout split to decide if the modeline row claims a
    /// line of vertical space.
    pub(crate) fn modeline_visible(&self) -> bool {
        self.session_usage.is_some()
    }

    /// Snapshot the session-usage handle, returning `None` when the
    /// host has not registered one or the lock is poisoned.
    pub(crate) fn session_usage_snapshot(&self) -> Option<crate::usage::SessionUsage> {
        self.session_usage
            .as_ref()
            .and_then(|h| h.lock().ok().map(|g| g.clone()))
    }

    /// Register the plugin commands the host wants exposed in the
    /// palette and on the `:` line. Names that collide with built-in
    /// specs are dropped; the host should log a warning at
    /// registration time.
    ///
    /// Builds one [`CommandSpec`] per plugin command, leaking the
    /// owned name, description, and per-arg schema into `&'static`
    /// storage so plugin commands participate in the same completion
    /// engine the builtins use. The leaked storage is bounded by the
    /// number of plugin commands the user installs.
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
    /// at the moment of opening. Without this, `Ctrl+R` is a no-op.
    pub fn set_session_lister(&mut self, lister: SessionLister) {
        self.session_lister = Some(lister);
    }

    /// Register the closure that produces the `:tree` session forest
    /// at open time. Without this, `:tree` reports it is unavailable.
    pub fn set_session_tree_source(&mut self, source: SessionTreeSource) {
        self.session_tree_source = Some(source);
    }

    /// Replace the list of plugin-supplied status-bar widgets.
    /// `render(width)` runs once per redraw inside the plugin runtime's
    /// Lua mutex; widgets that produce a non-empty string are painted
    /// on the right edge of the status bar in registration order.
    pub fn set_plugin_widgets(&mut self, widgets: Vec<Arc<kage_plugin::LuaWidget>>) {
        self.plugin_widgets = widgets;
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

    /// Wire the theme snapshot and pending-switch slots so
    /// `kage.theme.*` can read the active theme / list and request a
    /// switch. Without these the read APIs see empty values and
    /// `kage.theme.set` is a no-op.
    pub fn set_plugin_theme(
        &mut self,
        state: kage_plugin::SharedThemeState,
        request: kage_plugin::SharedThemeRequest,
    ) {
        self.plugin_theme_state = Some(state);
        self.plugin_theme_request = Some(request);
    }

    /// Wire the header/footer chrome slots populated by
    /// `kage.ui.set_header` / `kage.ui.set_footer`. Each redraw the
    /// active renderer (if any) is called with the row width and its
    /// styled lines replace the built-in status bar / modeline. Without
    /// this the Lua calls still register a renderer but the host never
    /// paints it.
    pub fn set_plugin_chrome(
        &mut self,
        header: kage_plugin::SharedChrome,
        footer: kage_plugin::SharedChrome,
    ) {
        self.plugin_header = Some(header);
        self.plugin_footer = Some(footer);
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

    /// Apply the configured startup theme (bundled name or a user
    /// `<name>.toml`). Silent on success; a bad name surfaces an
    /// inline error and leaves the default palette in place. Call
    /// after [`Self::set_themes_dir`] so user themes resolve.
    pub fn apply_startup_theme(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        self.apply_theme_resolved(name, false);
    }

    /// Register the plugin keybindings the App should dispatch.
    /// `chords` are canonical strings from the plugin runtime; an
    /// entry that fails to parse is dropped (the runtime already
    /// validated the grammar, so this only guards internal drift).
    pub fn set_plugin_keybindings(&mut self, chords: Vec<String>) {
        self.plugin_keybindings = chords
            .into_iter()
            .filter_map(|c| Chord::parse(&c).map(|m| (m, c)))
            .collect();
    }

    /// Register `[keybindings]` config entries: `chord -> command
    /// line`. A matching key runs the command through the cmdline
    /// executor, so anything `:` can do (including `quit` and plugin
    /// commands) is bindable. Returns one message per entry whose
    /// chord did not parse so the caller can surface it; a bad entry
    /// is dropped, never silently "sort of" applied.
    #[must_use]
    pub fn set_config_keybindings(&mut self, entries: Vec<(String, String)>) -> Vec<String> {
        let mut errors = Vec::new();
        self.config_keybindings = entries
            .into_iter()
            .filter_map(|(chord, command)| {
                if let Some(m) = Chord::parse(&chord) {
                    Some((m, chord, command))
                } else {
                    errors.push(format!(
                        "keybindings: cannot parse chord `{chord}` (bound to `{command}`)"
                    ));
                    None
                }
            })
            .collect();
        errors
    }

    /// Wire the channel the worker pushes blocking [`PluginDialog`]
    /// requests onto. Without this, `kage.ui.select` has nowhere to
    /// surface and the worker's send fails, which it treats as a
    /// cancel (the plugin call returns `nil`).
    pub fn set_plugin_dialog(&mut self, rx: std::sync::mpsc::Receiver<PluginDialog>) {
        self.dialog_rx = Some(rx);
    }

    pub(crate) fn refresh_plugin_widget_texts(&mut self, width: u16) {
        self.plugin_widget_texts = self
            .plugin_widgets
            .iter()
            .map(|w| w.render(width))
            .collect();
        self.plugin_header_lines = self
            .plugin_header
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|g| g.clone()))
            .map(|c| c.render(width))
            .unwrap_or_default();
        self.plugin_footer_lines = self
            .plugin_footer
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|g| g.clone()))
            .map(|c| c.render(width))
            .unwrap_or_default();
        self.plugin_status_cache.clear();
        if let Some(status) = self.plugin_status.as_ref()
            && let Ok(map) = status.lock()
        {
            self.plugin_status_cache
                .extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(usage_slot) = self.plugin_usage.as_ref()
            && let Some(snap) = self.session_usage_snapshot()
            && let Ok(mut slot) = usage_slot.lock()
        {
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

    /// Drain any pending `kage.compact()` request and forward it as
    /// [`RunRequest::CompactNow`] to the worker. The optional prompt
    /// is currently advisory; PE.C.4 will wire it into the compact
    /// hook.
    pub(crate) fn drain_plugin_compact_request(&mut self) {
        let Some(slot) = self.plugin_compact_request.as_ref() else {
            return;
        };
        let pending = slot.lock().ok().and_then(|mut g| g.take());
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
        let pending = slot.lock().ok().and_then(|mut g| g.take());
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
        let pending = slot.lock().ok().and_then(|mut g| g.take());
        if let Some(target) = pending {
            let _ = self.send_request(RunRequest::SwitchSession(target));
        }
    }

    /// Refresh the theme snapshot `kage.theme.*` reads, then drain a
    /// pending `kage.theme.set` and apply it on this thread (the same
    /// path as `:theme set`, so an unknown name surfaces an inline
    /// error rather than failing silently).
    pub(crate) fn drain_plugin_theme(&mut self) -> bool {
        let pending = self
            .plugin_theme_request
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|mut g| g.take()));
        if let Some(name) = pending {
            self.apply_theme_by_name(&name);
            return true;
        }
        false
    }

    /// Refresh the `kage.theme.*` snapshot: the current theme name plus
    /// the available-themes list. The latter scans the themes directory
    /// from disk, so this is far too costly to run on every event-loop
    /// wake (which fires at the streaming tick rate while the agent
    /// works). Plugins read this snapshot on human-timescale actions
    /// (opening a theme picker), so the loop refreshes it on a slow
    /// fixed cadence instead.
    pub(crate) fn refresh_plugin_theme_state(&mut self) {
        if let Some(state) = self.plugin_theme_state.as_ref()
            && let Ok(mut s) = state.lock()
        {
            s.current = crate::theme::current().name;
            s.available = crate::theme::Theme::available_names(self.themes_dir.as_deref());
        }
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
        if let Ok(mut s) = slot.lock() {
            *s = entries;
        }
    }
}
