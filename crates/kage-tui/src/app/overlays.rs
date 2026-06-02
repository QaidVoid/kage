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
                let cur = crate::theme::current().name;
                self.notify(format!("theme: {cur} (try `:theme list`)"));
            }
            "list" => {
                let cur = crate::theme::current().name;
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
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.push_custom("kage:theme", format!("themes:\n{names}"), false);
                }
            }
            "set" => {
                if sub_rest.is_empty() {
                    self.push_error("theme set: usage `:theme set <name>`");
                    return;
                }
                self.apply_theme_by_name(sub_rest);
            }
            other => {
                self.push_error(format!(
                    "theme: unknown subcommand `{other}` (try list, set, current)"
                ));
            }
        }
    }

    pub(crate) fn apply_theme_by_name(&mut self, name: &str) {
        self.apply_theme_resolved(name, true);
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
        if let Ok(mut buf) = self.buffer.lock() {
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

    /// Attach the OS clipboard image (Ctrl+V, or `:attach` with no
    /// path). On failure the real reason is shown inline - clipboard
    /// unavailable, no image on it, encode failure - so a setup that
    /// does not work is diagnosable, never a silent no-op.
    pub(crate) fn attach_clipboard_image(&mut self) {
        match crate::image::clipboard_image()
            .and_then(|bytes| crate::image::from_bytes(&bytes, "clipboard"))
        {
            Ok(att) => {
                let note = att.placeholder();
                self.input.attach_image(att);
                self.notify(format!("attached {note}"));
            }
            Err(e) => self.push_error(format!("paste image: {e}")),
        }
    }

    /// Handle a bracketed paste. If the pasted text is just a path to
    /// an existing image (a drag-drop, or a copied file), attach it
    /// instead of inserting the raw path as prompt text; otherwise
    /// paste verbatim. A path that looks like an image but fails to
    /// load surfaces the error rather than silently pasting the path.
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
        if text.trim().is_empty()
            && let Ok(bytes) = crate::image::clipboard_image()
            && let Ok(att) = crate::image::from_bytes(&bytes, "clipboard")
        {
            let note = att.placeholder();
            self.input.attach_image(att);
            self.notify(format!("attached {note}"));
            return;
        }
        self.input.paste(text);
    }

    /// `:attach [path]` - queue an image for the next prompt. With a
    /// `path`, load that file; with no argument, pull the image off
    /// the OS clipboard. Every failure path is explained inline (no
    /// path + nothing on the clipboard, no clipboard helper, bad
    /// file, unsupported format, too large) so a non-working setup
    /// is diagnosable rather than silent.
    pub(crate) fn attach_image_path(&mut self, rest: &str) {
        let path = rest.trim();
        let result = if path.is_empty() {
            crate::image::clipboard_image()
                .and_then(|bytes| crate::image::from_bytes(&bytes, "clipboard"))
        } else {
            crate::image::load_path(std::path::Path::new(path))
        };
        match result {
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
                self.pending_mouse_capture = Some(false);
                self.notify("mouse capture off - drag selects via the terminal's native clipboard");
            }
            "on" | "enable" => {
                self.pending_mouse_capture = Some(true);
                self.notify("mouse capture on - drag selects blocks inside kage");
            }
            "toggle" | "" => {
                let now_enabled = !self.pending_mouse_capture.unwrap_or(true);
                self.pending_mouse_capture = Some(now_enabled);
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
    /// flip back); the initial open passes `false` so `Ctrl+R` with
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

    /// Open the `:settings` dialog, seeding it from the loaded
    /// user/project config plus live state (active theme/model).
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
            .and_then(|m| m.lock().ok().map(|g| g.clone()))
            .unwrap_or_else(|| cfg.provider.default_model.clone());
        let init = SettingsInit {
            themes: crate::theme::Theme::available_names(self.themes_dir.as_deref()),
            theme: crate::theme::current().name,
            models: self.model_choices.iter().map(|p| p.value.clone()).collect(),
            model,
            mouse: self.pending_mouse_capture.unwrap_or(cfg.ui.mouse),
            threshold: cfg.loop_settings.compaction_threshold,
            keybindings: cfg
                .keybindings
                .bindings
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            editor_modeless: matches!(cfg.ui.editor, kage_core::config::EditorMode::Modeless),
        };
        self.settings_overlay = Some(SettingsOverlay::new(init));
    }

    /// Apply the settings-dialog result: live-switch theme / mouse /
    /// model, then persist the four fields to the user config file
    /// (comment-preserving). A persistence failure is surfaced, not
    /// swallowed.
    pub(crate) fn apply_settings(&mut self, value: &serde_json::Value) {
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

        if !theme.is_empty() && theme != crate::theme::current().name {
            self.apply_theme_by_name(theme);
        }
        if let Some(mouse) = mouse {
            self.pending_mouse_capture = Some(mouse);
        }
        if let Some(modeless) = editor_modeless {
            // Live-apply: the input editor flips immediately.
            self.input.set_modeless(modeless);
        }
        let current_model = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()));
        if !model.is_empty() && current_model.as_deref() != Some(model) {
            let _ = self.send_request(RunRequest::SwitchModel(model.to_owned()));
        }

        let Some(path) = kage_core::config::Config::default_path() else {
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
        match cfg.save(&path) {
            Ok(()) => self.notify("settings saved"),
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
                        let _ = self.send_request(RunRequest::DeleteSession(path));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Drive the active plugin dialog overlay (`kage.ui.*`). The
    /// overlay owns its keys; on resolve/close the chosen value is
    /// sent back to the parked worker through [`Self::active_dialog`],
    /// mapped per the dialog kind, then the overlay is dismissed.
    pub(crate) fn dispatch_plugin_overlay_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.plugin_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay.as_mut(), key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.plugin_overlay = None;
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.cancelled();
                    let _ = state.reply().send(answer);
                }
            }
            OverlayAction::Resolve(value) => {
                self.plugin_overlay = None;
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.resolved(&value);
                    let _ = state.reply().send(answer);
                }
            }
        }
        None
    }
}
