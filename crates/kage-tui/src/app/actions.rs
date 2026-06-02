//! Command validation, builtin dispatch, selection, and info screens.

use base64::Engine as _;

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    /// Validated command dispatch. Parses the argument string against
    /// the matched spec's schema and returns a [`CommandResult`]
    /// instead of pushing errors to the buffer. On
    /// [`CommandResult::ValidationError`], the caller keeps the
    /// cmdline open so the user can fix the input.
    pub(crate) fn run_command_validated(
        &mut self,
        line: &str,
        registry: &[&CommandSpec],
    ) -> CommandResult {
        let mut parts = line.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        if head.is_empty() {
            return CommandResult::Done(None);
        }

        // Resolve `head` to a plugin command (direct name or alias).
        let canonical = if self.plugin_commands.iter().any(|(n, _)| n == head) {
            Some(head.to_owned())
        } else {
            self.plugin_command_aliases
                .iter()
                .find(|(alias, _)| alias == head)
                .map(|(_, name)| name.clone())
        };

        // An `override_command` shadows a builtin: dispatch it ahead
        // of the builtin lookup.
        if let Some(name) = canonical
            .as_deref()
            .filter(|n| self.plugin_command_overrides.iter().any(|o| o == n))
        {
            let _ = self.send_request(RunRequest::InvokePluginCommand {
                name: name.to_owned(),
                args: rest.to_owned(),
            });
            return CommandResult::Done(None);
        }

        if let Some(spec) = crate::command::find_builtin_command(head) {
            let (target_spec, target_rest) = Self::resolve_subcommand_tree(spec, rest);
            if let Err(e) = crate::cmdparse::parse_input(target_spec, target_rest) {
                return CommandResult::ValidationError(e.to_string());
            }
            let exit = self.dispatch_builtin(spec.name, rest);
            return CommandResult::Done(exit);
        }

        if let Some(name) = canonical {
            let _ = self.send_request(RunRequest::InvokePluginCommand {
                name,
                args: rest.to_owned(),
            });
            return CommandResult::Done(None);
        }

        let mut msg = format!("unknown command: {head}");
        if let Some(suggestion) = crate::cmdparse::suggest_command(registry, head) {
            msg = format!("{msg} (did you mean :{suggestion}?)");
        }
        CommandResult::ValidationError(msg)
    }

    /// Walk the subcommand tree for commands like `theme set <name>`.
    /// Returns the leaf spec and the remaining argument substring
    /// after consuming subcommand names. If no subcommand matches,
    /// returns the parent spec with the full `rest`.
    pub(crate) fn resolve_subcommand_tree<'a, 'b>(
        spec: &'a CommandSpec,
        rest: &'b str,
    ) -> (&'a CommandSpec, &'b str) {
        if spec.subcommands.is_empty() {
            return (spec, rest);
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        if let Some(sub) = spec.subcommand(first) {
            let sub_rest = parts.next().unwrap_or("").trim();
            return Self::resolve_subcommand_tree(sub, sub_rest);
        }
        (spec, rest)
    }

    /// Execute a built-in command by canonical name with the
    /// remaining unparsed argument string. The match is on the
    /// primary name; aliases were already resolved by
    /// [`Self::run_command`].
    pub(crate) fn dispatch_builtin(&mut self, name: &str, rest: &str) -> Option<AppExit> {
        match name {
            "quit" => Some(AppExit::Quit),
            "cancel" => {
                self.trip_cancel();
                None
            }
            "model" => {
                if rest.is_empty() {
                    self.push_error("model: usage `:model <provider:id>`");
                } else {
                    let _ = self.send_request(RunRequest::SwitchModel(rest.to_owned()));
                }
                None
            }
            "fold" => {
                if rest == "all" {
                    self.set_all_folds(true);
                } else {
                    self.push_error("fold: usage `:fold all`");
                }
                None
            }
            "unfold" => {
                if rest == "all" {
                    self.set_all_folds(false);
                } else {
                    self.push_error("unfold: usage `:unfold all`");
                }
                None
            }
            "theme" => {
                self.run_theme_command(rest);
                None
            }
            "mouse" => {
                self.run_mouse_command(rest);
                None
            }
            "help" => {
                self.push_help();
                None
            }
            "keybindings" => {
                self.push_keybindings();
                None
            }
            "events" => {
                self.push_events();
                None
            }
            "attach" => {
                self.attach_image_path(rest);
                None
            }
            "compact" => {
                let _ = self.send_request(RunRequest::CompactNow);
                None
            }
            "settings" => {
                self.open_settings();
                None
            }
            "tree" => {
                self.open_session_tree();
                None
            }
            "clone" => {
                let _ = self.send_request(RunRequest::CloneSession);
                None
            }
            "new" => {
                let _ = self.send_request(RunRequest::NewSession);
                None
            }
            "export" => {
                let dest = match rest.trim() {
                    "" => None,
                    path => Some(std::path::PathBuf::from(path)),
                };
                let _ = self.send_request(RunRequest::ExportSession(dest));
                None
            }
            "clear" => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.clear();
                }
                None
            }
            _ => None,
        }
    }

    pub(crate) fn push_error(&mut self, msg: impl Into<String>) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:error", msg, false);
        }
    }

    /// Copy the active screen selection to the system clipboard via
    /// OSC52. Walks every captured row in the selection range,
    /// strips renderer-only decoration glyphs (rule chars), trims
    /// trailing whitespace per row, joins with `\n`, and clears the
    /// selection.
    /// Push `text` to the system clipboard via the OSC52 escape.
    /// Returns the number of chars written (0 for empty input, which
    /// is a no-op). The terminal owns the actual clipboard handoff;
    /// failures to write stdout are silently dropped because a copy
    /// is best-effort and never load-bearing.
    pub(crate) fn copy_to_clipboard(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        text.chars().count()
    }

    pub(crate) fn yank_screen_selection(&mut self) {
        if self.screen_selection.is_none() {
            // No selection: `y` means "copy the focused response",
            // same raw-text path as `Y`.
            self.yank_focused_block();
            return;
        }
        let text = self.extract_selection_text();
        if text.is_empty() {
            self.clear_selection();
            return;
        }
        let n = Self::copy_to_clipboard(&text);
        self.clear_selection();
        self.notify(format!("yanked {n} chars to clipboard"));
    }

    pub(crate) fn clear_selection(&mut self) {
        self.screen_selection = None;
        self.captured_rows.clear();
    }

    /// Anchor a keyboard selection at the focused block's first
    /// visible row (or the viewport top if nothing's focused) and
    /// switch the mode to [`Mode::Visual`]. Subsequent
    /// [`InputAction::Visual*`] events move the cursor end.
    pub(crate) fn enter_visual_mode(&mut self) {
        let anchor = if let Ok(buf) = self.buffer.lock() {
            let area_x = buf.last_area_x();
            let area_y = buf.last_area_y();
            let virtual_top = buf.last_virtual_top();
            let row = buf
                .effective_focus()
                .and_then(|idx| buf.screen_top_of(idx))
                .unwrap_or(area_y);
            let vrow = virtual_top.saturating_add(usize::from(row.saturating_sub(area_y)));
            (vrow, area_x)
        } else {
            (0, 0)
        };
        self.captured_rows.clear();
        self.screen_selection = Some((anchor, anchor));
        self.input.switch_mode(Mode::Visual);
    }

    pub(crate) fn move_visual_cursor(&mut self, dvrow: i32, dcol: i32) {
        let Some((anchor, cursor)) = self.screen_selection else {
            return;
        };
        let (mut vrow, mut col) = cursor;
        if dvrow != 0 {
            let next = i64::try_from(vrow).unwrap_or(i64::MAX) + i64::from(dvrow);
            vrow = usize::try_from(next.max(0)).unwrap_or(0);
        }
        if dcol != 0 {
            let next = i32::from(col).saturating_add(dcol).max(0);
            col = u16::try_from(next).unwrap_or(u16::MAX);
        }
        self.screen_selection = Some((anchor, (vrow, col)));
        self.scroll_visual_cursor_into_view(vrow);
    }

    pub(crate) fn snap_visual_cursor_x(&mut self, target_col: i32) {
        let Some((anchor, cursor)) = self.screen_selection else {
            return;
        };
        let (vrow, _) = cursor;
        let col = if target_col <= 0 {
            0
        } else if let Ok(buf) = self.buffer.lock() {
            buf.last_area_width().saturating_sub(1)
        } else {
            0
        };
        self.screen_selection = Some((anchor, (vrow, col)));
    }

    /// Keep the visual cursor on screen by adjusting buffer scroll.
    /// Cursor above the viewport top scrolls up; below the bottom
    /// scrolls down. Otherwise no-op.
    pub(crate) fn scroll_visual_cursor_into_view(&mut self, cursor_vrow: usize) {
        if let Ok(mut buf) = self.buffer.lock() {
            let area_height = usize::from(buf.last_area_height());
            if area_height == 0 {
                return;
            }
            let visible_top = buf.last_virtual_top();
            let visible_bot = visible_top.saturating_add(area_height);
            let current_scroll = buf.scroll();
            if cursor_vrow < visible_top {
                let delta = visible_top - cursor_vrow;
                buf.set_scroll(current_scroll.saturating_add(delta));
            } else if cursor_vrow >= visible_bot {
                let delta = cursor_vrow + 1 - visible_bot;
                buf.set_scroll(current_scroll.saturating_sub(delta));
            }
        }
    }

    /// Yank the entire content of the currently focused block by
    /// projecting its screen rows onto captured cells. Limitation:
    /// only rows that have been visible (and thus captured) since
    /// the last selection clear contribute text - tall blocks the
    /// user hasn't scrolled fully through return only the visible
    /// portion. Auto-scroll on entering visual covers the keyboard
    /// path; for `Y` we just use whatever cells we have right now.
    pub(crate) fn yank_focused_block(&mut self) {
        // Yank the block's raw source text from the model, not the
        // markdown-rendered screen cells: the user wants the original
        // assistant text (verbatim ```fences```, list bullets, etc.)
        // to paste elsewhere, not the syntect-styled reflow.
        let Ok(buf) = self.buffer.lock() else {
            return;
        };
        let Some(idx) = buf.effective_focus() else {
            return;
        };
        let text = buf.block_text(idx).unwrap_or_default();
        drop(buf);
        let text = text.trim_end().to_owned();
        if text.is_empty() {
            return;
        }
        let n = Self::copy_to_clipboard(&text);
        self.notify(format!("yanked {n} chars to clipboard"));
    }

    /// Copy block `idx`'s raw source to the clipboard. Backs the
    /// context menu's Copy row; like `Y` but for an explicit block
    /// rather than whatever has focus.
    pub(crate) fn copy_block_raw(&mut self, idx: usize) {
        let text = {
            let Ok(buf) = self.buffer.lock() else {
                return;
            };
            buf.block_text(idx).unwrap_or_default()
        };
        let text = text.trim_end();
        let n = Self::copy_to_clipboard(text);
        if n > 0 {
            self.notify(format!("copied {n} chars to clipboard"));
        }
    }

    /// Run a context-menu action against the block it targeted.
    pub(crate) fn run_context_action(&mut self, action: ContextAction, block_idx: usize) {
        match action {
            ContextAction::Copy => self.copy_block_raw(block_idx),
        }
    }

    /// Right mouse press at screen `(col, row)`: open a context menu
    /// over the block under the cursor. A press over no block (or
    /// outside the buffer pane) dismisses any open menu instead.
    pub(crate) fn open_context_menu(&mut self, col: u16, row: u16) {
        let idx = {
            let Ok(buf) = self.buffer.lock() else {
                return;
            };
            let area_y = buf.last_area_y();
            let area_h = buf.last_area_height();
            if row < area_y || row >= area_y.saturating_add(area_h) {
                None
            } else {
                buf.block_at_screen_row(row)
            }
        };
        self.context_menu = idx.map(|idx| ContextMenu::new(col, row, idx));
    }

    /// Left click while a context menu is open: a click on a row runs
    /// its action; a click anywhere else just dismisses. Either way
    /// the click is consumed here so it never also starts a drag
    /// selection on the buffer beneath.
    pub(crate) fn context_menu_click(&mut self, col: u16, row: u16) {
        if self.context_menu.is_none() {
            return;
        }
        let viewport = {
            let Ok(buf) = self.buffer.lock() else {
                self.context_menu = None;
                return;
            };
            ratatui::layout::Rect {
                x: buf.last_area_x(),
                y: buf.last_area_y(),
                width: buf.last_area_width(),
                height: buf.last_area_height(),
            }
        };
        let (hit, idx) = {
            let menu = self.context_menu.as_mut().expect("checked above");
            (menu.handle_click(viewport, col, row), menu.block_idx())
        };
        self.context_menu = None;
        if let Some(action) = hit {
            self.run_context_action(action, idx);
        }
    }

    /// Route a key into the open context menu. Modal while present:
    /// navigation and Esc are consumed, a stray key is swallowed, and
    /// activation runs the row's action then closes the menu.
    pub(crate) fn dispatch_context_menu_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let outcome = {
            let menu = self.context_menu.as_mut()?;
            menu.handle_key(key)
        };
        match outcome {
            ContextMenuOutcome::Navigated => {}
            ContextMenuOutcome::Dismissed => self.context_menu = None,
            ContextMenuOutcome::Activated(action) => {
                let idx = self.context_menu.as_ref().map(ContextMenu::block_idx);
                self.context_menu = None;
                if let Some(idx) = idx {
                    self.run_context_action(action, idx);
                }
            }
        }
        None
    }

    pub(crate) fn extract_selection_text(&self) -> String {
        let Some((anchor, cursor)) = self.screen_selection else {
            return String::new();
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        // Copy exactly the captured cells under the selection: the
        // rendered text for the selected rows and columns, decoration
        // (chrome) cells dropped. Partial drags through any block -
        // assistant, thinking, tool - yield only what is highlighted,
        // never the whole block.
        let mut out = String::new();
        for vrow in start.0..=end.0 {
            let Some(grid_row) = self.captured_rows.get(&vrow) else {
                if !out.is_empty() {
                    out.push('\n');
                }
                continue;
            };
            let from_col = if vrow == start.0 {
                usize::from(start.1)
            } else {
                0
            };
            let to_col = if vrow == end.0 {
                usize::from(end.1).saturating_add(1)
            } else {
                grid_row.len()
            };
            let to_col = to_col.min(grid_row.len());
            if from_col >= to_col {
                if !out.is_empty() {
                    out.push('\n');
                }
                continue;
            }
            let slice: String = grid_row[from_col..to_col]
                .iter()
                .filter(|cell| !cell.decoration)
                .map(|cell| cell.ch)
                .collect();
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(slice.trim_end());
        }
        out
    }

    pub(crate) fn push_help(&mut self) {
        let mut lines = vec!["available commands:".to_owned()];
        for spec in crate::command::BUILTIN_COMMANDS {
            help_render_spec(&mut lines, spec, ":", 0);
        }
        let body = lines.join("\n");
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:help", body, false);
        }
    }

    /// Render the active key bindings: user `[keybindings]` config
    /// first (authoritative), then plugin-registered chords, then the
    /// fixed reserved keys the TUI handles itself. Honest about the
    /// last group rather than pretending everything is rebindable.
    pub(crate) fn push_keybindings(&mut self) {
        let mut lines = vec!["key bindings (first match wins, top to bottom):".to_owned()];

        lines.push(String::new());
        lines.push("[keybindings] config:".to_owned());
        if self.config_keybindings.is_empty() {
            lines.push("  (none; add a [keybindings] table to config.toml)".to_owned());
        } else {
            for (_, chord, command) in &self.config_keybindings {
                lines.push(format!("  {chord:<16} :{command}"));
            }
        }

        lines.push(String::new());
        lines.push("plugin (kage.register_keybinding):".to_owned());
        if self.plugin_keybindings.is_empty() {
            lines.push("  (none)".to_owned());
        } else {
            for (_, chord) in &self.plugin_keybindings {
                lines.push(format!("  {chord:<16} plugin handler"));
            }
        }

        lines.push(String::new());
        lines.push("reserved (handled by the TUI; not rebindable):".to_owned());
        for (chord, what) in [
            ("ctrl+q", "quit (unless you bind ctrl+q in config)"),
            ("ctrl+c", "interrupt the in-flight turn"),
            (":", "command line"),
            ("/", "search"),
            ("esc", "leave a mode / close an overlay"),
        ] {
            lines.push(format!("  {chord:<16} {what}"));
        }

        let body = lines.join("\n");
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:help", body, false);
        }
    }

    /// Render every event a plugin can hook with `kage.on`, grouped
    /// by dispatch kind, sourced from the single
    /// [`kage_plugin::KNOWN_EVENTS`] catalog so it cannot drift from
    /// what the host actually fires.
    pub(crate) fn push_events(&mut self) {
        let mut lines = vec![
            "events (kage.on(name, fn)); kinds: notification | transform | predicate | veto"
                .to_owned(),
        ];
        for kind in ["notification", "transform", "predicate", "veto"] {
            lines.push(String::new());
            lines.push(format!("{kind}:"));
            for (name, _, desc) in kage_plugin::KNOWN_EVENTS
                .iter()
                .filter(|(_, k, _)| *k == kind)
            {
                lines.push(format!("  {name:<24} {desc}"));
            }
        }
        let body = lines.join("\n");
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:help", body, false);
        }
    }
}
