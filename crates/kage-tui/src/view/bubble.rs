//! Chat-bubble and block line builders.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Render a user prompt as a tinted full-width band with a thin themed
/// left-edge rule and a `>` glyph before the first line.
pub(crate) fn user_block_lines(text: &str, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
    let theme = crate::theme::current();
    let style = Style::default()
        .fg(theme.focus_color)
        .add_modifier(Modifier::BOLD);
    let glyph = Style::default()
        .fg(theme.user_rule)
        .add_modifier(DECORATION_MARKER);
    let content = text
        .split('\n')
        .enumerate()
        .map(|(i, raw)| {
            Line::from(vec![
                Span::styled(if i == 0 { "> " } else { "  " }, glyph),
                Span::styled(raw.to_owned(), style),
            ])
        })
        .collect();
    wrap_in_bubble_focused(content, theme.user_rule, theme.user_bg, width, emphasis)
}

/// Width in cells of the focus-rule chrome reserved on
/// every non-bubble block (assistant text, thinking, custom,
/// standalone tool result). One cell for the rule glyph or its
/// blank stand-in, one cell of padding before the body.
pub(crate) const FOCUS_RULE_WIDTH: usize = 2;

/// Prepend a left-edge focus rule to every visual row of an
/// already-built non-bubble block's render.
///
/// The column is reserved unconditionally so toggling focus does
/// not shift the body horizontally; the rule glyph only paints while
/// the block is focused or matches a search. The renderer additionally
/// pre-wraps each logical line to `width - FOCUS_RULE_WIDTH` display
/// columns so the rule prefix lands on **every** visual row, including
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
) -> Vec<Line<'static>> {
    let prefix: Span<'static> = if emphasis == Emphasis::None {
        Span::styled(
            " ".repeat(FOCUS_RULE_WIDTH),
            Style::default().add_modifier(DECORATION_MARKER),
        )
    } else {
        let style = Style::default()
            .fg(emphasis.rule_color(crate::theme::current().focus_color))
            .add_modifier(Modifier::BOLD)
            .add_modifier(DECORATION_MARKER);
        Span::styled(format!("{} ", emphasis.rule_glyph()), style)
    };
    prefix_rows(lines, width, &prefix)
}

/// Pre-wrap `lines` to the body width and start every visual row with
/// `prefix`.
fn prefix_rows(
    lines: Vec<Line<'static>>,
    width: u16,
    prefix: &Span<'static>,
) -> Vec<Line<'static>> {
    let body_width = usize::from(width).saturating_sub(FOCUS_RULE_WIDTH).max(1);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        for row_spans in split_line_into_rows(line, body_width) {
            let mut spans = Vec::with_capacity(row_spans.len() + 1);
            spans.push(prefix.clone());
            spans.extend(row_spans);
            out.push(Line::from(spans));
        }
    }
    out
}

/// Left rule plus the pads on either side of a band's content.
const BUBBLE_CHROME: usize = 3;

/// Content cells available on one row of a band `width` cells wide.
pub(crate) fn bubble_content_width(width: u16) -> usize {
    usize::from(width).saturating_sub(BUBBLE_CHROME).max(1)
}

/// Wrap a vector of content lines in a full-width band: each row
/// starts with a colored left-edge rule and every cell is given the
/// background color. Lines wider than the band wrap onto rows that
/// carry the rule too.
///
/// Spans inside `content` are reused as-is except their background is
/// overridden with `bg` so the band reads as a uniform block.
pub(crate) fn wrap_in_bubble_focused(
    content: Vec<Line<'static>>,
    rule_color: Color,
    bg: Color,
    width: u16,
    emphasis: Emphasis,
) -> Vec<Line<'static>> {
    let max_content = bubble_content_width(width);
    let interior = max_content + BUBBLE_CHROME - 1;
    let rule_style = Style::default()
        .fg(emphasis.rule_color(rule_color))
        .bg(bg)
        .add_modifier(Modifier::BOLD)
        .add_modifier(DECORATION_MARKER);
    let rule_glyph = emphasis.rule_glyph();
    let bg_only = Style::default().bg(bg).add_modifier(DECORATION_MARKER);
    let make_row = |visual_spans: Vec<Span<'static>>| -> Line<'static> {
        let used_width: usize = visual_spans.iter().map(|s| s.content.width()).sum();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(visual_spans.len() + 3);
        spans.push(Span::styled(rule_glyph.to_owned(), rule_style));
        spans.push(Span::styled(" ", bg_only));
        for s in visual_spans {
            spans.push(Span::styled(s.content, s.style.bg(bg)));
        }
        let used = 1 + used_width;
        if used < interior {
            spans.push(Span::styled(" ".repeat(interior - used), bg_only));
        }
        Line::from(spans)
    };
    let mut out: Vec<Line<'static>> = Vec::with_capacity(content.len());
    for line in content {
        for visual_spans in split_line_into_rows(line, max_content) {
            out.push(make_row(visual_spans));
        }
    }
    out
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
