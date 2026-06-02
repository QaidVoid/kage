//! The App run loop, frame draw, and run-state queries.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

impl App {
    /// Drive the event loop until the user quits. Returns the exit
    /// reason. The caller is expected to drop the [`Tui`] (which
    /// restores the terminal) before printing anything to stdout.
    pub fn run(&mut self, tui: &mut Tui) -> Result<AppExit, TuiError> {
        // The plugin-read snapshots (theme list, session list) are
        // rebuilt by scanning the filesystem. Refreshing them on every
        // wake costs a directory read per loop iteration - 20/s while
        // the agent streams, which is most of the idle/working CPU when
        // a plugin registers for them. They feed picker-style APIs
        // consumed at human timescale, so a coarse cadence is
        // indistinguishable to the plugin and effectively free.
        const PLUGIN_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);
        // First frame is unconditional - we always paint once before
        // entering the steady-state event loop.
        let mut last_buffer_version = self.buffer_version();
        let mut last_spinner_idx = crate::view::spinner_frame_index();
        let mut needs_redraw = true;
        let mut last_plugin_snapshot: Option<Instant> = None;
        loop {
            if let Some(enable) = self.pending_mouse_capture.take() {
                tui.set_mouse_capture(enable);
            }
            self.drain_plugin_compact_request();
            self.drain_plugin_fork_request();
            self.drain_plugin_switch_request();
            // Dialog + theme drains can mutate the visible screen
            // (overlay open, theme swap). Without this, the worker
            // pushes a `kage.ui.select` request from a /command, we
            // open the overlay, but `needs_redraw` is still false and
            // the loop blocks on `event::poll` until the user
            // happens to press a key. Force a paint on the next pass.
            if self.drain_plugin_dialog() {
                needs_redraw = true;
            }
            if self.drain_plugin_theme() {
                needs_redraw = true;
            }
            if last_plugin_snapshot.is_none_or(|t| t.elapsed() >= PLUGIN_SNAPSHOT_INTERVAL) {
                self.refresh_plugin_theme_state();
                self.refresh_plugin_session_list();
                last_plugin_snapshot = Some(Instant::now());
            }
            if needs_redraw {
                self.draw(tui)?;
                last_buffer_version = self.buffer_version();
                last_spinner_idx = crate::view::spinner_frame_index();
                needs_redraw = false;
            }
            // Wake periodically to repaint streaming tool-call
            // timers ("running 1.2s") and to pick up worker-thread
            // mutations that race ahead of any input event. While
            // the agent is mid-turn we shorten the wake interval to
            // ~one spinner frame so the modeline tick stays smooth
            // even with no streaming deltas (e.g. waiting on a slow
            // first token from the provider).
            // Computed once and reused for the redraw gate below;
            // `has_running_tool_call` locks the buffer and scans every
            // block, so calling it twice per iteration is wasteful.
            let animating = self.is_working() || self.has_running_tool_call();
            let tick = if animating {
                // 50ms keeps the wake latency low so streamed deltas
                // surface promptly and shaves the worst-case lag after
                // `working` flips false (e.g. after a cancel takes
                // effect) so the user perceives the spinner stopping as
                // effectively instant rather than tail-end-of-the-100ms-
                // window. The wake is cheap; the redraw it may trigger
                // is gated on actual visible change below.
                Duration::from_millis(50)
            } else {
                // 200ms idle wake (5 Hz) keeps plugin-dialog and other
                // worker-pushed state visible without the user having
                // to press a key. A 1s wake felt frozen: after a
                // /command that opened a `kage.ui.*` dialog, the
                // overlay would not appear until the next keypress or
                // the next tick. The CPU cost of 5 idle wakes per
                // second is negligible; the redraw gate below still
                // skips repaints when nothing visible changed.
                Duration::from_millis(200)
            };
            let mut deadline = Instant::now() + tick;
            // Toasts auto-expire on a wall-clock schedule independent
            // of key input; cap the poll deadline at the next toast
            // expiration and force a redraw each tick so the overlay
            // appears immediately when pushed from a worker thread
            // and disappears when its deadline fires, regardless of
            // whether the user pressed a key.
            if let Some(toast_deadline) = self.next_toast_deadline() {
                if toast_deadline < deadline {
                    deadline = toast_deadline;
                }
                needs_redraw = true;
            }
            while Instant::now() < deadline {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default();
                if event::poll(remaining)? {
                    // Any handled event might have altered something
                    // user-visible (cursor in picker, mode switch,
                    // input edit, scroll). Tracking each potential
                    // change site is brittle; mark for redraw and
                    // let the next iteration paint.
                    needs_redraw = true;
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            log_key_event(&key);
                            if let Some(exit) = self.dispatch_key(key) {
                                if let Some(state) = self.active_dialog.take() {
                                    let _ = state.reply().send(None);
                                }
                                return Ok(exit);
                            }
                        }
                        Event::Paste(text) => self.handle_paste(&text),
                        Event::Mouse(mouse) => self.handle_mouse_event(mouse),
                        Event::Resize(_, _) => {
                            // Width changed; every cached height is
                            // measured against the prior width and is
                            // now stale.
                            if let Ok(mut buf) = self.buffer.lock() {
                                buf.invalidate_all_heights();
                            }
                        }
                        _ => {}
                    }
                    break;
                }
                // No event arrived; check the worker thread for
                // buffer mutations (streaming deltas, tool results)
                // and break out to repaint if the version moved.
                let v = self.buffer_version();
                if v != last_buffer_version {
                    needs_redraw = true;
                    break;
                }
            }
            // Periodic-wake fallthrough: while the agent is mid-turn or
            // a tool is in-flight, the only thing that changes without
            // a buffer mutation is the modeline spinner, and it only
            // advances on a 100ms cadence. Repaint solely when its
            // frame index has actually moved since the last paint, so a
            // static buffer during a long tool call (build, test run,
            // sleep) costs ~10 redraws/s rather than one per 50ms wake,
            // and never repaints a byte-identical frame.
            if !needs_redraw && animating {
                let idx = crate::view::spinner_frame_index();
                if idx != last_spinner_idx {
                    last_spinner_idx = idx;
                    needs_redraw = true;
                }
            }
        }
    }

    /// True when the worker has marked the [`crate::usage::SessionUsage`]
    /// snapshot as `working`. The render path uses it to drive the
    /// modeline spinner; the event loop uses it to force periodic
    /// redraws so the spinner animates.
    pub(crate) fn is_working(&self) -> bool {
        self.session_usage
            .as_ref()
            .and_then(|h| h.lock().ok().map(|g| g.working))
            .unwrap_or(false)
    }

    /// Read the buffer's current mutation counter without holding
    /// the lock across the rest of the loop.
    pub(crate) fn buffer_version(&self) -> u64 {
        self.buffer.lock().map_or(0, |b| b.version())
    }

    /// Ensure `search_match_set` is up to date. Recomputes when the
    /// pattern changed or the buffer version moved (new blocks,
    /// streaming deltas). Returns a reference to the match set and
    /// the pattern slice for use by `emphasis_for`.
    pub(crate) fn refresh_search_matches(
        &mut self,
    ) -> Option<(&std::collections::HashSet<usize>, &str)> {
        let pattern = self.search_pattern.as_deref()?;
        let version = self.buffer_version();
        if version != self.search_match_version {
            self.search_match_set = self
                .buffer
                .lock()
                .ok()
                .map(|b| b.match_indices(pattern).into_iter().collect())
                .unwrap_or_default();
            self.search_match_version = version;
        }
        Some((&self.search_match_set, pattern))
    }

    /// True when there's at least one in-flight tool call (a
    /// `ToolCall` block whose matching `ToolResult` hasn't arrived).
    /// The renderer paints "running Xs" for these and we want it to
    /// tick even on an otherwise idle event loop.
    pub(crate) fn has_running_tool_call(&self) -> bool {
        let Ok(buf) = self.buffer.lock() else {
            return false;
        };
        let blocks = buf.blocks();
        let mut pending: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for b in blocks {
            if let crate::buffer::Block::ToolCall { call_id, .. } = b {
                pending.insert(call_id.as_str());
            }
        }
        for b in blocks {
            if let crate::buffer::Block::ToolResult { call_id, .. } = b {
                pending.remove(call_id.as_str());
            }
        }
        !pending.is_empty()
    }

    /// Emit a DECSCUSR cursor-shape escape if the desired shape for
    /// the current mode + pane focus differs from the last shape we
    /// emitted. Reapplying the same shape every frame causes some
    /// terminals (kitty, mlterm) to flicker the cursor briefly.
    pub(crate) fn sync_cursor_style(&mut self) {
        use ratatui::crossterm::cursor::SetCursorStyle;
        let pane_focused = self.input.focused_pane() == Pane::Input;
        let key = (self.input.mode(), pane_focused);
        if self.last_cursor_style == Some(key) {
            return;
        }
        // Buffer pane focused: cursor is hidden in the input card;
        // fall back to the user's shell-default shape so anywhere
        // ratatui happens to paint a cursor matches ambient style.
        // Visual + Input pane keeps the input cursor hidden during
        // buffer-cell visual selection, but we leave the shape as
        // Block so the next mode change starts from a sensible
        // default.
        let style = match key {
            (Mode::Insert, true) => SetCursorStyle::SteadyBar,
            (Mode::Normal | Mode::Visual, true) => SetCursorStyle::SteadyBlock,
            (_, false) => SetCursorStyle::DefaultUserShape,
        };
        let _ = ratatui::crossterm::execute!(std::io::stdout(), style);
        self.last_cursor_style = Some(key);
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn draw(&mut self, tui: &mut Tui) -> Result<(), TuiError> {
        self.sync_cursor_style();
        // compute_search_match_count locks self.buffer internally; do
        // it BEFORE we hold the lock or we'll deadlock the moment a
        // search is active.
        let search_match_count = self.compute_search_match_count();
        let search_match_set = self.refresh_search_matches().map(|(set, _)| set).cloned();
        let render_width = tui.terminal().size().map_or(80, |r| r.width);
        self.refresh_plugin_widget_texts(render_width);
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let cmdline = self.cmdline.as_ref();
        let model_snapshot = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()));
        let status = view::StatusCtx {
            model: model_snapshot.as_deref(),
            session_id: self.status_session_id.as_deref(),
            search_pattern: self.search_pattern.as_deref(),
            search_match_set: search_match_set.as_ref(),
            search_line: self.search_line.as_ref(),
            search_match_count,
            plugin_widgets: &self.plugin_widget_texts,
            plugin_status: &self.plugin_status_cache,
            plugin_header: &self.plugin_header_lines,
            plugin_footer: &self.plugin_footer_lines,
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let session_usage = self.session_usage_snapshot();
        let live_toasts = self.live_toasts();
        let bottom = if self.modeline_visible() {
            crate::layout::STATUS_BOTTOM_LINES_DEFAULT
        } else {
            0
        };
        // The autocomplete popup yields to every modal layer; it only
        // paints during plain input editing.
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
        let plugin_overlay = self.plugin_overlay.as_mut();
        let slash_palette = self.slash_palette.as_ref();
        let input_completion = if show_completion {
            self.input_completion.as_ref()
        } else {
            None
        };
        let context_menu = self.context_menu.as_ref();
        let input = &self.input;
        tui.terminal().draw(|frame| {
            // Compute the input region size from the *visual* row
            // count after wrap, not the logical `\n` count, so a
            // long single line that overflows the body width grows
            // the input card instead of being silently clipped.
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
            if let Some(palette) = slash_palette {
                palette.render(frame, regions);
                palette.place_cursor(frame, regions);
            }
            if let Some(completion) = input_completion {
                completion.render(frame, regions);
            }
            if let Some(menu) = context_menu {
                menu.render(frame, regions.buffer);
            }
            if let Some(overlay) = plugin_overlay {
                let modal = overlay.measure(frame.area());
                frame.render_widget(crate::opaque::OpaqueClear, modal);
                let theme = crate::theme::current();
                let ctx = crate::overlay::OverlayCtx {
                    theme: &theme,
                    viewport: frame.area(),
                };
                overlay.render(modal, frame.buffer_mut(), &ctx);
            }
        })?;
        self.captured_rows = captured_rows;
        Ok(())
    }
}
