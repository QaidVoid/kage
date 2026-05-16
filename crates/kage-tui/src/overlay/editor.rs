//! Multi-line text editor overlay.
//!
//! Bare-minimum primitive that PE.B's `kage.ui.editor(title, prefill?)`
//! will wrap. The overlay paints a large bordered modal hosting a
//! multi-line text buffer. `Ctrl+S` resolves with the edited string as
//! a JSON string value; Esc and Ctrl+C close without resolving.
//!
//! Deliberately small: arrow keys, Backspace, Enter for newlines,
//! visible cursor cell. PE.B may layer kill-ring, undo, or bracketed
//! paste on top once it consumes this; the primitive itself stays
//! lean.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// Modal multi-line text editor.
#[derive(Debug)]
pub struct EditorOverlay {
    title: String,
    lines: Vec<String>,
    /// `(row, col)` cursor position. `col` is in characters, not bytes.
    cursor: (usize, usize),
}

impl EditorOverlay {
    /// Build an empty editor with the given title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }

    /// Prefill the editor with `text`. Newlines split into rows.
    #[must_use]
    pub fn with_prefill(mut self, text: impl AsRef<str>) -> Self {
        let text = text.as_ref();
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_owned).collect()
        };
        let last = self.lines.len().saturating_sub(1);
        let col = self.lines[last].chars().count();
        self.cursor = (last, col);
        self
    }

    /// Current text with `\n` joining lines.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn line(&self) -> &str {
        &self.lines[self.cursor.0]
    }

    fn line_mut(&mut self) -> &mut String {
        &mut self.lines[self.cursor.0]
    }

    fn insert_char(&mut self, c: char) {
        let col = self.cursor.1;
        let line = self.line_mut();
        let byte_idx: usize = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
        line.insert(byte_idx, c);
        self.cursor.1 += 1;
    }

    fn insert_newline(&mut self) {
        let (row, col) = self.cursor;
        let line = self.line_mut();
        let byte_idx: usize = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
        let tail = line.split_off(byte_idx);
        self.lines.insert(row + 1, tail);
        self.cursor = (row + 1, 0);
    }

    fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            let target = col - 1;
            let line = self.line_mut();
            let byte_idx: usize = line
                .char_indices()
                .nth(target)
                .map_or(line.len(), |(i, _)| i);
            let next: usize = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
            line.drain(byte_idx..next);
            self.cursor.1 = target;
        } else if row > 0 {
            let removed = self.lines.remove(row);
            let prev_len = self.lines[row - 1].chars().count();
            self.lines[row - 1].push_str(&removed);
            self.cursor = (row - 1, prev_len);
        }
    }

    fn move_left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.cursor.1 = col - 1;
        } else if row > 0 {
            let prev_len = self.lines[row - 1].chars().count();
            self.cursor = (row - 1, prev_len);
        }
    }

    fn move_right(&mut self) {
        let (row, col) = self.cursor;
        let line_len = self.line().chars().count();
        if col < line_len {
            self.cursor.1 = col + 1;
        } else if row + 1 < self.lines.len() {
            self.cursor = (row + 1, 0);
        }
    }

    fn move_up(&mut self) {
        if self.cursor.0 == 0 {
            return;
        }
        self.cursor.0 -= 1;
        let line_len = self.line().chars().count();
        self.cursor.1 = self.cursor.1.min(line_len);
    }

    fn move_down(&mut self) {
        if self.cursor.0 + 1 >= self.lines.len() {
            return;
        }
        self.cursor.0 += 1;
        let line_len = self.line().chars().count();
        self.cursor.1 = self.cursor.1.min(line_len);
    }
}

impl OverlayWidget for EditorOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = available.width.clamp(40, 100);
        let height = available.height.clamp(10, 20);
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        Widget::render(crate::opaque::OpaqueClear, area, buf);
        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Blue));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let body_area = chunks[0];
        let lines: Vec<Line<'static>> = self
            .lines
            .iter()
            .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(Color::White))))
            .collect();
        Widget::render(Paragraph::new(lines), body_area, buf);

        self.paint_cursor(body_area, buf);

        Widget::render(
            Paragraph::new(Line::from(Span::styled(
                "ctrl-s save  esc cancel",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))),
            chunks[1],
            buf,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return OverlayAction::Resolve(serde_json::Value::String(self.text()));
        }
        match key.code {
            KeyCode::Esc => OverlayAction::Close,
            KeyCode::Char(c) => {
                self.insert_char(c);
                OverlayAction::Stay
            }
            KeyCode::Enter => {
                self.insert_newline();
                OverlayAction::Stay
            }
            KeyCode::Backspace => {
                self.backspace();
                OverlayAction::Stay
            }
            KeyCode::Left => {
                self.move_left();
                OverlayAction::Stay
            }
            KeyCode::Right => {
                self.move_right();
                OverlayAction::Stay
            }
            KeyCode::Up => {
                self.move_up();
                OverlayAction::Stay
            }
            KeyCode::Down => {
                self.move_down();
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.cursor.1 = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.cursor.1 = self.line().chars().count();
                OverlayAction::Stay
            }
            _ => OverlayAction::Stay,
        }
    }
}

