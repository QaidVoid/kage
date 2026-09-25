//! Fuzzy-filtered modal picker.
//!
//! [`OverlayPicker`] serves both the `Ctrl+P` model switcher and the
//! `Ctrl+S` session resume picker. The data model (search, selection,
//! paginated window) is identical to the standalone [`crate::picker`]
//! used outside the TUI; we share [`crate::picker::PickItem`],
//! [`crate::picker::filter`], and [`crate::picker::compute_window`].
//!
//! Implements [`OverlayWidget`]. A thin Frame-based
//! [`OverlayPicker::render`] wrapper preserves the call shape App
//! still uses today; both paths end in the same Buffer-level paint.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Widget,
};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::picker::{PickItem, compute_window, filter};
use crate::view::UnicodeWidthStr as _;
use crate::view::truncate_to_width;

/// Key hint painted on the picker's bottom row.
const HELP_HINT: &str = "up/down select  enter confirm  type to filter  esc cancel";

/// Widest a picker may grow on a narrow terminal, where 80% of the
/// width would cut long rows: a 100-column row less a one-cell margin
/// on each side.
const NARROW_MAX_WIDTH: u16 = 98;

/// Stateful picker rendered as a modal overlay.
#[derive(Debug)]
pub struct OverlayPicker {
    title: String,
    items: Vec<PickItem>,
    search: String,
    selected: usize,
    scroll_offset: usize,
    note: Option<String>,
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
        Self::new_ordered(title, items)
    }

    /// Construct a picker that keeps the caller's row order, grouped
    /// or not. Use this when the order carries meaning, such as
    /// newest-first message targets.
    #[must_use]
    pub fn new_ordered(title: impl Into<String>, items: Vec<PickItem>) -> Self {
        Self {
            title: title.into(),
            items,
            search: String::new(),
            selected: 0,
            scroll_offset: 0,
            note: None,
        }
    }

    /// Add a muted note row between the list and the key hint.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
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
        // Width: longest label plus a little padding for the
        // gutter/badge/right-aligned chunk, never narrower than the
        // title or the key hint, clamped between a comfortable
        // minimum and 80% of available so the picker is big enough
        // to read but small enough that a tiny menu does not eat the
        // whole screen. Narrow terminals allow up to
        // [`NARROW_MAX_WIDTH`] instead, so long rows are not cut early.
        let rows = self
            .items
            .iter()
            .map(|i| {
                let mut len = i.label.width();
                if let Some(r) = &i.right {
                    len += r.width() + 2;
                }
                if i.badge.is_some() {
                    len += 3; // " B "
                }
                u16::try_from(len).unwrap_or(u16::MAX)
            })
            .max()
            .unwrap_or(20)
            .saturating_add(6); // borders + selection gutter
        let title = u16::try_from(self.title.width() + 4).unwrap_or(u16::MAX);
        let hint = u16::try_from(HELP_HINT.width() + 2).unwrap_or(u16::MAX);
        let note = self
            .note
            .as_deref()
            .map_or(0, |n| u16::try_from(n.width() + 2).unwrap_or(u16::MAX));
        let longest = rows.max(title).max(hint).max(note);
        let max_w = (available.width.saturating_mul(80) / 100)
            .max(available.width.saturating_sub(2).min(NARROW_MAX_WIDTH))
            .max(30);
        let want_w = longest.clamp(30, max_w);

        // Height: distinct groups produce a section header + blank
        // spacer; plus borders (2), search row (1), help row (1),
        // plus the items themselves. Cap at 80% so a huge list still
        // leaves the conversation context visible behind the overlay.
        let groups: std::collections::BTreeSet<&str> = self
            .items
            .iter()
            .filter_map(|i| i.group.as_deref())
            .collect();
        let item_rows = u16::try_from(self.items.len()).unwrap_or(u16::MAX);
        let group_rows = u16::try_from(groups.len()).unwrap_or(0).saturating_mul(2);
        let chrome = 4 + u16::from(self.note.is_some()); // borders, search, note, help
        let want_h = item_rows
            .saturating_add(group_rows)
            .saturating_add(chrome)
            .max(6);
        let max_h = (available.height.saturating_mul(80) / 100).max(6);
        let want_h = want_h.min(max_h);

        center_absolute(available, want_w, want_h)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        let block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::theme::current().overlay_border));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(u16::from(self.note.is_some())),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_search(buf, chunks[0]);
        self.render_list(buf, chunks[1]);
        if let Some(note) = &self.note {
            let style = Style::default().fg(crate::theme::current().muted_fg);
            Widget::render(
                Paragraph::new(Span::styled(note.clone(), style)),
                chunks[2],
                buf,
            );
        }
        Self::render_help(buf, chunks[3]);
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

    fn handle_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        self.search.push_str(&clean);
        self.selected = 0;
        self.scroll_offset = 0;
    }
}

