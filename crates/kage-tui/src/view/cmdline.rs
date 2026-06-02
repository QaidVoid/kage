//! Cmdline/search popup and cursor rendering.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Same as [`place_cmdline_cursor`] but for the `/` search line.
pub(super) fn place_search_cursor(frame: &mut Frame, regions: Regions, line: &CommandLine) {
    place_cmdline_cursor(frame, regions, line);
}

/// Walk every `Line` in `lines` and split spans whose text contains
/// `pattern` (ASCII case-insensitive) into pre-match / match /
/// post-match chunks, applying a high-contrast yellow highlight to
/// the matches. Allocates nothing per call beyond the rebuilt span
/// vector; matters because this runs per frame across every block.
pub(crate) fn highlight_matches_in_lines(lines: &mut [Line<'static>], pattern: &str) {
    let needle = pattern.trim();
    if needle.is_empty() {
        return;
    }
    for line in lines {
        // Alloc-free pre-check: most on-screen lines during a search
        // contain no match, so leave them completely untouched rather
        // than take + rebuild + reallocate their span vec every frame.
        if !line
            .spans
            .iter()
            .any(|s| ascii_ifind(&s.content, needle, 0).is_some())
        {
            continue;
        }
        let original = std::mem::take(&mut line.spans);
        let mut rebuilt: Vec<Span<'static>> = Vec::with_capacity(original.len() + 2);
        for span in original {
            if ascii_ifind(&span.content, needle, 0).is_some() {
                rebuilt.extend(split_span_for_match(span, needle));
            } else {
                // No match in this span: move it through untouched
                // (no per-span `vec![span]` allocation).
                rebuilt.push(span);
            }
        }
        line.spans = rebuilt;
    }
}

/// Find the byte position of `needle` inside `haystack` ignoring
/// ASCII case, starting at `from`. No allocation. Returns absolute
/// byte position into `haystack`.
fn ascii_ifind(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || from >= h.len() || h.len() - from < n.len() {
        return None;
    }
    let limit = h.len() - n.len();
    'outer: for i in from..=limit {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

fn split_span_for_match(span: Span<'static>, needle: &str) -> Vec<Span<'static>> {
    if ascii_ifind(&span.content, needle, 0).is_none() {
        return vec![span];
    }
    let hit = span.style.patch(
        Style::default()
            .bg(crate::theme::current().match_color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED),
    );
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0;
    while let Some(abs) = ascii_ifind(&span.content, needle, cursor) {
        if abs > cursor {
            out.push(Span::styled(
                span.content[cursor..abs].to_owned(),
                span.style,
            ));
        }
        let end = abs + needle.len();
        out.push(Span::styled(span.content[abs..end].to_owned(), hit));
        if end <= cursor {
            // Defensive: needle was zero-width or boundary fudged;
            // bail to avoid an infinite loop.
            break;
        }
        cursor = end;
    }
    if cursor < span.content.len() {
        out.push(Span::styled(span.content[cursor..].to_owned(), span.style));
    }
    if out.is_empty() {
        return vec![span];
    }
    out
}

/// Maximum number of completion rows painted in the popup. Anything
/// beyond this is summarized as "+ N more" on the last row.
const POPUP_MAX_VISIBLE: usize = 8;

/// Reuses the slash palette's blue accent so the popup visually
/// reads as the same surface; selected rows use white-on-blue.
/// Background is [`Theme::modeline_bg`] (dark navy) so the popup is
/// distinct from the status row's [`Theme::status_bg`], rather than
/// merging into a single dark-gray block.
fn popup_styles() -> (Style, Style, Style) {
    let theme = crate::theme::current();
    let bg = theme.modeline_bg;
    let row = Style::default().fg(Color::White).bg(bg);
    let sel = Style::default()
        .fg(Color::White)
        .bg(Color::Blue)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.status_dim_fg).bg(bg);
    (row, sel, dim)
}

/// Paint an inline validation error below the cmdline status row.
/// Shown when the host set [`CommandLine::set_error`] after a failed
/// submit attempt. The error is rendered in the tool-error foreground
/// colour so it is visually distinct from the completion popup. The
/// popup is suppressed while an error is visible so the user focuses
/// on fixing the input.
pub(super) fn render_cmdline_error(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    let Some(err) = cmdline.error() else {
        return;
    };
    let theme = crate::theme::current();
    let bg = theme.modeline_bg;
    let fg = theme.tool_error_fg;
    let style = Style::default().fg(fg).bg(bg);

    let y = regions.status.y.saturating_add(1);
    if y >= regions.buffer.y.saturating_add(regions.buffer.height) {
        return;
    }
    let width = regions.status.width.max(regions.buffer.width);
    let area = Rect {
        x: regions.status.x,
        y,
        width,
        height: 1,
    };

    let marker = "! ";
    let marker_chars = marker.len();
    let inner = usize::from(area.width).saturating_sub(marker_chars);
    let text = truncate_to_width(err, inner);
    let total_chars = marker_chars + text.chars().count();
    let pad = usize::from(area.width).saturating_sub(total_chars);
    let line = Line::from(vec![
        Span::styled(marker.to_owned(), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("{text}{}", " ".repeat(pad)), style),
    ]);
    frame.render_widget(crate::opaque::OpaqueClear, area);
    frame.render_widget(Paragraph::new(line), area);
}

/// Paint the completion popup over the conversation buffer when the
/// cmdline has candidate completions. Each row shows the value plus an
/// optional dimmed description; the [`CommandLine::selected`] row is
/// highlighted. When there are more items than fit, a sliding window
/// follows the selection and `... N more above` / `... N more below`
/// indicator rows show how many candidates are off-screen. Suppressed
/// when an inline error is active.
pub(super) fn render_cmdline_popup(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    // Suppress the popup when an error is shown so the user can
    // focus on fixing the input.
    if cmdline.error().is_some() {
        return;
    }
    let completions = cmdline.completions();
    if completions.items.is_empty() {
        return;
    }
    let total = completions.items.len();
    let max_visible = POPUP_MAX_VISIBLE.min(total);
    let (offset, window) = popup_scroll_window(cmdline.selected(), total, max_visible);
    let above = offset;
    let below = total.saturating_sub(offset + window);
    let rows_above = usize::from(above > 0);
    let rows_below = usize::from(below > 0);
    let total_rows = window + rows_above + rows_below;

    let buf_h = usize::from(regions.buffer.height);
    let total_rows = total_rows.min(buf_h);
    if total_rows == 0 {
        return;
    }
    let height = u16::try_from(total_rows).unwrap_or(u16::MAX);

    let width = popup_width(regions, completions);
    if width == 0 {
        return;
    }
    let anchor_x = regions.status.x.saturating_add(1);
    let area = Rect {
        x: anchor_x,
        y: regions.status.y.saturating_add(1),
        width,
        height,
    };
    if area.y >= regions.buffer.y.saturating_add(regions.buffer.height) {
        return;
    }

    let (row_style, sel_style, dim_style) = popup_styles();
    let max_value_chars = completions
        .items
        .iter()
        .skip(offset)
        .take(window)
        .map(|c| c.value.chars().count())
        .max()
        .unwrap_or(0);
    let inner_width = usize::from(area.width);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total_rows);

    if rows_above > 0 {
        lines.push(Line::from(Span::styled(
            pad_to_width(&format!("  ... {above} more above"), inner_width),
            dim_style,
        )));
    }

    for (i, item) in completions
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(window)
    {
        let selected = cmdline.selected() == Some(i);
        let value_style = if selected { sel_style } else { row_style };
        let desc_style = if selected { sel_style } else { dim_style };
        lines.push(popup_row(
            item.value.as_str(),
            item.description.as_deref(),
            max_value_chars,
            inner_width,
            value_style,
            desc_style,
        ));
    }

    if rows_below > 0 {
        lines.push(Line::from(Span::styled(
            pad_to_width(&format!("  ... {below} more below"), inner_width),
            dim_style,
        )));
    }

    frame.render_widget(crate::opaque::OpaqueClear, area);
    frame.render_widget(Paragraph::new(lines).style(row_style), area);
}

/// Compute the visible-items window for the completion popup so the
/// selected row stays in view as the user cycles past the bottom or
/// scrolls back above the top. Returns `(offset, window_len)` where
/// `offset` is the index of the first item to render and `window_len`
/// is how many to render (clamped to `max_visible`).
fn popup_scroll_window(
    selected: Option<usize>,
    total: usize,
    max_visible: usize,
) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let sel = selected.unwrap_or(0);
    let offset = if sel < max_visible {
        0
    } else {
        (sel + 1)
            .saturating_sub(max_visible)
            .min(total - max_visible)
    };
    (offset, max_visible)
}

