//! In-TUI picker overlay.
//!
//! Unlike the standalone [`crate::picker::pick`] (which owns the
//! terminal in raw mode for one-shot prompts like `kage auth login`),
//! [`OverlayPicker`] runs *inside* the App's render loop: it draws
//! into a centered modal `Rect` over the conversation buffer using
//! ratatui widgets, and consumes key events through the same path
//! the rest of the App uses.
//!
//! The data model (search, selection, paginated window) is identical
//! to the standalone picker - both share [`crate::picker::PickItem`],
//! [`crate::picker::filter`], and [`crate::picker::compute_window`].

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::picker::{PickItem, compute_window, filter};

/// Outcome of [`OverlayPicker::handle_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerEvent {
    /// Key consumed; no decision yet.
    Pending,
    /// User pressed Enter on a row; carries the row's value.
    Picked(String),
    /// User pressed Esc / Ctrl+C without selecting.
    Cancelled,
}

/// Stateful picker rendered as a modal overlay.
#[derive(Debug)]
pub struct OverlayPicker {
    title: String,
    items: Vec<PickItem>,
    search: String,
    selected: usize,
    scroll_offset: usize,
}

impl OverlayPicker {
    /// Construct a picker with the given header and rows. Items are
    /// sorted alphabetically by label to match the standalone picker.
    #[must_use]
    pub fn new(title: impl Into<String>, mut items: Vec<PickItem>) -> Self {
        items.sort_by(|a, b| a.label.cmp(&b.label));
        Self {
            title: title.into(),
            items,
            search: String::new(),
            selected: 0,
            scroll_offset: 0,
        }
    }

