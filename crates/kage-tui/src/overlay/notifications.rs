//! Live, scrollable retained notification history (`/notifications`).

use std::time::Instant;

use kage_core::sync::lock;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr as _;

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::theme::Theme;
use crate::toast::{SharedToasts, ToastKind};

/// The `/notifications` modal, showing retained messages newest first.
///
/// Opening or scrolling does not acknowledge notifications. The user can
/// explicitly mark the retained history reviewed with `r`.
#[derive(Debug)]
pub struct NotificationsOverlay {
    toasts: SharedToasts,
    scroll: usize,
    line_count: usize,
    viewport_rows: u16,
    row_keys: Vec<Option<(Instant, usize)>>,
}

impl NotificationsOverlay {
    /// Open a live view of `toasts` without changing its unread state.
    #[must_use]
    pub fn new(toasts: SharedToasts) -> Self {
        Self {
            toasts,
            scroll: 0,
            line_count: 0,
            viewport_rows: 1,
            row_keys: Vec::new(),
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(
            self.line_count
                .saturating_sub(usize::from(self.viewport_rows.max(1))),
        );
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp_scroll();
    }

    fn lines(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        self.rows(width, theme)
            .into_iter()
            .map(|(_, line)| line)
            .collect()
    }

    fn rows(&self, width: u16, theme: &Theme) -> Vec<(Option<(Instant, usize)>, Line<'static>)> {
        let body = Style::default().fg(theme.overlay_fg);
        let muted = Style::default().fg(theme.muted_fg);
        let groups = {
            let state = lock(&self.toasts);
            let mut groups = Vec::new();
            for notification in state.history.iter().rev() {
                let mut rows = Vec::new();
                let (label, color) = match notification.kind {
                    ToastKind::Info => ("INFO", theme.focus_color),
                    ToastKind::Success => ("SUCCESS", theme.success_fg),
                    ToastKind::Warning => ("WARNING", theme.warning_fg),
                    ToastKind::Error => ("ERROR", theme.tool_error_fg),
                };
                rows.push(Line::from(vec![
                    Span::styled(
                        format!("[{label}] "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "source: {}",
                            display_text(&notification.source).replace('\n', " ")
                        ),
                        body,
                    ),
                    Span::styled(format!(" | x{}", notification.count), muted),
                    Span::styled(
                        if notification.reviewed {
                            " | reviewed"
                        } else {
                            " | unreviewed"
                        },
                        muted,
                    ),
                ]));
                rows.extend(
                    display_text(&notification.text)
                        .split('\n')
                        .map(|line| Line::from(Span::styled(line.to_owned(), body))),
                );
                groups.push((notification.first_seen, rows));
            }
            groups
        };
        let width = usize::from(width.max(1));
        let mut rows = Vec::new();
        for (group, lines) in groups {
            let mut group_rows: Vec<_> = lines
                .into_iter()
                .flat_map(|line| wrap_line(line, width))
                .enumerate()
                .map(|(index, line)| (Some((group, index)), line))
                .collect();
            group_rows.push((Some((group, group_rows.len())), Line::default()));
            rows.extend(group_rows);
        }
        rows.pop();
        if rows.is_empty() {
            rows.extend(
                wrap_line(
                    Line::from(Span::styled("No notifications yet.", muted)),
                    width,
                )
                .into_iter()
                .map(|line| (None, line)),
            );
        }
        rows
    }
}

/// Normalize whitespace and expose control characters for display only.
fn display_text(text: &str) -> String {
    let mut display = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\t' => display.push_str("    "),
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                display.push('\n');
            }
            '\n' => display.push('\n'),
            character if character.is_control() => display.extend(character.escape_default()),
            character => display.push(character),
        }
    }
    display
}

