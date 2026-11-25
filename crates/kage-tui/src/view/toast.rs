//! Compact floating notification cards and their pointer geometry.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::DECORATION_MARKER;
use crate::theme::Theme;
use crate::toast::{Toast, ToastKind};

const HISTORY_HINT: &str = "/notifications";
const MAX_BODY_LINES: usize = 4;

/// Conversation corner used to anchor a floating notification card.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToastPosition {
    /// Anchor to the conversation's upper-left corner.
    TopLeft,
    /// Anchor to the conversation's upper-right corner.
    TopRight,
    /// Anchor to the conversation's lower-left corner.
    BottomLeft,
    /// Anchor to the conversation's lower-right corner.
    #[default]
    BottomRight,
}

impl ToastPosition {
    /// Parse a corner name such as `bottom-right`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "top-left" => Some(Self::TopLeft),
            "top-right" => Some(Self::TopRight),
            "bottom-left" => Some(Self::BottomLeft),
            "bottom-right" => Some(Self::BottomRight),
            _ => None,
        }
    }
}

/// Painted card geometry for pointer hover and dismissal handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToastHit {
    /// Complete opaque card, including its padding and side borders.
    pub area: Rect,
    /// Clickable close glyph inside the card.
    pub dismiss: Rect,
    /// Identity of the active acknowledgement displayed by this card.
    pub toast_id: u64,
}

/// Paint the first live acknowledgement within the conversation.
///
/// Only one opaque card is painted. Its border shows how many live entries are
/// waiting behind it. Truncated messages point to notification history, which
/// retains the complete original message.
pub fn render_toasts(
    frame: &mut Frame,
    area: Rect,
    toasts: &[Toast],
    theme: &Theme,
    position: ToastPosition,
) -> Option<ToastHit> {
    let area = area.intersection(frame.area());
    if area.is_empty() {
        return None;
    }
    let now = std::time::Instant::now();
    let mut live = toasts.iter().filter(|toast| toast.is_live(now));
    let toast = live.next()?;
    let pending = live.count();
    let label = match toast.kind {
        ToastKind::Info => "\u{2022}",
        ToastKind::Success => "\u{2713}",
        ToastKind::Warning => "! Warning",
        ToastKind::Error => "! Error",
    };
    let badge = if pending == 0 {
        String::new()
    } else {
        format!(" +{pending} ")
    };
    let layout = card_layout(area, &normalize(&toast.text), label, &badge, position);
    let card = layout.area;
    let base = Style::default()
        .fg(theme.overlay_fg)
        .bg(theme.user_bg)
        .add_modifier(DECORATION_MARKER);
    clear_card(frame, card, base);
    let outline = base.fg(theme.muted_fg);
    let buf = frame.buffer_mut();
    let dismiss = Rect::new(
        card.right() - if layout.bordered { 3 } else { 1 },
        card.y,
        1,
        1,
    );
    if layout.bordered {
        for x in card.x + 1..card.right() - 1 {
            buf.set_stringn(x, card.y, "\u{2500}", 1, outline);
            buf.set_stringn(x, card.bottom() - 1, "\u{2500}", 1, outline);
        }
        for y in card.y + 1..card.bottom() - 1 {
            buf.set_stringn(card.x, y, "\u{2502}", 1, outline);
            buf.set_stringn(card.right() - 1, y, "\u{2502}", 1, outline);
        }
        for (x, y, glyph) in [
            (card.x, card.y, "\u{256d}"),
            (card.right() - 1, card.y, "\u{256e}"),
            (card.x, card.bottom() - 1, "\u{2570}"),
            (card.right() - 1, card.bottom() - 1, "\u{256f}"),
        ] {
            buf.set_stringn(x, y, glyph, 1, outline);
        }
        let title_x = card.x + 2;
        let title_width = usize::from(dismiss.x.saturating_sub(title_x + 1));
        let show_badge = !badge.is_empty() && title_width > label.width() + badge.width();
        let label_width = if show_badge {
            title_width - badge.width() - 1
        } else {
            title_width
        };
        buf.set_stringn(title_x, card.y, label, label_width, base);
        buf.set_stringn(
            title_x,
            card.y,
            &label[..label.chars().next().map_or(0, char::len_utf8)],
            label_width.min(1),
            base.fg(accent_for(theme, toast.kind)),
        );
        if show_badge {
            let x = dismiss.x - 1 - u16::try_from(badge.width()).unwrap_or(0);
            buf.set_stringn(x, card.y, &badge, badge.width(), outline);
        }
    }
    for (row, line) in layout.lines.iter().enumerate() {
        let y = layout.content.y + u16::try_from(row).unwrap_or(0);
        if y < layout.content.bottom() {
            buf.set_stringn(
                layout.content.x,
                y,
                line,
                usize::from(layout.content.width),
                base,
            );
        }
    }
    if layout.clipped {
        paint_history_hint(buf, &layout, base, theme.muted_fg);
    }
    buf.set_stringn(dismiss.x, dismiss.y, "x", 1, outline);
    Some(ToastHit {
        area: card,
        dismiss,
        toast_id: toast.id,
    })
}

