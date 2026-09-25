//! Render the toast strip below the conversation buffer.
//!
//! Toasts take rows of their own at the bottom of the conversation
//! area, one per toast with the newest at the bottom, so they never
//! cover conversation text, the start card or a notice. Each toast is
//! a one-row card against the right edge: a colored accent bar, a
//! kind icon and the message, cut to fit.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Block as RtBlock;
use unicode_width::UnicodeWidthStr;

use super::DECORATION_MARKER;
use super::truncate_to_width;
use crate::theme::Theme;
use crate::toast::{Toast, ToastKind};

/// Maximum cell width of a toast, irrespective of buffer width.
/// Chosen so a sentence-long notification reads comfortably without
/// eating the row.
const MAX_TOAST_WIDTH: u16 = 60;
/// Smallest meaningful toast width; below this the renderer skips
/// painting.
const MIN_TOAST_WIDTH: u16 = 18;
/// Cells of right margin between a toast and the area's right edge.
const RIGHT_MARGIN: u16 = 2;
/// Cells of chrome to the left of the message text:
/// 1 accent bar + 1 pad + 1 icon + 1 pad. Kept in sync with the
/// painter below.
const LEFT_CHROME: u16 = 4;
/// Cells of chrome to the right of the message text (right pad).
const RIGHT_CHROME: u16 = 1;

/// Rows the toast strip takes from a conversation area `height` rows
/// tall: one per toast, leaving at least half the area to the
/// conversation. Zero when the area is too narrow to paint a toast.
#[must_use]
pub fn toast_rows(toasts: usize, area: Rect) -> u16 {
    if area.width.saturating_sub(RIGHT_MARGIN * 2) < MIN_TOAST_WIDTH {
        return 0;
    }
    u16::try_from(toasts)
        .unwrap_or(u16::MAX)
        .min(area.height / 2)
}

/// Paints the newest `toasts` (newest last in the slice) into `area`,
/// one per row, newest at the bottom.
pub fn render_toasts(frame: &mut Frame, area: Rect, toasts: &[Toast], theme: &Theme) {
    let max_width = area.width.saturating_sub(RIGHT_MARGIN * 2);
    if toasts.is_empty() || area.height == 0 || max_width < MIN_TOAST_WIDTH {
        return;
    }
    let toast_width = compute_toast_width(toasts, max_width);
    let shown = toasts.len().min(usize::from(area.height));
    let rows = (area.y..area.bottom()).rev();
    for (y, toast) in rows.zip(toasts.iter().rev().take(shown)) {
        let card = Rect {
            x: area
                .right()
                .saturating_sub(RIGHT_MARGIN)
                .saturating_sub(toast_width),
            y,
            width: toast_width,
            height: 1,
        };
        paint_toast(frame, card, toast, theme);
    }
}

