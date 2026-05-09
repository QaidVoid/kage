//! Interactive TUI event loop.
//!
//! [`App::run`] owns the [`Tui`] and a [`SharedBuffer`], polls crossterm
//! key events, drives [`InputState`], applies [`InputAction`]s to the
//! buffer, and redraws the screen ~30 times a second. Submitting a
//! prompt fires a `RunRequest` through the provided sink; the host is
//! responsible for spawning the agent loop on a worker thread and
//! pushing its events into the same `SharedBuffer` via [`TuiHooks`].

use std::sync::mpsc::{Sender, TrySendError};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};

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

/// Request the host should act on. Either the user submitted a prompt
/// (the host runs the agent loop in a worker thread), the user asked
/// to cancel the in-flight turn, or the user picked a different model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunRequest {
    /// New user prompt to submit to the agent loop.
    Submit(String),
    /// Trip the agent loop's cancellation flag.
    Cancel,
    /// Switch to a different `provider:model` for subsequent turns.
    SwitchModel(String),
}

/// Outcome of [`App::run`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppExit {
    /// User pressed `Ctrl+Q` / `:q` to leave the TUI cleanly.
    Quit,
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
        }
    }

    /// Replace the model list shown when the user opens the in-TUI
    /// picker. The host computes this from its provider registry +
    /// catalog.
    pub fn set_model_choices(&mut self, choices: Vec<PickItem>) {
        self.model_choices = choices;
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
                            if let Some(exit) = self.dispatch_key(key) {
                                return Ok(exit);
                            }
                        }
                        Event::Paste(text) => self.input.paste(&text),
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
        let buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let input_text_lines =
            u16::try_from(self.input.text().lines().count().max(1)).unwrap_or(u16::MAX);
        let input_height = input_height_for(input_text_lines + 1);
        tui.terminal().draw(|frame| {
            let regions = split(frame.area(), input_height);
            view::render(frame, regions, &buffer, &self.input);
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

        let actions = self.input.handle_key(key);
        for action in actions {
            if let Some(exit) = self.apply(action) {
                return Some(exit);
            }
        }
        None
    }

    fn dispatch_picker_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        let picker = self.picker.as_mut()?;
        match picker.handle_key(key) {
            PickerEvent::Pending => {}
            PickerEvent::Cancelled => {
                self.picker = None;
            }
            PickerEvent::Picked(value) => {
                self.picker = None;
                let _ = self.send_request(RunRequest::SwitchModel(value));
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
            InputAction::ScrollToTop => self.set_scroll(0),
            InputAction::ScrollToBottom => self.scroll_to_bottom(),
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
                }
            }
            InputAction::EnterMode(_)
            | InputAction::BeginCommand
            | InputAction::BeginSearch
            | InputAction::Yank => {}
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
            let current = i64::try_from(buf.scroll()).unwrap_or(i64::MAX);
            let target = current + i64::from(delta);
            let clamped = usize::try_from(target.max(0)).unwrap_or(0);
            buf.set_scroll(clamped);
        }
    }

    fn set_scroll(&mut self, scroll: usize) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_scroll(scroll);
        }
    }

    fn scroll_to_bottom(&mut self) {
        if let Ok(mut buf) = self.buffer.lock() {
            let total = buf.total_lines();
            buf.set_scroll(total);
        }
    }

    fn toggle_last_fold(&mut self) {
        if let Ok(mut buf) = self.buffer.lock() {
            let last_foldable = buf
                .blocks()
                .iter()
                .enumerate()
                .rfind(|(_, b)| b.is_foldable())
                .map(|(i, _)| i);
            if let Some(idx) = last_foldable {
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
        let buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let input_text_lines =
            u16::try_from(self.input.text().lines().count().max(1)).unwrap_or(u16::MAX);
        let input_height = input_height_for(input_text_lines + 1);
        let picker = self.picker.as_mut();
        terminal
            .draw(|frame| {
                let regions = split(frame.area(), input_height);
                view::render(frame, regions, &buffer, &self.input);
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
            if row.contains("> hello") {
                found_user = true;
            }
        }
        assert!(found_user);
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