struct CardLayout {
    area: Rect,
    content: Rect,
    bordered: bool,
    lines: Vec<String>,
    clipped: bool,
    footer: bool,
}

fn card_layout(
    area: Rect,
    text: &str,
    label: &str,
    badge: &str,
    position: ToastPosition,
) -> CardLayout {
    let inset_x = if area.width >= 16 { 2 } else { 0 };
    let inset_y = u16::from(area.height >= 5);
    let available_width = area.width.saturating_sub(2 * inset_x).min(60);
    let available_height = area.height.saturating_sub(2 * inset_y);
    let natural = text.lines().map(UnicodeWidthStr::width).max().unwrap_or(0);
    let header_width = label.width()
        + 6
        + if badge.is_empty() {
            0
        } else {
            badge.width() + 1
        };
    let width = u16::try_from(natural.saturating_add(4).max(header_width).clamp(12, 60))
        .unwrap_or(60)
        .min(available_width);
    let bordered = width >= 7 && available_height >= 3;
    let border = u16::from(bordered);
    let pad_x = u16::from(bordered);
    let content_width = width.saturating_sub(2 * (border + pad_x));
    let content_height = available_height.saturating_sub(2 * border + u16::from(!bordered));
    let capacity = usize::from(content_height).clamp(1, MAX_BODY_LINES);
    let (mut lines, clipped) = wrap_body(text, usize::from(content_width), capacity);
    let footer = clipped && content_height > 1;
    if footer && lines.len() >= usize::from(content_height) {
        lines.pop();
    }
    let height = (2 * border
        + u16::from(!bordered)
        + u16::try_from(lines.len()).unwrap_or(0)
        + u16::from(footer))
    .min(available_height)
    .max(1);
    let x = match position {
        ToastPosition::TopLeft | ToastPosition::BottomLeft => area.x + inset_x,
        ToastPosition::TopRight | ToastPosition::BottomRight => area.right() - inset_x - width,
    };
    let y = match position {
        ToastPosition::TopLeft | ToastPosition::TopRight => area.y + inset_y,
        ToastPosition::BottomLeft | ToastPosition::BottomRight => area.bottom() - inset_y - height,
    };
    let content_y = y + 1;
    CardLayout {
        area: Rect::new(x, y, width, height),
        content: Rect::new(
            x + border + pad_x,
            content_y,
            content_width,
            height.saturating_sub(border + 1),
        ),
        bordered,
        lines,
        clipped,
        footer,
    }
}

