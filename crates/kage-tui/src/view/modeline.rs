//! Modeline, chrome, spinner, and input geometry.

use super::*;

use crate::theme::Slot;

/// Map plugin-supplied [`kage_plugin::ChromeLine`]s onto ratatui
/// lines against the active theme. `base` carries the row's default
/// fg/bg. A span's `hl` group patches it first, then its `fg` / `bg`
/// override it when the string resolves (see
/// [`crate::theme::Theme::span_color`]), and the attribute bits map to
/// terminal modifiers. An unresolvable color is dropped so the span
/// inherits rather than failing the whole row.
pub(crate) fn chrome_lines_to_ratatui(
    lines: &[kage_plugin::ChromeLine],
    base: Style,
) -> Vec<Line<'static>> {
    let theme = crate::theme::current();
    lines
        .iter()
        .map(|cl| {
            let spans: Vec<Span<'static>> = cl
                .spans
                .iter()
                .map(|sp| {
                    let mut style = base;
                    if let Some(group) = sp.hl.as_deref() {
                        style = style.patch(theme.group_style(group));
                    }
                    let color = |name: Option<&str>, slot| {
                        name.and_then(|name| theme.span_color(name, slot))
                    };
                    if let Some(c) = color(sp.fg.as_deref(), Slot::Fg) {
                        style = style.fg(c);
                    }
                    if let Some(c) = color(sp.bg.as_deref(), Slot::Bg) {
                        style = style.bg(c);
                    }
                    let a = sp.attrs;
                    if a.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if a.dim() {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if a.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if a.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    Span::styled(sp.text.clone(), style)
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// Lowercase label for a permission override pill.
pub(super) fn mode_label(mode: kage_core::permissions::PermissionAction) -> &'static str {
    match mode {
        kage_core::permissions::PermissionAction::Allow => "allow",
        kage_core::permissions::PermissionAction::Ask => "ask",
        kage_core::permissions::PermissionAction::Deny => "deny",
    }
}

/// Format a token count compactly so the modeline stays narrow:
/// under 1k as raw digits, then `k` / `M` / `B` with adaptive
/// precision and trailing zeros trimmed (`21M`, not `21000k` or
/// `21.0M`; `78.7k`; `1.16k`; `1.5M`).
pub(crate) fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    #[expect(clippy::cast_precision_loss, reason = "a count shown to one decimal")]
    let (value, suffix) = if n < 1_000_000 {
        (n as f64 / 1_000.0, 'k')
    } else if n < 1_000_000_000 {
        (n as f64 / 1_000_000.0, 'M')
    } else {
        (n as f64 / 1_000_000_000.0, 'B')
    };
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let mut s = format!("{value:.decimals$}");
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        s.truncate(trimmed.len());
    }
    s.push(suffix);
    s
}

/// Pick a braille spinner glyph keyed off wall-clock time so the
/// modeline ticks while the agent is working without us having to
/// thread a frame counter through `App::draw`. Cycle period ~= 1
/// second (10 frames at 100 ms each).
const SPINNER_FRAMES: &[&str] = &[
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

/// Index into the spinner frame table for the current wall-clock
/// instant. The frame advances on a 100ms cadence. The event loop
/// reads this to repaint only when the glyph actually moves instead of
/// once per wake, so a static buffer during a long tool call does not
/// cost a full redraw every poll interval.
pub(crate) fn spinner_frame_index() -> usize {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    ((now / 100) as usize) % SPINNER_FRAMES.len()
}

pub(crate) fn spinner_frame() -> &'static str {
    SPINNER_FRAMES[spinner_frame_index()]
}

pub(crate) fn mode_border_color(theme: &crate::theme::Theme, mode: Mode) -> Color {
    match mode {
        Mode::Normal => theme.input_border_normal,
        Mode::Insert => theme.input_border_insert,
        Mode::Visual => theme.input_border_visual,
    }
}

pub(crate) fn mode_pill_style(theme: &crate::theme::Theme, mode: Mode) -> Style {
    let fg = match mode {
        Mode::Normal => theme.input_pill_normal_fg,
        Mode::Insert => theme.input_pill_insert_fg,
        Mode::Visual => theme.input_pill_visual_fg,
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

/// Total visual rows the input text occupies inside `body_width`,
/// counting wrapped continuation rows. Empty logical lines still
/// count for one row each (so a trailing newline grows the input).
#[must_use]
pub fn input_visual_row_count(text: &str, body_width: u16) -> u16 {
    u16::try_from(wrap_input_rows(text, body_width).len()).unwrap_or(u16::MAX)
}

/// Visual `(row, col)` of the cursor in the wrapped layout. Walks
/// the same wrap plan [`build_input_body_lines`] paints so the
/// cursor lands on the row and column that match what's on screen,
/// regardless of whether the row break was a soft (word) or hard
/// (mid-character) cut.
pub(crate) fn input_visual_cursor(text: &str, cursor: usize, body_width: u16) -> (u16, u16) {
    let rows = wrap_input_rows(text, body_width);
    if rows.is_empty() {
        return (0, 0);
    }
    let cursor = cursor.min(text.len());
    for (idx, (start, end)) in rows.iter().enumerate() {
        if cursor <= *end {
            let row_text = text.get(*start..cursor).unwrap_or("");
            let col = row_text.width();
            return (
                u16::try_from(idx).unwrap_or(u16::MAX),
                u16::try_from(col).unwrap_or(u16::MAX),
            );
        }
    }
    let (last_start, last_end) = rows[rows.len() - 1];
    let last_chars = text[last_start..last_end].width();
    (
        u16::try_from(rows.len() - 1).unwrap_or(u16::MAX),
        u16::try_from(last_chars).unwrap_or(u16::MAX),
    )
}

/// How many rows to scroll the input Paragraph so the cursor's
/// visual row stays inside `body_area`. Wrap-aware: a long single
/// logical line that wraps to many visual rows scrolls correctly.
pub(crate) fn input_scroll_offset(input: &InputState, body_area: ratatui::layout::Rect) -> u16 {
    if body_area.height == 0 || body_area.width == 0 {
        return 0;
    }
    let (cursor_row, _) = input_visual_cursor(input.text(), input.cursor(), body_area.width);
    let max_visible_row = body_area.height.saturating_sub(1);
    cursor_row.saturating_sub(max_visible_row)
}

/// Compute the screen position of the prompt cursor inside the input
/// body area. Returns `None` if `body_area` is empty. Wrap-aware:
/// the visual `(row, col)` mirrors what `Paragraph::wrap` paints,
/// so a long line that wraps places the cursor on the right wrapped
/// row instead of clamping to the right edge of row 0.
pub(crate) fn input_cursor_position(
    input: &InputState,
    body_area: ratatui::layout::Rect,
    scroll_off: u16,
) -> Option<(u16, u16)> {
    if body_area.height == 0 || body_area.width == 0 {
        return None;
    }
    let max_x = body_area.x + body_area.width - 1;
    let max_y = body_area.y + body_area.height - 1;
    let (row, col) = input_visual_cursor(input.text(), input.cursor(), body_area.width);
    let row_offset = row.saturating_sub(scroll_off);
    let cx = body_area.x.saturating_add(col).min(max_x);
    let cy = body_area.y.saturating_add(row_offset).min(max_y);
    Some((cx, cy))
}

#[cfg(test)]
mod tests {
    use kage_plugin::{ChromeLine, ChromeSpan};

    use super::*;
    use crate::theme::{self, Theme};

    fn span(fg: &str) -> ChromeLine {
        ChromeLine {
            spans: vec![ChromeSpan {
                text: "x".into(),
                fg: Some(fg.into()),
                ..ChromeSpan::default()
            }],
        }
    }

    #[test]
    fn span_color_resolves_a_theme_role_then_ratatui_grammar() {
        theme::set_current(Theme {
            muted_fg: Color::Rgb(1, 2, 3),
            ..Theme::default()
        });
        let lines = chrome_lines_to_ratatui(
            &[span("muted_fg"), span("red"), span("nope")],
            Style::default(),
        );
        theme::reset_current_for_tests();
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[0].style.fg, None);
    }

    #[test]
    fn span_hl_and_group_colors_resolve_at_paint_time() {
        use kage_core::highlight::HlSpec;
        use kage_plugin::ChromeAttrs;

        let mut hl = theme::groups_for("default", None)
            .unwrap()
            .into_highlights("default");
        hl.set(
            "Loud",
            HlSpec {
                bg: Some("#0a0b0c".into()),
                italic: true,
                underline: true,
                ..HlSpec::default()
            },
        )
        .unwrap();
        theme::set_current(Theme::from_groups(&hl));
        let muted = theme::current().muted_fg;
        let bubble = theme::current().user_bg;
        let mut bold = ChromeAttrs::empty();
        bold.insert(ChromeAttrs::BOLD);
        let spans = [
            ChromeSpan {
                hl: Some("KageMuted".into()),
                attrs: bold,
                ..ChromeSpan::default()
            },
            ChromeSpan {
                hl: Some("Loud".into()),
                fg: Some("KageMuted".into()),
                ..ChromeSpan::default()
            },
            ChromeSpan {
                fg: Some("Loud".into()),
                bg: Some("KageUserBubble".into()),
                ..ChromeSpan::default()
            },
        ];
        let lines = chrome_lines_to_ratatui(
            &[ChromeLine {
                spans: spans.to_vec(),
            }],
            Style::default().fg(Color::White),
        );
        theme::reset_current_for_tests();
        let [a, b, c] = [0, 1, 2].map(|i| lines[0].spans[i].style);
        assert_eq!(a.fg, Some(muted));
        assert!(a.add_modifier.contains(Modifier::BOLD));
        assert_eq!(b.fg, Some(muted));
        assert_eq!(b.bg, Some(Color::Rgb(10, 11, 12)));
        assert!(
            b.add_modifier
                .contains(Modifier::ITALIC | Modifier::UNDERLINED)
        );
        assert_eq!(c.fg, Some(Color::White));
        assert_eq!(c.bg, Some(bubble));
    }
}
