//! Render the toast overlay above the conversation buffer.
//!
//! Toasts paint top-right inside the buffer area, newest-on-top,
//! stacked vertically with a one-row gap. Each toast is a three-row
//! card: a colored vertical accent bar on the left, a top pad row,
//! a content row with a kind-icon + message, and a bottom pad row.
//! Widths adapt to text up to a sensible cap so long notifications
//! do not eclipse the conversation pane.
//!
//! The renderer is exposed behind a small trait so a plugin can
//! later swap the implementation entirely (PE.A wiring).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Block as RtBlock;

use super::DECORATION_MARKER;
use crate::theme::Theme;
use crate::toast::{Toast, ToastKind};

/// Maximum cell width of a toast, irrespective of buffer width.
/// Chosen so a sentence-long notification reads comfortably without
/// eating the conversation.
const MAX_TOAST_WIDTH: u16 = 60;
/// Smallest meaningful toast width; below this the renderer skips
/// painting (the buffer area is too narrow to add chrome on top).
const MIN_TOAST_WIDTH: u16 = 18;
/// Cells of right margin between the toast block and the buffer's
/// right edge, so the overlay does not sit flush against the
/// terminal frame.
const RIGHT_MARGIN: u16 = 2;
/// Cells of top margin between the buffer's top edge and the first
/// toast row.
const TOP_MARGIN: u16 = 1;
/// Vertical gap (rows) between stacked toasts.
const ROW_GAP: u16 = 1;
/// Height of every toast card: top pad row, content row, bottom pad
/// row. Three rows is enough to feel substantial without dominating
/// the conversation pane.
const TOAST_HEIGHT: u16 = 3;
/// Cells of chrome to the left of the message text:
/// 1 accent bar + 1 pad + 1 icon + 1 pad. Kept in sync with the
/// painter below.
const LEFT_CHROME: u16 = 4;
/// Cells of chrome to the right of the message text (right pad).
const RIGHT_CHROME: u16 = 1;

/// Paints `toasts` (newest last in the slice) as an overlay onto
/// `buffer_area`, top-right.
///
/// Returns silently when `toasts` is empty, the buffer area is too
/// narrow, or the lock-acquired snapshot is empty.
pub fn render_toasts(frame: &mut Frame, buffer_area: Rect, toasts: &[Toast], theme: &Theme) {
    if toasts.is_empty() {
        return;
    }
    let max_width = buffer_area.width.saturating_sub(RIGHT_MARGIN * 2);
    if max_width < MIN_TOAST_WIDTH || buffer_area.height < TOP_MARGIN + TOAST_HEIGHT {
        return;
    }
    let toast_width = compute_toast_width(toasts, max_width);

    // Newest at top: iterate the slice in reverse so the most
    // recent push lands closest to the buffer's top edge.
    let mut row_cursor = buffer_area.top().saturating_add(TOP_MARGIN);
    let bottom_limit = buffer_area.bottom();
    for toast in toasts.iter().rev() {
        if row_cursor.saturating_add(TOAST_HEIGHT) > bottom_limit {
            break;
        }
        let area = Rect {
            x: buffer_area
                .right()
                .saturating_sub(RIGHT_MARGIN)
                .saturating_sub(toast_width),
            y: row_cursor,
            width: toast_width,
            height: TOAST_HEIGHT,
        };
        paint_toast(frame, area, toast, theme);
        row_cursor = row_cursor
            .saturating_add(TOAST_HEIGHT)
            .saturating_add(ROW_GAP);
    }
}

