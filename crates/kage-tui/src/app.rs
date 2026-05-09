//! Interactive TUI event loop.
//!
//! [`App::run`] owns the [`Tui`] and a [`SharedBuffer`], polls crossterm
//! key events, drives [`InputState`], applies [`InputAction`]s to the
//! buffer, and redraws the screen ~30 times a second. Submitting a
//! prompt fires a `RunRequest` through the provided sink; the host is
//! responsible for spawning the agent loop on a worker thread and
//! pushing its events into the same `SharedBuffer` via [`TuiHooks`].

use std::io::Write;
use std::sync::mpsc::{Sender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ratatui::crossterm::event::{self, Event, KeyEventKind, MouseEventKind};

use crate::cmdline::{CommandLine, CommandLineEvent};
use crate::error::TuiError;
use crate::events::SharedBuffer;
use crate::input::{InputAction, InputState, Mode, Pane};
use crate::layout::{input_height_for, split};
use crate::overlay::{OverlayPicker, PickerEvent};
use crate::picker::PickItem;
use crate::terminal::Tui;
use crate::view;

/// Lines scrolled per mouse wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;

/// When `KAGE_DEBUG_KEYS` is set to a non-empty value, every press is
/// appended to the file at that path (or `$XDG_STATE_HOME/kage/keys.log`
/// when the value is `1`). Lets us diagnose terminal-specific quirks
/// like "Shift+Enter doesn't transmit" without instrumenting the host.
fn log_key_event(key: &ratatui::crossterm::event::KeyEvent) {
    let Ok(value) = std::env::var("KAGE_DEBUG_KEYS") else {
        return;
    };
    if value.is_empty() {
        return;
    }
    let path = if value == "1" {
        let Some(home) = std::env::var_os("XDG_STATE_HOME").or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = std::path::PathBuf::from(h);
                p.push(".local/state");
                p.into_os_string()
            })
        }) else {
            return;
        };
        let mut p = std::path::PathBuf::from(home);
        p.push("kage");
        let _ = std::fs::create_dir_all(&p);
        p.push("keys.log");
        p
    } else {
        std::path::PathBuf::from(value)
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "{:?}  modifiers={:?}  kind={:?}",
            key.code, key.modifiers, key.kind
        );
    }
}

/// Request the host should act on. Either the user submitted a prompt
/// (the host runs the agent loop in a worker thread), the user asked
/// to cancel the in-flight turn, the user picked a different model,
/// or the user picked a prior session to resume into the current TUI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunRequest {
    /// New user prompt to submit to the agent loop.
    Submit(String),
    /// Trip the agent loop's cancellation flag.
    Cancel,
    /// Switch to a different `provider:model` for subsequent turns.
    SwitchModel(String),
    /// Replay the session at the given path into the conversation
    /// buffer and pre-load its history into the agent context. The
    /// next [`RunRequest::Submit`] continues from that history.
    ResumeSession(std::path::PathBuf),
    /// Invoke a plugin-registered command by name with the trailing
    /// argument string. The host runs it on the worker thread (so the
    /// main thread keeps painting) and pushes its output as a custom
    /// block.
    InvokePluginCommand {
        /// Plugin command name (without the leading `/` or `:`).
        name: String,
        /// Whatever followed the command on the cmdline; an empty
        /// string when the command takes no arguments.
        args: String,
    },
}

/// Outcome of [`App::run`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppExit {
    /// User pressed `Ctrl+Q` / `:q` to leave the TUI cleanly.
    Quit,
}

/// Which overlay picker is currently open. Determines how
/// [`PickerEvent::Picked`] is dispatched: a model id triggers a switch,
/// a session path triggers a resume, a command name runs the same
/// handler the `:` command line uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerKind {
    Model,
    Session,
    Command,
}

/// Closure that returns the current set of resumable sessions on
/// demand. Listing happens on the main thread when the user presses
/// `Ctrl+R`, so a fresh scan reflects any sessions written elsewhere
/// since the TUI started.
pub type SessionLister = Box<dyn Fn() -> Vec<PickItem> + Send + 'static>;