/// Wrap unbroken diagnostics at grapheme boundaries within the cell width.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut text = String::new();
        for grapheme in span.styled_graphemes(Style::default()) {
            let cells = grapheme.symbol.width();
            if used > 0 && used + cells > width {
                if !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            text.push_str(grapheme.symbol);
            used += cells;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    rows.push(Line::from(spans));
    rows
}

impl OverlayWidget for NotificationsOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = 88.min(available.width.saturating_sub(2));
        let rows = self
            .lines(width.saturating_sub(2), &crate::theme::current())
            .len();
        let height = u16::try_from(rows)
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(available.height.saturating_sub(2));
        Rect::new(
            available.x + available.width.saturating_sub(width) / 2,
            available.y + available.height.saturating_sub(height) / 2,
            width,
            height,
        )
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>) {
        Widget::render(crate::opaque::OpaqueClear, area, buf);
        let theme = ctx.theme;
        let (total, unread) = {
            let state = lock(&self.toasts);
            (state.history.len(), state.unread_count())
        };
        let block = Block::default()
            .title(" notifications ")
            .title(
                Line::from(Span::styled(
                    format!(" {total} retained, {unread} need attention "),
                    Style::default().fg(theme.muted_fg),
                ))
                .right_aligned(),
            )
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.overlay_border));
        let inner = block.inner(area);
        Widget::render(block, area, buf);
        let anchor = if self.scroll > 0 {
            self.row_keys.get(self.scroll).copied().flatten()
        } else {
            None
        };
        let (keys, lines): (Vec<_>, Vec<_>) = self.rows(inner.width, theme).into_iter().unzip();
        if let Some(anchor) = anchor {
            let position = keys
                .iter()
                .position(|key| *key == Some(anchor))
                .or_else(|| {
                    keys.iter()
                        .rposition(|key| key.is_some_and(|(group, _)| group == anchor.0))
                });
            if let Some(position) = position {
                self.scroll = position;
            }
        }
        self.row_keys = keys;
        self.viewport_rows = inner.height;
        self.line_count = lines.len();
        self.clamp_scroll();
        let visible = lines.into_iter().skip(self.scroll);
        Widget::render(Paragraph::new(visible.collect::<Vec<_>>()), inner, buf);
    }

    fn footer_hint(&self) -> &'static str {
        "up/down j/k scroll \u{b7} pgup/pgdn home/end \u{b7} r mark all reviewed \u{b7} esc/q close"
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return OverlayAction::Close;
        }
        let page = isize::try_from(self.viewport_rows.saturating_sub(1).max(1)).unwrap_or(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => return OverlayAction::Close,
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::PageDown => self.scroll_by(page),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => {
                self.scroll = usize::MAX;
                self.clamp_scroll();
            }
            KeyCode::Char('r') if key.modifiers.is_empty() => {
                lock(&self.toasts).mark_reviewed();
            }
            _ => {}
        }
        OverlayAction::Stay
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::toast::{Notification, shared_toasts};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn notification(text: &str, kind: ToastKind, source: &str, count: u64) -> Notification {
        let now = Instant::now();
        Notification {
            text: text.to_owned(),
            kind,
            source: source.to_owned(),
            count,
            first_seen: now,
            last_seen: now,
            reviewed: false,
        }
    }

    fn overlay(text: &str) -> NotificationsOverlay {
        let toasts = shared_toasts();
        lock(&toasts)
            .history
            .push_back(notification(text, ToastKind::Error, "configuration", 3));
        NotificationsOverlay::new(toasts)
    }

    fn paint(overlay: &mut NotificationsOverlay, area: Rect) -> Buffer {
        let theme = Theme::default();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf, &ctx);
        buf
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn wraps_full_unicode_diagnostics_without_losing_characters() {
        let diagnostic = format!(
            "{}\n{}\nFINAL_LINE",
            "long/path/without/spaces/".repeat(30),
            "\u{754c}\u{1f980}e\u{301}\u{2764}\u{fe0f}\u{1f469}\u{200d}\u{1f4bb}".repeat(40),
        );
        let mut overlay = overlay(&diagnostic);
        let lines = overlay.lines(13, &Theme::default());
        assert!(lines.iter().all(|line| line.width() <= 13));
        let wrapped = text(&lines);
        assert!(wrapped.ends_with(&diagnostic.replace('\n', "")));
        let area = Rect::new(0, 0, 15, 6);
        let initial = paint(&mut overlay, area);
        assert!(!buffer_text(&initial).contains("FINAL_LINE"));
        overlay.handle_key(key(KeyCode::End));
        let end = paint(&mut overlay, area);
        assert!(buffer_text(&end).contains("FINAL_LINE"));
        assert!(overlay.scroll > 0);
    }

    #[test]
    fn metadata_includes_severity_source_repeat_count_and_review_state() {
        let overlay = overlay("bad input");
        let contents = text(&overlay.lines(80, &Theme::default()));
        for expected in [
            "[ERROR]",
            "source: configuration",
            "x3",
            "unreviewed",
            "bad input",
        ] {
            assert!(
                contents.contains(expected),
                "missing {expected}: {contents}"
            );
        }
        for (kind, label) in [
            (ToastKind::Info, "INFO"),
            (ToastKind::Success, "SUCCESS"),
            (ToastKind::Warning, "WARNING"),
        ] {
            lock(&overlay.toasts).history.back_mut().unwrap().kind = kind;
            assert!(text(&overlay.lines(80, &Theme::default())).contains(label));
        }
    }

    #[test]
    fn history_is_live_and_newest_first() {
        let overlay = overlay("older message");
        lock(&overlay.toasts).history.push_back(notification(
            "newer message",
            ToastKind::Success,
            "plugin",
            1,
        ));
        let contents = text(&overlay.lines(80, &Theme::default()));
        assert!(contents.find("newer message") < contents.find("older message"));
    }

    #[test]
    fn review_is_explicit_and_preserves_history() {
        let mut overlay = overlay("retained diagnostic");
        paint(&mut overlay, Rect::new(0, 0, 40, 8));
        assert_eq!(lock(&overlay.toasts).unread_count(), 1);
        overlay.handle_key(key(KeyCode::Down));
        assert_eq!(lock(&overlay.toasts).unread_count(), 1);
        assert_eq!(
            overlay.handle_key(key(KeyCode::Char('r'))),
            OverlayAction::Stay
        );
        let state = lock(&overlay.toasts);
        assert_eq!(state.unread_count(), 0);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].text, "retained diagnostic");
        assert_eq!(state.history[0].count, 3);
        assert!(state.history[0].reviewed);
    }

    #[test]
    fn tiny_and_zero_areas_are_safe() {
        let mut overlay = overlay("\u{754c}\u{1f980} diagnostic");
        for width in 0..8 {
            for height in 0..6 {
                let area = Rect::new(3, 2, width, height);
                let measured = overlay.measure(area);
                assert_eq!(measured.intersection(area), measured);
                paint(&mut overlay, measured);
                overlay.handle_key(key(KeyCode::End));
                overlay.handle_key(key(KeyCode::PageUp));
            }
        }
    }

    #[test]
    fn empty_state_and_footer_are_readable() {
        let mut overlay = NotificationsOverlay::new(shared_toasts());
        let buffer = paint(&mut overlay, Rect::new(0, 0, 40, 5));
        assert!(buffer_text(&buffer).contains("No notifications yet."));
        assert!(overlay.footer_hint().contains("r mark all reviewed"));
    }

    #[test]
    fn keyboard_scrolls_visual_rows_and_clamps() {
        let mut overlay = overlay(&"diagnostic ".repeat(80));
        paint(&mut overlay, Rect::new(0, 0, 30, 7));
        overlay.handle_key(key(KeyCode::Char('j')));
        assert_eq!(overlay.scroll, 1);
        overlay.handle_key(key(KeyCode::Down));
        assert_eq!(overlay.scroll, 2);
        overlay.handle_key(key(KeyCode::Char('k')));
        overlay.handle_key(key(KeyCode::Up));
        assert_eq!(overlay.scroll, 0);
        overlay.handle_key(key(KeyCode::PageDown));
        assert_eq!(overlay.scroll, 4);
        overlay.handle_key(key(KeyCode::PageUp));
        assert_eq!(overlay.scroll, 0);
        overlay.handle_key(key(KeyCode::End));
        let bottom = overlay.line_count - usize::from(overlay.viewport_rows);
        assert_eq!(overlay.scroll, bottom);
        overlay.handle_key(key(KeyCode::Down));
        assert_eq!(overlay.scroll, bottom);
        overlay.handle_key(key(KeyCode::Home));
        overlay.handle_key(key(KeyCode::Up));
        assert_eq!(overlay.scroll, 0);
    }

    #[test]
    fn display_normalizes_controls_without_changing_retained_text() {
        let diagnostic = "left\tright\r\nsecond\rthird\x1bfourth\0tail";
        let source = "plugin\tsource\r\nname\x07";
        let overlay = overlay(diagnostic);
        lock(&overlay.toasts).history[0].source = source.to_owned();
        let lines = overlay.lines(120, &Theme::default());
        let rendered: Vec<_> = lines
            .iter()
            .map(|line| text(std::slice::from_ref(line)))
            .collect();
        assert!(rendered[0].contains("source: plugin    source name\\u{7}"));
        assert_eq!(rendered[1], "left    right");
        assert_eq!(rendered[2], "second");
        assert_eq!(rendered[3], "third\\u{1b}fourth\\u{0}tail");
        let state = lock(&overlay.toasts);
        assert_eq!(state.history[0].text, diagnostic);
        assert_eq!(state.history[0].source, source);
    }

    fn first_body_line(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.x + 1..area.right() - 1)
            .map(|x| buffer[(x, area.y + 1)].symbol())
            .collect()
    }

    #[test]
    fn incoming_notifications_keep_the_scrolled_reading_position() {
        let diagnostic = (0..40)
            .map(|index| format!("diagnostic line {index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut overlay = overlay(&diagnostic);
        let area = Rect::new(0, 0, 48, 8);
        paint(&mut overlay, area);
        overlay.scroll_by(12);
        let before = paint(&mut overlay, area);
        let anchor = overlay.row_keys[overlay.scroll];
        for index in 0..3 {
            lock(&overlay.toasts).history.push_back(notification(
                &format!("incoming message {index}"),
                ToastKind::Info,
                "plugin",
                1,
            ));
            let after = paint(&mut overlay, area);
            assert_eq!(overlay.row_keys[overlay.scroll], anchor);
            assert_eq!(first_body_line(&after), first_body_line(&before));
        }
        overlay.handle_key(key(KeyCode::Home));
        paint(&mut overlay, area);
        lock(&overlay.toasts).history.push_back(notification(
            "newest message",
            ToastKind::Info,
            "plugin",
            1,
        ));
        let newest = paint(&mut overlay, area);
        assert_eq!(overlay.scroll, 0);
        assert!(buffer_text(&newest).contains("newest message"));
    }

    #[test]
    fn duplicate_group_reordering_keeps_the_scrolled_reading_position() {
        let diagnostic = (0..40)
            .map(|index| format!("diagnostic line {index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut overlay = overlay(&diagnostic);
        lock(&overlay.toasts).history.push_back(notification(
            "more recent message",
            ToastKind::Info,
            "plugin",
            1,
        ));
        let area = Rect::new(0, 0, 48, 8);
        paint(&mut overlay, area);
        overlay.scroll_by(15);
        let before = paint(&mut overlay, area);
        let anchor = overlay.row_keys[overlay.scroll];
        {
            let mut state = lock(&overlay.toasts);
            let mut repeated = state.history.pop_front().unwrap();
            repeated.count += 1;
            repeated.last_seen = Instant::now();
            state.history.push_back(repeated);
        }
        let after = paint(&mut overlay, area);
        assert_eq!(overlay.row_keys[overlay.scroll], anchor);
        assert_eq!(first_body_line(&after), first_body_line(&before));
    }

    #[test]
    fn anchored_history_resizes_and_survives_eviction_safely() {
        let mut overlay = overlay(&"long diagnostic with \u{754c} text ".repeat(100));
        paint(&mut overlay, Rect::new(0, 0, 48, 8));
        overlay.scroll_by(40);
        paint(&mut overlay, Rect::new(0, 0, 48, 8));
        for area in [
            Rect::new(0, 0, 22, 6),
            Rect::new(0, 0, 100, 12),
            Rect::new(0, 0, 1, 1),
            Rect::default(),
            Rect::new(0, 0, 40, 8),
        ] {
            paint(&mut overlay, area);
            assert!(overlay.scroll < overlay.line_count);
            assert_eq!(overlay.row_keys.len(), overlay.line_count);
        }
        lock(&overlay.toasts).history.clear();
        let empty = paint(&mut overlay, Rect::new(0, 0, 40, 8));
        assert_eq!(overlay.scroll, 0);
        assert!(buffer_text(&empty).contains("No notifications yet."));
    }

    #[test]
    fn modal_consumes_other_keys_paste_and_release_events() {
        let mut overlay = overlay("untouched");
        for code in [KeyCode::Enter, KeyCode::Char('x'), KeyCode::Backspace] {
            assert_eq!(overlay.handle_key(key(code)), OverlayAction::Stay);
        }
        overlay.handle_paste("must not reach the draft");
        let mut release = key(KeyCode::Char('r'));
        release.kind = KeyEventKind::Release;
        assert_eq!(overlay.handle_key(release), OverlayAction::Stay);
        assert_eq!(lock(&overlay.toasts).unread_count(), 1);
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert_eq!(overlay.handle_key(key(code)), OverlayAction::Close);
        }
    }
}
