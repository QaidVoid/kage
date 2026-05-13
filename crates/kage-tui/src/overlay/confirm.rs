//! Yes/no/cancel confirmation overlay.
//!
//! Bare-minimum primitive that PE.B's `kage.ui.confirm(title, message)`
//! will wrap. The overlay paints a centered modal with a title row, a
//! wrapped message body, and two highlighted options ("Yes" / "No").
//! Enter resolves with the current selection as a JSON boolean; Esc
//! and Ctrl+C close without resolving.
//!
//! Tab / Left / Right move between Yes and No. `y` and `n` are
//! shortcuts that resolve immediately.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// Which option the user is hovering. Defaults to [`Choice::No`] so a
/// stray Enter on a destructive prompt doesn't fire the dangerous path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    /// Confirm.
    Yes,
    /// Decline.
    No,
}

/// Modal yes/no overlay.
#[derive(Debug)]
pub struct ConfirmOverlay {
    title: String,
    message: String,
    selected: Choice,
}

impl ConfirmOverlay {
    /// Build a confirm dialog with the given title and message body.
    /// Default selection is [`Choice::No`].
    #[must_use]
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            selected: Choice::No,
        }
    }

    /// Override the default selection (e.g. for affirmative prompts
    /// where `Yes` is the safe answer).
    #[must_use]
    pub fn with_default(mut self, choice: Choice) -> Self {
        self.selected = choice;
        self
    }

    /// Current highlighted option. Exposed for tests.
    #[must_use]
    pub fn selected(&self) -> Choice {
        self.selected
    }

    fn toggle(&mut self) {
        self.selected = match self.selected {
            Choice::Yes => Choice::No,
            Choice::No => Choice::Yes,
        };
    }
}

impl OverlayWidget for ConfirmOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = available.width.clamp(30, 60);
        let height: u16 = 7;
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
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        Widget::render(
            Paragraph::new(self.message.clone())
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::White)),
            chunks[0],
            buf,
        );

        let yes_style = button_style(self.selected == Choice::Yes);
        let no_style = button_style(self.selected == Choice::No);
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled("  Yes  ", yes_style),
            Span::raw("   "),
            Span::styled("  No  ", no_style),
        ]);
        Widget::render(Paragraph::new(line), chunks[1], buf);
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        match key.code {
            KeyCode::Esc => OverlayAction::Close,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.toggle();
                OverlayAction::Stay
            }
            KeyCode::Char('y' | 'Y') => OverlayAction::Resolve(serde_json::Value::Bool(true)),
            KeyCode::Char('n' | 'N') => OverlayAction::Resolve(serde_json::Value::Bool(false)),
            KeyCode::Enter => {
                let value = matches!(self.selected, Choice::Yes);
                OverlayAction::Resolve(serde_json::Value::Bool(value))
            }
            _ => OverlayAction::Stay,
        }
    }
}

fn button_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn defaults_to_no() {
        let c = ConfirmOverlay::new("Delete?", "are you sure?");
        assert_eq!(c.selected(), Choice::No);
    }

    #[test]
    fn enter_resolves_with_current_selection() {
        let mut c = ConfirmOverlay::new("ok?", "msg");
        let action = c.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            OverlayAction::Resolve(serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn tab_toggles_selection() {
        let mut c = ConfirmOverlay::new("ok?", "msg");
        c.handle_key(key(KeyCode::Tab));
        assert_eq!(c.selected(), Choice::Yes);
        c.handle_key(key(KeyCode::Tab));
        assert_eq!(c.selected(), Choice::No);
    }

    #[test]
    fn y_shortcut_resolves_true() {
        let mut c = ConfirmOverlay::new("ok?", "msg");
        let action = c.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            action,
            OverlayAction::Resolve(serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn n_shortcut_resolves_false() {
        let mut c = ConfirmOverlay::new("ok?", "msg");
        let action = c.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            action,
            OverlayAction::Resolve(serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn esc_closes_without_resolving() {
        let mut c = ConfirmOverlay::new("ok?", "msg");
        assert_eq!(c.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
    }

    #[test]
    fn with_default_overrides_initial_selection() {
        let c = ConfirmOverlay::new("ok?", "msg").with_default(Choice::Yes);
        assert_eq!(c.selected(), Choice::Yes);
    }

    fn snapshot(c: &mut ConfirmOverlay, area: Rect) -> Vec<String> {
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::Theme::default();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        let modal = c.measure(area);
        c.render(modal, &mut buf, &ctx);
        let mut out = Vec::with_capacity(usize::from(area.height));
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn render_default_paints_title_message_and_buttons() {
        let mut c = ConfirmOverlay::new("Delete?", "are you sure?");
        let lines = snapshot(&mut c, Rect::new(0, 0, 80, 24));
        assert!(lines.iter().any(|l| l.contains("Delete?")), "title missing");
        assert!(
            lines.iter().any(|l| l.contains("are you sure?")),
            "message missing"
        );
        assert!(
            lines.iter().any(|l| l.contains("Yes") && l.contains("No")),
            "buttons missing"
        );
    }

    #[test]
    fn render_narrow_viewport_still_paints_box() {
        let mut c = ConfirmOverlay::new("Save?", "unsaved changes");
        let lines = snapshot(&mut c, Rect::new(0, 0, 32, 12));
        assert!(lines.iter().any(|l| l.contains("Save?")));
    }

    #[test]
    fn render_after_tab_highlights_yes_button() {
        let mut c = ConfirmOverlay::new("ok?", "msg");
        c.handle_key(key(KeyCode::Tab));
        assert_eq!(c.selected(), Choice::Yes);
        // After Tab, Yes is the highlighted option; render still
        // includes both labels.
        let lines = snapshot(&mut c, Rect::new(0, 0, 80, 24));
        assert!(lines.iter().any(|l| l.contains("Yes")));
        assert!(lines.iter().any(|l| l.contains("No")));
    }
}
