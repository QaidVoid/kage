//! Input area rendering.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Width in cells of the leading prompt glyph plus its trailing
/// space. Painted on the first content row only; subsequent rows
/// (multi-line draft, soft-wrapped continuation) align under the
/// glyph slot but stay blank.
pub(crate) const INPUT_GLYPH_WIDTH: u16 = 1;

/// Single-character prompt glyph painted at the start of the input
/// content. Plain ASCII so it renders the same in every terminal and
/// doesn't trigger our "no fancy chars" lint when grep'd.
const INPUT_GLYPH: &str = "|";

/// Default placeholder text shown when the input is empty.
pub(crate) const INPUT_PLACEHOLDER_INSERT: &str = "Send a message...";
pub(crate) const INPUT_PLACEHOLDER_NORMAL: &str = "press i to type, Esc to scroll";

pub(super) fn render_input(frame: &mut Frame, regions: Regions, input: &InputState) {
    let area = regions.input;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let mode = input.mode();
    let pane_focused = input.focused_pane() == Pane::Input;
    // Buffer pane focused: recede the input chrome to the muted tier
    // so the eye tracks the focused buffer block, but stay visible
    // (the bars no longer sit on a band, so `DIM` would vanish).
    let border_color = if pane_focused {
        mode_border_color(&theme, mode)
    } else {
        theme.muted_fg
    };
    let pill_style = if pane_focused {
        mode_pill_style(&theme, mode)
    } else {
        Style::default().fg(theme.muted_fg)
    };

    // Attachments show inline in the prompt as editable
    // `[image #N ...]` markers, so the top border stays just the
    // mode pill.
    let top_line: Vec<Span<'static>> =
        vec![Span::styled(format!(" {} ", mode_label(mode)), pill_style)];

    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(top_line));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let glyph_width = INPUT_GLYPH_WIDTH.min(inner.width);
    let body_width = inner.width.saturating_sub(glyph_width);
    let body_area = ratatui::layout::Rect::new(
        inner.x.saturating_add(glyph_width),
        inner.y,
        body_width,
        inner.height,
    );
    let scroll_off = input_scroll_offset(input, body_area);

    let glyph_area = ratatui::layout::Rect::new(inner.x, inner.y, glyph_width, 1);
    if glyph_width >= INPUT_GLYPH_WIDTH {
        let glyph = Paragraph::new(Line::from(Span::styled(
            INPUT_GLYPH.to_string(),
            Style::default().fg(theme.input_glyph_fg),
        )));
        frame.render_widget(glyph, glyph_area);
    }

    if input.text().is_empty() {
        if let Some(text) = placeholder_for(mode) {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(theme.input_placeholder_fg)
                    .add_modifier(Modifier::ITALIC),
            )));
            frame.render_widget(placeholder, body_area);
        }
    } else if body_width > 0 {
        // Visual mode paints the selection; otherwise, when the
        // cursor is parked at the tail of an `[image #N ...]` chip,
        // paint that whole chip so the user sees the block one
        // Backspace will delete as a unit.
        let (range, highlight) = if mode == Mode::Visual {
            (
                input.input_visual_range(),
                Style::default().bg(theme.selection_color),
            )
        } else if let Some(r) = input.armed_image_range() {
            (
                Some(r),
                Style::default()
                    .bg(theme.selection_color)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (None, Style::default())
        };
        // Lines are pre-wrapped at body_width chars to match
        // input_visual_cursor exactly; no Paragraph::wrap needed.
        let lines = build_input_body_lines(input.text(), range, highlight, body_width);
        let body = Paragraph::new(lines).scroll((scroll_off, 0));
        frame.render_widget(body, body_area);
    }

    // Cursor visibility: present in the input card whenever the
    // input pane has window focus AND the user is in a mode where
    // we want a hardware cursor on the input. That includes Normal
    // (vim cursor), Insert (editing), and an active input-pane
    // visual selection. Buffer-cell visual leaves the input cursor
    // hidden because the user's attention is on the buffer overlay.
    let input_visual_active = mode == Mode::Visual && input.input_visual_range().is_some();
    let show_cursor = input.focused_pane() == Pane::Input
        && (matches!(mode, Mode::Normal | Mode::Insert) || input_visual_active);
    if show_cursor && let Some(pos) = input_cursor_position(input, body_area, scroll_off) {
        frame.set_cursor_position(pos);
    }
}

