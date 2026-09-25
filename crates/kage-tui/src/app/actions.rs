//! Command validation, builtin dispatch, selection, and info screens.

use base64::Engine as _;

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;
use crate::command::{ArgValue, ParsedArgs};

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
            let args = match crate::cmdparse::parse_input(target_spec, target_rest) {
                Ok(args) => args,
                Err(e) => return CommandResult::ValidationError(e.to_string()),
            };
            let exit = self.dispatch_builtin(spec.name, rest, &args);
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
            msg = format!("{msg} (did you mean /{suggestion}?)");
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
    /// remaining unparsed argument string and the arguments parsed
    /// against the matched spec. The match is on the primary name;
    /// aliases were already resolved by [`Self::run_command`].
    // A flat dispatch table over every builtin command; the line
    // count is the command list, not complexity.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn dispatch_builtin(
        &mut self,
        name: &str,
        rest: &str,
        args: &ParsedArgs,
    ) -> Option<AppExit> {
        match name {
            "quit" => Some(AppExit::Quit),
            "cancel" => {
                self.trip_cancel();
                None
            }
            "model" => {
                if rest.is_empty() {
                    let _ = self.apply(InputAction::OpenModelPicker);
                } else {
                    let _ = self.send_request(RunRequest::SwitchModel(rest.to_owned()));
                }
                None
            }
            "fold" => {
                if rest == "all" {
                    self.set_all_folds(true);
                } else {
                    self.push_error("fold: usage `/fold all`");
                }
                None
            }
            "unfold" => {
                if rest == "all" {
                    self.set_all_folds(false);
                } else {
                    self.push_error("unfold: usage `/unfold all`");
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
                self.open_help();
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
            "permission" => {
                self.run_permission_command(rest);
                None
            }
            "login" => {
                self.run_login_command(rest);
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
                let dest = match args.get("file") {
                    Some(ArgValue::Path(path)) => Some(std::path::PathBuf::from(path)),
                    _ => None,
                };
                let _ = self.send_request(RunRequest::ExportSession(dest));
                None
            }
            "clear" => {
                let mut buf = lock(&self.buffer);
                buf.clear();
                None
            }
            "noh" => {
                self.search_pattern = None;
                None
            }
            "reload" => {
                let _ = self.send_request(RunRequest::ReloadPlugins);
                None
            }
            _ => None,
        }
    }

    /// Handle `:login [provider]`: queue the login flow for the run
    /// loop, which owns the terminal the flow suspends.
    pub(crate) fn run_login_command(&mut self, rest: &str) {
        let arg = rest.trim();
        if self.login_runner.is_none() {
            self.push_error("login: unavailable in this host");
            return;
        }
        self.pending_login = Some(if arg.is_empty() {
            PendingLogin::Picker
        } else {
            PendingLogin::Provider(arg.to_owned())
        });
    }

    /// Handle `:permission [mode]`: dispatch the override request,
    /// or report the current mode when called with no argument.
    pub(crate) fn run_permission_command(&mut self, rest: &str) {
        let arg = rest.trim();
        let parsed = match arg {
            "" => None,
            "allow" | "default" => Some(None),
            "ask" => Some(Some(kage_core::permissions::PermissionAction::Ask)),
            "deny" => Some(Some(kage_core::permissions::PermissionAction::Deny)),
            other => {
                self.push_error(format!(
                    "permission: unknown mode `{other}` (allow|ask|deny|default)"
                ));
                return;
            }
        };
        let Some(mode) = parsed else {
            let current = self
                .session_usage
                .as_ref()
                .and_then(|u| lock(u).permission_mode);
            let label = match current {
                Some(kage_core::permissions::PermissionAction::Ask) => "ask".to_owned(),
                Some(kage_core::permissions::PermissionAction::Deny) => "deny".to_owned(),
                _ => "default (configured rules)".to_owned(),
            };
            let mut buf = lock(&self.buffer);
            buf.push_custom("kage:help", format!("permission mode: {label}"), false);
            return;
        };
        let _ = self.send_request(RunRequest::SetPermissionMode(mode));
    }

    pub(crate) fn push_error(&mut self, msg: impl Into<String>) {
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:error", msg, false);
    }

    /// Push `text` to the system clipboard: the OSC52 escape for the
    /// terminal, and `arboard` on a background thread for terminals
    /// that ignore OSC52. Returns the number of chars written (0 for
    /// empty input, which is a no-op). A copy is best-effort, so
    /// failures are silently dropped.
    pub(crate) fn copy_to_clipboard(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        // Tests must not overwrite the developer's real clipboard.
        #[cfg(not(test))]
        set_os_clipboard(text.to_owned());
        text.chars().count()
    }

    /// Copy the active screen selection to the clipboard, or the
    /// focused block's source when nothing is selected, and clear the
    /// selection.
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
        self.notify(format!("copied {n} characters"));
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
        let anchor = {
            let buf = lock(&self.buffer);
            let area_x = buf.last_area_x();
            let area_y = buf.last_area_y();
            let virtual_top = buf.last_virtual_top();
            let row = buf
                .effective_focus()
                .and_then(|idx| buf.screen_top_of(idx))
                .unwrap_or(area_y);
            let vrow = virtual_top.saturating_add(usize::from(row.saturating_sub(area_y)));
            (vrow, area_x)
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
        } else {
            let buf = lock(&self.buffer);
            buf.last_area_width().saturating_sub(1)
        };
        self.screen_selection = Some((anchor, (vrow, col)));
    }

    /// Keep the visual cursor on screen by adjusting buffer scroll.
    /// Cursor above the viewport top pins the viewport at the cursor
    /// row; below the bottom scrolls just past it. Otherwise no-op.
    pub(crate) fn scroll_visual_cursor_into_view(&mut self, cursor_vrow: usize) {
        let mut buf = lock(&self.buffer);
        let area_height = usize::from(buf.last_area_height());
        if area_height == 0 {
            return;
        }
        let visible_top = buf.last_virtual_top();
        let visible_bot = visible_top.saturating_add(area_height);
        if cursor_vrow < visible_top {
            buf.set_scroll(cursor_vrow);
        } else if cursor_vrow >= visible_bot {
            buf.set_scroll(visible_top + (cursor_vrow + 1 - visible_bot));
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
        let buf = lock(&self.buffer);
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
        self.notify(format!("copied {n} characters"));
    }

    /// Copy block `idx`'s raw source to the clipboard. Backs the
    /// context menu's Copy row; like `Y` but for an explicit block
    /// rather than whatever has focus.
    pub(crate) fn copy_block_raw(&mut self, idx: usize) {
        let text = {
            let buf = lock(&self.buffer);
            buf.block_text(idx).unwrap_or_default()
        };
        let text = text.trim_end();
        let n = Self::copy_to_clipboard(text);
        if n > 0 {
            self.notify(format!("copied {n} characters"));
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
            let buf = lock(&self.buffer);
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
            let buf = lock(&self.buffer);
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

    /// Render the live keymap per mode with the owner of each mapping,
    /// then the keys the editor grammar handles and the two hatches.
    pub(crate) fn push_keybindings(&mut self) {
        use kage_core::keymap::{Mode as KeymapMode, display_keys};
        let mut lines = vec!["key mappings (last set wins):".to_owned()];
        {
            let keymap = lock(&self.keymap);
            let entries = keymap.entries();
            for (mode, what) in [
                (KeymapMode::Global, "any editing state"),
                (KeymapMode::Insert, "insert and the modeless editor"),
                (KeymapMode::Normal, "vim normal mode"),
                (KeymapMode::Buffer, "vim normal mode, conversation pane"),
                (KeymapMode::Visual, "visual mode"),
            ] {
                let rows: Vec<String> = entries
                    .iter()
                    .filter(|e| e.mode == mode)
                    .map(|e| {
                        let rhs = match &e.mapping.rhs {
                            Rhs::Action {
                                name,
                                arg: Some(arg),
                            } => format!("action:{name}({arg})"),
                            Rhs::Action { name, arg: None } => format!("action:{name}"),
                            Rhs::Command(command) => format!(":{command}"),
                            Rhs::Lua(_) => "lua function".to_owned(),
                            Rhs::Nop => "<Nop>".to_owned(),
                        };
                        let lhs = display_keys(e.lhs);
                        format!("  {lhs:<12} {rhs:<32} {}", e.mapping.owner)
                    })
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                lines.push(String::new());
                lines.push(format!("{mode}: {what}"));
                lines.extend(rows);
            }
        }
        lines.push(String::new());
        lines.push(
            "built in (editor grammar; a mapping or <Nop> shadows these, del cannot remove them):"
                .to_owned(),
        );
        for row in [
            "vim motions, operators, counts, registers, r, undo, redo",
            "readline edits and the kill ring (<C-a/e/w/u/k/y>, <C-/>, <M-b/f/d>, <M-BS>)",
            "<CR> submit (steers a running turn), <S-CR> and <M-CR> newline, <Up> and <Down> history",
            "modeless <Esc> clears the draft (<Up> restores it), else interrupts the turn",
            "vim <Esc>, insert <C-o> (expand a paste or fold), <C-g> external editor",
            "modeless empty-prompt /, ! and ?, conversation pane i and a",
        ] {
            lines.push(format!("  {row}"));
        }
        lines.push(String::new());
        lines.push(
            "hatches (above every layer; they yield only to init.lua or config.toml):".to_owned(),
        );
        lines.push("  <C-q>        quit".to_owned());
        lines.push(
            "  <C-c>        clear the draft, else interrupt the turn, else twice to quit"
                .to_owned(),
        );

        let body = lines.join("\n");
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:help", body, false);
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
        let mut buf = lock(&self.buffer);
        buf.push_custom("kage:help", body, false);
    }
}

/// Hand `text` to the OS clipboard off the UI thread. On X11 and
/// Wayland the thread keeps serving the text until another client
/// takes the clipboard over, since the contents vanish with their
/// owner.
#[cfg(not(test))]
fn set_os_clipboard(text: String) {
    std::thread::spawn(move || {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        #[cfg(all(
            unix,
            not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
        ))]
        let set = {
            use arboard::SetExtLinux;
            clipboard.set().wait()
        };
        #[cfg(not(all(
            unix,
            not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
        )))]
        let set = clipboard.set();
        let _ = set.text(text);
    });
}
