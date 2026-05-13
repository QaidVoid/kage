//! Single-line text input overlay.
//!
//! Bare-minimum primitive that PE.B's `kage.ui.input(title, placeholder?)`
//! will wrap. The overlay paints a centered modal with a title row and
//! a single-line editor. Enter resolves with the typed string as a
//! JSON string value; Esc and Ctrl+C close without resolving.
//!
//! Internally wraps a [`crate::cmdline::CommandLine`] with an empty
//! command registry so the same Backspace / Left / Right / Home / End
//! key map applies. No completion popup ever opens because there are
//! no commands to match.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};

use crate::cmdline::{CommandLine, CommandLineEvent};
use crate::cmdparse::EmptyResolver;
use crate::command::CommandSpec;
use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// Modal single-line input overlay.
#[derive(Debug)]
pub struct InputOverlay {
    title: String,
    placeholder: Option<String>,
    cmdline: CommandLine,
}

impl InputOverlay {
    /// Build an empty input dialog with the given title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            placeholder: None,
            cmdline: CommandLine::new(),
        }
    }

    /// Attach a placeholder shown in dimmed text while the input is
    /// empty. The placeholder is not part of the resolved value.
    #[must_use]
    pub fn with_placeholder(mut self, hint: impl Into<String>) -> Self {
        self.placeholder = Some(hint.into());
        self
    }

    /// Current typed text. Exposed for tests.
    #[must_use]
    pub fn text(&self) -> &str {
        self.cmdline.text()
    }
}

impl OverlayWidget for InputOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = available.width.clamp(30, 60);
        let height: u16 = 5;
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        Widget::render(Clear, area, buf);
        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Blue));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);

        let line = if self.cmdline.text().is_empty() {
            if let Some(hint) = self.placeholder.as_deref() {
                Line::from(Span::styled(
                    hint.to_owned(),
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                Line::from("")
            }
        } else {
            Line::from(Span::styled(
                self.cmdline.text().to_owned(),
                Style::default().fg(Color::White),
            ))
        };
        Widget::render(Paragraph::new(line), chunks[0], buf);

        Widget::render(
            Paragraph::new(Line::from(Span::styled(
                "enter confirm  esc cancel",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))),
            chunks[1],
            buf,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        let empty: [&CommandSpec; 0] = [];
        match self.cmdline.handle_key(key, &empty, &EmptyResolver) {
            CommandLineEvent::Pending => OverlayAction::Stay,
            CommandLineEvent::Cancelled => OverlayAction::Close,
            CommandLineEvent::Submit(text) => {
                OverlayAction::Resolve(serde_json::Value::String(text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn typing_appends_to_text() {
        let mut i = InputOverlay::new("Name");
        for c in "abc".chars() {
            i.handle_key(ch(c));
        }
        assert_eq!(i.text(), "abc");
    }

    #[test]
    fn enter_resolves_with_typed_text() {
        let mut i = InputOverlay::new("Name");
        for c in "hi".chars() {
            i.handle_key(ch(c));
        }
        let action = i.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            OverlayAction::Resolve(serde_json::Value::String("hi".into()))
        );
    }

    #[test]
    fn esc_closes_without_resolving() {
        let mut i = InputOverlay::new("Name");
        assert_eq!(i.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
    }

    #[test]
    fn backspace_deletes_previous_char() {
        let mut i = InputOverlay::new("Name");
        for c in "ab".chars() {
            i.handle_key(ch(c));
        }
        i.handle_key(key(KeyCode::Backspace));
        assert_eq!(i.text(), "a");
    }

    #[test]
    fn placeholder_is_optional() {
        let i = InputOverlay::new("Name").with_placeholder("type something");
        assert_eq!(i.text(), "");
    }
}