fn paint_history_hint(
    buf: &mut ratatui::buffer::Buffer,
    layout: &CardLayout,
    base: Style,
    color: Color,
) {
    let body_width = usize::from(layout.content.width);
    let hint = if body_width >= 18 {
        "... /notifications"
    } else if body_width >= HISTORY_HINT.len() {
        HISTORY_HINT
    } else {
        "..."
    };
    let row = layout.content.y
        + if layout.footer {
            u16::try_from(layout.lines.len()).unwrap_or(0)
        } else {
            0
        };
    if row >= layout.content.bottom() {
        return;
    }
    let x = layout.content.x;
    buf.set_stringn(x, row, " ".repeat(body_width), body_width, base);
    if !layout.footer && body_width > hint.width() + 1 {
        let preview = fit_graphemes(&layout.lines[0], body_width - hint.width() - 1);
        buf.set_stringn(x, row, &preview, body_width, base);
        let hint_x = x + u16::try_from(preview.width() + 1).unwrap_or(0);
        buf.set_stringn(hint_x, row, hint, hint.width(), base.fg(color));
    } else {
        buf.set_stringn(x, row, hint, body_width, base.fg(color));
    }
}

fn clear_card(frame: &mut Frame, area: Rect, style: Style) {
    let buf = frame.buffer_mut();
    for y in area.y..area.bottom() {
        if area.x > buf.area.x && buf[(area.x - 1, y)].symbol().width() > 1 {
            // A crossing wide glyph would make the terminal diff skip the border.
            buf[(area.x - 1, y)].set_symbol(" ");
        }
        for x in area.x..area.right() {
            let cell = &mut buf[(x, y)];
            cell.reset();
            cell.set_style(style);
        }
    }
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => '\n',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect()
}

fn fit_graphemes(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    let span = Span::raw(text);
    for grapheme in span.styled_graphemes(Style::default()) {
        let size = grapheme.symbol.width();
        if used + size > width {
            break;
        }
        result.push_str(grapheme.symbol);
        used += size;
    }
    result
}

fn wrap_body(text: &str, width: usize, limit: usize) -> (Vec<String>, bool) {
    if width == 0 {
        return (vec![String::new()], !text.is_empty());
    }
    let mut lines = Vec::new();
    let mut replaced = false;
    for paragraph in text.split('\n') {
        if lines.len() >= limit {
            return (lines, true);
        }
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split_whitespace() {
            let word_width = word.width();
            if used > 0 {
                if used + 1 + word_width <= width {
                    line.push(' ');
                    used += 1;
                } else {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                    if lines.len() == limit {
                        return (lines, true);
                    }
                }
            }
            let span = Span::raw(word);
            for grapheme in span.styled_graphemes(Style::default()) {
                let mut symbol = grapheme.symbol;
                let mut size = symbol.width();
                if size > width {
                    symbol = "?";
                    size = 1;
                    replaced = true;
                }
                if used + size > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                    if lines.len() == limit {
                        return (lines, true);
                    }
                }
                line.push_str(symbol);
                used += size;
            }
        }
        if lines.len() == limit {
            return (lines, true);
        }
        lines.push(line);
    }
    (lines, replaced)
}

