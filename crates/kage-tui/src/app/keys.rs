//! Key-event dispatch across the App modes and overlays.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    pub(crate) fn dispatch_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        // Global escape hatch before any modal layer: ctrl+q quits.
        // It yields only when the user has explicitly bound ctrl+q to
        // something in `[keybindings]` - then their config wins and
        // quit is reachable via whatever chord they mapped `quit` to,
        // so the panic hatch stays for everyone who did not rebind it.
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl_q =
            key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q'));
        if ctrl_q
            && !self
                .config_keybindings
                .iter()
                .any(|(m, _, _)| m.matches(&key))
        {
            return Some(AppExit::Quit);
        }

        // Raw plugin terminal-input hooks see the key before any modal
        // layer (but never before the ctrl+q hatch above, so a hook
        // cannot wedge the UI). A truthy return consumes it.
        if let Some(hooks) = self.terminal_hooks.as_ref() {
            let snapshot = hooks.lock().map(|h| h.clone()).unwrap_or_default();
            if !snapshot.is_empty() {
                let descriptor = key_event_to_json(key);
                if snapshot.iter().any(|hook| hook.handle(&descriptor)) {
                    return None;
                }
            }
        }

        // A blocking plugin dialog is the top-most modal layer: the
        // worker is parked waiting for its answer.
        if self.plugin_overlay.is_some() {
            return self.dispatch_plugin_overlay_key(key);
        }

        // The right-click context menu is a light modal layer above
        // the pickers: while it is open it owns the keyboard.
        if self.context_menu.is_some() {
            return self.dispatch_context_menu_key(key);
        }

        // When the picker overlay is open, it owns the keyboard.
        if self.picker.is_some() {
            return self.dispatch_picker_key(key);
        }

        // The settings dialog is a modal sibling of the picker.
        if self.settings_overlay.is_some() {
            return self.dispatch_settings_key(key);
        }

        // The `:tree` session browser is also a modal sibling.
        if self.session_tree.is_some() {
            return self.dispatch_session_tree_key(key);
        }

        // The slash palette is its own modal layer, taking precedence
        // over the cmdline and search line.
        if self.slash_palette.is_some() {
            return self.dispatch_slash_palette_key(key);
        }

        // The `:` command line is the next-most-modal layer.
        if self.cmdline.is_some() {
            return self.dispatch_cmdline_key(key);
        }

        // The `/` search line is also modal while open.
        if self.search_line.is_some() {
            return self.dispatch_search_key(key);
        }

        // `[keybindings]` config is user-authoritative: checked before
        // plugin and builtin handling so a user can always reclaim a
        // key. The bound string runs through the same executor as the
        // `:` cmdline, so `quit`, plugin commands, everything works.
        if let Some(command) = self
            .config_keybindings
            .iter()
            .find(|(matcher, _, _)| matcher.matches(&key))
            .map(|(_, _, command)| command.clone())
        {
            let registry = cmdline_registry(&self.plugin_command_specs);
            return match self.run_command_validated(&command, &registry) {
                CommandResult::Done(exit) => exit,
                CommandResult::ValidationError(msg) => {
                    self.push_error(format!("keybinding `{command}`: {msg}"));
                    None
                }
            };
        }

        // Ctrl+V: attach an image from the OS clipboard. Terminals
        // with an image-only clipboard send the literal key (no
        // bracketed paste / no text), so this is the reliable
        // trigger; a text clipboard arrives as `Event::Paste`
        // instead and never reaches here. Honored after `[keybindings]`
        // so a user can still rebind ctrl+v.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('v')) {
            self.attach_clipboard_image();
            return None;
        }

        // Plugin keybindings win over builtin Normal/Insert handling
        // (last writer wins), but never over an open modal layer
        // above, the global quit hatch, or user config above.
        if let Some(chord) = self
            .plugin_keybindings
            .iter()
            .find(|(matcher, _)| matcher.matches(&key))
            .map(|(_, chord)| chord.clone())
        {
            let _ = self.send_request(RunRequest::InvokePluginKeybinding { chord });
            return None;
        }

        // The autocomplete popup is non-modal: it only consumes its
        // own navigation/accept/dismiss keys. Anything else falls
        // through to normal editing and then re-queries the stack.
        if self.input_completion.is_some() {
            let action = self
                .input_completion
                .as_mut()
                .expect("input completion present")
                .handle_key(key);
            match action {
                CompletionAction::Navigated => return None,
                CompletionAction::Dismissed => {
                    self.input_completion = None;
                    return None;
                }
                CompletionAction::Accepted(item) => {
                    self.accept_completion(&item);
                    return None;
                }
                CompletionAction::PassThrough => {}
            }
        }

        let actions = self.input.handle_key(key);
        for action in actions {
            if let Some(exit) = self.apply(action) {
                return Some(exit);
            }
        }
        self.refresh_input_completion();
        None
    }

    /// Re-query the autocomplete provider stack from the current
    /// prompt text and rebuild the popup. A no-op (and closes any open
    /// popup) unless plugins registered providers and the user is
    /// actively typing in the input pane.
    pub(crate) fn refresh_input_completion(&mut self) {
        let has_sources =
            !self.autocomplete_providers.is_empty() || self.completion_workdir.is_some();
        if !has_sources
            || self.input.focused_pane() != Pane::Input
            || self.input.mode() != Mode::Insert
        {
            self.input_completion = None;
            return;
        }
        let text = self.input.text();
        let cursor = self.input.cursor();
        let prefix = prefix_before_cursor(text, cursor);
        let mut items = Vec::new();
        for provider in self.autocomplete_providers.iter().rev() {
            let got = provider.complete(prefix, text, cursor);
            if !got.is_empty() {
                items = got;
                break;
            }
        }
        if items.is_empty()
            && let Some(workdir) = self.completion_workdir.as_deref()
        {
            items = file_completions(workdir, prefix, cursor);
        }
        self.input_completion = InputCompletion::new(items);
    }

    /// Splice an accepted candidate into the input. Uses the item's
    /// explicit `range` when present, otherwise replaces the prefix
    /// span the host computed. Re-queries afterward so a provider can
    /// offer a follow-up (e.g. path segments).
    pub(crate) fn accept_completion(&mut self, item: &kage_plugin::AutocompleteItem) {
        let cursor = self.input.cursor();
        let (start, end) = if let Some((from, to)) = item.range {
            (from, to)
        } else {
            let plen = prefix_before_cursor(self.input.text(), cursor).len();
            (cursor.saturating_sub(plen), cursor)
        };
        self.input.splice(start, end, &item.value);
        self.input_completion = None;
        self.refresh_input_completion();
    }

    pub(crate) fn dispatch_search_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let line = self.search_line.as_mut()?;
        match line.handle_key(key, &[], &EmptyResolver) {
            CommandLineEvent::Pending => None,
            CommandLineEvent::Cancelled => {
                self.search_line = None;
                None
            }
            CommandLineEvent::Submit(text) => {
                self.search_line = None;
                self.search_pattern = Some(text);
                self.jump_to_search_match(true);
                None
            }
        }
    }

    /// Build `(current_1_indexed, total)` for the right-edge match
    /// counter, or `None` when no search is active.
    pub(crate) fn compute_search_match_count(&self) -> Option<(usize, usize)> {
        let pattern = self.search_pattern.as_deref()?;
        let buf = self.buffer.lock().ok()?;
        let matches = buf.match_indices(pattern);
        let focus = buf.effective_focus().unwrap_or(usize::MAX);
        let current = matches
            .iter()
            .position(|i| *i == focus)
            .map_or(0, |p| p + 1);
        Some((current, matches.len()))
    }

    /// Jump focus to the next or previous block whose content matches
    /// the active search pattern. No-op when no pattern is set.
    pub(crate) fn jump_to_search_match(&mut self, forward: bool) {
        let Some(pattern) = self.search_pattern.clone() else {
            return;
        };
        if let Ok(mut buf) = self.buffer.lock() {
            let from = buf.effective_focus().unwrap_or(0);
            let next = if forward {
                buf.next_match(from, &pattern)
            } else {
                buf.prev_match(from, &pattern)
            };
            if let Some(n) = next {
                buf.set_focus(Some(n));
            }
        }
    }

    pub(crate) fn dispatch_cmdline_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let registry = cmdline_registry(&self.plugin_command_specs);
        let resolver = AppResolver {
            models: &self.model_choices,
            plugin_commands: &self.plugin_commands,
            sessions: self.session_lister.as_ref(),
            themes_dir: self.themes_dir.as_deref(),
        };
        let event = self
            .cmdline
            .as_mut()
            .map(|cl| cl.handle_key(key, &registry, &resolver));
        let event = event?;
        match event {
            CommandLineEvent::Pending => None,
            CommandLineEvent::Cancelled => {
                self.cmdline = None;
                None
            }
            CommandLineEvent::Submit(text) => {
                let result = self.run_command_validated(&text, &registry);
                match result {
                    CommandResult::Done(exit) => {
                        self.cmdline = None;
                        exit
                    }
                    CommandResult::ValidationError(msg) => {
                        if let Some(cl) = self.cmdline.as_mut() {
                            cl.set_error(msg);
                        }
                        None
                    }
                }
            }
        }
    }

    pub(crate) fn dispatch_slash_palette_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let action = self
            .slash_palette
            .as_mut()
            .map(|sp| crate::overlay::OverlayWidget::handle_key(sp, key))?;
        match action {
            OverlayAction::Stay | OverlayAction::PropagateKey => None,
            OverlayAction::Close => {
                self.slash_palette = None;
                None
            }
            OverlayAction::Resolve(value) => {
                let serde_json::Value::String(text) = value else {
                    self.slash_palette = None;
                    return None;
                };
                let registry = cmdline_registry(&self.plugin_command_specs);
                let result = self.run_command_validated(&text, &registry);
                match result {
                    CommandResult::Done(exit) => {
                        self.slash_palette = None;
                        exit
                    }
                    CommandResult::ValidationError(msg) => {
                        if let Some(sp) = self.slash_palette.as_mut() {
                            sp.set_error(msg);
                        }
                        None
                    }
                }
            }
        }
    }
}