fn compute_toast_width(toasts: &[Toast], max: u16) -> u16 {
    let longest = toasts
        .iter()
        .map(|t| u16::try_from(t.text.width()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(MIN_TOAST_WIDTH);
    let want = longest.saturating_add(LEFT_CHROME + RIGHT_CHROME);
    want.clamp(MIN_TOAST_WIDTH, MAX_TOAST_WIDTH).min(max)
}

fn paint_toast(frame: &mut Frame, area: Rect, toast: &Toast, theme: &Theme) {
    let accent_fg = accent_for(theme, toast.kind);
    let card_bg = theme.modeline_bg;
    let chrome_style = Style::default().bg(card_bg).add_modifier(DECORATION_MARKER);
    // The decoration marker makes cell-based selection skip the card.
    frame.render_widget(RtBlock::default().style(chrome_style), area);

    let buf = frame.buffer_mut();
    let accent = Style::default()
        .fg(accent_fg)
        .bg(card_bg)
        .add_modifier(DECORATION_MARKER);
    buf.set_string(area.x, area.y, "\u{258E}", accent);
    buf.set_string(
        area.x.saturating_add(2),
        area.y,
        icon_for(toast.kind),
        accent.add_modifier(Modifier::BOLD),
    );
    let body_width = area
        .width
        .saturating_sub(LEFT_CHROME)
        .saturating_sub(RIGHT_CHROME);
    let text = truncate_to_width(&toast.text, usize::from(body_width), "\u{2026}");
    buf.set_string(
        area.x.saturating_add(LEFT_CHROME),
        area.y,
        text,
        Style::default().fg(theme.assistant_fg).bg(card_bg),
    );
}

fn accent_for(theme: &Theme, kind: ToastKind) -> ratatui::style::Color {
    match kind {
        ToastKind::Info | ToastKind::Success => theme.user_rule,
        ToastKind::Warning => theme.tool_rule,
        ToastKind::Error => theme.tool_error_fg,
    }
}

fn icon_for(kind: ToastKind) -> &'static str {
    match kind {
        ToastKind::Info => "\u{2022}",    // bullet
        ToastKind::Success => "\u{2713}", // check
        ToastKind::Warning => "\u{26a0}", // warning sign
        ToastKind::Error => "\u{2717}",   // ballot x
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::theme::Theme;
    use crate::toast::Toast;

    fn render_into(width: u16, height: u16, toasts: &[Toast]) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|f| render_toasts(f, Rect::new(0, 0, width, height), toasts, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn blank(rows: &[String]) -> bool {
        rows.iter().all(|row| row.chars().all(|c| c == ' '))
    }

    #[test]
    fn empty_toasts_paint_nothing() {
        assert!(blank(&render_into(40, 3, &[])));
    }

    #[test]
    fn a_toast_is_one_row_against_the_right_edge() {
        let rows = render_into(40, 1, &[Toast::info("hello")]);
        let row = rows[0].trim_end();
        assert!(row.contains("\u{258E} \u{2022} hello"), "{row:?}");
        let bar = row.chars().position(|c| c == '\u{258E}');
        assert_eq!(bar, Some(usize::from(40 - RIGHT_MARGIN - MIN_TOAST_WIDTH)));
    }

    #[test]
    fn wide_char_toast_paints_the_full_message() {
        let msg = "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}";
        let rows = render_into(60, 1, &[Toast::info(msg)]);
        let compact: String = rows[0].chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains(msg), "{:?}", rows[0]);
    }

    #[test]
    fn the_newest_toast_is_on_the_bottom_row() {
        let rows = render_into(40, 2, &[Toast::info("older"), Toast::info("newer")]);
        assert!(rows[0].contains("older"), "{rows:?}");
        assert!(rows[1].contains("newer"), "{rows:?}");
        let rows = render_into(40, 1, &[Toast::info("older"), Toast::info("newer")]);
        assert!(rows[0].contains("newer"), "{rows:?}");
    }

    #[test]
    fn long_message_is_truncated_with_ellipsis() {
        let rows = render_into(40, 1, &[Toast::info("a".repeat(200))]);
        assert!(rows[0].contains('\u{2026}'));
    }

    #[test]
    fn skips_painting_when_the_area_is_too_narrow() {
        assert!(blank(&render_into(8, 2, &[Toast::info("hi")])));
        assert_eq!(toast_rows(1, Rect::new(0, 0, 8, 10)), 0);
    }

    #[test]
    fn the_strip_takes_one_row_per_toast_up_to_half_the_area() {
        let area = Rect::new(0, 0, 60, 10);
        assert_eq!(toast_rows(0, area), 0);
        assert_eq!(toast_rows(2, area), 2);
        assert_eq!(toast_rows(9, area), 5);
    }

    #[test]
    fn kinds_use_their_icons() {
        let with = |kind| {
            let toast = Toast::with_kind("x", kind, std::time::Duration::from_secs(60));
            render_into(40, 1, &[toast]).concat()
        };
        assert!(with(ToastKind::Warning).contains('\u{26a0}'));
        assert!(with(ToastKind::Error).contains('\u{2717}'));
        assert!(with(ToastKind::Success).contains('\u{2713}'));
    }
}
