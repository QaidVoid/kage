//! Modeline, chrome, spinner, and input geometry.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Paint the bottom modeline. When the host has registered a
/// [`SessionUsage`] handle, the row shows the active model, the
/// running token totals (input / output) and the context-window
/// fill. Otherwise the row is filled with the modeline background
/// so the chrome reads as a coherent strip rather than an unstyled
/// terminal row. Mode is intentionally absent here - the colored
/// pill on the input border is the canonical mode display.
/// Map plugin-supplied [`kage_plugin::ChromeLine`]s onto ratatui
/// lines. `base` carries the row's default fg/bg; a span's `fg` / `bg`
/// overrides it when the string parses, and the attribute bits map to
/// terminal modifiers. An unparseable color is dropped so the span
/// inherits `base` rather than failing the whole row.
pub(crate) fn chrome_lines_to_ratatui(
    lines: &[kage_plugin::ChromeLine],
    base: Style,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|cl| {
            let spans: Vec<Span<'static>> = cl
                .spans
                .iter()
                .map(|sp| {
                    let mut style = base;
                    if let Some(c) = sp.fg.as_deref().and_then(parse_chrome_color) {
                        style = style.fg(c);
                    }
                    if let Some(c) = sp.bg.as_deref().and_then(parse_chrome_color) {
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

/// Resolve a plugin color string via ratatui's color grammar (named
/// colors such as `red`, `#rrggbb` hex, or an indexed number).
/// Unparseable input yields `None`.
fn parse_chrome_color(name: &str) -> Option<Color> {
    name.parse::<Color>().ok()
}

pub(super) fn render_modeline(
    frame: &mut Frame,
    regions: Regions,
    usage: Option<&SessionUsage>,
    plugin_footer: &[kage_plugin::ChromeLine],
) {
    let area = regions.status_bottom;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let bg = Style::default().bg(theme.modeline_bg);
    let fg = Style::default().fg(theme.modeline_fg).bg(theme.modeline_bg);
    if !plugin_footer.is_empty() {
        let lines = chrome_lines_to_ratatui(plugin_footer, fg);
        let paragraph = Paragraph::new(lines).alignment(Alignment::Left).style(bg);
        frame.render_widget(paragraph, area);
        return;
    }
    // Blended into the canvas (no band): a `DIM` separator would
    // vanish, so use the readable muted tier.
    let dim = Style::default().fg(theme.muted_fg).bg(theme.modeline_bg);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(u) = usage
        && (!u.model.is_empty() || u.total_tokens() > 0 || u.current_context > 0 || u.working)
    {
        spans.push(Span::styled(" ", bg));
        // Working spinner: a 10-frame braille ticker keyed off
        // wall-clock time so it animates without a frame counter
        // on the App. When idle, paint a single dim dot so the
        // strip width stays stable across transitions.
        if u.working {
            let frame = spinner_frame();
            spans.push(Span::styled(
                format!("{frame} "),
                fg.add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("  ", bg));
        }
        // Logical groups separated by a muted dot: model, context
        // fill, cumulative io (+ cost), thinking level. Each is
        // labelled so a field reads on its own; the dot only ever
        // appears between groups, never trailing.
        let mut prior_group = false;
        let sep = |spans: &mut Vec<Span<'static>>, prior: &mut bool| {
            if *prior {
                spans.push(Span::styled(" . ", dim));
            }
            *prior = true;
        };
        if !u.model.is_empty() {
            sep(&mut spans, &mut prior_group);
            spans.push(Span::styled(
                u.model.clone(),
                fg.add_modifier(Modifier::BOLD),
            ));
        }
        if u.context_window > 0 {
            sep(&mut spans, &mut prior_group);
            #[allow(clippy::cast_precision_loss)]
            let pct =
                (u.current_context as f64 / u.context_window as f64 * 100.0).clamp(0.0, 999.9);
            spans.push(Span::styled(
                format!(
                    "ctx {}/{} ({:.0}%)",
                    format_token_count(u.current_context),
                    format_token_count(u.context_window),
                    pct
                ),
                fg,
            ));
        } else if u.current_context > 0 {
            sep(&mut spans, &mut prior_group);
            spans.push(Span::styled(
                format!("ctx {}", format_token_count(u.current_context)),
                fg,
            ));
        }
        // Cumulative session totals (what the user has been charged
        // for since the session started), distinct from `ctx` above.
        // Cost rides in the same group as the io it paid for.
        sep(&mut spans, &mut prior_group);
        spans.push(Span::styled(
            format!(
                "io {}+{}",
                format_token_count(u.input_tokens),
                format_token_count(u.output_tokens)
            ),
            fg,
        ));
        if u.total_cost > 0.0 {
            spans.push(Span::styled(format!(" ${:.4}", u.total_cost), fg));
        }
        if let Some(level) = u.thinking_level
            && !level.is_off()
        {
            sep(&mut spans, &mut prior_group);
            spans.push(Span::styled(
                format!("think:{}", level.label()),
                fg.add_modifier(Modifier::BOLD),
            ));
        }
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = usize::from(area.width).saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg));
    }
    let line = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Left)
        .style(bg);
    frame.render_widget(line, area);
}

/// Format a token count compactly so the modeline stays narrow:
/// under 1k as raw digits, then `k` / `M` / `B` with adaptive
/// precision and trailing zeros trimmed (`21M`, not `21000k` or
/// `21.0M`; `78.7k`; `1.16k`; `1.5M`).
pub(crate) fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
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
    #[allow(clippy::cast_possible_truncation)]
    {
        ((now / 100) as usize) % SPINNER_FRAMES.len()
    }
}

fn spinner_frame() -> &'static str {
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

pub(crate) fn placeholder_for(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Insert => Some(INPUT_PLACEHOLDER_INSERT),
        Mode::Normal => Some(INPUT_PLACEHOLDER_NORMAL),
        Mode::Visual => None,
    }
}

/// How many rows to scroll the input Paragraph so that the cursor row
/// always stays inside the visible content area. Once the prompt has
/// more rows than the input area can fit ([`INPUT_CONTENT_MAX_LINES`]
/// from `layout.rs`), scrolling is the only way to keep typing
/// visible.
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
            let col = row_text.chars().count();
            return (
                u16::try_from(idx).unwrap_or(u16::MAX),
                u16::try_from(col).unwrap_or(u16::MAX),
            );
        }
    }
    let (last_start, last_end) = rows[rows.len() - 1];
    let last_chars = text[last_start..last_end].chars().count();
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