/// Built-in commands offered by the slash palette and the `:` line.
/// `(name, description)`. Plugin commands will join this set later.
const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("help", "show available commands"),
    ("q", "quit"),
    ("quit", "quit"),
    ("cancel", "cancel the in-flight turn"),
    ("model", "switch to provider:model (takes one arg)"),
    ("fold all", "fold every foldable block"),
    ("unfold all", "unfold every foldable block"),
    ("theme", "switch palette (use `:theme list` to enumerate)"),
    (
        "mouse",
        "toggle mouse capture (`mouse off` lets the terminal handle text selection)",
    ),
];

fn builtin_command_picker_items() -> Vec<PickItem> {
    BUILTIN_COMMANDS
        .iter()
        .map(|(name, desc)| {
            let label = format!("{name:<14}  {desc}");
            PickItem::simple((*name).to_owned()).with_label(label)
        })
        .collect()
}

/// Runtime state for the interactive TUI loop.
pub struct App {
    buffer: SharedBuffer,
    input: InputState,
    requests: Sender<RunRequest>,
    /// Available `provider:model` ids the model picker offers. Empty
    /// when the host has not registered any models with the App.
    model_choices: Vec<PickItem>,
    /// Active modal overlay, if any. Drives both render and input
    /// routing while present.
    picker: Option<OverlayPicker>,
    /// Which picker is open, mirroring [`Self::picker`]. Used to
    /// dispatch the picked value to the right `RunRequest`.
    picker_kind: Option<PickerKind>,
    /// Provider of resumable sessions for the session picker. None
    /// disables the picker (Ctrl+R is a no-op).
    session_lister: Option<SessionLister>,
    /// Open `:` command line, if any. While present it owns key input
    /// and replaces the status bar's mode pill.
    cmdline: Option<CommandLine>,
    /// Open `/` search line, if any. Reuses the [`CommandLine`]
    /// widget; painted with a `/` prefix instead of `:`.
    search_line: Option<CommandLine>,
    /// The most recently submitted search pattern. While set, blocks
    /// containing the pattern render with a Match emphasis and `n` /
    /// `N` walk between them.
    search_pattern: Option<String>,
    /// Status bar context the host populates: live model id and a
    /// short session-id pill. Held as `Arc<Mutex<...>>` so the worker
    /// thread can update them out from under the renderer (model
    /// switches mid-session).
    status_model: Option<Arc<Mutex<String>>>,
    status_session_id: Option<String>,
    /// Plugin-registered command names + descriptions for palette
    /// display. Builtin names take precedence on collision.
    plugin_commands: Vec<(String, String)>,
    /// Pending request to toggle terminal mouse capture, applied by
    /// `run` between iterations. `None` means leave the capture state
    /// as-is. The indirection exists because `run_command` can't
    /// reach `Tui` directly; only [`Self::run`] holds it.
    pending_mouse_capture: Option<bool>,
    /// In-progress mouse gesture started by a left-button press. The
    /// tuple is `(down_row, down_block_idx, dragged)`. `dragged` flips
    /// to true the first time a drag event arrives; on `Up`, a
    /// non-dragged click on the block's header row toggles its fold,
    /// while a dragged release leaves the screen selection in place
    /// for the user to yank with `y`.
    mouse_drag_anchor: Option<(u16, usize, bool)>,
    /// Active screen selection in `(virtual_row, col)` coordinates,
    /// where `virtual_row` is the index of the row across the whole
    /// rendered buffer (independent of scroll position). Painted as
    /// a bg overlay over whatever the renderer drew so it covers
    /// tool blocks and chrome equally without changing layout.
    /// Tracking in virtual-row space lets the selection survive a
    /// scroll: rows that go off-screen stay selected, and their
    /// previously-captured text remains available for yank.
    screen_selection: Option<((usize, u16), (usize, u16))>,
    /// Cell snapshots accumulated for every virtual row the user has
    /// dragged through during the current selection. Indexed by
    /// virtual row; each entry stores the row's painted chars and
    /// per-cell decoration flag as captured when it was visible.
    /// Cleared on `MouseDown` (new selection) or after a yank. Lets
    /// `y` recover the full selected text even when part of the
    /// selection has scrolled off-screen.
    captured_rows: std::collections::BTreeMap<usize, Vec<view::CapturedCell>>,
    /// Last DECSCUSR cursor shape we emitted to the terminal, keyed
    /// by `(mode, pane_focused_on_input)`. Stored so [`Self::draw`]
    /// can skip the escape on frames where the cursor shape would be
    /// identical, avoiding a flicker on terminals that briefly hide
    /// the cursor when the style is reapplied.
    last_cursor_style: Option<(Mode, bool)>,
    /// Optional shared snapshot of the current session's running
    /// token totals + context window. When `Some`, the renderer
    /// claims a one-row modeline below the input card and paints
    /// `model :: in/out :: total/window (pct)`. Updated by the host
    /// worker thread after every turn.
    session_usage: Option<crate::usage::SharedSessionUsage>,
}

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
            session_lister: None,
            cmdline: None,
            status_model: None,
            status_session_id: None,
            plugin_commands: Vec::new(),
            search_line: None,
            search_pattern: None,
            mouse_drag_anchor: None,
            pending_mouse_capture: None,
            screen_selection: None,
            captured_rows: std::collections::BTreeMap::new(),
            last_cursor_style: None,
            session_usage: None,
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

    /// Whether the host has registered a session-usage handle. Used
    /// by the layout split to decide if the modeline row claims a
    /// line of vertical space.
    fn modeline_visible(&self) -> bool {
        self.session_usage.is_some()
    }

    /// Snapshot the session-usage handle, returning `None` when the
    /// host has not registered one or the lock is poisoned.
    fn session_usage_snapshot(&self) -> Option<crate::usage::SessionUsage> {
        self.session_usage
            .as_ref()
            .and_then(|h| h.lock().ok().map(|g| g.clone()))
    }

    /// Register the plugin commands the host wants exposed in the
    /// palette and on the `:` line. Pairs are `(name, description)`.
    /// Names that collide with built-ins are dropped; the host should
    /// log a warning at registration time.
    pub fn set_plugin_commands(&mut self, mut commands: Vec<(String, String)>) {
        commands.retain(|(n, _)| !BUILTIN_COMMANDS.iter().any(|(b, _)| *b == n));
        self.plugin_commands = commands;
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

    /// Drive the event loop until the user quits. Returns the exit
    /// reason. The caller is expected to drop the [`Tui`] (which
    /// restores the terminal) before printing anything to stdout.
    pub fn run(&mut self, tui: &mut Tui) -> Result<AppExit, TuiError> {
        // First frame is unconditional - we always paint once before
        // entering the steady-state event loop.
        let mut last_buffer_version = self.buffer_version();
        let mut needs_redraw = true;
        loop {
            if let Some(enable) = self.pending_mouse_capture.take() {
                tui.set_mouse_capture(enable);
            }
            if needs_redraw {
                self.draw(tui)?;
                last_buffer_version = self.buffer_version();
                needs_redraw = false;
            }
            // Wake periodically to repaint streaming tool-call
            // timers ("running 1.2s") and to pick up worker-thread
            // mutations that race ahead of any input event.
            let deadline = Instant::now() + Duration::from_secs(1);
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
                                return Ok(exit);
                            }
                        }
                        Event::Paste(text) => self.input.paste(&text),
                        Event::Mouse(mouse) => match mouse.kind {
                            MouseEventKind::ScrollUp => self.scroll_by(-MOUSE_SCROLL_LINES),
                            MouseEventKind::ScrollDown => self.scroll_by(MOUSE_SCROLL_LINES),
                            MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                                self.mouse_down(mouse.row, mouse.column);
                            }
                            MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left) => {
                                self.mouse_drag(mouse.row, mouse.column);
                            }
                            MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left) => {
                                self.mouse_up(mouse.row);
                            }
                            _ => {}
                        },
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
            // Periodic-wake fallthrough: if a streaming tool call is
            // in-flight, repaint anyway so the elapsed-time pill
            // ticks visibly. Cheap because the renderer's height
            // cache is hot at this point. Same for the modeline
            // spinner while the agent is mid-turn.
            if !needs_redraw && (self.has_running_tool_call() || self.is_working()) {
                needs_redraw = true;
            }
        }
    }

    /// True when the worker has marked the [`crate::usage::SessionUsage`]
    /// snapshot as `working`. The render path uses it to drive the
    /// modeline spinner; the event loop uses it to force periodic
    /// redraws so the spinner animates.
    fn is_working(&self) -> bool {
        self.session_usage
            .as_ref()
            .and_then(|h| h.lock().ok().map(|g| g.working))
            .unwrap_or(false)
    }

    /// Read the buffer's current mutation counter without holding
    /// the lock across the rest of the loop.
    fn buffer_version(&self) -> u64 {
        self.buffer.lock().map_or(0, |b| b.version())
    }

    /// True when there's at least one in-flight tool call (a
    /// `ToolCall` block whose matching `ToolResult` hasn't arrived).
    /// The renderer paints "running Xs" for these and we want it to
    /// tick even on an otherwise idle event loop.
    fn has_running_tool_call(&self) -> bool {
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
    fn sync_cursor_style(&mut self) {
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

    fn draw(&mut self, tui: &mut Tui) -> Result<(), TuiError> {
        self.sync_cursor_style();
        // compute_search_match_count locks self.buffer internally; do
        // it BEFORE we hold the lock or we'll deadlock the moment a
        // search is active.
        let search_match_count = self.compute_search_match_count();
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
            search_line: self.search_line.as_ref(),
            search_match_count,
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let session_usage = self.session_usage_snapshot();
        let bottom = if self.modeline_visible() {
            crate::layout::STATUS_BOTTOM_LINES_DEFAULT
        } else {
            0
        };
        let picker = self.picker.as_mut();
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
            );
            if let Some(picker) = picker {
                picker.render(frame, frame.area());
            }
        })?;
        self.captured_rows = captured_rows;
        Ok(())
    }

    fn dispatch_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        // Global escape hatches before passing to any modal layer.
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return Some(AppExit::Quit);
        }

        // When the picker overlay is open, it owns the keyboard.
        if self.picker.is_some() {
            return self.dispatch_picker_key(key);
        }

        // The `:` command line is the next-most-modal layer.
        if self.cmdline.is_some() {
            return self.dispatch_cmdline_key(key);
        }

        // The `/` search line is also modal while open.
        if self.search_line.is_some() {
            return self.dispatch_search_key(key);
        }

        let actions = self.input.handle_key(key);
        for action in actions {
            if let Some(exit) = self.apply(action) {
                return Some(exit);
            }
        }
        None
    }

    fn dispatch_search_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        let line = self.search_line.as_mut()?;
        match line.handle_key(key) {
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
    fn compute_search_match_count(&self) -> Option<(usize, usize)> {
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
    fn jump_to_search_match(&mut self, forward: bool) {
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

    fn dispatch_cmdline_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let cmdline = self.cmdline.as_mut()?;
        match cmdline.handle_key(key) {
            CommandLineEvent::Pending => None,
            CommandLineEvent::Cancelled => {
                self.cmdline = None;
                None
            }
            CommandLineEvent::Submit(text) => {
                self.cmdline = None;
                self.run_command(&text)
            }
        }
    }

    /// Dispatch a `:` command. Recognised commands are documented in
    /// the `:help` output below; unknown commands surface a `kage:error`
    /// block in the buffer rather than failing silently.
    fn run_command(&mut self, line: &str) -> Option<AppExit> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        match head {
            "q" | "quit" => Some(AppExit::Quit),
            "cancel" => {
                let _ = self.send_request(RunRequest::Cancel);
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
            "fold" if rest == "all" => {
                self.set_all_folds(true);
                None
            }
            "unfold" if rest == "all" => {
                self.set_all_folds(false);
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
            "" => None,
            other => {
                if self.plugin_commands.iter().any(|(n, _)| n == other) {
                    let _ = self.send_request(RunRequest::InvokePluginCommand {
                        name: other.to_owned(),
                        args: rest.to_owned(),
                    });
                } else {
                    self.push_error(format!("unknown command: {other}"));
                }
                None
            }
        }
    }

    fn push_error(&mut self, msg: impl Into<String>) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:error", msg, false);
        }
    }

    /// Copy the active screen selection to the system clipboard via
    /// OSC52. Walks every captured row in the selection range,
    /// strips renderer-only decoration glyphs (rule chars), trims
    /// trailing whitespace per row, joins with `\n`, and clears the
    /// selection.
    fn yank_screen_selection(&mut self) {
        let text = self.extract_selection_text();
        if text.is_empty() {
            self.clear_selection();
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        self.clear_selection();
        self.notify(format!(
            "yanked {} chars to clipboard",
            text.chars().count()
        ));
    }

    fn clear_selection(&mut self) {
        self.screen_selection = None;
        self.captured_rows.clear();
    }

    /// Anchor a keyboard selection at the focused block's first
    /// visible row (or the viewport top if nothing's focused) and
    /// switch the mode to [`Mode::Visual`]. Subsequent
    /// [`InputAction::Visual*`] events move the cursor end.
    fn enter_visual_mode(&mut self) {
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

    fn move_visual_cursor(&mut self, dvrow: i32, dcol: i32) {
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

    fn snap_visual_cursor_x(&mut self, target_col: i32) {
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
    fn scroll_visual_cursor_into_view(&mut self, cursor_vrow: usize) {
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
    fn yank_focused_block(&mut self) {
        // Force a fresh capture by walking the focused block's
        // screen rows (currently visible) and pulling text from
        // captured_rows.
        let Ok(buf) = self.buffer.lock() else {
            return;
        };
        let Some(idx) = buf.effective_focus() else {
            return;
        };
        let virtual_top = buf.last_virtual_top();
        let area_y = buf.last_area_y();
        let Some((top, bot)) = buf.screen_rows_of(idx) else {
            return;
        };
        drop(buf);
        let mut text = String::new();
        for screen_row in top..bot {
            let vrow = virtual_top.saturating_add(usize::from(screen_row.saturating_sub(area_y)));
            let Some(cells) = self.captured_rows.get(&vrow) else {
                continue;
            };
            let line: String = cells
                .iter()
                .filter(|cell| !cell.decoration)
                .map(|cell| cell.ch)
                .collect();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line.trim_end());
        }
        if text.is_empty() {
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        self.notify(format!(
            "yanked {} chars to clipboard",
            text.chars().count()
        ));
    }

    fn extract_selection_text(&self) -> String {
        let Some((anchor, cursor)) = self.screen_selection else {
            return String::new();
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
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

    fn push_help(&mut self) {
        let body = "available commands:\n  \
                    :q, :quit       leave the TUI\n  \
                    :cancel         cancel the in-flight turn\n  \
                    :model <id>     switch to provider:model (e.g. anthropic:claude-sonnet-4-5)\n  \
                    :fold all       fold every foldable block\n  \
                    :unfold all     unfold every foldable block\n  \
                    :theme <name>   switch palette (use `:theme list` to enumerate)\n  \
                    :mouse off      release mouse capture so the terminal can select/copy text\n  \
                    :mouse on       re-enable kage's mouse handling (click-to-fold, drag-select)\n  \
                    :help           show this help";
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:help", body, false);
        }
    }

    fn run_theme_command(&mut self, rest: &str) {
        match rest {
            "" => {
                let cur = crate::theme::current().name;
                self.notify(format!("theme: {cur} (try `:theme list`)"));
            }
            "list" => {
                let cur = crate::theme::current().name;
                let names = crate::theme::Theme::bundled_names()
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
            name => {
                let names = crate::theme::Theme::bundled_names();
                if !names.contains(&name) {
                    self.push_error(format!("unknown theme: {name} (try `:theme list`)"));
                    return;
                }
                let theme = crate::theme::Theme::by_name(name);
                crate::theme::set_current(theme);
                if let Ok(mut buf) = self.buffer.lock() {
                    // Force a fresh layout pass: every block's
                    // cached height was measured against the prior
                    // theme's bubble background, which doesn't
                    // change geometry but invalidating is cheap and
                    // protects against future theme-driven height
                    // tweaks (different rule glyph widths, etc.).
                    buf.invalidate_all_heights();
                }
                self.notify(format!("theme: {name}"));
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:notify", msg, false);
        }
    }

    fn run_mouse_command(&mut self, rest: &str) {
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

    fn dispatch_picker_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        let picker = self.picker.as_mut()?;
        match picker.handle_key(key) {
            PickerEvent::Pending => {}
            PickerEvent::Cancelled => {
                self.picker = None;
                self.picker_kind = None;
            }
            PickerEvent::Picked(value) => {
                let kind = self.picker_kind;
                self.picker = None;
                self.picker_kind = None;
                match kind {
                    Some(PickerKind::Model) => {
                        let _ = self.send_request(RunRequest::SwitchModel(value));
                    }
                    Some(PickerKind::Session) => {
                        let _ = self.send_request(RunRequest::ResumeSession(
                            std::path::PathBuf::from(value),
                        ));
                    }
                    Some(PickerKind::Command) => {
                        return self.run_command(&value);
                    }
                    None => {}
                }
            }
        }
        None
    }

    fn apply(&mut self, action: InputAction) -> Option<AppExit> {
        // Phase 9.10/9.11/9.17 will wire BeginCommand/BeginSearch/Yank;
        // for now they fall through to the silent EnterMode arm so the
        // modal state machine still cycles cleanly.
        match action {
            InputAction::Submit(text) => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.push_user(text.clone());
                }
                let _ = self.send_request(RunRequest::Submit(text));
            }
            InputAction::Scroll(delta) => self.scroll_by(delta),
            InputAction::ScrollToTop => self.set_scroll(usize::MAX),
            InputAction::ScrollToBottom => self.set_scroll(0),
            InputAction::ToggleFold => self.toggle_last_fold(),
            InputAction::UnfoldAll => self.set_all_folds(false),
            InputAction::FoldAll => self.set_all_folds(true),
            InputAction::Cancel => {
                let _ = self.send_request(RunRequest::Cancel);
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
                let mut items = builtin_command_picker_items();
                for (name, desc) in &self.plugin_commands {
                    let label = format!("{name:<14}  {desc}  [plugin]");
                    items.push(PickItem::simple(name.clone()).with_label(label));
                }
                if !items.is_empty() {
                    self.picker = Some(OverlayPicker::new("Run command", items));
                    self.picker_kind = Some(PickerKind::Command);
                }
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
                if let Some(lister) = self.session_lister.as_ref() {
                    let items = lister();
                    if !items.is_empty() {
                        self.picker = Some(OverlayPicker::new("Resume session", items));
                        self.picker_kind = Some(PickerKind::Session);
                    }
                }
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
        }
        None
    }

    fn send_request(&mut self, req: RunRequest) -> Result<(), TrySendError<RunRequest>> {
        match self.requests.send(req) {
            Ok(()) => Ok(()),
            Err(err) => Err(TrySendError::Disconnected(err.0)),
        }
    }

    /// Mouse left-button press: anchor a virtual-row selection at
    /// the click position. Any prior selection (and its captured
    /// text) is dropped. Focus snaps to whichever block sits under
    /// the click so subsequent keyboard gestures act on it.
    fn mouse_down(&mut self, row: u16, col: u16) {
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
    fn mouse_drag(&mut self, row: u16, col: u16) {
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
    /// zero-width selection; a dragged release leaves the selection
    /// in place so the user can `y` to copy it.
    fn mouse_up(&mut self, row: u16) {
        let Some((_down_row, anchor_idx, dragged)) = self.mouse_drag_anchor.take() else {
            return;
        };
        if dragged {
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

    fn scroll_by(&mut self, delta: i32) {
        if let Ok(mut buf) = self.buffer.lock() {
            // Positive delta = move toward newest (decrement rows-up);
            // negative = move toward oldest (increment rows-up).
            let current = i64::try_from(buf.scroll()).unwrap_or(i64::MAX);
            let target = (current - i64::from(delta)).max(0);
            let clamped = usize::try_from(target).unwrap_or(0);
            buf.set_scroll(clamped);
        }
    }

    fn set_scroll(&mut self, scroll: usize) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_scroll(scroll);
        }
    }

    fn toggle_last_fold(&mut self) {
        if let Ok(mut buf) = self.buffer.lock() {
            if let Some(idx) = buf.effective_focus() {
                buf.toggle_fold(idx);
            }
        }
    }

    fn set_all_folds(&mut self, folded: bool) {
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
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let session_usage = self.session_usage_snapshot();
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
            search_line: self.search_line.as_ref(),
            search_match_count,
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let picker = self.picker.as_mut();
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
                );
                if let Some(picker) = picker {
                    picker.render(frame, frame.area());
                }
            })
            .map_err(|err| TuiError::Io(std::io::Error::other(err.to_string())))?;
        self.captured_rows = captured_rows;
        Ok(())
    }
}

/// Translate an absolute terminal `(row, col)` mouse position to a
/// Best-effort label for the current mode, exposed for the host's
/// status-bar widget.
#[must_use]
pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::events::shared_buffer;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_q_exits_immediately() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let exit = app.handle_key(ctrl('q'));
        assert_eq!(exit, Some(AppExit::Quit));
    }

    #[test]
    fn submitting_a_prompt_pushes_user_block_and_request() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        // Enter insert, type "hi", press Enter.
        app.handle_key(key('i'));
        app.handle_key(key('h'));
        app.handle_key(key('i'));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let req = rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(req, RunRequest::Submit("hi".into()));
        let buf = buffer.lock().unwrap();
        assert!(matches!(
            buf.blocks().last(),
            Some(crate::buffer::Block::User { text }) if text == "hi"
        ));
        assert_eq!(app.input().mode(), Mode::Normal);
    }

    #[test]
    fn ctrl_c_in_normal_emits_cancel_request() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(ctrl('c'));
        assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
    }

    #[test]
    fn render_into_paints_status_and_buffer() {
        let buffer = shared_buffer();
        if let Ok(mut buf) = buffer.lock() {
            buf.push_user("hello");
        }
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        app.render_into(&mut terminal).unwrap();
        let buf = terminal.backend().buffer();
        let mut found_user = false;
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            // The bubble pads the text with surrounding spaces; the
            // exact prefix glyph is renderer-internal.
            if row.contains(" hello ") {
                found_user = true;
            }
        }
        assert!(found_user);
    }

    fn snapshot_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn pasted_text_lands_in_input_area_with_newline_preserved() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(key('i'));
        app.input.paste("first\nsecond");
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        app.render_into(&mut terminal).unwrap();
        let rows = snapshot_rows(&terminal);
        assert!(rows.iter().any(|r| r.contains("first")));
        assert!(rows.iter().any(|r| r.contains("second")));
    }

    #[test]
    fn scrolling_up_freezes_viewport_when_more_content_arrives() {
        let buffer = shared_buffer();
        if let Ok(mut buf) = buffer.lock() {
            for i in 0..20 {
                buf.push_user(format!("line{i}"));
            }
        }
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        // Stage C made k/j/G pane-aware: switch to buffer pane so
        // scrolling keys hit the buffer instead of moving the input
        // cursor.
        app.input.set_focused_pane(Pane::Buffer);
        // User scrolls up by 5 from the bottom.
        for _ in 0..5 {
            app.handle_key(key('k'));
        }
        let scroll_after_user = buffer.lock().unwrap().scroll();
        assert_eq!(scroll_after_user, 5);
        // Streaming delta arrives.
        if let Ok(mut buf) = buffer.lock() {
            buf.append_assistant_delta("new\nstreaming\ncontent");
        }
        // User's scroll position is preserved.
        assert_eq!(buffer.lock().unwrap().scroll(), 5);
        // Pressing G snaps back to bottom (auto-follow rearmed).
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        assert_eq!(buffer.lock().unwrap().scroll(), 0);
        assert!(buffer.lock().unwrap().is_following());
    }

    #[test]
    fn history_walk_replaces_input_text() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_history(vec!["older".into(), "newer".into()]);
        app.handle_key(key('i'));
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "newer");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "older");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "newer");
    }

    #[test]
    fn fold_all_then_unfold_all_toggles_folds() {
        let buffer = shared_buffer();
        if let Ok(mut buf) = buffer.lock() {
            buf.append_thinking_delta("step one");
            buf.finish_streaming();
        }
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        // zM folds all
        app.handle_key(key('z'));
        app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE));
        if let Ok(buf) = buffer.lock() {
            assert!(matches!(
                buf.blocks()[0],
                crate::buffer::Block::Thinking { folded: true, .. }
            ));
        }
        // zR opens all
        app.handle_key(key('z'));
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        if let Ok(buf) = buffer.lock() {
            assert!(matches!(
                buf.blocks()[0],
                crate::buffer::Block::Thinking { folded: false, .. }
            ));
        }
    }
}
