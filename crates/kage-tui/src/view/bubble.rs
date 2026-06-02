//! Chat-bubble and block line builders.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Render a user prompt as a tinted full-width "chat bubble" with a
/// thin themed left-edge rule and one row of padding above and below
/// the text.
pub(crate) fn user_block_lines(text: &str, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
    let theme = crate::theme::current();
    let mut content: Vec<Line<'static>> = Vec::new();
    for raw in text.split('\n') {
        content.push(Line::from(Span::styled(
            raw.to_owned(),
            Style::default()
                .fg(theme.focus_color)
                .add_modifier(Modifier::BOLD),
        )));
    }
    wrap_in_bubble_focused(
        content,
        theme.user_rule,
        theme.user_bg,
        width,
        emphasis,
        None,
    )
}

/// Width in cells of the focus-rule chrome that PB.5 reserves on
/// every non-bubble block (assistant text, thinking, custom,
/// standalone tool result). One cell for the rule glyph or its
/// blank stand-in, one cell of padding before the body.
pub(crate) const FOCUS_RULE_WIDTH: usize = 2;

/// Prepend a left-edge focus rule to every visual row of an
/// already-built non-bubble block's render.
///
/// PB.5 reserves the column unconditionally so toggling focus does
/// not shift the body horizontally; PB.6 additionally pre-wraps
/// each logical line to `width - FOCUS_RULE_WIDTH` display columns
/// so the rule prefix lands on **every** visual row, including
/// wrapped continuations. Without the pre-wrap, ratatui's
/// `Paragraph::wrap` would only see one logical line with the
/// prefix and fold the rest of the text below the rule. The
/// pre-wrap must measure in the same display-width metric ratatui
/// uses, or a row of wide glyphs overflows and ratatui re-folds it
/// onto a prefix-less continuation.
pub(crate) fn mark_emphasis(
    lines: Vec<Line<'static>>,
    width: u16,
    emphasis: Emphasis,
    persistent_rule: Option<Color>,
) -> Vec<Line<'static>> {
    let prefix: Span<'static> = if emphasis == Emphasis::None {
        match persistent_rule {
            // A recessive always-on spine so the turn is anchored
            // even when it is not the focus/search target.
            Some(c) => Span::styled(
                format!("{} ", emphasis.rule_glyph()),
                Style::default().fg(c).add_modifier(DECORATION_MARKER),
            ),
            None => Span::styled(
                " ".repeat(FOCUS_RULE_WIDTH),
                Style::default().add_modifier(DECORATION_MARKER),
            ),
        }
    } else {
        let style = Style::default()
            .fg(emphasis.rule_color(Color::White))
            .add_modifier(Modifier::BOLD)
            .add_modifier(DECORATION_MARKER);
        Span::styled(format!("{} ", emphasis.rule_glyph()), style)
    };
    let body_width = usize::from(width).saturating_sub(FOCUS_RULE_WIDTH).max(1);
    let mut out: Vec<Line<'static>> =
        Vec::with_capacity(lines.len() + widget::BlockPadding::BOTTOM);
    for line in lines {
        for row_spans in split_line_into_rows(line, body_width) {
            let mut spans = Vec::with_capacity(row_spans.len() + 1);
            spans.push(prefix.clone());
            spans.extend(row_spans);
            out.push(Line::from(spans));
        }
    }
    // PB.7: trailing pad row(s) so non-bubble blocks have the same
    // visual separation bubbles already get from their bottom pad.
    // Carries the gutter so the rule reads as continuous.
    for _ in 0..widget::BlockPadding::BOTTOM {
        out.push(Line::from(vec![prefix.clone()]));
    }
    out
}

