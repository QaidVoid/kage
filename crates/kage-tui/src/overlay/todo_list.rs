//! Scrollable todo-list overlay (`/todo_list`).
//!
//! [`TodoListOverlay`] shows every task of the session's todo list,
//! one row per task with the same status glyphs the pinned box uses.
//! The pinned box caps at a few rows; this is the whole plan. It is a
//! reading surface, not a command surface: every key either scrolls
//! or closes.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::theme::Theme;
use crate::view::UnicodeWidthStr as _;
use crate::view::todo::{TodoRow, TodoStrip};

/// Scroll hint painted over the bottom border while rows are hidden.
const MORE_HINT: &str = " more: up/down ";

/// Narrowest the box gets, so a list of tiny titles still reads.
const MIN_WIDTH: u16 = 24;

/// One todo row as the overlay paints it: the status glyph and its
/// tier.
#[derive(Debug)]
struct TaskRow {
    glyph: &'static str,
    glyph_fg: fn(&Theme) -> ratatui::style::Color,
    title: String,
    done: bool,
}

/// The `/todo_list` modal: every task of the session's plan.
#[derive(Debug)]
pub struct TodoListOverlay {
    title: String,
    rows: Vec<TaskRow>,
    done: usize,
    total: usize,
    /// First visible row index.
    scroll: usize,
    /// Visual rows from the last paint, after wrapping.
    line_count: usize,
    /// Inner height from the last paint, used to clamp scrolling
    /// before the next frame and to size page jumps.
    viewport_rows: u16,
}

impl TodoListOverlay {
    /// Build the viewer from `strip`, the transcript's latest write.
    #[must_use]
    pub fn new(strip: &TodoStrip) -> Self {
        let mut overlay = Self {
            title: " todos ".to_owned(),
            rows: Vec::new(),
            done: 0,
            total: 0,
            scroll: 0,
            line_count: 0,
            viewport_rows: 1,
        };
        overlay.set_strip(strip);
        overlay
    }

    /// Swap in `strip`, the latest write, keeping the scroll position.
    pub(crate) fn set_strip(&mut self, strip: &TodoStrip) {
        self.rows = strip
            .items
            .iter()
            .map(|row: &TodoRow| match row.status.as_str() {
                "done" => TaskRow {
                    glyph: "\u{2713}",
                    glyph_fg: |theme| theme.success_fg,
                    title: row.title.clone(),
                    done: true,
                },
                "in_progress" => TaskRow {
                    glyph: "\u{25cf}",
                    glyph_fg: |theme| theme.tool_pending_rule,
                    title: row.title.clone(),
                    done: false,
                },
                _ => TaskRow {
                    glyph: "\u{25cb}",
                    glyph_fg: |theme| theme.muted_fg,
                    title: row.title.clone(),
                    done: false,
                },
            })
            .collect();
        self.done = strip.done;
        self.total = strip.items.len();
        self.line_count = self.rows.len();
        self.clamp_scroll();
    }

    /// Clamp the scroll offset so the last row stays visible.
    fn clamp_scroll(&mut self) {
        let visible = usize::from(self.viewport_rows.max(1));
        let max = self.line_count.saturating_sub(visible);
        self.scroll = self.scroll.min(max);
    }

    /// Move the scroll by `delta` rows in either direction.
    fn scroll_by(&mut self, delta: isize) {
        let current = isize::try_from(self.scroll).unwrap_or(0);
        let moved = current + delta;
        self.scroll = usize::try_from(moved.max(0)).unwrap_or(0);
        self.clamp_scroll();
    }

    /// The tasks as visual rows `width` cells wide, titles clipped to
    /// the row.
    fn lines(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let body = Style::default().fg(theme.overlay_fg);
        let muted = Style::default().fg(theme.muted_fg);
        let width = usize::from(width.max(1));
        self.rows
            .iter()
            .map(|row| {
                let mut title_style = if row.done { muted } else { body };
                if row.done {
                    title_style = title_style.add_modifier(Modifier::CROSSED_OUT);
                }
                let glyph_style = Style::default()
                    .fg((row.glyph_fg)(theme))
                    .add_modifier(Modifier::BOLD);
                let title =
                    crate::view::truncate_to_width(&row.title, width.saturating_sub(4), "...");
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(row.glyph, glyph_style),
                    Span::raw(" "),
                    Span::styled(title, title_style),
                ])
            })
            .collect()
    }

    /// Widest a task row paints, borders excluded.
    fn content_width(&self) -> usize {
        self.rows
            .iter()
            .map(|row| row.title.width())
            .max()
            .unwrap_or(0)
            .saturating_add(4)
    }
}

