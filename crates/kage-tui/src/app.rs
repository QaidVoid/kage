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
use crate::input::{InputAction, InputState, Mode};
use crate::layout::{input_height_for, split};
use crate::overlay::{OverlayPicker, PickerEvent};
use crate::picker::PickItem;
use crate::terminal::Tui;
use crate::view;

/// One frame target; ratatui handles diffing so a higher rate is fine.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Lines scrolled per mouse wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;

/// Number of rows the input text occupies, counting a trailing newline
/// as a fresh empty row. `String::lines()` would drop that row, which
/// would leave the input area undersized after `Shift+Enter` until the
/// user types a visible character.
fn text_row_count(text: &str) -> usize {
    text.split('\n').count()
}

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
        }
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
        loop {
            self.draw(tui)?;
            let deadline = Instant::now() + FRAME_INTERVAL;
            while Instant::now() < deadline {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default();
                if event::poll(remaining)? {
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
                            _ => {}
                        },
                        Event::Resize(_, _) => {
                            // Re-render on the next iteration.
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn draw(&mut self, tui: &mut Tui) -> Result<(), TuiError> {
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let input_text_lines = u16::try_from(text_row_count(self.input.text())).unwrap_or(u16::MAX);
        let input_height = input_height_for(input_text_lines + 1);
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
        };
        tui.terminal().draw(|frame| {
            let regions = split(frame.area(), input_height);
            view::render(frame, regions, &mut buffer, &self.input, cmdline, &status);
            if let Some(picker) = self.picker.as_mut() {
                picker.render(frame, frame.area());
            }
        })?;
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

    /// Copy the current visual selection to the system clipboard via
    /// OSC52. The escape sequence is emitted directly to stdout (we
    /// own the alt screen). A `kage:notify` block confirms the yank
    /// so the user knows the gesture worked even when the terminal's
    /// clipboard support is silent.
    fn yank_visual_selection(&mut self) {
        let text = match self.buffer.lock() {
            Ok(buf) => buf
                .visual_range()
                .map(|(lo, hi)| buf.selection_text(lo, hi))
                .unwrap_or_default(),
            Err(_) => return,
        };
        if text.is_empty() {
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_visual_anchor(None);
            buf.push_custom(
                "kage:notify",
                format!("yanked {} chars to clipboard", text.chars().count()),
                false,
            );
        }
    }

    fn push_help(&mut self) {
        let body = "available commands:\n  \
                    :q, :quit       leave the TUI\n  \
                    :cancel         cancel the in-flight turn\n  \
                    :model <id>     switch to provider:model (e.g. anthropic:claude-sonnet-4-5)\n  \
                    :fold all       fold every foldable block\n  \
                    :unfold all     unfold every foldable block\n  \
                    :help           show this help";
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:help", body, false);
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
            InputAction::EnterMode(mode) => match mode {
                Mode::Visual => {
                    if let Ok(mut buf) = self.buffer.lock() {
                        let anchor = buf.effective_focus();
                        buf.set_visual_anchor(anchor);
                    }
                }
                Mode::Normal | Mode::Insert => {
                    if let Ok(mut buf) = self.buffer.lock() {
                        buf.set_visual_anchor(None);
                    }
                }
            },
            InputAction::Yank => {
                self.yank_visual_selection();
            }
            InputAction::BeginSearch => {
                self.search_line = Some(CommandLine::new());
            }
            InputAction::SearchNext => self.jump_to_search_match(true),
            InputAction::SearchPrev => self.jump_to_search_match(false),
        }
        None
    }

    fn send_request(&mut self, req: RunRequest) -> Result<(), TrySendError<RunRequest>> {
        match self.requests.send(req) {
            Ok(()) => Ok(()),
            Err(err) => Err(TrySendError::Disconnected(err.0)),
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
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let input_text_lines = u16::try_from(text_row_count(self.input.text())).unwrap_or(u16::MAX);
        let input_height = input_height_for(input_text_lines + 1);
        let picker = self.picker.as_mut();
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
        };
        terminal
            .draw(|frame| {
                let regions = split(frame.area(), input_height);
                view::render(frame, regions, &mut buffer, &self.input, cmdline, &status);
                if let Some(picker) = picker {
                    picker.render(frame, frame.area());
                }
            })
            .map_err(|err| TuiError::Io(std::io::Error::other(err.to_string())))?;
        Ok(())
    }
}

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
