//! Key-event dispatch across the App modes and overlays.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

/// How long an armed quit, or the note that the draft was cleared,
/// waits for the next press.
const ESCALATION_WINDOW: Duration = Duration::from_secs(2);

/// The key that asked [`App::escalate`] to step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Trigger {
    /// `Esc` in the modeless editor.
    Esc,
    /// The global `Ctrl+C` hatch.
    CtrlC,
}

/// What the last escalation step left for the next press.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Escalation {
    /// The draft was cleared. Up restores it.
    DraftCleared,
    /// Idle with an empty draft: one more `Ctrl+C` quits.
    QuitArmed,
}

/// What an open search line restores on `Esc`: the pattern and the
/// view from before it opened.
#[derive(Debug)]
pub(crate) struct SearchOrigin {
    pattern: Option<String>,
    focus: Option<usize>,
    scroll: Option<usize>,
}

impl App {
    /// The editing state keys resolve in, which selects the keymap
    /// modes to search.
    pub(crate) fn edit_state(&self) -> EditState {
        match (self.input.is_modeless(), self.input.mode()) {
            (true, _) | (false, Mode::Insert) => EditState::Insert,
            (false, Mode::Visual) => EditState::Visual,
            (false, Mode::Normal) if self.input.focused_pane() == Pane::Buffer => {
                EditState::NormalBuffer
            }
            (false, Mode::Normal) => EditState::NormalInput,
        }
    }

    /// Whether `init.lua` or `config.toml` mapped `key` alone in the
    /// current editing state. The global hatches and the external
    /// editor key yield to such a mapping.
    pub(crate) fn user_mapped(&self, key: &ratatui::crossterm::event::KeyEvent) -> bool {
        let Some(key) = key_from_event(key) else {
            return false;
        };
        match lock(&self.keymap).lookup(self.edit_state().modes(), &[key]) {
            Lookup::Exact(m) | Lookup::Prefix { exact: Some(m) } => m.user_owned(),
            _ => false,
        }
    }

    /// Whether a raw plugin terminal-input hook consumed `key`. See
    /// the call site in [`Self::dispatch_key`] for the ordering
    /// rationale.
    fn consumed_by_terminal_hook(&self, key: &ratatui::crossterm::event::KeyEvent) -> bool {
        let Some(hooks) = self.terminal_hooks.as_ref() else {
            return false;
        };
        let snapshot = lock(hooks).clone();
        if snapshot.is_empty() {
            return false;
        }
        let descriptor = key_event_to_json(*key);
        snapshot.iter().any(|hook| hook.handle(&descriptor))
    }

