//! The App run loop, frame draw, and run-state queries.

use std::io;

use crossbeam_channel::{Receiver, select_biased};

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

use crate::terminal::InputReader;

impl App {
    /// Drive the event loop until the user quits. Returns the exit
    /// reason. The caller is expected to drop the [`Tui`] (which
    /// restores the terminal) before printing anything to stdout.
    /// Dispatch terminal-suspending chords, wait for input, engine
    /// events and deadlines, and drive the worker. Long by nature: it
    /// is the whole event loop.
    #[allow(clippy::too_many_lines)]
    pub fn run(&mut self, tui: &mut Tui) -> Result<AppExit, TuiError> {
        // Shortest gap between two paints driven by engine events, so a
        // burst of streamed deltas repaints at most once per frame.
        const FRAME: Duration = Duration::from_millis(16);
        let input = InputReader::spawn()?;
        let mut engine = self.forward_engine_events();
        // Always paint once before the steady-state loop.
        let mut last_buffer_version = self.buffer_version();
        let mut last_spinner_idx = crate::view::spinner_frame_index();
        let mut last_draw = Instant::now();
        let mut needs_redraw = true;
        self.color_depth = tui.color_depth();
        loop {
            if self.apply_option_changes() {
                needs_redraw = true;
            }
            if let Some(enable) = self.pending_mouse_capture.take() {
                tui.set_mouse_capture(enable);
            }
            self.drain_plugin_compact_request();
            self.drain_plugin_fork_request();
            self.drain_plugin_switch_request();
            self.drain_clipboard_attach();
            if self.drain_plugin_refresh() {
                needs_redraw = true;
            }
            // Dialog + theme drains can mutate the visible screen
            // (overlay open, theme swap). Without this, the worker
            // pushes a `kage.ui.select` request from a /command, we
            // open the overlay, but `needs_redraw` is still false and
            // the loop waits until the user happens to press a key.
            // Force a paint on the next pass.
            if self.drain_plugin_dialog() {
                needs_redraw = true;
            }
            if self.drain_engine_events() || self.take_plugin_redraw() {
                needs_redraw = true;
            }
            if self.refresh_highlights() {
                needs_redraw = true;
            }
            let now = Instant::now();
            if self.escalation.is_some_and(|(_, until)| until <= now) {
                self.escalation = None;
                needs_redraw = true;
            }
            if self.keymap_deadline().is_some_and(|at| at <= now) {
                needs_redraw = true;
                let routed = self.tick_keymap(now);
                if let Some(exit) = self.apply_routed(routed) {
                    if let Some(state) = self.active_dialog.take() {
                        let _ = state.reply().send(None);
                    }
                    return Ok(exit);
                }
                self.refresh_input_completion();
            }
            self.refresh_plugin_session_list_if_stale();
            if needs_redraw {
                self.draw(tui)?;
                last_draw = Instant::now();
                last_buffer_version = self.buffer_version();
                last_spinner_idx = crate::view::spinner_frame_index();
                needs_redraw = false;
            }
            // Timers and the spinner tick with no input or deltas, so a
            // run in flight wakes about once per spinner frame.
            // Computed once and reused for the redraw gate below;
            // `has_running_tool_call` locks the buffer and scans every
            // block, so calling it twice per iteration is wasteful.
            let animating =
                self.is_working() || self.is_run_in_flight() || self.has_running_tool_call();
            let tick = if animating {
                // 50ms keeps the spinner and the timers moving, and
                // shaves the worst-case lag after `working` flips false
                // (e.g. after a cancel takes effect) so the spinner
                // stops effectively at once. The redraw it may trigger
                // is gated on actual visible change below.
                Duration::from_millis(50)
            } else {
                // 200ms idle wake (5 Hz) picks up state that worker and
                // plugin threads change without waking the loop: plugin
                // dialogs, hot-reload snapshots, plugin output, clipboard
                // reads and host log lines. A 1s wake felt frozen: after
                // a /command that opened a `kage.ui.*` dialog, the
                // overlay would not appear until the next keypress. The
                // redraw gate below still skips repaints when nothing
                // visible changed.
                Duration::from_millis(200)
            };
            let mut deadline = Instant::now() + tick;
            // Toasts auto-expire on a wall-clock schedule independent
            // of key input; cap the wait at the next toast expiration
            // and force a redraw each tick so the overlay appears
            // immediately when pushed from a worker thread and
            // disappears when its deadline fires, regardless of
            // whether the user pressed a key.
            if let Some(toast_deadline) = self.next_toast_deadline() {
                if toast_deadline < deadline {
                    deadline = toast_deadline;
                }
                needs_redraw = true;
            }
            // A pending key sequence resolves at its timeout even when
            // no further key arrives.
            if let Some(keymap_deadline) = self.keymap_deadline() {
                deadline = deadline.min(keymap_deadline);
            }
            // An armed quit and the cleared-draft note lapse on their
            // own, and the footer hint changes with them.
            if let Some((_, until)) = self.escalation {
                deadline = deadline.min(until);
            }
            if let Some(event) = wait(input.events(), &mut engine, deadline, last_draw + FRAME) {
                // Only events that can change the screen set the
                // redraw flag. `Moved` mouse events (the terminal
                // reports one per pixel of travel while capture is
                // on) and focus flips mutate nothing visible, and
                // repainting per event would pin a core.
                match event? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        log_key_event(&key);
                        needs_redraw = true;
                        // `Ctrl+G` suspends the terminal for an
                        // external editor, so it bypasses dispatch.
                        if self.external_edit_key(key) {
                            self.edit_in_external_editor(tui);
                        } else if let Some(exit) = self.dispatch_key(key) {
                            if let Some(state) = self.active_dialog.take() {
                                let _ = state.reply().send(None);
                            }
                            return Ok(exit);
                        }
                        // `:login` defers here: only this loop
                        // owns the [`Tui`] it suspends.
                        if self.consume_pending_login(tui) {
                            needs_redraw = true;
                        }
                    }
                    Event::Paste(text) => {
                        needs_redraw = true;
                        self.handle_paste(&text);
                    }
                    Event::Mouse(mouse) => {
                        if !matches!(mouse.kind, MouseEventKind::Moved) {
                            needs_redraw = true;
                        }
                        self.handle_mouse_event(mouse);
                    }
                    Event::Resize(_, _) => {
                        // Width changed; every cached height is
                        // measured against the prior width and is
                        // now stale.
                        needs_redraw = true;
                        let mut buf = lock(&self.buffer);
                        buf.invalidate_all_heights();
                    }
                    _ => {}
                }
                input.resume();
            } else if self.buffer_version() != last_buffer_version {
                // Another thread changed the buffer. Engine events
                // wait for the drain at the top of the loop.
                needs_redraw = true;
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

    /// Move the engine events onto a thread that passes them back
    /// through `engine_rx` and signals each arrival on the returned
    /// channel, so the run loop can wait on them next to terminal
    /// input. `None` when no engine events are wired.
    fn forward_engine_events(&mut self) -> Option<Receiver<()>> {
        let source = self.engine_rx.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        self.engine_rx = Some(rx);
        std::thread::spawn(move || {
            for envelope in source {
                if tx.send(envelope).is_err() {
                    break;
                }
                let _ = wake_tx.try_send(());
            }
        });
        Some(wake_rx)
    }

    /// True when the worker has marked the [`crate::usage::SessionUsage`]
    /// snapshot as `working`. The render path uses it to drive the
    /// spinner and the working row; the event loop uses it to force
    /// periodic redraws so they animate.
    pub(crate) fn is_working(&self) -> bool {
        self.session_usage.as_ref().is_some_and(|h| lock(h).working)
    }

    /// Read the buffer's current mutation counter without holding
    /// the lock across the rest of the loop.
    pub(crate) fn buffer_version(&self) -> u64 {
        lock(&self.buffer).version()
    }

    /// Recompute `search_match_set` when stale (pattern changed or
    /// buffer version moved) and clear it when no pattern is active.
    /// Blocks that enter or leave the set drop their render caches,
    /// which bake in the match rule. Call before `search_matches`.
    /// Streaming deltas inside the re-parse throttle window skip the
    /// rescan: the match list may lag the live text by one window,
    /// which the counter and jump already tolerate.
    pub(crate) fn refresh_search_matches(&mut self) {
        let Some(pattern) = self.search_pattern.as_deref() else {
            if !self.search_match_set.is_empty() {
                let mut buf = lock(&self.buffer);
                for &idx in &self.search_match_set {
                    buf.invalidate_height(idx);
                }
            }
            self.search_match_set.clear();
            self.search_match_pattern.clear();
            return;
        };
        let pattern_changed = pattern != self.search_match_pattern;
        let version = self.buffer_version();
        if !pattern_changed
            && version != self.search_match_version
            && lock(&self.buffer).stream_edits_pending()
        {
            return;
        }
        if pattern_changed || version != self.search_match_version {
            let mut buf = lock(&self.buffer);
            let matches = buf.match_indices(pattern);
            let old = &self.search_match_set;
            let entered = matches.iter().filter(|i| old.binary_search(i).is_err());
            let left = old.iter().filter(|i| matches.binary_search(i).is_err());
            for &idx in entered.chain(left) {
                buf.invalidate_height(idx);
            }
            self.search_match_version = buf.version();
            drop(buf);
            self.search_match_set = matches;
            self.search_match_pattern = pattern.to_owned();
        }
    }

    /// Cached block indices matching the active pattern, in buffer
    /// order. Empty when no search is active. Call
    /// `refresh_search_matches` first.
    pub(crate) fn search_matches(&self) -> &[usize] {
        &self.search_match_set
    }

    /// True when there's at least one in-flight tool call. The
    /// renderer ticks its timer, so the loop repaints even when
    /// otherwise idle.
    pub(crate) fn has_running_tool_call(&self) -> bool {
        lock(&self.buffer).has_running_tool_call()
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

    pub(crate) fn draw(&mut self, tui: &mut Tui) -> Result<(), TuiError> {
        self.sync_cursor_style();
        self.paint(tui.terminal())
    }

    /// Paint one frame into `terminal`: the chrome, the buffer and
    /// every open overlay.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn paint<B>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<(), TuiError>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // Enforce the scrollback cap before anything reads block
        // indices: compaction shifts them, and its version bump makes
        // the search-match list below rebuild against the new numbering.
        lock(&self.buffer).trim_scrollback();
        // compute_search_match_count locks self.buffer internally; do
        // it BEFORE we hold the lock or we'll deadlock the moment a
        // search is active.
        let search_match_count = self.compute_search_match_count();
        let search_match_set = if self.search_pattern.is_some() {
            Some(self.search_matches().to_vec())
        } else {
            None
        };
        let render_width = terminal.size().map_or(80, |r| r.width);
        self.refresh_plugin_widget_texts_if_due(render_width);
        let (mut buffer, buffer_version) = self.take_draw_snapshot();
        let mut session_usage = self.session_usage_snapshot();
        if self.focus.is_some()
            && let Some(usage) = session_usage.as_mut()
        {
            usage.working = self.is_run_in_flight();
        }
        let hint = self.footer_hint();
        let activity = self.activity_label(&buffer, render_width);
        let title = self
            .slots
            .as_ref()
            .and_then(|slots| lock(&slots.ui_state()).session_title.clone());
        let model_id = self.model_id(session_usage.as_ref());
        let model_label = self.model_label(model_id.as_deref());
        let start_keys = self.start_keys();
        let agents_key = self.key_label("OpenAgents");
        if self.agents_overlay.is_some() {
            let rows = self.agents_overlay_rows();
            if let Some(overlay) = self.agents_overlay.as_mut() {
                overlay.set_rows(rows);
            }
        }
        let cwd = self
            .completion_workdir
            .as_ref()
            .map(|dir| dir.display().to_string());
        let cmdline = self.cmdline.as_ref();
        let agent_rows = self.agent_rows();
        let pending: Vec<view::PendingPrompt> = self
            .pending
            .iter()
            .filter(|(session, _)| *session == self.focus)
            .map(|(_, p)| p.clone())
            .collect();
        let breadcrumb = self.breadcrumb();
        let placeholder = self.agent_placeholder();
        let status = view::StatusCtx {
            model: model_label.as_deref(),
            session_id: self.status_session_id.as_deref(),
            title: title.as_deref(),
            search_pattern: self.search_pattern.as_deref(),
            search_match_set: search_match_set.as_deref(),
            search_line: self.search_line.as_ref(),
            search_match_count,
            plugin_widgets: &self.plugin_widget_texts,
            plugin_status: &self.plugin_status_cache,
            slots: self.slot_frame(render_width),
            hint: Some(hint.as_str()),
            activity: activity.as_deref(),
            cwd: cwd.as_deref(),
            model_id: model_id.as_deref(),
            start: self.start_info.as_ref(),
            start_keys,
            pending: &pending,
            agents: &agent_rows,
            agents_key: agents_key.as_deref(),
            breadcrumb: breadcrumb.as_ref(),
            placeholder: placeholder.as_deref(),
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let live_toasts = self.live_toasts();
        // The autocomplete popup yields to every modal layer; it only
        // paints during plain input editing.
        let show_completion = self.input_completion.is_some()
            && self.slash_palette.is_none()
            && self.cmdline.is_none()
            && self.search_line.is_none()
            && self.picker.is_none()
            && self.settings_overlay.is_none()
            && self.session_tree.is_none()
            && self.agents_overlay.is_none()
            && self.approval_panel.is_none()
            && self.help_overlay.is_none()
            && self.plugin_overlay.is_none();
        let picker = self.picker.as_mut();
        let settings_overlay = self.settings_overlay.as_mut();
        let session_tree = self.session_tree.as_mut();
        let help_overlay = self.help_overlay.as_mut();
        let agents_overlay = self.agents_overlay.as_mut();
        let plugin_overlay = self.plugin_overlay.as_mut();
        let approval = self
            .approval_panel
            .as_ref()
            .map(|panel| (panel, self.permission_queue.len()));
        let slash_palette = self
            .slash_palette
            .as_ref()
            .filter(|_| self.approval_panel.is_none());
        let input_completion = if show_completion {
            self.input_completion.as_ref()
        } else {
            None
        };
        let context_menu = self.context_menu.as_ref();
        let input = &self.input;
        let color_depth = self.color_depth;
        let mut pinned_area = ratatui::layout::Rect::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut heights =
                    view::chrome_heights(&status, session_usage.as_ref(), input, area.width);
                if let Some((panel, _)) = approval {
                    heights.input = panel.height(area.width).min(area.height * 3 / 5);
                }
                let regions = split(area, heights);
                let mut view_regions = regions;
                // The palette and the completion popup anchor to the
                // input box, below the pinned agents and pending rows.
                let mut box_regions = regions;
                if approval.is_some() {
                    view_regions.input.height = 0;
                } else {
                    let (agents, _, input_box) =
                        view::split_input(regions.input, agent_rows.len(), status.pending.len());
                    pinned_area = agents;
                    box_regions.input = input_box;
                }
                view::render(
                    frame,
                    view_regions,
                    &mut buffer,
                    input,
                    cmdline,
                    &status,
                    screen_selection,
                    &mut captured_rows,
                    session_usage.as_ref(),
                    &live_toasts,
                );
                if let Some((panel, waiting)) = approval {
                    panel.render(frame, regions.input, waiting);
                }
                let above_input = ratatui::layout::Rect::new(
                    area.x,
                    area.y,
                    area.width,
                    regions.input.y.saturating_sub(area.y),
                );
                if let Some(picker) = picker {
                    picker.render(frame, above_input);
                }
                if let Some(settings) = settings_overlay {
                    settings.render(frame, above_input);
                }
                if let Some(tree) = session_tree {
                    tree.render(frame, above_input);
                }
                if let Some(agents) = agents_overlay {
                    agents.render(frame, above_input);
                }
                if let Some(help) = help_overlay {
                    let modal = crate::overlay::OverlayWidget::measure(help, above_input);
                    frame.render_widget(crate::opaque::OpaqueClear, modal);
                    let theme = crate::theme::current();
                    let ctx = crate::overlay::OverlayCtx {
                        theme: &theme,
                        viewport: above_input,
                    };
                    crate::overlay::OverlayWidget::render(help, modal, frame.buffer_mut(), &ctx);
                }
                if let Some(palette) = slash_palette {
                    palette.render(frame, box_regions);
                    palette.place_cursor(frame, box_regions);
                }
                if let Some(completion) = input_completion {
                    completion.render(frame, box_regions);
                }
                if let Some(menu) = context_menu {
                    menu.render(frame, regions.buffer);
                }
                if let Some(overlay) = plugin_overlay {
                    let modal = overlay.measure(area);
                    frame.render_widget(crate::opaque::OpaqueClear, modal);
                    let theme = crate::theme::current();
                    let ctx = crate::overlay::OverlayCtx {
                        theme: &theme,
                        viewport: area,
                    };
                    overlay.render(modal, frame.buffer_mut(), &ctx);
                }
                color_depth.apply(frame.buffer_mut());
            })
            .map_err(|err| TuiError::Io(std::io::Error::other(err.to_string())))?;
        // Merge renderer-owned state (caches, clamped scroll, last-frame
        // geometry) from the snapshot back into the live buffer. The
        // mutex is never held across the paint itself. Then park the
        // drawn snapshot: an unchanged version redraws it verbatim.
        lock(&self.buffer).merge_render_state(&buffer);
        self.park_draw_snapshot(buffer, buffer_version);
        self.captured_rows = captured_rows;
        self.pinned_hits = (pinned_area.y..pinned_area.bottom())
            .zip(agent_rows.iter().take(view::AGENT_MAX_ROWS))
            .map(|(row, agent)| (row, agent.session))
            .collect();
        Ok(())
    }