impl OverlayWidget for TodoListOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = usize::from(available.width.saturating_sub(2))
            .min(self.content_width().max(usize::from(MIN_WIDTH)));
        let width = u16::try_from(width).unwrap_or(u16::MAX);
        let rows = self
            .lines(width.saturating_sub(2), &crate::theme::current())
            .len();
        let wanted = u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2);
        let height = wanted.min(available.height.saturating_sub(2));
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        Widget::render(crate::opaque::OpaqueClear, area, buf);
        let theme = crate::theme::current();
        let muted = Style::default().fg(theme.muted_fg);
        let summary = format!(" {} of {} done ", self.done, self.total);
        let block = Block::default()
            .title(Line::from(Span::styled(
                self.title.clone(),
                Style::default()
                    .fg(theme.overlay_fg)
                    .add_modifier(Modifier::BOLD),
            )))
            .title(Line::from(Span::styled(summary, muted)).right_aligned())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.overlay_border));
        let inner = block.inner(area);
        Widget::render(block, area, buf);
        let mut lines = self.lines(inner.width, &theme);
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("  the todo list is empty", muted)));
        }
        self.viewport_rows = inner.height;
        self.line_count = lines.len();
        self.clamp_scroll();
        let body =
            Paragraph::new(lines).scroll((u16::try_from(self.scroll).unwrap_or(u16::MAX), 0));
        Widget::render(body, inner, buf);

        // Scroll hint in the bottom-right corner of the box, over the
        // border row, so the affordance is visible without a footer.
        let more_below = self.scroll + usize::from(inner.height) < self.line_count;
        if more_below && area.width > 12 {
            let hint_width = u16::try_from(MORE_HINT.width()).unwrap_or(u16::MAX);
            let x = area.x + area.width.saturating_sub(hint_width + 1);
            let y = area.y + area.height.saturating_sub(1);
            let hint_area = Rect::new(x, y, hint_width, 1);
            Widget::render(
                Paragraph::new(Line::from(Span::styled(MORE_HINT, muted))),
                hint_area,
                buf,
            );
        }
    }

    fn footer_hint(&self) -> &'static str {
        "up/down to scroll \u{b7} esc to close"
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        let page = isize::from(i16::try_from(self.viewport_rows.max(1)).unwrap_or(i16::MAX)) - 1;
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'Q') => OverlayAction::Close,
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_by(1);
                OverlayAction::Stay
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_by(-1);
                OverlayAction::Stay
            }
            KeyCode::PageDown => {
                self.scroll_by(page);
                OverlayAction::Stay
            }
            KeyCode::PageUp => {
                self.scroll_by(-page);
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.scroll = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.scroll = usize::MAX;
                self.clamp_scroll();
                OverlayAction::Stay
            }
            _ => OverlayAction::PropagateKey,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    fn strip(todos: &serde_json::Value) -> TodoStrip {
        let blocks = [crate::buffer::Block::ToolCall {
            call_id: "c".to_owned(),
            name: "todo_list".to_owned(),
            input_summary: String::new(),
            input_pretty: String::new(),
            input: std::sync::Arc::new(serde_json::json!({ "todos": todos })),
            folded: false,
            phase: crate::view::tool_view::ToolPhase::Done,
            progress: String::new(),
            started_at: std::time::Instant::now(),
            diff: None,
        }];
        crate::view::todo::from_blocks(&blocks)
    }

    #[test]
    fn rows_carry_one_entry_per_task() {
        let strip = strip(&serde_json::json!([
            { "title": "a", "status": "done" },
            { "title": "b", "status": "in_progress" },
            { "title": "c", "status": "pending" },
        ]));
        let overlay = TodoListOverlay::new(&strip);
        assert_eq!(overlay.rows.len(), 3);
        assert_eq!(overlay.done, 1);
        assert_eq!(overlay.total, 3);
        assert!(overlay.rows[0].done);
        assert!(!overlay.rows[1].done);
    }

    #[test]
    fn esc_closes_and_arrows_scroll() {
        let strip = strip(&serde_json::json!([
            { "title": "a", "status": "pending" },
            { "title": "b", "status": "pending" },
        ]));
        let mut overlay = TodoListOverlay::new(&strip);
        assert_eq!(
            overlay.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            OverlayAction::Close
        );
        overlay.viewport_rows = 1;
        assert_eq!(
            overlay.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            OverlayAction::Stay
        );
        assert_eq!(overlay.scroll, 1);
    }
}
