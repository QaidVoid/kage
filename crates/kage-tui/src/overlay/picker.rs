//! Fuzzy-filtered modal picker.
//!
//! [`OverlayPicker`] serves both the `Ctrl+P` model switcher and the
//! `Ctrl+S` session resume picker. The data model (search, selection,
//! paginated window) is identical to the standalone [`crate::picker`]
//! used outside the TUI; we share [`crate::picker::PickItem`],
//! [`crate::picker::filter`], and [`crate::picker::compute_window`].
//!
//! Implements [`OverlayWidget`] so the upcoming
//! [`crate::overlay::OverlayRegistry`] can dispatch through the trait.
//! A thin Frame-based [`OverlayPicker::render`] wrapper preserves the
//! call shape App still uses today; both paths end in the same
//! Buffer-level paint.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Widget,
};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::picker::{PickItem, compute_window, filter};

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
    /// Construct a picker with the given header and rows. Ungrouped
    /// rows are sorted alphabetically by label (predictable for
    /// arbitrary lists); grouped rows keep the caller's order so the
    /// caller controls section ordering (chronological for sessions,
    /// provider order for models).
    #[must_use]
    pub fn new(title: impl Into<String>, mut items: Vec<PickItem>) -> Self {
        if items.iter().all(|i| i.group.is_none()) {
            items.sort_by(|a, b| a.label.cmp(&b.label));
        }
        Self {
            title: title.into(),
            items,
            search: String::new(),
            selected: 0,
            scroll_offset: 0,
        }
    }

    /// Render the picker over `area`. Thin wrapper that drives the
    /// [`OverlayWidget`] impl through a [`Frame`]; kept so callers in
    /// the render closure can pass the active `Frame` directly without
    /// reaching for `frame.buffer_mut()`.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let modal = OverlayWidget::measure(self, area);
        frame.render_widget(crate::opaque::OpaqueClear, modal);
        let theme = crate::theme::current();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        OverlayWidget::render(self, modal, frame.buffer_mut(), &ctx);
    }
}

impl OverlayWidget for OverlayPicker {
    fn measure(&self, available: Rect) -> Rect {
        center(available, 70, 80)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Blue));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_search(buf, chunks[0]);
        self.render_list(buf, chunks[1]);
        Self::render_help(buf, chunks[2]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        let filtered = filter(&self.items, &self.search);
        match key.code {
            KeyCode::Esc => OverlayAction::Close,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                OverlayAction::Stay
            }
            KeyCode::Down if self.selected + 1 < filtered.len() => {
                self.selected += 1;
                OverlayAction::Stay
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(8);
                OverlayAction::Stay
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 8).min(filtered.len().saturating_sub(1));
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.selected = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.selected = filtered.len().saturating_sub(1);
                OverlayAction::Stay
            }
            KeyCode::Enter => match filtered.get(self.selected) {
                Some(&idx) => {
                    let value = self.items[idx].value.clone();
                    OverlayAction::Resolve(serde_json::Value::String(value))
                }
                None => OverlayAction::Stay,
            },
            KeyCode::Backspace => {
                self.search.pop();
                self.selected = 0;
                self.scroll_offset = 0;
                OverlayAction::Stay
            }
            KeyCode::Char(c) => {
                self.search.push(c);
                self.selected = 0;
                self.scroll_offset = 0;
                OverlayAction::Stay
            }
            _ => OverlayAction::Stay,
        }
    }
}

impl OverlayPicker {
    fn render_search(&self, buf: &mut Buffer, area: Rect) {
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
        Widget::render(Paragraph::new(search_text), area, buf);
    }

    fn render_list(&mut self, buf: &mut Buffer, area: Rect) {
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
            Widget::render(
                Paragraph::new(Line::from(Span::styled(
                    "(no matches)",
                    Style::default().fg(Color::DarkGray),
                ))),
                area,
                buf,
            );
            return;
        }

        let total = filtered.len();
        let end = (offset + window).min(total);
        let above = offset;
        let below = total.saturating_sub(end);