    /// Drive the picker forward by one key event. Returns whether the
    /// key produced a final decision.
    pub fn handle_key(&mut self, key: KeyEvent) -> PickerEvent {
        if key.kind != KeyEventKind::Press {
            return PickerEvent::Pending;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return PickerEvent::Cancelled;
        }
        let filtered = filter(&self.items, &self.search);
        match key.code {
            KeyCode::Esc => PickerEvent::Cancelled,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickerEvent::Pending
            }
            KeyCode::Down if self.selected + 1 < filtered.len() => {
                self.selected += 1;
                PickerEvent::Pending
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(8);
                PickerEvent::Pending
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 8).min(filtered.len().saturating_sub(1));
                PickerEvent::Pending
            }
            KeyCode::Home => {
                self.selected = 0;
                PickerEvent::Pending
            }
            KeyCode::End => {
                self.selected = filtered.len().saturating_sub(1);
                PickerEvent::Pending
            }
            KeyCode::Enter => match filtered.get(self.selected) {
                Some(&idx) => PickerEvent::Picked(self.items[idx].value.clone()),
                None => PickerEvent::Pending,
            },
            KeyCode::Backspace => {
                self.search.pop();
                self.selected = 0;
                self.scroll_offset = 0;
                PickerEvent::Pending
            }
            KeyCode::Char(c) => {
                self.search.push(c);
                self.selected = 0;
                self.scroll_offset = 0;
                PickerEvent::Pending
            }
            _ => PickerEvent::Pending,
        }
    }

    /// Render the picker as a centered modal over `area`. The caller
    /// is expected to draw the rest of the frame first; [`Clear`]
    /// blanks the modal region before the picker paints.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let modal = center(area, 70, 80);
        frame.render_widget(Clear, modal);

        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Blue));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        // search (1) + list (rest) + help (1)
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_search(frame, chunks[0]);
        self.render_list(frame, chunks[1]);
        Self::render_help(frame, chunks[2]);
    }

    fn render_search(&self, frame: &mut Frame, area: Rect) {
        let search_text = if self.search.is_empty() {
            Line::from(vec![
                Span::styled("/ ", Style::default().fg(Color::DarkGray)),
                Span::styled("type to filter", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            Line::from(vec![
                Span::styled("/ ", Style::default().fg(Color::DarkGray)),
                Span::styled(self.search.clone(), Style::default().fg(Color::White)),
            ])
        };
        frame.render_widget(Paragraph::new(search_text), area);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let filtered = filter(&self.items, &self.search);
        let max_visible = usize::from(area.height);
        let (offset, window) = compute_window(
            self.scroll_offset,
            self.selected,
            filtered.len(),
            max_visible,
        );
        self.scroll_offset = offset;

        if filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "(no matches)",
                    Style::default().fg(Color::DarkGray),
                ))),
                area,
            );
            return;
        }

        let total = filtered.len();
        let end = (offset + window).min(total);
        let above = offset;
        let below = total.saturating_sub(end);

        let mut items: Vec<ListItem<'static>> = Vec::with_capacity(window + 2);
        if above > 0 {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("... {above} more above"),
                Style::default().fg(Color::DarkGray),
            ))));
        }
        for (vi, &idx) in filtered.iter().enumerate().take(end).skip(offset) {
            let item = &self.items[idx];
            let is_sel = vi == self.selected;
            let badge = item.badge.unwrap_or(' ');
            let badge_color = if item.badge == Some('*') {
                Color::Green
            } else {
                Color::DarkGray
            };
            let label_style = if is_sel {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!(" {badge} "), Style::default().fg(badge_color)),
                Span::styled(item.label.clone(), label_style),
            ])));
        }
        if below > 0 {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("... {below} more below"),
                Style::default().fg(Color::DarkGray),
            ))));
        }

        // Highlight the selected row by translating its absolute index
        // into a visible-row index that includes the optional "above"
        // indicator row.
        let visible_index = self.selected.saturating_sub(offset) + usize::from(above > 0);
        let mut state = ListState::default();
        state.select(Some(visible_index));
        frame.render_stateful_widget(
            List::new(items).highlight_style(Style::default().add_modifier(Modifier::BOLD)),
            area,
            &mut state,
        );
    }

    fn render_help(frame: &mut Frame, area: Rect) {
        let line = Line::from(Span::styled(
            "up/down select  enter confirm  type to filter  esc cancel",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }
}

fn center(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let h = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area)[1];
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(h)[1]
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyEvent;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn pick(items: &[&str]) -> OverlayPicker {
        OverlayPicker::new("Pick", items.iter().map(|s| PickItem::simple(*s)).collect())
    }

    #[test]
    fn enter_picks_the_selected_value() {
        let mut p = pick(&["a", "b", "c"]);
        // Sorted by label, selected starts at 0 = "a".
        let ev = p.handle_key(key(KeyCode::Enter));
        assert_eq!(ev, PickerEvent::Picked("a".into()));
    }

    #[test]
    fn down_arrow_moves_selection_then_enter_picks() {
        let mut p = pick(&["a", "b", "c"]);
        assert_eq!(p.handle_key(key(KeyCode::Down)), PickerEvent::Pending);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            PickerEvent::Picked("b".into())
        );
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        let mut p = pick(&["a"]);
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PickerEvent::Cancelled);
        let mut p2 = pick(&["a"]);
        assert_eq!(p2.handle_key(ctrl('c')), PickerEvent::Cancelled);
    }

    #[test]
    fn typing_filters_the_visible_set() {
        let mut p = pick(&["anthropic", "openai", "gemini"]);
        // "open" - only "openai" contains it.
        for c in "open".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        let ev = p.handle_key(key(KeyCode::Enter));
        assert_eq!(ev, PickerEvent::Picked("openai".into()));
    }

    #[test]
    fn selection_clamps_when_filter_shrinks_results() {
        let mut p = pick(&["alpha", "beta", "gamma"]);
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        // Now selected = 2 = "gamma". Type "alp" so only one row remains.
        for c in "alp".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        // Enter should still pick "alpha" (selection reset on each
        // search edit).
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            PickerEvent::Picked("alpha".into())
        );
    }

    #[test]
    fn picker_with_no_matches_returns_pending_on_enter() {
        let mut p = pick(&["a"]);
        for c in "zzz".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PickerEvent::Pending);
    }
}
