//! `:` ex-style command line shown on the status row.
//!
//! [`CommandLine`] is a small text field opened by [`crate::App`] when
//! the user presses `:` in [`crate::Mode::Normal`]. It owns its own text
//! and cursor and reports user actions via [`CommandLineEvent`]:
//! pending (still typing), cancelled (Esc / Ctrl+C), or submit (Enter).
//!
//! Rendering lives in [`crate::view`]; navigation and editing here. The
//! widget intentionally stays separate from [`crate::InputState`] so
//! the modal state machine doesn't have to grow another mode for what
//! is conceptually a transient overlay.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Outcome of [`CommandLine::handle_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandLineEvent {
    /// Keystroke was handled but no decision yet.
    Pending,
    /// User pressed Esc or Ctrl+C; close without running.
    Cancelled,
    /// User pressed Enter; carries the typed command (without the
    /// leading `:`). Empty input is reported as cancelled, not submit.
    Submit(String),
}

/// Single-line text field with a small set of keystrokes.
#[derive(Debug, Default)]
pub struct CommandLine {
    text: String,
    cursor: usize,
}

impl CommandLine {
    /// Construct an empty command line.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current command text (without the leading `:`).
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor inside [`Self::text`].
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Drive the widget by one key press.
    pub fn handle_key(&mut self, key: KeyEvent) -> CommandLineEvent {
        if key.kind != KeyEventKind::Press {
            return CommandLineEvent::Pending;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return CommandLineEvent::Cancelled;
        }
        match key.code {
            KeyCode::Esc => CommandLineEvent::Cancelled,
            KeyCode::Enter => {
                let trimmed = self.text.trim().to_owned();
                if trimmed.is_empty() {
                    CommandLineEvent::Cancelled
                } else {
                    CommandLineEvent::Submit(trimmed)
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                CommandLineEvent::Pending
            }
            KeyCode::Left => {
                self.move_cursor(-1);
                CommandLineEvent::Pending
            }
            KeyCode::Right => {
                self.move_cursor(1);
                CommandLineEvent::Pending
            }
            KeyCode::Home => {
                self.cursor = 0;
                CommandLineEvent::Pending
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                CommandLineEvent::Pending
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                CommandLineEvent::Pending
            }
            _ => CommandLineEvent::Pending,
        }
    }

    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    fn move_cursor(&mut self, delta: i32) {
        let target = i64::try_from(self.cursor).unwrap_or(0) + i64::from(delta);
        if target < 0 {
            self.cursor = 0;
        } else if let Ok(pos) = usize::try_from(target) {
            self.cursor = pos.min(self.text.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn typing_appends_to_text() {
        let mut cl = CommandLine::new();
        for c in "model".chars() {
            cl.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(cl.text(), "model");
        assert_eq!(cl.cursor(), 5);
    }

    #[test]
    fn enter_submits_trimmed_text() {
        let mut cl = CommandLine::new();
        for c in "  q  ".chars() {
            cl.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            cl.handle_key(key(KeyCode::Enter)),
            CommandLineEvent::Submit("q".into())
        );
    }

    #[test]
    fn enter_on_empty_text_cancels() {
        let mut cl = CommandLine::new();
        assert_eq!(
            cl.handle_key(key(KeyCode::Enter)),
            CommandLineEvent::Cancelled
        );
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        let mut cl = CommandLine::new();
        cl.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            cl.handle_key(key(KeyCode::Esc)),
            CommandLineEvent::Cancelled
        );
        let mut cl2 = CommandLine::new();
        cl2.handle_key(key(KeyCode::Char('x')));
        assert_eq!(cl2.handle_key(ctrl('c')), CommandLineEvent::Cancelled);
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            cl.handle_key(key(KeyCode::Char(c)));
        }
        cl.handle_key(key(KeyCode::Backspace));
        assert_eq!(cl.text(), "ab");
        assert_eq!(cl.cursor(), 2);
    }

    #[test]
    fn left_right_move_cursor() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            cl.handle_key(key(KeyCode::Char(c)));
        }
        cl.handle_key(key(KeyCode::Left));
        cl.handle_key(key(KeyCode::Left));
        assert_eq!(cl.cursor(), 1);
        cl.handle_key(key(KeyCode::Char('X')));
        assert_eq!(cl.text(), "aXbc");
    }

    #[test]
    fn home_and_end_jump() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            cl.handle_key(key(KeyCode::Char(c)));
        }
        cl.handle_key(key(KeyCode::Home));
        assert_eq!(cl.cursor(), 0);
        cl.handle_key(key(KeyCode::End));
        assert_eq!(cl.cursor(), 3);
    }
}