impl EditorOverlay {
    fn paint_cursor(&self, body_area: Rect, buf: &mut Buffer) {
        let (row, col) = self.cursor;
        if row >= usize::from(body_area.height) {
            return;
        }
        let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
        let col_u16 = u16::try_from(col).unwrap_or(u16::MAX);
        let x = body_area.x.saturating_add(col_u16);
        let y = body_area.y.saturating_add(row_u16);
        if x >= body_area.right() || y >= body_area.bottom() {
            return;
        }
        let cell = &mut buf[(x, y)];
        cell.set_style(
            Style::default()
                .bg(Color::White)
                .fg(Color::Black)
                .add_modifier(Modifier::REVERSED),
        );
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

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn typing_appends_to_first_line() {
        let mut e = EditorOverlay::new("Edit");
        for c in "abc".chars() {
            e.handle_key(ch(c));
        }
        assert_eq!(e.text(), "abc");
    }

    #[test]
    fn enter_inserts_newline() {
        let mut e = EditorOverlay::new("Edit");
        for c in "ab".chars() {
            e.handle_key(ch(c));
        }
        e.handle_key(key(KeyCode::Enter));
        for c in "cd".chars() {
            e.handle_key(ch(c));
        }
        assert_eq!(e.text(), "ab\ncd");
    }

    #[test]
    fn ctrl_s_resolves_with_text() {
        let mut e = EditorOverlay::new("Edit").with_prefill("hello");
        let action = e.handle_key(ctrl('s'));
        assert_eq!(
            action,
            OverlayAction::Resolve(serde_json::Value::String("hello".into()))
        );
    }

    #[test]
    fn esc_closes() {
        let mut e = EditorOverlay::new("Edit");
        assert_eq!(e.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
    }

    #[test]
    fn backspace_joins_with_previous_line() {
        let mut e = EditorOverlay::new("Edit").with_prefill("a\nb");
        // Cursor is at end of "b". Backspace x2 -> "a"
        e.handle_key(key(KeyCode::Backspace));
        e.handle_key(key(KeyCode::Backspace));
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn arrows_move_cursor() {
        let mut e = EditorOverlay::new("Edit").with_prefill("ab\ncd");
        e.handle_key(key(KeyCode::Up));
        // Cursor at (0, 2). Type X -> "abX\ncd"
        e.handle_key(ch('X'));
        assert_eq!(e.text(), "abX\ncd");
    }

    #[test]
    fn with_prefill_initial_text() {
        let e = EditorOverlay::new("Edit").with_prefill("preset");
        assert_eq!(e.text(), "preset");
    }

    fn snapshot(e: &mut EditorOverlay, area: Rect) -> Vec<String> {
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::Theme::default();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        let modal = e.measure(area);
        e.render(modal, &mut buf, &ctx);
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
    fn render_default_paints_title_and_help() {
        let mut e = EditorOverlay::new("Compose");
        let lines = snapshot(&mut e, Rect::new(0, 0, 80, 24));
        assert!(lines.iter().any(|l| l.contains("Compose")));
        assert!(lines.iter().any(|l| l.contains("ctrl-s save")));
    }

    #[test]
    fn render_with_prefill_paints_lines() {
        let mut e = EditorOverlay::new("Compose").with_prefill("first\nsecond");
        let lines = snapshot(&mut e, Rect::new(0, 0, 80, 24));
        assert!(lines.iter().any(|l| l.contains("first")));
        assert!(lines.iter().any(|l| l.contains("second")));
    }

    #[test]
    fn render_narrow_viewport_still_paints_box() {
        let mut e = EditorOverlay::new("Compose");
        let lines = snapshot(&mut e, Rect::new(0, 0, 42, 12));
        assert!(lines.iter().any(|l| l.contains("Compose")));
    }
}