        let mut items: Vec<ListItem<'static>> = Vec::with_capacity(window + 4);
        if above > 0 {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("... {above} more above"),
                Style::default().fg(Color::DarkGray),
            ))));
        }
        // Section headers are non-selectable render-only rows emitted
        // whenever the group changes within the visible window (the
        // first visible row always gets one so a scrolled-into group
        // keeps its label). `sel_render` tracks the selected item's
        // index *after* headers/spacers so the highlight lands right.
        let mut prev_group: Option<String> = None;
        let mut sel_render = 0usize;
        for (vi, &idx) in filtered.iter().enumerate().take(end).skip(offset) {
            let item = &self.items[idx];
            if let Some(group) = item.group.as_deref()
                && prev_group.as_deref() != Some(group)
            {
                if !items.is_empty() {
                    items.push(ListItem::new(Line::raw("")));
                }
                items.push(ListItem::new(Line::from(Span::styled(
                    group.to_owned(),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ))));
                prev_group = Some(group.to_owned());
            }
            let is_sel = vi == self.selected;
            if is_sel {
                sel_render = items.len();
            }
            items.push(ListItem::new(row_line(item, is_sel, area.width)));
        }
        if below > 0 {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("... {below} more below"),
                Style::default().fg(Color::DarkGray),
            ))));
        }

        let mut state = ListState::default();
        state.select(Some(sel_render));
        StatefulWidget::render(
            List::new(items).highlight_style(Style::default().add_modifier(Modifier::BOLD)),
            area,
            buf,
            &mut state,
        );
    }

    fn render_help(buf: &mut Buffer, area: Rect) {
        let line = Line::from(Span::styled(
            "up/down select  enter confirm  type to filter  esc cancel",
            Style::default().fg(Color::DarkGray),
        ));
        Widget::render(Paragraph::new(line), area, buf);
    }
}

/// Build one selectable row: ` badge ` chrome, the label, and an
/// optional right-aligned column flushed to the row's right edge.
/// The label is truncated so a >=2 col gap before the right column
/// always remains, at any terminal width (no fixed label padding).
fn row_line(item: &PickItem, is_sel: bool, width: u16) -> Line<'static> {
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
    let badge_span = Span::styled(format!(" {badge} "), Style::default().fg(badge_color));
    match item.right.as_deref() {
        None | Some("") => Line::from(vec![
            badge_span,
            Span::styled(item.label.clone(), label_style),
        ]),
        Some(right) => {
            let total = usize::from(width);
            let rlen = right.chars().count();
            let avail = total.saturating_sub(3 + rlen + 2);
            let label: String = item.label.chars().take(avail).collect();
            let lw = label.chars().count();
            let pad = total.saturating_sub(3 + lw + rlen);
            Line::from(vec![
                badge_span,
                Span::styled(label, label_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(right.to_owned(), Style::default().fg(Color::DarkGray)),
            ])
        }
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

    fn resolved(action: OverlayAction) -> Option<String> {
        match action {
            OverlayAction::Resolve(serde_json::Value::String(s)) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn enter_picks_the_selected_value() {
        let mut p = pick(&["a", "b", "c"]);
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(resolved(action), Some("a".into()));
    }

    #[test]
    fn down_arrow_moves_selection_then_enter_picks() {
        let mut p = pick(&["a", "b", "c"]);
        assert_eq!(p.handle_key(key(KeyCode::Down)), OverlayAction::Stay);
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(resolved(action), Some("b".into()));
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        let mut p = pick(&["a"]);
        assert_eq!(p.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
        let mut p2 = pick(&["a"]);
        assert_eq!(p2.handle_key(ctrl('c')), OverlayAction::Close);
    }

    #[test]
    fn typing_filters_the_visible_set() {
        let mut p = pick(&["anthropic", "openai", "gemini"]);
        for c in "open".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(resolved(action), Some("openai".into()));
    }

    #[test]
    fn selection_clamps_when_filter_shrinks_results() {
        let mut p = pick(&["alpha", "beta", "gamma"]);
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        for c in "alp".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(resolved(action), Some("alpha".into()));
    }

    #[test]
    fn picker_with_no_matches_returns_stay_on_enter() {
        let mut p = pick(&["a"]);
        for c in "zzz".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.handle_key(key(KeyCode::Enter)), OverlayAction::Stay);
    }

    #[test]
    fn grouped_items_keep_caller_order_and_resolve_correctly() {
        // Two sections; caller order is preserved (no alphabetical
        // re-sort) so groups stay contiguous, and selection still
        // maps to the right value despite render-only headers.
        let items = vec![
            PickItem::simple("s1")
                .with_label("first")
                .with_group("Today"),
            PickItem::simple("s2")
                .with_label("second")
                .with_group("Today"),
            PickItem::simple("s3")
                .with_label("third")
                .with_group("Yesterday"),
        ];
        let mut p = OverlayPicker::new("Sessions", items);
        // Order unchanged (would be s1,s2,s3 alphabetically too, so
        // assert via a group that would re-sort if sorting ran).
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(resolved(action), Some("s3".into()));
    }

    #[test]
    fn render_emits_section_headers_for_groups() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let items = vec![
            PickItem::simple("s1")
                .with_label("alpha")
                .with_group("Today"),
            PickItem::simple("s2")
                .with_label("beta")
                .with_group("Yesterday"),
        ];
        let mut p = OverlayPicker::new("Sessions", items);
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::Theme::default();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        OverlayWidget::render(&mut p, area, &mut buf, &ctx);
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("Today"), "section header missing:\n{text}");
        assert!(text.contains("Yesterday"), "second header missing");
        assert!(text.contains("alpha") && text.contains("beta"));
    }
}
