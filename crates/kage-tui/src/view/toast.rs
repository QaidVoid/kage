//! Render the toast overlay above the conversation buffer.
//!
//! Toasts paint top-right inside the buffer area, newest-on-top,
//! stacked vertically with one row gap. Each toast is one row with
//! a colored left edge, the message, and right padding. Widths
//! adapt to text up to a sensible cap so long notifications don't
//! eclipse the conversation pane.
//!
//! The renderer is exposed behind a small trait so a plugin can
//! later swap the implementation entirely (PE.A wiring).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Paragraph};

use super::DECORATION_MARKER;
use crate::theme::Theme;
use crate::toast::{Toast, ToastKind};

/// Maximum cell width of a toast, irrespective of buffer width.
/// Chosen so a sentence-long notification reads comfortably without
/// eating the conversation.
const MAX_TOAST_WIDTH: u16 = 60;
/// Smallest meaningful toast width; below this the renderer skips
/// painting (the buffer area is too narrow to add chrome on top).
const MIN_TOAST_WIDTH: u16 = 14;
/// Cells of right margin between the toast block and the buffer's
/// right edge, so the overlay does not sit flush against the
/// terminal frame.
const RIGHT_MARGIN: u16 = 1;
/// Cells of top margin between the buffer's top edge and the first
/// toast row. Mirrors `RIGHT_MARGIN`.
const TOP_MARGIN: u16 = 1;
/// Vertical gap (rows) between stacked toasts.
const ROW_GAP: u16 = 0;

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
    if max_width < MIN_TOAST_WIDTH {
        return;
    }
    let toast_width = compute_toast_width(toasts, max_width);

    // Newest at top: iterate the slice in reverse so the most
    // recent push lands closest to the buffer's top edge.
    let mut row_cursor = buffer_area.top().saturating_add(TOP_MARGIN);
    let bottom_limit = buffer_area.bottom();
    for toast in toasts.iter().rev() {
        if row_cursor >= bottom_limit {
            break;
        }
        let area = Rect {
            x: buffer_area
                .right()
                .saturating_sub(RIGHT_MARGIN)
                .saturating_sub(toast_width),
            y: row_cursor,
            width: toast_width,
            height: 1,
        };
        paint_toast(frame, area, toast, theme);
        row_cursor = row_cursor.saturating_add(1).saturating_add(ROW_GAP);
    }
}

fn compute_toast_width(toasts: &[Toast], max: u16) -> u16 {
    let longest = toasts
        .iter()
        .map(|t| u16::try_from(t.text.chars().count()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(MIN_TOAST_WIDTH);
    // 4 cells of chrome: 1 accent column + 1 left pad + 1 right pad
    // + 1 right edge; everything else is text.
    let want = longest.saturating_add(4);
    want.clamp(MIN_TOAST_WIDTH, MAX_TOAST_WIDTH).min(max)
}

fn paint_toast(frame: &mut Frame, area: Rect, toast: &Toast, theme: &Theme) {
    let (accent_fg, body_fg) = colors_for(theme, toast.kind);
    let bg_style = Style::default()
        .bg(theme.modeline_bg)
        .add_modifier(DECORATION_MARKER);
    // Background fill so the toast occludes whatever buffer content
    // sits below it. The background block carries the decoration
    // marker so cell-based selection skips the overlay.
    frame.render_widget(RtBlock::default().style(bg_style), area);

    let body_width = area.width.saturating_sub(3); // accent + left pad + right pad
    let truncated = truncate_chars(&toast.text, usize::from(body_width));

    let spans = vec![
        Span::styled(
            "\u{258c}".to_owned(),
            Style::default()
                .fg(accent_fg)
                .bg(theme.modeline_bg)
                .add_modifier(Modifier::BOLD)
                .add_modifier(DECORATION_MARKER),
        ),
        Span::styled(
            " ".to_owned(),
            Style::default()
                .bg(theme.modeline_bg)
                .add_modifier(DECORATION_MARKER),
        ),
        Span::styled(
            truncated,
            Style::default()
                .fg(body_fg)
                .bg(theme.modeline_bg)
                .add_modifier(DECORATION_MARKER),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
    // Reserve the trailing slot for an ellipsis-equivalent so the
    // user knows truncation happened.
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('\u{2026}');
    out
}

fn colors_for(theme: &Theme, kind: ToastKind) -> (ratatui::style::Color, ratatui::style::Color) {
    match kind {
        ToastKind::Info | ToastKind::Success => (theme.user_rule, theme.assistant_fg),
        ToastKind::Warning => (theme.tool_rule, theme.assistant_fg),
        ToastKind::Error => (theme.tool_error_fg, theme.assistant_fg),
    }
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
        let painted = render_into(40, 5, &[]);
        for line in painted.lines() {
            assert!(line.chars().all(|c| c == ' '), "got {line:?}");
        }
    }

    #[test]
    fn toast_paints_top_right_with_accent_glyph() {
        let painted = render_into(40, 5, &[Toast::info("hello")]);
        // Row 0 is the top margin (blank), row 1 has the toast.
        let rows: Vec<&str> = painted.lines().collect();
        assert!(
            rows[0].chars().all(|c| c == ' '),
            "row 0 should be margin, got {:?}",
            rows[0]
        );
        assert!(
            rows[1].contains('\u{258c}'),
            "row 1 should contain accent glyph, got {:?}",
            rows[1]
        );
        assert!(
            rows[1].contains("hello"),
            "row 1 should contain message, got {:?}",
            rows[1]
        );
    }

    #[test]
    fn newest_toast_paints_above_older_ones() {
        let painted = render_into(40, 5, &[Toast::info("older"), Toast::info("newer")]);
        let rows: Vec<&str> = painted.lines().collect();
        assert!(rows[1].contains("newer"));
        assert!(rows[2].contains("older"));
    }

    #[test]
    fn long_message_is_truncated_with_ellipsis() {
        let long = "a".repeat(200);
        let painted = render_into(40, 5, &[Toast::info(long)]);
        assert!(painted.contains('\u{2026}'));
    }

    #[test]
    fn skips_painting_when_buffer_too_narrow() {
        let painted = render_into(8, 5, &[Toast::info("hi")]);
        for line in painted.lines() {
            assert!(line.chars().all(|c| c == ' '), "got {line:?}");
        }
    }
}