/// Wrap a vector of content lines in a full-width "bubble": each row
/// starts with a colored left-edge rule, every cell is given the
/// background color, and a one-row pad sits above and below.
///
/// Each input line is truncated to fit on exactly one visual row; if
/// the content would have overflowed the buffer width and wrapped,
/// the wrap would break the bubble's visual cohesion (the wrapped
/// continuation has no leading rule and no trailing pad). Trade off:
/// the user can expand the block to read the full content.
///
/// Spans inside `content` are reused as-is except their background is
/// overridden with `bg` so the bubble reads as a uniform block.
pub(crate) fn wrap_in_bubble_focused(
    content: Vec<Line<'static>>,
    rule_color: Color,
    bg: Color,
    width: u16,
    emphasis: Emphasis,
    content_window: Option<(usize, usize)>,
) -> Vec<Line<'static>> {
    const RULE_WIDTH: usize = 1;
    const LEFT_PAD: usize = 1;
    const RIGHT_PAD: usize = 1;
    let total = usize::from(width);
    let interior = total
        .saturating_sub(RULE_WIDTH)
        .max(LEFT_PAD + RIGHT_PAD + 1);
    let max_content = interior.saturating_sub(LEFT_PAD + RIGHT_PAD);
    let rule_style = Style::default()
        .fg(emphasis.rule_color(rule_color))
        .bg(bg)
        .add_modifier(Modifier::BOLD)
        .add_modifier(DECORATION_MARKER);
    let rule_glyph = emphasis.rule_glyph();
    let bg_only = Style::default().bg(bg).add_modifier(DECORATION_MARKER);
    let pad_row = || -> Line<'static> {
        Line::from(vec![
            Span::styled(rule_glyph.to_owned(), rule_style),
            Span::styled(" ".repeat(interior), bg_only),
        ])
    };
    let make_row = |visual_spans: Vec<Span<'static>>| -> Line<'static> {
        let used_chars: usize = visual_spans.iter().map(|s| s.content.chars().count()).sum();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(visual_spans.len() + 3);
        spans.push(Span::styled(rule_glyph.to_owned(), rule_style));
        spans.push(Span::styled(" ".repeat(LEFT_PAD), bg_only));
        for s in visual_spans {
            spans.push(Span::styled(s.content, s.style.bg(bg)));
        }
        let used = LEFT_PAD + used_chars;
        if used < interior {
            spans.push(Span::styled(" ".repeat(interior - used), bg_only));
        }
        Line::from(spans)
    };

    if let Some((skip_rows, take_rows)) = content_window {
        let mut out: Vec<Line<'static>> = Vec::with_capacity(take_rows + 2);
        out.push(pad_row());
        let mut rows_produced = 0usize;
        let mut rows_skipped = 0usize;
        let target = skip_rows.saturating_add(take_rows);
        for line in content {
            if rows_produced >= take_rows {
                break;
            }
            for visual_spans in split_line_into_rows(line, max_content) {
                if rows_skipped < skip_rows {
                    rows_skipped += 1;
                    continue;
                }
                out.push(make_row(visual_spans));
                rows_produced += 1;
                if rows_produced >= take_rows {
                    break;
                }
            }
            if rows_skipped + rows_produced >= target {
                break;
            }
        }
        out.push(pad_row());
        out
    } else {
        let mut out: Vec<Line<'static>> = Vec::with_capacity(content.len() + 2);
        out.push(pad_row());
        for line in content {
            for visual_spans in split_line_into_rows(line, max_content) {
                out.push(make_row(visual_spans));
            }
        }
        out.push(pad_row());
        out
    }
}

/// Split one logical line into one or more visual rows, each holding
/// at most `max` characters across its spans. Style is preserved per
/// span; long spans are chunked. Empty input yields one empty row.
///
/// This is character-wise, not word-wise: it never breaks mid-word at
/// a fancy boundary, just at exactly `max` chars. Trade off: simple
/// math, OK for code/path content; English prose can mid-word break.
/// Word-aware row split for a styled line.
///
/// Walks the line's spans as a flat `(char, style)` stream, packs as
/// many chars as fit into `max` columns, and breaks at the most recent
/// ASCII space when the next char would overflow. The space is
/// consumed (not painted on either row) so the result reads cleanly
/// across the wrap. Words longer than `max` fall back to a
/// mid-character break.
///
/// Style boundaries are preserved: each output row is rebuilt as a
/// minimal sequence of `Span`s, coalescing consecutive chars that
/// share a style.
pub(crate) fn split_line_into_rows(line: Line<'static>, max: usize) -> Vec<Vec<Span<'static>>> {
    if max == 0 || line.spans.is_empty() {
        return vec![Vec::new()];
    }
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in line.spans {
        let style = span.style;
        for c in span.content.chars() {
            chars.push((c, style));
        }
    }
    if chars.is_empty() {
        return vec![Vec::new()];
    }

    // Accumulate display width, the unicode-width metric ratatui's
    // `Paragraph` wrap uses. Counting `char`s instead lets a row of
    // wide glyphs (CJK, emoji) overflow `max` cells; the outer
    // `Paragraph::wrap` then folds the overflow onto a continuation
    // row that never received the gutter prefix, so the left rule
    // appears to skip wrapped text.
    let cw = |c: char| UnicodeWidthChar::width(c).unwrap_or(0);
    let row_width = |chars: &[(char, Style)]| -> usize { chars.iter().map(|&(c, _)| cw(c)).sum() };

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0usize;
    let mut row_used = 0usize;
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        if row_used >= max {
            if let Some(sp) = last_space.filter(|&s| s > row_start) {
                ranges.push((row_start, sp));
                row_start = sp + 1;
                row_used = row_width(&chars[row_start..i]);
                last_space = None;
                continue;
            }
            ranges.push((row_start, i));
            row_start = i;
            row_used = 0;
            last_space = None;
        }
        if chars[i].0 == ' ' {
            last_space = Some(i);
        }
        row_used += cw(chars[i].0);
        i += 1;
    }
    ranges.push((row_start, chars.len()));

    let mut rows: Vec<Vec<Span<'static>>> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        rows.push(spans_for_range(&chars[start..end]));
    }
    rows
}

fn spans_for_range(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut current_style: Option<Style> = None;
    let mut current_content = String::new();
    for &(c, st) in chars {
        if Some(st) != current_style {
            if !current_content.is_empty() {
                out.push(Span::styled(
                    std::mem::take(&mut current_content),
                    current_style.unwrap_or_default(),
                ));
            }
            current_style = Some(st);
        }
        current_content.push(c);
    }
    if !current_content.is_empty() {
        out.push(Span::styled(
            current_content,
            current_style.unwrap_or_default(),
        ));
    }
    out
}

pub(crate) fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_owned(), style)))
        .collect()
}