/// Build the [`Line`]s for the input body using word-aware wrap at
/// `body_width`. Pre-wrapping (rather than letting `Paragraph::wrap`
/// do it) keeps the visual layout perfectly in sync with
/// [`input_visual_cursor`]: both consume the same row plan from
/// [`wrap_input_rows`], so the cursor lands exactly under the char it
/// indexes regardless of where the wrap broke.
///
/// When `highlight_range` is `Some`, each row range is further split
/// into pre / highlighted / post spans (styled with `highlight`) so
/// the band paints across wrap boundaries cleanly. Used for the
/// Visual selection and for the armed image-chip block.
fn build_input_body_lines(
    text: &str,
    highlight_range: Option<(usize, usize)>,
    highlight: Style,
    body_width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (start, end) in wrap_input_rows(text, body_width) {
        push_input_row(&mut out, text, start, end, highlight_range, highlight);
    }
    out
}

/// Word-aware wrap plan for the input area.
///
/// Returns one `(byte_start, byte_end)` per visual row. The ranges
/// project directly into the source `text` so callers that need a
/// cursor's `(row, col)` (or selection spans) can index the same
/// rows that get painted.
///
/// Wrap rules:
/// - Logical lines (split on `\n`) are wrapped independently. An
///   empty logical line still produces one zero-length row so a
///   trailing newline grows the input.
/// - Within a logical line, a row is filled greedily by characters.
///   When the next character would overflow `body_width`, the row is
///   cut at the most recent ASCII space (the space is consumed and
///   not painted on either side); if no break point exists in the
///   row, the cut is mid-character.
/// - A "word" longer than `body_width` is split at `body_width`
///   character boundaries until it fits.
pub(crate) fn wrap_input_rows(text: &str, body_width: u16) -> Vec<(usize, usize)> {
    let bw = usize::from(body_width.max(1));
    let mut rows = Vec::new();
    let mut byte_offset = 0usize;
    for line in text.split('\n') {
        let line_start = byte_offset;
        let line_bytes = line.len();
        wrap_one_logical_line(line, line_start, bw, &mut rows);
        byte_offset = line_start + line_bytes + 1;
    }
    rows
}

fn wrap_one_logical_line(
    line: &str,
    line_start_abs: usize,
    bw: usize,
    rows: &mut Vec<(usize, usize)>,
) {
    let line_end_abs = line_start_abs + line.len();
    if line.is_empty() {
        rows.push((line_start_abs, line_end_abs));
        return;
    }
    let mut row_start_abs = line_start_abs;
    let mut row_chars = 0usize;
    let mut last_space_abs: Option<usize> = None;
    let mut byte_pos = line_start_abs;
    for c in line.chars() {
        let c_len = c.len_utf8();
        if row_chars >= bw {
            if let Some(sb) = last_space_abs.filter(|&s| s > row_start_abs) {
                rows.push((row_start_abs, sb));
                row_start_abs = sb + 1;
            } else {
                rows.push((row_start_abs, byte_pos));
                row_start_abs = byte_pos;
            }
            row_chars = line[(row_start_abs - line_start_abs)..(byte_pos - line_start_abs)]
                .chars()
                .count();
            last_space_abs = None;
        }
        if c == ' ' {
            last_space_abs = Some(byte_pos);
        }
        byte_pos += c_len;
        row_chars += 1;
    }
    rows.push((row_start_abs, line_end_abs));
}

/// Append one wrapped visual row spanning `text[start..end]` to
/// `out`, splitting into selection-aware spans when `visual_range`
/// overlaps the slice.
fn push_input_row(
    out: &mut Vec<Line<'static>>,
    text: &str,
    start: usize,
    end: usize,
    visual_range: Option<(usize, usize)>,
    highlight: Style,
) {
    let chunk = &text[start..end];
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some((vs, ve)) = visual_range {
        let sel_start = if vs <= start {
            0
        } else if vs >= end {
            chunk.len()
        } else {
            vs - start
        };
        let sel_end = if ve <= start {
            0
        } else if ve >= end {
            chunk.len()
        } else {
            ve - start
        };
        if sel_start > 0 {
            spans.push(Span::raw(chunk[..sel_start].to_owned()));
        }
        if sel_end > sel_start {
            spans.push(Span::styled(
                chunk[sel_start..sel_end].to_owned(),
                highlight,
            ));
        }
        if sel_end < chunk.len() {
            spans.push(Span::raw(chunk[sel_end..].to_owned()));
        }
    } else {
        spans.push(Span::raw(chunk.to_owned()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    out.push(Line::from(spans));
}