impl OverlayPicker {
    fn render_search(&self, buf: &mut Buffer, area: Rect) {
        let t = crate::theme::current();
        let search_text = if self.search.is_empty() {
            Line::from(vec![
                Span::styled("/ ", Style::default().fg(t.muted_fg)),
                Span::styled("type to filter", Style::default().fg(t.muted_fg)),
            ])
        } else {
            Line::from(vec![
                Span::styled("/ ", Style::default().fg(t.muted_fg)),
                Span::styled(self.search.clone(), Style::default().fg(t.overlay_fg)),
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

        let t = crate::theme::current();

        if filtered.is_empty() {
            Widget::render(
                Paragraph::new(Line::from(Span::styled(
                    "(no matches)",
                    Style::default().fg(t.muted_fg),
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
                Style::default().fg(t.muted_fg),
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
                    Style::default().fg(t.muted_fg).add_modifier(Modifier::BOLD),
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
                Style::default().fg(t.muted_fg),
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
            HELP_HINT,
            Style::default().fg(crate::theme::current().muted_fg),
        ));
        Widget::render(Paragraph::new(line), area, buf);
    }
}

/// Build one selectable row: ` badge ` chrome, the label, and an
/// optional right-aligned column flushed to the row's right edge.
/// The label is truncated so a >=2 col gap before the right column
/// always remains, at any terminal width (no fixed label padding).
fn row_line(item: &PickItem, is_sel: bool, width: u16) -> Line<'static> {
    let t = crate::theme::current();
    let badge = item.badge.unwrap_or(' ');
    let badge_color = if item.badge == Some('*') {
        t.success_fg
    } else {
        t.muted_fg
    };
    let label_style = if is_sel {
        Style::default()
            .fg(t.overlay_selected_fg)
            .bg(t.overlay_selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.overlay_fg)
    };
    let badge_span = Span::styled(format!(" {badge} "), Style::default().fg(badge_color));
    match item.right.as_deref() {
        None | Some("") => Line::from(vec![
            badge_span,
            Span::styled(item.label.clone(), label_style),
        ]),
        Some(right) => {
            let total = usize::from(width);
            let rlen = right.width();
            let avail = total.saturating_sub(3 + rlen + 2);
            let label: String = truncate_to_width(&item.label, avail, "");
            let lw = label.width();
            let pad = total.saturating_sub(3 + lw + rlen);
            Line::from(vec![
                badge_span,
                Span::styled(label, label_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(right.to_owned(), Style::default().fg(t.muted_fg)),
            ])
        }
    }
}

/// Center a rectangle of exactly `want_w` x `want_h` cells inside
/// `area`. Clamped to the available size so over-large requests just
/// fill the area. Used by [`OverlayPicker::measure`] now that picker
/// dimensions follow content; the percentage-based [`center`] below
/// remains for the few callers that still want a fixed-ratio modal.
fn center_absolute(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let w = want_w.min(area.width);
    let h = want_h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
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
    fn paste_extends_the_search_filter() {
        let mut p = pick(&["apple", "banana"]);
        p.handle_key(key(KeyCode::Char('b')));
        p.handle_paste("an\n");
        assert_eq!(p.search, "ban");
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll_offset, 0);
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

    #[test]
    fn measure_fits_the_title_and_key_hint() {
        let title = "Resume session - this dir (Tab toggles all dirs)";
        let p = OverlayPicker::new(title, vec![PickItem::simple("a")]);
        let m = p.measure(Rect::new(0, 0, 200, 40));
        assert!(usize::from(m.width) >= title.width() + 4);
        assert!(usize::from(m.width) >= HELP_HINT.width() + 2);
    }

    #[test]
    fn new_ordered_keeps_caller_order() {
        let items = ["b", "c", "a"].map(PickItem::simple).to_vec();
        let mut p = OverlayPicker::new_ordered("Pick", items);
        assert_eq!(
            resolved(p.handle_key(key(KeyCode::Enter))),
            Some("b".into())
        );
    }
}
