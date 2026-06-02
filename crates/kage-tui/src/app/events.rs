//! Submit, mouse, scroll/fold, and render entry points.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    /// Resolve an `InputAction::Submit`: paint the user block, then
    /// either push onto the steering queue (text-only mid-run with a
    /// queue attached) or dispatch as a `RunRequest::Submit` over the
    /// worker channel. Image-bearing submits always take the channel
    /// path because the steering hook only carries text.
    pub(crate) fn handle_submit(&mut self, text: String) {
        let images = self.input.take_attached();
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_user(text.clone());
            for img in &images {
                buf.push_custom("kage:image", img.placeholder(), false);
            }
        }
        let queue_steering =
            images.is_empty() && self.steering.is_some() && self.is_run_in_flight();
        if !queue_steering {
            let _ = self.send_request(RunRequest::Submit { text, images });
            return;
        }
        let pushed = self
            .steering
            .as_ref()
            .and_then(|q| q.lock().ok().map(|mut g| g.push_back(text)))
            .is_some();
        if pushed {
            self.notify("queued for next turn");
        }
    }

    pub(crate) fn apply(&mut self, action: InputAction) -> Option<AppExit> {
        // Phase 9.10/9.11/9.17 will wire BeginCommand/BeginSearch/Yank;
        // for now they fall through to the silent EnterMode arm so the
        // modal state machine still cycles cleanly.
        match action {
            InputAction::Submit(text) => self.handle_submit(text),
            InputAction::Scroll(delta) => self.scroll_by(delta),
            InputAction::ScrollToTop => self.set_scroll(usize::MAX),
            InputAction::ScrollToBottom => self.set_scroll(0),
            InputAction::ToggleFold => self.toggle_last_fold(),
            InputAction::UnfoldAll => self.set_all_folds(false),
            InputAction::FoldAll => self.set_all_folds(true),
            InputAction::Cancel => {
                self.trip_cancel();
            }
            InputAction::OpenModelPicker => {
                if !self.model_choices.is_empty() {
                    self.picker = Some(OverlayPicker::new(
                        "Switch model",
                        self.model_choices.clone(),
                    ));
                    self.picker_kind = Some(PickerKind::Model);
                }
            }
            InputAction::OpenCommandPalette => {
                let registry = cmdline_registry(&self.plugin_command_specs);
                let ctx = SlashContext {
                    models: self.model_choices.iter().map(|p| p.value.clone()).collect(),
                    plugin_commands: self.plugin_commands.clone(),
                    sessions: self
                        .session_lister
                        .as_ref()
                        .map(|f| f(true))
                        .unwrap_or_default(),
                    themes: crate::theme::Theme::available_names(self.themes_dir.as_deref()),
                };
                let mut palette = SlashPalette::new(registry, ctx);
                palette.refresh();
                self.slash_palette = Some(palette);
            }
            InputAction::FocusPrev => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.focus_prev_any();
                }
            }
            InputAction::FocusNext => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.focus_next_any();
                }
            }
            InputAction::OpenSessionPicker => {
                // Default to this directory's sessions. If there are
                // none here but some elsewhere, open in all-dirs
                // scope so Ctrl+R is never a dead key in a fresh dir.
                self.session_scope_all = false;
                if let Some(lister) = self.session_lister.as_ref()
                    && lister(false).is_empty()
                    && !lister(true).is_empty()
                {
                    self.session_scope_all = true;
                }
                self.open_session_picker(false);
            }
            InputAction::BeginCommand => {
                self.cmdline = Some(CommandLine::new());
            }
            InputAction::EnterMode(_) => {}
            InputAction::Yank => self.yank_screen_selection(),
            InputAction::ClearSelection => self.clear_selection(),
            InputAction::EnterVisual => self.enter_visual_mode(),
            InputAction::VisualLeft => self.move_visual_cursor(0, -1),
            InputAction::VisualRight => self.move_visual_cursor(0, 1),
            InputAction::VisualUp => self.move_visual_cursor(-1, 0),
            InputAction::VisualDown => self.move_visual_cursor(1, 0),
            InputAction::VisualLineStart => self.snap_visual_cursor_x(0),
            InputAction::VisualLineEnd => self.snap_visual_cursor_x(i32::MAX),
            InputAction::YankFocusedBlock => self.yank_focused_block(),
            InputAction::BeginSearch => {
                self.search_line = Some(CommandLine::new());
            }
            InputAction::SearchNext => self.jump_to_search_match(true),
            InputAction::SearchPrev => self.jump_to_search_match(false),
            InputAction::CyclePane => {
                self.input.toggle_focused_pane();
            }
            InputAction::FocusPane(pane) => {
                self.input.set_focused_pane(pane);
            }
            InputAction::CycleThinkingLevel => {
                let _ = self.send_request(RunRequest::CycleThinkingLevel);
            }
        }
        None
    }

    pub(crate) fn send_request(&mut self, req: RunRequest) -> Result<(), TrySendError<RunRequest>> {
        match self.requests.send(req) {
            Ok(()) => Ok(()),
            Err(err) => Err(TrySendError::Disconnected(err.0)),
        }
    }

    /// Dispatch one crossterm mouse event. Scroll moves the buffer
    /// (and closes any context menu so it cannot hang in mid-air); a
    /// right press opens the context menu; a left press either feeds
    /// the open menu or starts the normal selection gesture.
    pub(crate) fn handle_mouse_event(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::MouseButton;
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.context_menu = None;
                self.scroll_by(-MOUSE_SCROLL_LINES);
            }
            MouseEventKind::ScrollDown => {
                self.context_menu = None;
                self.scroll_by(MOUSE_SCROLL_LINES);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.open_context_menu(mouse.column, mouse.row);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.context_menu.is_some() {
                    self.context_menu_click(mouse.column, mouse.row);
                } else {
                    self.mouse_down(mouse.row, mouse.column);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_drag(mouse.row, mouse.column);
            }
            MouseEventKind::Up(MouseButton::Left) => self.mouse_up(mouse.row),
            _ => {}
        }
    }

    /// Mouse left-button press: anchor a virtual-row selection at
    /// the click position. Any prior selection (and its captured
    /// text) is dropped. Focus snaps to whichever block sits under
    /// the click so subsequent keyboard gestures act on it.
    pub(crate) fn mouse_down(&mut self, row: u16, col: u16) {
        self.captured_rows.clear();
        if let Ok(mut buf) = self.buffer.lock() {
            let area_y = buf.last_area_y();
            let area_height = buf.last_area_height();
            if row < area_y || row >= area_y.saturating_add(area_height) {
                // Click landed outside the buffer rectangle. Anything
                // below the buffer is the input card or modeline; the
                // top status row is above. Clicks below the buffer
                // focus the input pane (vim-style window focus); top
                // status clicks leave focus alone.
                self.screen_selection = None;
                self.mouse_drag_anchor = None;
                if row >= area_y.saturating_add(area_height) {
                    self.input.set_focused_pane(Pane::Input);
                }
                return;
            }
            // Click inside the buffer area focuses the buffer pane.
            self.input.set_focused_pane(Pane::Buffer);
            let vrow = buf
                .last_virtual_top()
                .saturating_add(usize::from(row - area_y));
            self.screen_selection = Some(((vrow, col), (vrow, col)));
            if let Some(idx) = buf.block_at_screen_row(row) {
                buf.set_focus(Some(idx));
                self.mouse_drag_anchor = Some((row, idx, false));
            } else {
                self.mouse_drag_anchor = None;
            }
        }
    }

    /// Mouse drag while left-button is held: extend the selection
    /// cursor to the virtual-row under `(row, col)`. Drag rows
    /// outside the buffer area clamp to the closest visible row so
    /// sweeping past the input area still extends correctly.
    pub(crate) fn mouse_drag(&mut self, row: u16, col: u16) {
        let Some((anchor, _)) = self.screen_selection else {
            return;
        };
        if let Ok(buf) = self.buffer.lock() {
            let area_y = buf.last_area_y();
            let area_height = buf.last_area_height();
            if area_height == 0 {
                return;
            }
            let last_visible_row = area_y.saturating_add(area_height).saturating_sub(1);
            let clamped_row = row.clamp(area_y, last_visible_row);
            let vrow = buf
                .last_virtual_top()
                .saturating_add(usize::from(clamped_row - area_y));
            self.screen_selection = Some((anchor, (vrow, col)));
            if let Some((_, _, ref mut dragged)) = self.mouse_drag_anchor {
                *dragged = true;
            }
        }
    }

    /// Mouse left-button release: a non-dragged release on a block's
    /// header row toggles fold and clears the just-anchored
    /// zero-width selection; a dragged release copies the highlighted
    /// selection straight to the clipboard without waiting for `y`.
    pub(crate) fn mouse_up(&mut self, row: u16) {
        let Some((_down_row, anchor_idx, dragged)) = self.mouse_drag_anchor.take() else {
            return;
        };
        if dragged {
            // The render after the final drag event already captured
            // every selected row into `captured_rows`, so this sees
            // the same state a `y` press would: extract, copy, clear.
            self.yank_screen_selection();
            return;
        }
        // Plain click: clear the zero-width selection we anchored on
        // press, then maybe toggle a fold on the header row.
        self.clear_selection();
        if let Ok(mut buf) = self.buffer.lock()
            && buf.screen_top_of(anchor_idx) == Some(row)
        {
            buf.toggle_fold(anchor_idx);
        }
    }

    pub(crate) fn scroll_by(&mut self, delta: i32) {
        if let Ok(mut buf) = self.buffer.lock() {
            // Positive delta = move toward newest (decrement rows-up);
            // negative = move toward oldest (increment rows-up).
            let current = i64::try_from(buf.scroll()).unwrap_or(i64::MAX);
            let target = (current - i64::from(delta)).max(0);
            let clamped = usize::try_from(target).unwrap_or(0);
            buf.set_scroll(clamped);
        }
    }

    pub(crate) fn set_scroll(&mut self, scroll: usize) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_scroll(scroll);
        }
    }

    pub(crate) fn toggle_last_fold(&mut self) {
        if let Ok(mut buf) = self.buffer.lock() {
            if let Some(idx) = buf.effective_focus() {
                buf.toggle_fold(idx);
            }
        }
    }

    pub(crate) fn set_all_folds(&mut self, folded: bool) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_all_folded(folded);
        }
    }

    /// Read-only borrow of the input state. Tests use this to assert
    /// mode transitions without driving a real terminal.
    #[must_use]
    pub fn input(&self) -> &InputState {
        &self.input
    }

    /// Apply a key directly without going through crossterm. Used by
    /// tests and by external command handlers (Phase 9.10) that want to
    /// drive the modal state machine programmatically.
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        self.dispatch_key(key)
    }

    /// Force a redraw onto an arbitrary terminal. Tests use this with
    /// [`ratatui::backend::TestBackend`] to capture the rendered frame.
    pub fn render_into<B>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<(), TuiError>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let search_match_count = self.compute_search_match_count();
        let render_width = terminal.size().map_or(80, |r| r.width);
        self.refresh_plugin_widget_texts(render_width);
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let session_usage = self.session_usage_snapshot();
        let live_toasts = self.live_toasts();
        let bottom = if self.modeline_visible() {
            crate::layout::STATUS_BOTTOM_LINES_DEFAULT
        } else {
            0
        };
        let model_snapshot = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()));
        let cmdline = self.cmdline.as_ref();
        let status = view::StatusCtx {
            model: model_snapshot.as_deref(),
            session_id: self.status_session_id.as_deref(),
            search_pattern: self.search_pattern.as_deref(),
            search_match_set: None,
            search_line: self.search_line.as_ref(),
            search_match_count,
            plugin_widgets: &self.plugin_widget_texts,
            plugin_status: &self.plugin_status_cache,
            plugin_header: &self.plugin_header_lines,
            plugin_footer: &self.plugin_footer_lines,
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let show_completion = self.input_completion.is_some()
            && self.slash_palette.is_none()
            && self.cmdline.is_none()
            && self.search_line.is_none()
            && self.picker.is_none()
            && self.settings_overlay.is_none()
            && self.session_tree.is_none()
            && self.plugin_overlay.is_none();
        let picker = self.picker.as_mut();
        let settings_overlay = self.settings_overlay.as_mut();
        let session_tree = self.session_tree.as_mut();
        let input_completion = if show_completion {
            self.input_completion.as_ref()
        } else {
            None
        };
        let context_menu = self.context_menu.as_ref();
        let input = &self.input;
        terminal
            .draw(|frame| {
                let body_width = frame
                    .area()
                    .width
                    .saturating_sub(2 + view::INPUT_GLYPH_WIDTH);
                let input_visual_lines = view::input_visual_row_count(input.text(), body_width);
                let input_height = input_height_for(input_visual_lines);
                let regions = split(frame.area(), input_height, bottom);
                view::render(
                    frame,
                    regions,
                    &mut buffer,
                    input,
                    cmdline,
                    &status,
                    screen_selection,
                    &mut captured_rows,
                    session_usage.as_ref(),
                    &live_toasts,
                );
                if let Some(picker) = picker {
                    picker.render(frame, frame.area());
                }
                if let Some(settings) = settings_overlay {
                    settings.render(frame, frame.area());
                }
                if let Some(tree) = session_tree {
                    tree.render(frame, frame.area());
                }
                if let Some(completion) = input_completion {
                    completion.render(frame, regions);
                }
                if let Some(menu) = context_menu {
                    menu.render(frame, regions.buffer);
                }
            })
            .map_err(|err| TuiError::Io(std::io::Error::other(err.to_string())))?;
        self.captured_rows = captured_rows;
        Ok(())
    }
}