fn popup_width(regions: Regions, completions: &crate::cmdparse::Completions) -> u16 {
    let max_value = completions
        .items
        .iter()
        .map(|c| c.value.chars().count())
        .max()
        .unwrap_or(0);
    let max_desc = completions
        .items
        .iter()
        .filter_map(|c| c.description.as_deref().map(|d| d.chars().count()))
        .max()
        .unwrap_or(0);
    let separator = if max_desc > 0 { 2 } else { 0 };
    let leading = 2;
    let mut desired = leading + max_value + separator + max_desc;
    // When the popup will scroll, reserve enough room to paint the
    // "... N more above/below" indicator. ~22 cells fits up to four-
    // digit counts without truncation.
    if completions.items.len() > POPUP_MAX_VISIBLE {
        desired = desired.max(22);
    }
    let viewport = usize::from(regions.buffer.width.max(regions.status.width));
    let cap = viewport.saturating_sub(2).min(80);
    let width = desired.min(cap).max(max_value + leading);
    u16::try_from(width.min(viewport)).unwrap_or(u16::MAX)
}

fn popup_row(
    value: &str,
    description: Option<&str>,
    value_col_chars: usize,
    inner_width: usize,
    value_style: Style,
    desc_style: Style,
) -> Line<'static> {
    let leading = "  ";
    let leading_chars = leading.chars().count();
    let value_chars = value.chars().count();
    let value_pad = value_col_chars.saturating_sub(value_chars);
    let after_value = leading_chars + value_chars + value_pad;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(leading.to_owned(), value_style));
    spans.push(Span::styled(value.to_owned(), value_style));
    if value_pad > 0 {
        spans.push(Span::styled(" ".repeat(value_pad), value_style));
    }
    if let Some(desc) = description {
        let remaining = inner_width.saturating_sub(after_value).saturating_sub(2);
        if remaining > 0 {
            let truncated = truncate_to_width(desc, remaining);
            spans.push(Span::styled("  ".to_owned(), desc_style));
            spans.push(Span::styled(truncated, desc_style));
        }
    }
    let painted: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if painted < inner_width {
        spans.push(Span::styled(" ".repeat(inner_width - painted), value_style));
    }
    Line::from(spans)
}

fn truncate_to_width(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('\u{2026}');
    out
}

fn pad_to_width(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_owned();
    }
    let mut out = s.to_owned();
    out.push_str(&" ".repeat(width - n));
    out
}

/// Position the terminal cursor on the status row at the cmdline's
/// editing position when the `:` command line is open. Without this
/// the user has no visual cue where typing will land.
pub(super) fn place_cmdline_cursor(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    let row = regions.status;
    if row.width == 0 {
        return;
    }
    let prefix_width = 1u16;
    let col = u16::try_from(cmdline.text()[..cmdline.cursor()].chars().count()).unwrap_or(u16::MAX);
    let cx = row
        .x
        .saturating_add(prefix_width)
        .saturating_add(col)
        .min(row.x + row.width - 1);
    frame.set_cursor_position((cx, row.y));
}
