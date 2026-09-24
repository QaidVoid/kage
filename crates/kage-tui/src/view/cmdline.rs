//! The `:` command line and the `/` search line, painted over the
//! footer row like vim, with their error row and completion popup
//! stacked above it.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Paint the open `:` command line over `row`.
pub(super) fn render_cmdline_line(frame: &mut Frame, row: Rect, cmdline: &CommandLine) {
    let line = Line::from(vec![
        Span::styled(":", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(cmdline.text().to_owned()),
    ]);
    frame.render_widget(crate::opaque::OpaqueClear, row);
    frame.render_widget(Paragraph::new(line), row);
}

/// Paint the open `/` search line over `row`, with the match count on
/// the right.
pub(super) fn render_search_line(
    frame: &mut Frame,
    row: Rect,
    line: &CommandLine,
    count: Option<(usize, usize)>,
) {
    let theme = crate::theme::current();
    let prefix = Style::default()
        .fg(theme.match_color)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled("/", prefix), Span::raw(line.text().to_owned())];
    if let Some(count) = count {
        let label = super::slot::search_count_label(count);
        let used = 1 + line.text().width() + label.width();
        let pad = usize::from(row.width).saturating_sub(used);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(label, Style::default().fg(theme.muted_fg)));
    }
    frame.render_widget(crate::opaque::OpaqueClear, row);
    frame.render_widget(Paragraph::new(Line::from(spans)), row);
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
    let theme = crate::theme::current();
    let hit = span.style.patch(
        Style::default()
            .bg(theme.match_color)
            .fg(theme.selection_fg)
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

/// Selected rows use the overlay selection roles so every popup
/// reads as the same surface. Background is [`Theme::modeline_bg`]
/// so the popup stands apart from the input it covers.
fn popup_styles() -> (Style, Style, Style) {
    let theme = crate::theme::current();
    let bg = theme.modeline_bg;
    let row = Style::default().fg(theme.overlay_fg).bg(bg);
    let sel = Style::default()
        .fg(theme.overlay_selected_fg)
        .bg(theme.overlay_selected_bg)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.status_dim_fg).bg(bg);
    (row, sel, dim)
}

/// Paint an inline validation error on the row above the cmdline.
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

    if regions.footer.y <= regions.buffer.y {
        return;
    }
    let area = Rect {
        x: regions.footer.x,
        y: regions.footer.y - 1,
        width: regions.footer.width,
        height: 1,
    };

    let marker = "! ";
    let marker_chars = marker.len();
    let inner = usize::from(area.width).saturating_sub(marker_chars);
    let text = truncate_to_width(err, inner, "\u{2026}");
    let total_width = marker_chars + text.width();
    let pad = usize::from(area.width).saturating_sub(total_width);
    let line = Line::from(vec![
        Span::styled(marker.to_owned(), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("{text}{}", " ".repeat(pad)), style),
    ]);
    frame.render_widget(crate::opaque::OpaqueClear, area);
    frame.render_widget(Paragraph::new(line), area);
}

/// Paint the completion popup directly above the cmdline when it has
/// candidate completions. Each row shows the value plus an
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

    let space = usize::from(regions.footer.y.saturating_sub(regions.buffer.y));
    let total_rows = total_rows.min(space);
    if total_rows == 0 {
        return;
    }
    let height = u16::try_from(total_rows).unwrap_or(u16::MAX);

    let width = popup_width(regions, completions);
    if width == 0 {
        return;
    }
    let area = Rect {
        x: regions.footer.x.saturating_add(1),
        y: regions.footer.y - height,
        width,
        height,
    };

    let (row_style, sel_style, dim_style) = popup_styles();
    let max_value_width = completions
        .items
        .iter()
        .skip(offset)
        .take(window)
        .map(|c| c.value.width())
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
            max_value_width,
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

fn popup_width(regions: Regions, completions: &crate::cmdparse::Completions) -> u16 {
    let max_value = completions
        .items
        .iter()
        .map(|c| c.value.width())
        .max()
        .unwrap_or(0);
    let max_desc = completions
        .items
        .iter()
        .filter_map(|c| c.description.as_deref().map(UnicodeWidthStr::width))
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
    let viewport = usize::from(regions.footer.width);
    let cap = viewport.saturating_sub(2).min(80);
    let width = desired.min(cap).max(max_value + leading);
    u16::try_from(width.min(viewport)).unwrap_or(u16::MAX)
}

fn popup_row(
    value: &str,
    description: Option<&str>,
    value_col_width: usize,
    inner_width: usize,
    value_style: Style,
    desc_style: Style,
) -> Line<'static> {
    let leading = "  ";
    let leading_width = leading.width();
    let value_width = value.width();
    let value_pad = value_col_width.saturating_sub(value_width);
    let after_value = leading_width + value_width + value_pad;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(leading.to_owned(), value_style));
    spans.push(Span::styled(value.to_owned(), value_style));
    if value_pad > 0 {
        spans.push(Span::styled(" ".repeat(value_pad), value_style));
    }
    if let Some(desc) = description {
        let remaining = inner_width.saturating_sub(after_value).saturating_sub(2);
        if remaining > 0 {
            let truncated = truncate_to_width(desc, remaining, "\u{2026}");
            spans.push(Span::styled("  ".to_owned(), desc_style));
            spans.push(Span::styled(truncated, desc_style));
        }
    }
    let painted: usize = spans.iter().map(|s| s.content.width()).sum();
    if painted < inner_width {
        spans.push(Span::styled(" ".repeat(inner_width - painted), value_style));
    }
    Line::from(spans)
}

/// Position the terminal cursor on the footer row at the editing
/// position of the open `:` or `/` line. Without this the user has no
/// visual cue where typing will land.
pub(super) fn place_cmdline_cursor(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    let row = regions.footer;
    if row.width == 0 || row.height == 0 {
        return;
    }
    let prefix_width = 1u16;
    let col = u16::try_from(cmdline.text()[..cmdline.cursor()].width()).unwrap_or(u16::MAX);
    let cx = row
        .x
        .saturating_add(prefix_width)
        .saturating_add(col)
        .min(row.x + row.width - 1);
    frame.set_cursor_position((cx, row.y));
}