fn accent_for(theme: &Theme, kind: ToastKind) -> Color {
    match kind {
        ToastKind::Info => theme.user_rule,
        ToastKind::Success => theme.success_fg,
        ToastKind::Warning => theme.warning_fg,
        ToastKind::Error => theme.tool_error_fg,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::toast::DEFAULT_TOAST_DURATION;

    fn render(area: Rect, toasts: &[Toast], position: ToastPosition) -> (Buffer, Option<ToastHit>) {
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        let mut hit = None;
        terminal
            .draw(|frame| {
                let bounds = frame.area();
                let buf = frame.buffer_mut();
                for y in bounds.y..bounds.bottom() {
                    for x in bounds.x..bounds.right() {
                        buf[(x, y)]
                            .set_symbol("z")
                            .set_style(Style::default().fg(Color::Red).bg(Color::Blue));
                    }
                }
                hit = render_toasts(frame, area, toasts, &Theme::default(), position);
            })
            .unwrap();
        (terminal.backend().buffer().clone(), hit)
    }

    fn text(buf: &Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn toast(kind: ToastKind, text: impl Into<String>) -> Toast {
        Toast::with_kind(text, kind, DEFAULT_TOAST_DURATION)
    }

    #[test]
    fn parses_all_corners_and_defaults_to_bottom_right() {
        assert_eq!(ToastPosition::default(), ToastPosition::BottomRight);
        for (name, position) in [
            ("top-left", ToastPosition::TopLeft),
            ("top-right", ToastPosition::TopRight),
            ("bottom-left", ToastPosition::BottomLeft),
            ("bottom-right", ToastPosition::BottomRight),
        ] {
            assert_eq!(ToastPosition::parse(name), Some(position));
        }
        assert_eq!(ToastPosition::parse("center"), None);
    }

    #[test]
    fn corners_anchor_inside_conversation_with_nonzero_origins() {
        for area in [Rect::new(0, 0, 80, 24), Rect::new(7, 3, 65, 20)] {
            for position in [
                ToastPosition::TopLeft,
                ToastPosition::TopRight,
                ToastPosition::BottomLeft,
                ToastPosition::BottomRight,
            ] {
                let (_, hit) = render(area, &[Toast::info("Saved")], position);
                let hit = hit.unwrap();
                assert_eq!(hit.area.intersection(area), hit.area);
                assert_eq!(hit.dismiss.intersection(hit.area), hit.dismiss);
                assert!(hit.area.width <= 60);
                match position {
                    ToastPosition::TopLeft | ToastPosition::BottomLeft => {
                        assert_eq!(hit.area.x, area.x + 2);
                    }
                    ToastPosition::TopRight | ToastPosition::BottomRight => {
                        assert_eq!(hit.area.right(), area.right() - 2);
                    }
                }
                match position {
                    ToastPosition::TopLeft | ToastPosition::TopRight => {
                        assert_eq!(hit.area.y, area.y + 1);
                    }
                    ToastPosition::BottomLeft | ToastPosition::BottomRight => {
                        assert_eq!(hit.area.bottom(), area.bottom() - 1);
                    }
                }
            }
        }
    }

    #[test]
    fn zero_and_tiny_areas_are_bounded() {
        for width in 0..18 {
            for height in 0..10 {
                for position in [
                    ToastPosition::TopLeft,
                    ToastPosition::TopRight,
                    ToastPosition::BottomLeft,
                    ToastPosition::BottomRight,
                ] {
                    let area = Rect::new(3, 4, width, height);
                    let (_, hit) = render(area, &[toast(ToastKind::Warning, "hello")], position);
                    if width == 0 || height == 0 {
                        assert_eq!(hit, None);
                    } else {
                        let hit = hit.unwrap();
                        assert_eq!(hit.area.intersection(area), hit.area);
                        assert_eq!(hit.dismiss.intersection(hit.area), hit.dismiss);
                    }
                }
            }
        }
    }

    #[test]
    fn clips_oversized_conversation_to_frame() {
        let (_, hit) = render(
            Rect::new(88, 28, 30, 30),
            &[Toast::info("Saved")],
            ToastPosition::BottomRight,
        );
        let hit = hit.unwrap();
        assert_eq!(hit.area.intersection(Rect::new(0, 0, 90, 30)), hit.area);
    }

    #[test]
    fn paints_only_first_live_card_with_pending_count() {
        let area = Rect::new(0, 0, 80, 24);
        let error = toast(ToastKind::Error, "first failure");
        let newer_error = toast(ToastKind::Error, "latest failure");
        let expected_id = error.id;
        let (buf, hit) = render(
            area,
            &[error, newer_error, Toast::info("newer info")],
            ToastPosition::BottomRight,
        );
        let hit = hit.unwrap();
        assert_eq!(hit.toast_id, expected_id);
        let painted = text(&buf, hit.area);
        assert!(painted.contains("Error"), "{painted}");
        assert!(painted.contains("first failure"), "{painted}");
        assert!(painted.contains("+2"), "{painted}");
        assert!(!painted.contains("latest failure"));
        assert!(!painted.contains("newer info"));
        assert_eq!(buf[(0, 0)].symbol(), "z");
    }

    #[test]
    fn expired_and_empty_queues_paint_nothing() {
        let area = Rect::new(0, 0, 80, 24);
        let expired = Toast::with_kind("expired", ToastKind::Error, std::time::Duration::ZERO);
        for toasts in [&[][..], &[expired][..]] {
            let (buf, hit) = render(area, toasts, ToastPosition::BottomRight);
            assert_eq!(hit, None);
            assert_eq!(buf[(79, 23)].symbol(), "z");
        }
        let expired = Toast::with_kind("expired", ToastKind::Error, std::time::Duration::ZERO);
        let (buf, hit) = render(
            area,
            &[expired, Toast::info("live")],
            ToastPosition::BottomRight,
        );
        assert!(text(&buf, hit.unwrap().area).contains("live"));
    }

    #[test]
    fn routine_cards_have_no_severity_prefix_or_history_link() {
        for kind in [ToastKind::Info, ToastKind::Success] {
            let (buf, hit) = render(
                Rect::new(0, 0, 80, 24),
                &[toast(kind, "Saved successfully")],
                ToastPosition::BottomRight,
            );
            let hit = hit.unwrap();
            let painted = text(&buf, hit.area);
            assert!(painted.contains("Saved successfully"), "{painted}");
            assert!(!painted.contains("Info"));
            assert!(!painted.contains("Success:"));
            assert!(!painted.contains(HISTORY_HINT));
            assert!(hit.area.width < 60);
        }
    }

    #[test]
    fn long_unbroken_diagnostics_have_bounded_height_and_history_marker() {
        let (buf, hit) = render(
            Rect::new(0, 0, 80, 24),
            &[toast(ToastKind::Error, "diagnostic".repeat(100))],
            ToastPosition::BottomRight,
        );
        let hit = hit.unwrap();
        assert_eq!(hit.area.height, 7);
        assert!(hit.area.width <= 60);
        let painted = text(&buf, hit.area);
        assert!(painted.contains("... /notifications"), "{painted}");
    }

    #[test]
    fn wraps_words_and_preserves_multiline_text() {
        assert_eq!(
            wrap_body("alpha beta gamma", 10, 4),
            (vec!["alpha beta".into(), "gamma".into()], false)
        );
        assert_eq!(
            wrap_body("first\nsecond\nthird", 10, 4),
            (vec!["first".into(), "second".into(), "third".into()], false)
        );
        assert_eq!(wrap_body("one\ntwo", 10, 1), (vec!["one".into()], true));
        assert_eq!(
            normalize("first\tword\r\nnext\rline\u{1b}\u{7}"),
            "first word\nnext\nline  "
        );
    }

    #[test]
    fn wrapping_preserves_cjk_combining_marks_and_emoji_graphemes() {
        for (message, width) in [
            ("\u{4f60}\u{597d}\u{4e16}\u{754c}", 4),
            ("e\u{301}e\u{301}e\u{301}", 2),
            ("\u{1f469}\u{200d}\u{1f4bb}\u{1f469}\u{200d}\u{1f4bb}", 2),
            ("\u{1f1fa}\u{1f1f8}\u{1f1fa}\u{1f1f8}", 2),
        ] {
            let (lines, clipped) = wrap_body(message, width, 4);
            assert!(!clipped, "{lines:?}");
            assert_eq!(lines.concat(), message);
            assert!(lines.iter().all(|line| line.width() <= width));
            assert!(lines.iter().all(|line| !line.starts_with('\u{301}')));
        }
        let emoji = "\u{1f469}\u{200d}\u{1f4bb}";
        assert_eq!(wrap_body(emoji, 1, 4), (vec!["?".into()], true));
    }

    #[test]
    fn rendered_unicode_and_controls_preserve_graphemes_inside_the_outline() {
        let message =
            "\u{4f60}\u{597d} e\u{301} \u{1f469}\u{200d}\u{1f4bb}\tready\r\nnext\u{1b}\u{7}";
        let (buf, hit) = render(
            Rect::new(0, 0, 80, 24),
            &[Toast::info(message)],
            ToastPosition::BottomRight,
        );
        let hit = hit.unwrap();
        let painted = text(&buf, hit.area);
        for grapheme in [
            "\u{4f60}",
            "\u{597d}",
            "e\u{301}",
            "\u{1f469}\u{200d}\u{1f4bb}",
        ] {
            assert!(painted.contains(grapheme), "{painted}");
        }
        assert!(painted.contains("ready"), "{painted}");
        assert!(painted.contains("next"), "{painted}");
        assert!(!painted.contains(['\t', '\r', '\u{1b}', '\u{7}']));
        assert_eq!(hit.area.height, 4);
        assert!(!painted.contains(HISTORY_HINT));
    }

    #[test]
    fn card_is_opaque_and_all_decoration_cells_have_neutral_background() {
        let area = Rect::new(7, 3, 65, 20);
        let theme = Theme::default();
        for kind in [
            ToastKind::Info,
            ToastKind::Success,
            ToastKind::Warning,
            ToastKind::Error,
        ] {
            let (buf, hit) = render(area, &[toast(kind, "body")], ToastPosition::BottomRight);
            let hit = hit.unwrap();
            for y in 0..30 {
                for x in 0..90 {
                    let cell = &buf[(x, y)];
                    if hit.area.contains((x, y).into()) {
                        assert_ne!(cell.symbol(), "z");
                        assert_eq!(cell.bg, theme.user_bg);
                        assert!(cell.modifier.contains(DECORATION_MARKER));
                    } else {
                        assert_eq!(cell.symbol(), "z");
                        assert_eq!(cell.bg, Color::Blue);
                    }
                }
            }
            assert_eq!(buf[(hit.area.x, hit.area.y)].fg, theme.muted_fg);
            assert_eq!(
                buf[(hit.area.x + 2, hit.area.y)].fg,
                accent_for(&theme, kind)
            );
            assert_eq!(buf[(hit.dismiss.x, hit.dismiss.y)].symbol(), "x");
            let body_cell = (hit.area.y..hit.area.bottom())
                .flat_map(|y| (hit.area.x..hit.area.right()).map(move |x| (x, y)))
                .find(|&(x, y)| buf[(x, y)].symbol() == "b")
                .unwrap();
            assert_eq!(buf[body_cell].fg, theme.overlay_fg);
        }
    }

    #[test]
    fn outlined_cards_are_compact_with_border_controls_and_readable_severity() {
        for (kind, label) in [
            (ToastKind::Info, "\u{2022}"),
            (ToastKind::Success, "\u{2713}"),
            (ToastKind::Warning, "! Warning"),
            (ToastKind::Error, "! Error"),
        ] {
            let (buf, hit) = render(
                Rect::new(0, 0, 80, 24),
                &[toast(kind, "Saved")],
                ToastPosition::BottomRight,
            );
            let hit = hit.unwrap();
            assert_eq!(hit.area.height, 3);
            assert_eq!(hit.dismiss.y, hit.area.y);
            let top = text(
                &buf,
                Rect {
                    height: 1,
                    ..hit.area
                },
            );
            assert!(top.starts_with('\u{256d}'), "{top}");
            assert!(top.ends_with('\u{256e}'), "{top}");
            assert!(top.contains(label), "{top}");
            let body = text(
                &buf,
                Rect {
                    y: hit.area.y + 1,
                    height: 1,
                    ..hit.area
                },
            );
            assert!(body.starts_with("\u{2502} Saved"), "{body}");
            assert!(body.ends_with(" \u{2502}"), "{body}");
            assert!(!text(&buf, hit.area).contains(['\u{258c}', '\u{2590}']));
        }
    }

    #[test]
    fn pending_badge_never_overlaps_body_or_close_at_any_width() {
        for width in 1..=80 {
            let (buf, hit) = render(
                Rect::new(0, 0, width, 12),
                &[
                    toast(ToastKind::Warning, "body"),
                    Toast::info("later"),
                    Toast::info("last"),
                ],
                ToastPosition::TopLeft,
            );
            let hit = hit.unwrap();
            assert_eq!(buf[(hit.dismiss.x, hit.dismiss.y)].symbol(), "x");
            if hit.area.width >= 20 {
                let top = text(
                    &buf,
                    Rect {
                        height: 1,
                        ..hit.area
                    },
                );
                assert!(top.contains("! Warning"), "{top}");
                assert!(top.contains("+2"), "{top}");
            }
            if hit.area.height > 1 {
                let body = text(
                    &buf,
                    Rect {
                        y: hit.area.y + 1,
                        height: hit.area.height - 1,
                        ..hit.area
                    },
                );
                assert!(!body.contains("+2"), "{body}");
                assert!(!body.contains('x'), "{body}");
            }
        }
    }

    #[test]
    fn older_info_is_not_preempted_by_newer_errors() {
        let first = Toast::info("first");
        let id = first.id;
        let (buf, hit) = render(
            Rect::new(0, 0, 80, 24),
            &[first, toast(ToastKind::Error, "later failure")],
            ToastPosition::BottomRight,
        );
        let hit = hit.unwrap();
        assert_eq!(hit.toast_id, id);
        assert!(text(&buf, hit.area).contains("first"));
        assert!(!text(&buf, hit.area).contains("Error"));
    }

    fn paint_underlay(frame: &mut Frame) {
        let area = frame.area();
        for y in area.y..area.bottom() {
            frame.buffer_mut().set_stringn(
                area.x,
                y,
                "z".repeat(usize::from(area.width)),
                usize::from(area.width),
                Style::default().fg(Color::Red).bg(Color::Blue),
            );
        }
    }

    #[test]
    fn consecutive_frames_replace_and_remove_cards_without_residue() {
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        let area = Rect::new(7, 3, 65, 20);
        for position in [
            ToastPosition::TopLeft,
            ToastPosition::TopRight,
            ToastPosition::BottomLeft,
            ToastPosition::BottomRight,
        ] {
            for toasts in [
                vec![toast(ToastKind::Error, "long diagnostic ".repeat(100))],
                vec![Toast::info(
                    "\u{4f60}\u{597d} e\u{301} \u{1f469}\u{200d}\u{1f4bb}",
                )],
                vec![Toast::info("Saved")],
                Vec::new(),
            ] {
                let expected = terminal
                    .draw(|frame| {
                        paint_underlay(frame);
                        render_toasts(frame, area, &toasts, &Theme::default(), position);
                    })
                    .unwrap()
                    .buffer
                    .clone();
                let actual = terminal.backend().buffer();
                for y in expected.area.y..expected.area.bottom() {
                    let mut x = expected.area.x;
                    while x < expected.area.right() {
                        assert_eq!(actual[(x, y)], expected[(x, y)], "cell ({x}, {y})");
                        x += u16::try_from(expected[(x, y)].symbol().width().max(1)).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn underlying_wide_glyph_cannot_hide_the_left_border_on_consecutive_frames() {
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        let area = Rect::new(7, 3, 65, 20);
        let toasts = [Toast::info("Saved")];
        let layout = card_layout(area, "Saved", "\u{2022}", "", ToastPosition::BottomRight);
        let glyph_x = layout.area.x - 1;
        for show in [false, true, true, false] {
            let mut hit = None;
            terminal
                .draw(|frame| {
                    paint_underlay(frame);
                    for y in layout.area.y..layout.area.bottom() {
                        frame
                            .buffer_mut()
                            .set_stringn(glyph_x, y, "\u{754c}", 2, Style::default());
                    }
                    if show {
                        hit = render_toasts(
                            frame,
                            area,
                            &toasts,
                            &Theme::default(),
                            ToastPosition::BottomRight,
                        );
                    }
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            if let Some(hit) = hit {
                assert_eq!(buf[(hit.area.x, hit.area.y)].symbol(), "\u{256d}");
                assert_eq!(buf[(hit.area.x, hit.area.y + 1)].symbol(), "\u{2502}");
            } else {
                assert_eq!(buf[(glyph_x, layout.area.y)].symbol(), "\u{754c}");
            }
        }
    }
}