    /// Produce the buffer to draw on and the live version it was
    /// taken at. When the live buffer is untouched since the last
    /// draw (and no stream reparse is pending), the parked snapshot
    /// is returned instead of deep-cloning every block again; on a
    /// large resumed session that clone dominates idle-frame cost.
    /// A fresh clone is already warm: every draw merges its renderer
    /// caches back into the live buffer before parking.
    pub(crate) fn take_draw_snapshot(&mut self) -> (crate::Buffer, u64) {
        let (live_version, stream_pending) = {
            let live = lock(&self.buffer);
            (live.version(), live.stream_edits_pending())
        };
        let reuse = !stream_pending && self.draw_snapshot_version == live_version;
        let buffer = match self.draw_snapshot.take() {
            Some(snap) if reuse => snap,
            _ => lock(&self.buffer).clone(),
        };
        (buffer, live_version)
    }

    /// Park a drawn snapshot for [`Self::take_draw_snapshot`] to
    /// hand back on the next unchanged frame.
    pub(crate) fn park_draw_snapshot(&mut self, buffer: crate::Buffer, version: u64) {
        self.draw_snapshot_version = version;
        self.draw_snapshot = Some(buffer);
    }
}

/// Block until terminal input arrives or `deadline` passes. An engine
/// wake-up ends the wait at `frame_at` instead, so engine events paint
/// at once after a quiet spell and at most once per frame in a burst.
/// A closed engine channel is dropped from later waits.
fn wait(
    input: &Receiver<io::Result<Event>>,
    engine: &mut Option<Receiver<()>>,
    mut deadline: Instant,
    frame_at: Instant,
) -> Option<io::Result<Event>> {
    let idle = crossbeam_channel::never();
    let mut woken = false;
    loop {
        let engine_arm = engine.as_ref().filter(|_| !woken).unwrap_or(&idle);
        let open = select_biased! {
            recv(input) -> event => {
                return Some(event.unwrap_or_else(|_| {
                    Err(io::Error::other("terminal input reader stopped"))
                }));
            }
            recv(engine_arm) -> wake => wake.is_ok(),
            default(deadline.saturating_duration_since(Instant::now())) => return None,
        };
        if !open {
            *engine = None;
        }
        woken = true;
        deadline = deadline.min(frame_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn input_wins_over_a_waiting_engine_event() {
        let (input_tx, input) = crossbeam_channel::bounded(1);
        let (wake_tx, wake) = crossbeam_channel::bounded(1);
        input_tx.send(Ok(Event::FocusGained)).unwrap();
        wake_tx.send(()).unwrap();
        let now = Instant::now();
        let event = wait(&input, &mut Some(wake), now + LONG, now);
        assert!(matches!(event, Some(Ok(Event::FocusGained))));
    }

    #[test]
    fn an_engine_event_ends_the_wait_at_the_frame() {
        let (_input_tx, input) = crossbeam_channel::bounded(1);
        let (wake_tx, wake) = crossbeam_channel::bounded(1);
        wake_tx.send(()).unwrap();
        let mut engine = Some(wake);
        let start = Instant::now();
        let frame_at = start + Duration::from_millis(20);
        assert!(wait(&input, &mut engine, start + LONG, frame_at).is_none());
        assert!(Instant::now() >= frame_at);
        assert!(start.elapsed() < LONG);
        assert!(engine.is_some());
    }

    #[test]
    fn a_closed_engine_channel_is_dropped() {
        let (_input_tx, input) = crossbeam_channel::bounded(1);
        let (_, wake) = crossbeam_channel::bounded::<()>(1);
        let mut engine = Some(wake);
        let now = Instant::now();
        assert!(wait(&input, &mut engine, now + LONG, now).is_none());
        assert!(engine.is_none());
    }
}