    /// Dispatch one key event through the modal layers, the keymap,
    /// and the editor grammar. Grows with every modal; the line count
    /// is layer plumbing, not complexity.
    pub(crate) fn dispatch_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        // Global escape hatches before any modal layer: ctrl+q quits,
        // ctrl+c escalates from every mode (insert, modeless, any open
        // overlay). Both yield to a mapping from `init.lua` or
        // `config.toml` on the chord, so `quit` and `:cancel` stay
        // reachable through whatever the user mapped instead.
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') if !self.user_mapped(&key) => return Some(AppExit::Quit),
                KeyCode::Char('c') if !self.user_mapped(&key) => {
                    return self.escalate(Trigger::CtrlC);
                }
                _ => {}
            }
        }
        self.escalation = None;

        // Raw plugin terminal-input hooks see the key before any modal
        // layer (but never before the global hatches above, so a hook
        // cannot wedge the UI). A truthy return consumes it.
        if self.consumed_by_terminal_hook(&key) {
            return None;
        }

        // A blocking plugin dialog is the top-most modal layer: the
        // worker is parked waiting for its answer.
        if self.plugin_overlay.is_some() {
            return self.dispatch_plugin_overlay_key(key);
        }

        // An approval panel is equally top-most: the worker is parked
        // inside the permission gate awaiting the decision. Ctrl+C
        // already hit the global cancel hatch above.
        if self.approval_panel.is_some() {
            return self.dispatch_permission_key(key);
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

        // The help reference is a scroll-only modal sibling: any key
        // it does not scroll with closes it.
        if self.help_overlay.is_some() {
            self.dispatch_help_key(key);
            return None;
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

        // The autocomplete popup is non-modal: it only consumes its
        // own navigation/accept/dismiss keys, before any mapping.
        // Anything else falls through to the keymap and the editor
        // and then re-queries the stack.
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

        let routed = self.route_editor_key(key, Instant::now());
        let exit = self.apply_routed(routed);
        if exit.is_none() {
            self.refresh_input_completion();
        }
        exit
    }

    /// One step of the Esc and Ctrl+C escalation: clear the draft (Up
    /// restores it), else interrupt the run in flight, else, for Esc,
    /// clear the search highlight, else, for Ctrl+C, arm quit, which a
    /// second press within
    /// [`ESCALATION_WINDOW`] carries out. Over an open overlay Ctrl+C
    /// only interrupts, since the draft is out of sight.
    pub(crate) fn escalate(&mut self, trigger: Trigger) -> Option<AppExit> {
        let now = Instant::now();
        let previous = self
            .escalation
            .take()
            .filter(|(_, until)| *until > now)
            .map(|(step, _)| step);
        if trigger == Trigger::CtrlC && self.keyboard_modal_open() {
            self.trip_cancel();
            return None;
        }
        if self.input.has_draft() {
            self.input.clear_draft();
            self.input_completion = None;
            self.escalation = Some((Escalation::DraftCleared, now + ESCALATION_WINDOW));
        } else if self.is_run_in_flight() {
            self.trip_cancel();
        } else if trigger == Trigger::Esc {
            self.search_pattern = None;
        } else if trigger == Trigger::CtrlC {
            if previous == Some(Escalation::QuitArmed) {
                return Some(AppExit::Quit);
            }
            self.escalation = Some((Escalation::QuitArmed, now + ESCALATION_WINDOW));
        }
        None
    }

    /// Resolve a key in the editing state: through the keymap
    /// sequencer, then the editor grammar for keys no mapping takes.
    /// A key that finishes a grammar command (after `g`, `z`, an
    /// operator or `r`) goes straight to the grammar.
    pub(crate) fn route_editor_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
        now: Instant,
    ) -> Vec<Routed> {
        let mapped = if self.input.is_pending() {
            None
        } else {
            key_from_event(&key)
        };
        let Some(mapped) = mapped else {
            return self.grammar(key);
        };
        let modes = self.edit_state().modes();
        let keymap = lock(&self.keymap);
        // With nothing buffered, a key that is not a prefix resolves
        // on its own, without the sequencer's buffers. Typing hits
        // this path on every key.
        if self.sequencer.deadline().is_none() {
            match keymap.lookup(modes, &[mapped]) {
                Lookup::None => {
                    drop(keymap);
                    return self.grammar(key);
                }
                Lookup::Exact(mapping) => {
                    let rhs = mapping.rhs.clone();
                    drop(keymap);
                    let mut routed = Vec::new();
                    Self::resolve_rhs(rhs, &mut routed);
                    return routed;
                }
                Lookup::Prefix { .. } => {}
            }
        }
        let steps = self.sequencer.feed(&keymap, modes, mapped, now);
        drop(keymap);
        self.resolve_steps(steps)
    }

    fn grammar(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Vec<Routed> {
        self.input
            .handle_key(key)
            .into_iter()
            .map(Routed::Input)
            .collect()
    }

    /// Resolve a pending key sequence whose timeout passed. A modal
    /// layer that opened meanwhile drops it instead.
    pub(crate) fn tick_keymap(&mut self, now: Instant) -> Vec<Routed> {
        if self.sequencer.deadline().is_none() {
            return Vec::new();
        }
        if self.keyboard_modal_open() {
            self.sequencer.clear();
            return Vec::new();
        }
        let modes = self.edit_state().modes();
        let steps = {
            let keymap = lock(&self.keymap);
            self.sequencer.tick(&keymap, modes, now)
        };
        self.resolve_steps(steps)
    }

    /// When a pending key sequence resolves on its own, if one is
    /// buffered.
    pub(crate) fn keymap_deadline(&self) -> Option<Instant> {
        self.sequencer.deadline()
    }

    fn resolve_rhs(rhs: Rhs, routed: &mut Vec<Routed>) {
        match rhs {
            Rhs::Action { name, arg } => {
                routed.extend(keymap::action(name, arg).map(Routed::Input));
            }
            Rhs::Command(command) => routed.push(Routed::Command(command)),
            Rhs::Lua(id) => routed.push(Routed::Lua(id)),
            Rhs::Nop => {}
        }
    }

    fn resolve_steps(&mut self, steps: Vec<Step>) -> Vec<Routed> {
        let mut routed = Vec::new();
        for step in steps {
            match step {
                Step::Fire(rhs) => Self::resolve_rhs(rhs, &mut routed),
                Step::Pending { .. } => {}
                Step::Replay(keys) => {
                    for key in keys {
                        let actions = self.input.handle_key(event_from_key(key));
                        routed.extend(actions.into_iter().map(Routed::Input));
                    }
                }
            }
        }
        routed
    }

    /// Carry out resolved keys in order. Stops at the first that
    /// exits.
    pub(crate) fn apply_routed(&mut self, routed: Vec<Routed>) -> Option<AppExit> {
        for item in routed {
            let exit = match item {
                Routed::Input(action) => self.apply(action),
                Routed::Command(command) => self.run_mapped_command(&command),
                Routed::Lua(id) => {
                    let _ = self.send_request(RunRequest::InvokeKeymap { id });
                    None
                }
            };
            if exit.is_some() {
                return exit;
            }
        }
        None
    }

    /// Run a mapping's command line through the same executor as the
    /// `:` cmdline, so `quit`, plugin commands, everything works.
    fn run_mapped_command(&mut self, command: &str) -> Option<AppExit> {
        let registry = cmdline_registry(&self.plugin_command_specs);
        match self.run_command_validated(command, &registry) {
            CommandResult::Done(exit) => exit,
            CommandResult::ValidationError(msg) => {
                self.push_error(format!("mapping `:{command}`: {msg}"));
                None
            }
        }
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

    /// Open the search line on the footer row, remembering the pattern
    /// and the view that `Esc` restores.
    pub(crate) fn begin_search(&mut self) {
        let buf = lock(&self.buffer);
        self.search_origin = Some(SearchOrigin {
            pattern: self.search_pattern.clone(),
            focus: buf.focus(),
            scroll: buf.scroll(),
        });
        drop(buf);
        self.search_line = Some(CommandLine::new());
    }

    /// Drive the open search line. Every edit searches live, Up and
    /// Down walk the matches, Enter keeps the pattern for `n` and `N`,
    /// and `Esc` restores the pattern and view from before.
    pub(crate) fn dispatch_search_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        use ratatui::crossterm::event::{KeyCode, KeyEventKind};
        let line = self.search_line.as_mut()?;
        if key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.jump_to_search_match(key.code == KeyCode::Down);
            return None;
        }
        match line.handle_key(key, &[], &EmptyResolver) {
            CommandLineEvent::Pending => self.preview_search(),
            CommandLineEvent::Cancelled => self.cancel_search(),
            CommandLineEvent::Submit(text) => {
                self.search_line = None;
                self.search_origin = None;
                self.search_pattern = Some(text);
            }
        }
        None
    }

    /// Search for the open line's text and focus the first match at or
    /// after where the search began, else the last match before it.
    /// With no match the view goes back to where it was.
    pub(crate) fn preview_search(&mut self) {
        let (Some(line), Some(origin)) = (&self.search_line, &self.search_origin) else {
            return;
        };
        let text = line.text();
        let (focus, scroll) = (origin.focus, origin.scroll);
        self.search_pattern = (!text.trim().is_empty()).then(|| text.to_owned());
        self.refresh_search_matches();
        let matches = self.search_matches();
        let mut buf = lock(&self.buffer);
        let from = focus.or_else(|| buf.last_selectable_index()).unwrap_or(0);
        let hit = matches
            .get(matches.partition_point(|&i| i < from))
            .or_else(|| matches.last());
        if let Some(&hit) = hit {
            buf.set_focus(Some(hit));
        } else {
            restore_view(&mut buf, focus, scroll);
        }
    }

    fn cancel_search(&mut self) {
        self.search_line = None;
        if let Some(origin) = self.search_origin.take() {
            self.search_pattern = origin.pattern;
            restore_view(&mut lock(&self.buffer), origin.focus, origin.scroll);
        }
    }

    /// Build `(current_1_indexed, total)` for the right-edge match
    /// counter, or `None` when no search is active.
    pub(crate) fn compute_search_match_count(&mut self) -> Option<(usize, usize)> {
        self.refresh_search_matches();
        self.search_pattern.as_ref()?;
        let focus = lock(&self.buffer).effective_focus().unwrap_or(usize::MAX);
        let matches = self.search_matches();
        let current = matches.binary_search(&focus).map_or(0, |p| p + 1);
        Some((current, matches.len()))
    }

    /// Jump focus to the next or previous block whose content matches
    /// the active search pattern. No-op when no pattern is set.
    pub(crate) fn jump_to_search_match(&mut self, forward: bool) {
        self.refresh_search_matches();
        if self.search_pattern.is_none() {
            return;
        }
        let matches = self.search_matches();
        let mut buf = lock(&self.buffer);
        let from = buf.effective_focus().unwrap_or(0);
        let pos = if forward {
            // First match strictly after `from`.
            matches.partition_point(|&i| i <= from)
        } else {
            // First match at-or-before `from`; step back one for the
            // strict predecessor.
            matches.partition_point(|&i| i < from)
        };
        let next = if forward {
            matches.get(pos)
        } else {
            pos.checked_sub(1).and_then(|p| matches.get(p))
        };
        if let Some(&n) = next {
            buf.set_focus(Some(n));
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

fn restore_view(buf: &mut crate::Buffer, focus: Option<usize>, scroll: Option<usize>) {
    buf.set_focus(focus);
    match scroll {
        Some(scroll) => buf.set_scroll(scroll),
        None => buf.follow(),
    }
}