fn compute_toast_width(toasts: &[Toast], max: u16) -> u16 {
    let longest = toasts
        .iter()
        .map(|t| u16::try_from(t.text.chars().count()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(MIN_TOAST_WIDTH);
    let want = longest.saturating_add(LEFT_CHROME + RIGHT_CHROME);
    want.clamp(MIN_TOAST_WIDTH, MAX_TOAST_WIDTH).min(max)
}

fn paint_toast(frame: &mut Frame, area: Rect, toast: &Toast, theme: &Theme) {
    let accent_fg = accent_for(theme, toast.kind);
    let card_bg = theme.modeline_bg;
    let text_fg = theme.assistant_fg;
    let chrome_style = Style::default().bg(card_bg).add_modifier(DECORATION_MARKER);

    // Background fill so the toast occludes whatever buffer content
    // sits below it. Carries the decoration marker so cell-based
    // selection skips the overlay.
    frame.render_widget(RtBlock::default().style(chrome_style), area);

    // Vertical accent bar, full toast height.
    let accent_area = Rect {
        x: area.x,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let accent_style = Style::default()
        .fg(accent_fg)
        .bg(card_bg)
        .add_modifier(Modifier::BOLD)
        .add_modifier(DECORATION_MARKER);
    let buf = frame.buffer_mut();
    for y in accent_area.y..accent_area.y + accent_area.height {
        buf.set_string(accent_area.x, y, "\u{2588}", accent_style);
    }

    // Content row sits in the vertical middle of the toast (row 1
    // of 3). Top and bottom rows are filled by the bg block above.
    let content_row = area.y.saturating_add(area.height / 2);
    let mut x = area.x.saturating_add(2); // accent (1) + left pad (1)

    let icon = icon_for(toast.kind);
    buf.set_string(
        x,
        content_row,
        icon,
        Style::default()
            .fg(accent_fg)
            .bg(card_bg)
            .add_modifier(Modifier::BOLD)
            .add_modifier(DECORATION_MARKER),
    );
    x = x.saturating_add(2); // icon (1) + pad (1)

    let body_width = area
        .width
        .saturating_sub(LEFT_CHROME)
        .saturating_sub(RIGHT_CHROME);
    let truncated = truncate_chars(&toast.text, usize::from(body_width));
    buf.set_string(
        x,
        content_row,
        truncated,
        Style::default().fg(text_fg).bg(card_bg),
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
        ToastKind::Info => "\u{2022}",     // bullet
        ToastKind::Success => "\u{2713}",  // check
        ToastKind::Warning => "\u{26a0}",  // warning sign
        ToastKind::Error => "\u{2717}",    // ballot x
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return chars[..max_chars].iter().collect();
    }
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::theme::Theme;
    use crate::toast::Toast;

    fn render_into(width: u16, height: u16, toasts: &[Toast]) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, width, height);
                render_toasts(f, area, toasts, &theme);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn empty_toasts_paint_nothing() {
        let painted = render_into(40, 6, &[]);
        for line in painted.lines() {
            assert!(line.chars().all(|c| c == ' '), "got {line:?}");
        }
    }

    #[test]
    fn toast_paints_three_row_card_with_accent_and_icon() {
        let painted = render_into(40, 6, &[Toast::info("hello")]);
        let rows: Vec<&str> = painted.lines().collect();
        // Row 0 is the top margin (blank). Rows 1..=3 are the toast.
        assert!(rows[0].chars().all(|c| c == ' '), "row 0 margin");
        assert!(
            rows[1].contains('\u{2588}'),
            "row 1 should contain accent block, got {:?}",
            rows[1]
        );
        // Middle (content) row carries the message and icon.
        assert!(
            rows[2].contains("hello"),
            "row 2 should contain message, got {:?}",
            rows[2]
        );
        assert!(
            rows[2].contains('\u{2022}'),
            "row 2 should contain info icon, got {:?}",
            rows[2]
        );
        // Bottom row of the toast still has the accent bar (full
        // height) but no text.
        assert!(
            rows[3].contains('\u{2588}'),
            "row 3 should still have accent bar, got {:?}",
            rows[3]
        );
    }

    #[test]
    fn newest_toast_paints_above_older_ones_with_row_gap() {
        let painted = render_into(40, 10, &[Toast::info("older"), Toast::info("newer")]);
        let rows: Vec<&str> = painted.lines().collect();
        // Newer toast: rows 1..=3, content row 2.
        assert!(
            rows[2].contains("newer"),
            "row 2 should be the newer toast, got {:?}",
            rows[2]
        );
        // Row gap at row 4.
        assert!(
            rows[4].chars().all(|c| c == ' '),
            "row 4 should be blank gap"
        );
        // Older toast: rows 5..=7, content row 6.
        assert!(
            rows[6].contains("older"),
            "row 6 should be the older toast, got {:?}",
            rows[6]
        );
    }

    #[test]
    fn long_message_is_truncated_with_ellipsis() {
        let long = "a".repeat(200);
        let painted = render_into(40, 6, &[Toast::info(long)]);
        assert!(painted.contains('\u{2026}'));
    }

    #[test]
    fn skips_painting_when_buffer_too_narrow() {
        let painted = render_into(8, 6, &[Toast::info("hi")]);
        for line in painted.lines() {
            assert!(line.chars().all(|c| c == ' '), "got {line:?}");
        }
    }

    #[test]
    fn skips_painting_when_buffer_too_short_for_any_toast() {
        let painted = render_into(40, 2, &[Toast::info("hi")]);
        for line in painted.lines() {
            assert!(line.chars().all(|c| c == ' '), "got {line:?}");
        }
    }

    #[test]
    fn warning_kind_uses_warning_icon() {
        let painted = render_into(
            40,
            6,
            &[Toast::with_kind(
                "heads up",
                ToastKind::Warning,
                std::time::Duration::from_secs(60),
            )],
        );
        assert!(painted.contains('\u{26a0}'));
    }

    #[test]
    fn error_kind_uses_error_icon() {
        let painted = render_into(
            40,
            6,
            &[Toast::with_kind(
                "boom",
                ToastKind::Error,
                std::time::Duration::from_secs(60),
            )],
        );
        assert!(painted.contains('\u{2717}'));
    }
}
