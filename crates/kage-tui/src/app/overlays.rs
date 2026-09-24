//! Theme, clipboard/paste, and overlay (picker/settings/tree) handling.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    pub(crate) fn run_theme_command(&mut self, rest: &str) {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("");
        let sub_rest = parts.next().unwrap_or("").trim();
        match sub {
            "" | "current" => {
                let cur = crate::theme::current().name.clone();
                self.notify(format!("theme: {cur} (try `/theme list`)"));
            }
            "list" => {
                let cur = crate::theme::current().name.clone();
                let names = crate::theme::Theme::available_names(self.themes_dir.as_deref())
                    .iter()
                    .map(|n| {
                        if *n == cur {
                            format!("* {n}")
                        } else {
                            format!("  {n}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut buf = lock(&self.buffer);
                buf.push_custom("kage:theme", format!("themes:\n{names}"), false);
            }
            "set" => {
                if sub_rest.is_empty() {
                    self.push_error("theme set: usage `/theme set <name>`");
                    return;
                }
                self.set_option("theme", OptionValue::Str(sub_rest.to_owned()));
            }
            other => {
                self.push_error(format!(
                    "theme: unknown subcommand `{other}` (try list, set, current)"
                ));
            }
        }
    }

    /// Set option `name` with source `runtime`, through the plugin
    /// runtime when one is wired so `option_set` fires. The change
    /// applies once it reaches the store. A rejected value surfaces
    /// inline.
    pub(crate) fn set_option(&mut self, name: &str, value: OptionValue) {
        let result = match &self.option_setter {
            Some(set) => set(name, value),
            None => lock(&self.options)
                .set(name, value, OptionSource::Runtime)
                .map(drop)
                .map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            self.push_error(err);
        }
    }

    /// Apply the option changes queued in the store since the last
    /// call. Returns whether anything was applied.
    pub(crate) fn apply_option_changes(&mut self) -> bool {
        let changes = lock(&self.options).take_changes();
        let mut applied = false;
        for change in changes.into_iter().filter(|c| c.old != c.new) {
            let announce = change.source == OptionSource::Runtime;
            applied |= self.apply_option(change.name, &change.new, announce);
        }
        applied
    }

    /// Apply one live option value. Options without a live effect
    /// return `false`. `announce` toasts a theme switch.
    pub(crate) fn apply_option(&mut self, name: &str, value: &OptionValue, announce: bool) -> bool {
        match (name, value) {
            ("theme", OptionValue::Str(theme)) => self.apply_theme_resolved(theme, announce),
            ("mouse", OptionValue::Bool(on)) => self.pending_mouse_capture = Some(*on),
            ("editor", OptionValue::Str(editor)) => self.input.set_modeless(editor == "modeless"),
            ("input_min_lines" | "input_max_lines", _) => {
                let (min, max) = {
                    let store = lock(&self.options);
                    let rows = |name| {
                        store
                            .get(name)
                            .and_then(OptionValue::as_int)
                            .and_then(|n| u16::try_from(n).ok())
                            .unwrap_or(1)
                    };
                    (rows("input_min_lines"), rows("input_max_lines"))
                };
                crate::layout::set_input_bounds(min, max);
            }
            _ => return false,
        }
        true
    }

    /// Resolve `name` against the bundled set and the user theme
    /// directory, then make it the active palette. `announce` toasts
    /// the switch (`:theme set`, settings, plugin); startup passes
    /// `false`. Resolution failures (unknown name, unreadable file,
    /// bad TOML) surface inline rather than failing silently.
    pub(crate) fn apply_theme_resolved(&mut self, name: &str, announce: bool) {
        let theme = match crate::theme::Theme::resolve(name, self.themes_dir.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                self.push_error(format!("theme: {e}"));
                return;
            }
        };
        crate::theme::set_current(theme);
        {
            let mut buf = lock(&self.buffer);
            // Force a fresh layout pass: every block's cached height
            // was measured against the prior theme's bubble
            // background, which doesn't change geometry but
            // invalidating is cheap and protects against future
            // theme-driven height tweaks (different rule glyph
            // widths, etc.).
            buf.invalidate_all_heights();
        }
        if announce {
            self.notify(format!("theme: {name}"));
        }
    }

    /// Queue an async OS-clipboard image read (Ctrl+V, or `:attach`
    /// with no path). The arboard read can block for hundreds of ms
    /// on some compositors or a stalled clipboard owner, so it runs
    /// on a background thread; the result surfaces through
    /// [`Self::drain_clipboard_attach`] on a later loop pass.
    pub(crate) fn request_clipboard_attach(&mut self) {
        let tx = self.attach_tx.clone();
        std::thread::spawn(move || {
            let result = crate::image::clipboard_image()
                .and_then(|bytes| crate::image::from_bytes(&bytes, "clipboard"));
            let _ = tx.send(result);
        });
    }

    /// Drain completed async clipboard attaches. Success attaches the
    /// image and toasts; failure pushes the real reason inline so a
    /// setup that does not work is diagnosable, never a silent no-op.
    pub(crate) fn drain_clipboard_attach(&mut self) {
        while let Ok(result) = self.attach_rx.try_recv() {
            match result {
                Ok(att) => {
                    let note = att.placeholder();
                    self.input.attach_image(att);
                    self.notify(format!("attached {note}"));
                }
                Err(e) => self.push_error(format!("paste image: {e}")),
            }
        }
    }

    /// Handle a bracketed paste. If the pasted text is just a path to
    /// an existing image (a drag-drop, or a copied file), attach it
    /// instead of inserting the raw path as prompt text; otherwise
    /// route the text to the active surface: a plugin overlay's
    /// `handle_paste`, the slash palette, the `:` cmdline, or the
    /// `/` search line. A modal that does not accept text swallows
    /// the paste so it cannot land in the hidden main input and
    /// silently vanish. With no overlay open the main input receives
    /// it verbatim. A path that looks like an image but fails to load
    /// surfaces the error rather than silently pasting the path.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        // A copied/dragged image *file* arrives as its path.
        if let Some(path) = crate::image::path_if_image(text) {
            match crate::image::load_path(&path) {
                Ok(att) => {
                    let note = att.placeholder();
                    self.input.attach_image(att);
                    self.notify(format!("attached {note}"));
                }
                Err(e) => self.push_error(format!("attach: {e}")),
            }
            return;
        }
        // A copied screenshot/image can't ride in the paste text; if
        // the terminal does deliver an empty bracketed paste for it,
        // treat that as an image-paste attempt (Ctrl+V is also
        // intercepted directly for terminals that send no event).
        if text.trim().is_empty() {
            self.request_clipboard_attach();
            return;
        }
        if let Some(overlay) = self.plugin_overlay.as_mut() {
            overlay.handle_paste(text);
            return;
        }
        if self.permission_overlay.is_some()
            || self.context_menu.is_some()
            || self.picker.is_some()
            || self.settings_overlay.is_some()
            || self.session_tree.is_some()
        {
            return;
        }
        if let Some(palette) = self.slash_palette.as_mut() {
            palette.paste(text);
            return;
        }
        if let Some(cl) = self.cmdline.as_mut() {
            let registry = cmdline_registry(&self.plugin_command_specs);
            let resolver = AppResolver {
                models: &self.model_choices,
                plugin_commands: &self.plugin_commands,
                sessions: self.session_lister.as_ref(),
                themes_dir: self.themes_dir.as_deref(),
            };
            cl.paste_str(text, &registry, &resolver);
            return;
        }
        if let Some(line) = self.search_line.as_mut() {
            let empty: [&CommandSpec; 0] = [];
            line.paste_str(text, &empty, &EmptyResolver);
            return;
        }
        self.input.paste(text);
    }

    /// `:attach [path]` - queue an image for the next prompt. With a
    /// `path`, load that file; with no argument, pull the image off
    /// the OS clipboard (off-thread, like Ctrl+V). Every failure path
    /// is explained inline (no path + nothing on the clipboard, no
    /// clipboard helper, bad file, unsupported format, too large) so
    /// a non-working setup is diagnosable rather than silent.
    pub(crate) fn attach_image_path(&mut self, rest: &str) {
        let path = rest.trim();
        if path.is_empty() {
            self.request_clipboard_attach();
            return;
        }
        match crate::image::load_path(std::path::Path::new(path)) {
            Ok(att) => {
                let note = att.placeholder();
                self.input.attach_image(att);
                self.notify(format!("attached {note}"));
            }
            Err(e) => self.push_error(format!("attach: {e}")),
        }
    }

    pub(crate) fn notify(&mut self, msg: impl Into<String>) {
        let Some(toasts) = &self.toasts else {
            return;
        };
        toast::push_toast(
            toasts,
            Toast::with_kind(msg, ToastKind::Info, toast::DEFAULT_TOAST_DURATION),
        );
    }

    pub(crate) fn run_mouse_command(&mut self, rest: &str) {
        match rest {
            "off" | "disable" => {
                self.set_option("mouse", OptionValue::Bool(false));
                self.notify("mouse capture off - drag selects via the terminal's native clipboard");
            }
            "on" | "enable" => {
                self.set_option("mouse", OptionValue::Bool(true));
                self.notify("mouse capture on - drag selects blocks inside kage");
            }
            "toggle" | "" => {
                let now_enabled =
                    lock(&self.options).get("mouse") != Some(&OptionValue::Bool(true));
                self.set_option("mouse", OptionValue::Bool(now_enabled));
                let state = if now_enabled { "on" } else { "off" };
                self.notify(format!("mouse capture {state}"));
            }
            other => {
                self.push_error(format!("mouse: unknown arg `{other}` (try on/off/toggle)"));
            }
        }
    }

    /// Title for the session picker, encoding the active scope and
    /// the `Ctrl+A` toggle so the binding is discoverable in-place.
    pub(crate) fn session_picker_title(all: bool) -> &'static str {
        if all {
            "Resume session - all dirs (Ctrl+A: this dir)"
        } else {
            "Resume session - this dir (Ctrl+A: all dirs)"
        }
    }

    /// (Re)build the session picker for the current
    /// [`Self::session_scope_all`] scope. `allow_empty` keeps the
    /// modal open with no rows (used by the toggle so the user can
    /// flip back); the initial open passes `false` so `Ctrl+S` with
    /// nothing to resume is a no-op rather than an empty dialog.
    pub(crate) fn open_session_picker(&mut self, allow_empty: bool) {
        let Some(lister) = self.session_lister.as_ref() else {
            return;
        };
        let items = lister(self.session_scope_all);
        if items.is_empty() && !allow_empty {
            return;
        }
        let title = Self::session_picker_title(self.session_scope_all);
        self.picker = Some(OverlayPicker::new(title, items));
        self.picker_kind = Some(PickerKind::Session);
    }

    /// Open the F3 history-jump picker: one row per user prompt,
    /// assistant reply, tool call, and notice, newest first.
    /// Resolving scrolls the focused block into view.
    pub(crate) fn open_jump_picker(&mut self) {
        let items: Vec<crate::picker::PickItem> = lock(&self.buffer)
            .jump_targets(72)
            .into_iter()
            .rev()
            .map(|(idx, label)| crate::picker::PickItem {
                value: idx.to_string(),
                label,
                badge: None,
                group: None,
                right: None,
            })
            .collect();
        if items.is_empty() {
            return;
        }
        self.picker = Some(OverlayPicker::new_ordered("Jump to message", items));
        self.picker_kind = Some(PickerKind::Jump);
    }

    pub(crate) fn dispatch_picker_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        // Ctrl+A toggles the session picker between this-directory and
        // all-directories scope, rebuilding it in place. Intercepted
        // before the picker sees the key (its Char arm would
        // otherwise treat `a` as search input).
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if self.picker_kind == Some(PickerKind::Session)
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('a'))
        {
            self.session_scope_all = !self.session_scope_all;
            self.open_session_picker(true);
            return None;
        }
        let picker = self.picker.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(picker, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.picker = None;
                self.picker_kind = None;
            }
            OverlayAction::Resolve(value) => {
                let kind = self.picker_kind;
                self.picker = None;
                self.picker_kind = None;
                let serde_json::Value::String(value) = value else {
                    return None;
                };
                match kind {
                    Some(PickerKind::Model) => {
                        let _ = self.send_request(RunRequest::SwitchModel(value));
                    }
                    Some(PickerKind::Session) => {
                        let _ = self.send_request(RunRequest::ResumeSession(
                            std::path::PathBuf::from(value),
                        ));
                    }
                    Some(PickerKind::Jump) => {
                        // Focus drives the renderer's scroll-into-view
                        // pass; the index always came from a live
                        // block list, but a stale pick must not panic.
                        if let Ok(idx) = value.parse::<usize>() {
                            lock(&self.buffer).set_focus(Some(idx));
                        }
                    }
                    None => {}
                }
            }
        }
        None
    }

    pub(crate) fn dispatch_settings_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.settings_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.settings_overlay = None;
            }
            OverlayAction::Resolve(value) => {
                self.settings_overlay = None;
                self.apply_settings(&value);
            }
        }
        None
    }

    /// Open the `?` keyboard reference. Scroll-only: closing is the
    /// only outcome.
    pub(crate) fn open_help(&mut self) {
        self.help_overlay = Some(crate::overlay::HelpOverlay::new(self.input.is_modeless()));
    }

    /// Drive the help reference. `Close` and `Resolve` both dismiss:
    /// there is nothing to resolve.
    pub(crate) fn dispatch_help_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let Some(overlay) = self.help_overlay.as_mut() else {
            return;
        };
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            crate::overlay::OverlayAction::Close | crate::overlay::OverlayAction::Resolve(_) => {
                self.help_overlay = None;
            }
            crate::overlay::OverlayAction::Stay | crate::overlay::OverlayAction::PropagateKey => {}
        }
    }

    /// Open the `:settings` dialog, seeding it from the option store
    /// plus live state (active model) and the configured keybindings.
    pub(crate) fn open_settings(&mut self) {
        let workdir = self
            .completion_workdir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let cfg = match kage_core::config::Config::load_layered(&workdir) {
            Ok(c) => c,
            Err(e) => {
                self.push_error(format!("settings: config load failed: {e}"));
                return;
            }
        };
        let model = self
            .status_model
            .as_ref()
            .map_or_else(|| cfg.provider.default_model.clone(), |m| lock(m).clone());
        let store = lock(&self.options);
        let text = |name| {
            store
                .get(name)
                .and_then(OptionValue::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let thinking_level = text("thinking_level");
        #[allow(clippy::cast_possible_truncation)]
        let threshold = store
            .get("compaction_threshold")
            .and_then(OptionValue::as_float)
            .unwrap_or_default() as f32;
        let init = SettingsInit {
            themes: crate::theme::Theme::available_names(self.themes_dir.as_deref()),
            theme: text("theme"),
            models: self.model_choices.iter().map(|p| p.value.clone()).collect(),
            model,
            mouse: store.get("mouse").and_then(OptionValue::as_bool) == Some(true),
            threshold,
            keybindings: cfg
                .keybindings
                .bindings
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            editor_modeless: text("editor") == "modeless",
            thinking_level: if thinking_level.is_empty() {
                "off".into()
            } else {
                thinking_level
            },
            from_lua: kage_core::options::OPTIONS
                .iter()
                .map(|def| def.name)
                .filter(|name| store.source(name) == Some(OptionSource::Lua))
                .collect(),
        };
        drop(store);
        self.settings_overlay = Some(SettingsOverlay::new(init));
    }

    /// Apply the settings-dialog result: set the changed options (theme,
    /// mouse and editor apply live), switch the model, then persist the changed fields to the user config
    /// file (comment-preserving). A persistence failure is surfaced,
    /// not swallowed. An empty resolve means nothing changed.
    pub(crate) fn apply_settings(&mut self, value: &serde_json::Value) {
        self.apply_settings_at(value, kage_core::config::Config::default_path());
    }

    /// [`Self::apply_settings`] against an explicit config path
    /// (`None` skips persistence). Lets tests pin the file instead
    /// of the process environment.
    pub(crate) fn apply_settings_at(
        &mut self,
        value: &serde_json::Value,
        path: Option<std::path::PathBuf>,
    ) {
        if value.as_object().is_some_and(serde_json::Map::is_empty) {
            self.notify("settings: nothing changed");
            return;
        }
        let theme = value.get("theme").and_then(|v| v.as_str()).unwrap_or("");
        let model = value.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let mouse = value.get("mouse").and_then(serde_json::Value::as_bool);
        let threshold = value
            .get("compaction_threshold")
            .and_then(serde_json::Value::as_f64);
        // `None` when the key is absent or unrecognized; only an
        // explicit "modeless"/"vim" changes anything.
        let editor_modeless = match value.get("editor").and_then(|v| v.as_str()) {
            Some("modeless") => Some(true),
            Some("vim") => Some(false),
            _ => None,
        };
        // `None` when the key is absent or not a ladder string; the
        // worker parses the same six names, so anything else is
        // refused here and can never reach the config or the session.
        let thinking_level = match value.get("thinking_level").and_then(|v| v.as_str()) {
            Some(level @ ("off" | "minimal" | "low" | "medium" | "high" | "xhigh")) => Some(level),
            _ => None,
        };

        if !theme.is_empty() && theme != crate::theme::current().name {
            self.set_option("theme", OptionValue::Str(theme.to_owned()));
        }
        if let Some(mouse) = mouse {
            self.set_option("mouse", OptionValue::Bool(mouse));
        }
        if let Some(modeless) = editor_modeless {
            let editor = if modeless { "modeless" } else { "vim" };
            self.set_option("editor", OptionValue::Str(editor.to_owned()));
        }
        if let Some(t) = threshold {
            self.set_option("compaction_threshold", OptionValue::Float(t));
        }
        if let Some(level) = thinking_level {
            self.set_option("thinking_level", OptionValue::Str(level.to_owned()));
        }
        let current_model = self.status_model.as_ref().map(|m| lock(m).clone());
        if !model.is_empty() && current_model.as_deref() != Some(model) {
            let _ = self.send_request(RunRequest::SwitchModel(model.to_owned()));
        }

        let Some(path) = path else {
            self.push_error("settings: no home directory; not persisted");
            return;
        };
        let mut cfg = match kage_core::config::Config::load(&path) {
            Ok(c) => c,
            Err(e) => {
                self.push_error(format!("settings: config load failed: {e}"));
                return;
            }
        };
        if !theme.is_empty() {
            theme.clone_into(&mut cfg.ui.theme);
        }
        if !model.is_empty() {
            model.clone_into(&mut cfg.provider.default_model);
        }
        if let Some(mouse) = mouse {
            cfg.ui.mouse = mouse;
        }
        if let Some(t) = threshold {
            #[allow(clippy::cast_possible_truncation)]
            {
                cfg.loop_settings.compaction_threshold = t as f32;
            }
        }
        if let Some(modeless) = editor_modeless {
            cfg.ui.editor = if modeless {
                kage_core::config::EditorMode::Modeless
            } else {
                kage_core::config::EditorMode::Vim
            };
        }
        if let Some(level) = thinking_level {
            cfg.ui.thinking_level = Some(level.to_owned());
        }
        match cfg.save(&path) {
            Ok(()) => {
                self.notify("settings saved");
                if let Some(level) = thinking_level {
                    // Follows the save: the live session only adopts
                    // a change that actually stuck.
                    let _ = self.send_request(RunRequest::SetThinkingLevel(level.to_owned()));
                }
            }
            Err(e) => self.push_error(format!("settings: save failed: {e}")),
        }
    }

    /// Open the `:tree` session browser, querying the wired source.
    pub(crate) fn open_session_tree(&mut self) {
        let Some(source) = self.session_tree_source.as_ref() else {
            self.push_error("tree: session browser unavailable");
            return;
        };
        let nodes = source();
        if nodes.is_empty() {
            self.notify("no sessions to browse yet");
            return;
        }
        self.session_tree = Some(SessionTreeOverlay::new(nodes));
    }

    pub(crate) fn dispatch_session_tree_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.session_tree.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.session_tree = None;
            }
            OverlayAction::Resolve(value) => {
                self.session_tree = None;
                let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return None;
                }
                let path = std::path::PathBuf::from(path);
                match action {
                    "resume" => {
                        let _ = self.send_request(RunRequest::ResumeSession(path));
                    }
                    "fork" => {
                        let _ = self.send_request(RunRequest::ForkSessionFile(path));
                    }
                    "delete" => {
                        // Stage the deletion behind a confirmation;
                        // the request is only sent once the confirm
                        // overlay resolves Yes.
                        self.pending_tree_delete = Some(path);
                        self.plugin_overlay = Some(Box::new(crate::overlay::ConfirmOverlay::new(
                            "Delete session",
                            "Delete this session? This cannot be undone.",
                        )));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Drive the active plugin dialog overlay (`kage.ui.*`), or the
    /// `:tree` delete confirmation hosted in the same slot. The
    /// overlay owns its keys; on resolve/close the chosen value is
    /// sent back to the parked worker through [`Self::active_dialog`]
    /// (plugin dialogs) or completes the staged session deletion,
    /// then the overlay is dismissed.
    pub(crate) fn dispatch_plugin_overlay_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.plugin_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay.as_mut(), key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.plugin_overlay = None;
                self.pending_tree_delete = None;
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.cancelled();
                    let _ = state.reply().send(answer);
                }
            }
            OverlayAction::Resolve(value) => {
                self.plugin_overlay = None;
                if let Some(path) = self.pending_tree_delete.take() {
                    if value.as_bool() == Some(true) {
                        let _ = self.send_request(RunRequest::DeleteSession(path));
                        self.notify("session deleted");
                    }
                    return None;
                }
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.resolved(&value);
                    let _ = state.reply().send(answer);
                }
            }
        }
        None
    }

    /// Drive the active permission prompt. The overlay resolves on
    /// every dismissal path (Esc denies), so the waiting run always gets
    /// an answer.
    pub(crate) fn dispatch_permission_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.permission_overlay.as_mut()?;
        let action = crate::overlay::OverlayWidget::handle_key(overlay, key);
        let value = match action {
            OverlayAction::Stay | OverlayAction::PropagateKey => return None,
            OverlayAction::Resolve(value) => value,
            OverlayAction::Close => serde_json::Value::String("deny".to_owned()),
        };
        self.permission_overlay = None;
        self.answer_permission(match value.as_str() {
            Some("allow_once") => PermissionDecision::AllowOnce,
            Some("allow_always") => PermissionDecision::AllowAlways,
            _ => PermissionDecision::Deny,
        });
        None
    }
}
