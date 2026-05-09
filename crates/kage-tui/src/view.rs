//! Render the conversation buffer and input area into a ratatui [`Frame`].
//!
//! [`render`] is the single entry point. It walks the buffer's blocks,
//! turns each one into a styled [`Line`], lays them out in a scrollable
//! [`Paragraph`], and paints the status bar and input area on top.
//!
//! Block styling lives in [`block_to_lines`]: assistant text is plain,
//! thinking is dimmed, tool calls render as a header line plus an
//! optional indented body, and custom blocks are passed through with
//! their `kind` shown in the header.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph, Wrap};

use crate::buffer::{Block, Buffer};
use crate::cmdline::CommandLine;
use crate::input::{InputState, Mode};
use crate::layout::Regions;

/// Paint the entire TUI for one frame.
pub fn render(
    frame: &mut Frame,
    regions: Regions,
    buffer: &Buffer,
    input: &InputState,
    cmdline: Option<&CommandLine>,
) {
    render_status(frame, regions, input, cmdline);
    render_buffer(frame, regions, buffer);
    render_input(frame, regions, input);
    if let Some(cl) = cmdline {
        place_cmdline_cursor(frame, regions, cl);
    }
}

fn render_status(
    frame: &mut Frame,
    regions: Regions,
    input: &InputState,
    cmdline: Option<&CommandLine>,
) {
    let line = if let Some(cl) = cmdline {
        Line::from(vec![
            Span::styled(":", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(cl.text().to_owned()),
        ])
    } else {
        let mode = mode_label(input.mode());
        Line::from(vec![
            Span::styled(format!(" {mode} "), mode_style(input.mode())),
            Span::raw(" kage"),
        ])
    };
    let paragraph = Paragraph::new(line)
        .alignment(Alignment::Left)
        .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(paragraph, regions.status);
}

/// Position the terminal cursor on the status row at the cmdline's
/// editing position when the `:` command line is open. Without this
/// the user has no visual cue where typing will land.
fn place_cmdline_cursor(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
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

fn render_buffer(frame: &mut Frame, regions: Regions, buffer: &Buffer) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let blocks = buffer.blocks();
    // Index every ToolResult by its call_id so we can pair with a
    // possibly-non-adjacent ToolCall. Parallel tool batches arrive as
    // call/call/call/.../result/result/result; without this lookup the
    // merge would only catch trailing adjacent pairs and parallel calls
    // would render as bare flat lines.
    let mut result_by_call: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        if let Block::ToolResult { call_id, .. } = b {
            result_by_call.entry(call_id).or_insert(i);
        }
    }
    let mut consumed_results: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, cur) in blocks.iter().enumerate() {
        match cur {
            Block::ToolCall { call_id, .. } => {
                if let Some(&result_idx) = result_by_call.get(call_id.as_str()) {
                    consumed_results.insert(result_idx);
                    for line in tool_pair_to_lines(cur, &blocks[result_idx], regions.buffer.width) {
                        lines.push(line);
                    }
                } else {
                    for line in block_to_lines(cur, regions.buffer.width) {
                        lines.push(line);
                    }
                }
            }
            Block::ToolResult { .. } if consumed_results.contains(&idx) => {
                continue;
            }
            _ => {
                for line in block_to_lines(cur, regions.buffer.width) {
                    lines.push(line);
                }
            }
        }
        lines.push(Line::raw(""));
    }
    let total_lines = lines.len();
    let visible = usize::from(regions.buffer.height);
    // [`Buffer::scroll`] is rows from the bottom; the Paragraph wants
    // rows from the top. Translate, clamping so the viewport never
    // drops past the last line of content.
    let max_scroll_back = total_lines.saturating_sub(visible);
    let scroll_back = buffer.scroll().min(max_scroll_back);
    let top_offset = max_scroll_back - scroll_back;
    let scroll = u16::try_from(top_offset).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, regions.buffer);
}

fn render_input(frame: &mut Frame, regions: Regions, input: &InputState) {
    let title = Line::from(format!(
        " prompt [{}] ",
        mode_label(input.mode()).to_lowercase()
    ));
    let scroll_off = input_scroll_offset(input, regions.input);
    let body = Paragraph::new(input.text())
        .wrap(Wrap { trim: false })
        .scroll((scroll_off, 0))
        .block(RtBlock::default().title(title).borders(Borders::TOP));
    frame.render_widget(body, regions.input);
    if input.mode() == Mode::Insert {
        if let Some(pos) = input_cursor_position(input, regions.input, scroll_off) {
            frame.set_cursor_position(pos);
        }
    }
}

/// How many rows to scroll the input Paragraph so that the cursor row
/// always stays inside the visible content area. Once the prompt has
/// more rows than the input area can fit (`INPUT_MAX_LINES = 8`),
/// scrolling is the only way to keep typing visible.
fn input_scroll_offset(input: &InputState, area: ratatui::layout::Rect) -> u16 {
    if area.height < 2 {
        return 0;
    }
    let content_height = usize::from(area.height - 1);
    if content_height == 0 {
        return 0;
    }
    let prefix = input.text().get(..input.cursor()).unwrap_or("");
    let cursor_row = prefix.matches('\n').count();
    let max_visible_row = content_height - 1;
    let off = cursor_row.saturating_sub(max_visible_row);
    u16::try_from(off).unwrap_or(u16::MAX)
}

/// Compute the screen position of the prompt cursor inside the input
/// region. Returns `None` if the input has no inner area (a one-row
/// region collapses to the title border alone).
fn input_cursor_position(
    input: &InputState,
    area: ratatui::layout::Rect,
    scroll_off: u16,
) -> Option<(u16, u16)> {
    if area.height < 2 || area.width == 0 {
        return None;
    }
    let inner_x = area.x;
    let inner_y = area.y + 1;
    let max_x = area.x + area.width - 1;
    let max_y = area.y + area.height - 1;
    let prefix = input.text().get(..input.cursor()).unwrap_or("");
    let row_offset = u16::try_from(prefix.matches('\n').count())
        .unwrap_or(u16::MAX)
        .saturating_sub(scroll_off);
    let last_line = prefix.rsplit('\n').next().unwrap_or("");
    let col_offset = u16::try_from(last_line.chars().count()).unwrap_or(u16::MAX);
    let cx = inner_x.saturating_add(col_offset).min(max_x);
    let cy = inner_y.saturating_add(row_offset).min(max_y);
    Some((cx, cy))
}

/// Convert one [`Block`] into its rendered [`Line`]s.
///
/// `width` is the rendering area's column count, used by blocks that
/// pad to a full-width visual block (`User` bubble). Other blocks
/// ignore it.
///
/// Folded blocks contribute one header line. Unfolded blocks contribute
/// the header plus the body. Assistant text has no header; it is the
/// content directly. Thinking text is rendered dimmed.
#[must_use]
pub fn block_to_lines(block: &Block, width: u16) -> Vec<Line<'static>> {
    match block {
        Block::User { text } => user_block_lines(text, width),
        Block::Assistant { text, .. } => plain_lines(text, assistant_style()),
        Block::Thinking { text, folded, .. } => {
            let mut out = Vec::new();
            out.push(header_line(
                fold_indicator(*folded),
                "thinking",
                None,
                thinking_style(),
            ));
            if !*folded {
                for body_line in plain_lines(text, thinking_style()) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
        Block::ToolCall {
            name,
            input_summary,
            input_pretty,
            folded,
            ..
        } => {
            let mut out = Vec::new();
            out.push(tool_header_line(
                fold_indicator(*folded),
                name,
                input_summary,
                tool_call_style(),
            ));
            if !*folded {
                for body_line in plain_lines(input_pretty, tool_call_style()) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
        Block::ToolResult {
            name,
            output,
            is_error,
            folded,
            ..
        } => {
            let mut out = Vec::new();
            out.push(tool_result_header_line(*folded, name, output, *is_error));
            if !*folded {
                let body_style = if *is_error {
                    tool_error_style()
                } else {
                    tool_result_style()
                };
                for body_line in truncated_body_lines(output, body_style) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
        Block::Custom {
            kind, text, folded, ..
        } => {
            let mut out = Vec::new();
            out.push(header_line(
                fold_indicator(*folded),
                kind,
                None,
                custom_style(),
            ));
            if !*folded {
                for body_line in plain_lines(text, custom_style()) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
    }
}

/// Background color for the user-prompt bubble. A muted slate that
/// reads as "tinted" against most terminal backgrounds.
const USER_BUBBLE_BG: Color = Color::Rgb(45, 53, 70);

/// Background color for tool-block bubbles. Slightly cooler and dimmer
/// than the user bubble so the two read as distinct units in a stacked
/// conversation.
const TOOL_BUBBLE_BG: Color = Color::Rgb(30, 34, 44);

/// Render a user prompt as a tinted full-width "chat bubble" with a
/// thin cyan left-edge rule and one row of padding above and below the
/// text.
fn user_block_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let mut content: Vec<Line<'static>> = Vec::new();
    for raw in text.split('\n') {
        content.push(Line::from(Span::styled(
            raw.to_owned(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
    }
    wrap_in_bubble(content, Color::Cyan, USER_BUBBLE_BG, width)
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
fn wrap_in_bubble(
    content: Vec<Line<'static>>,
    rule_color: Color,
    bg: Color,
    width: u16,
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
        .fg(rule_color)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let bg_only = Style::default().bg(bg);
    let pad_row = || -> Line<'static> {
        Line::from(vec![
            Span::styled("\u{258e}".to_owned(), rule_style),
            Span::styled(" ".repeat(interior), bg_only),
        ])
    };

    let mut out: Vec<Line<'static>> = Vec::with_capacity(content.len() + 2);
    out.push(pad_row());
    for line in content {
        let truncated = truncate_spans(line.spans, max_content);
        let used_chars: usize = truncated.iter().map(|s| s.content.chars().count()).sum();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(truncated.len() + 3);
        spans.push(Span::styled("\u{258e}".to_owned(), rule_style));
        spans.push(Span::styled(" ".repeat(LEFT_PAD), bg_only));
        for s in truncated {
            spans.push(Span::styled(s.content, s.style.bg(bg)));
        }
        let used = LEFT_PAD + used_chars;
        if used < interior {
            spans.push(Span::styled(" ".repeat(interior - used), bg_only));
        }
        out.push(Line::from(spans));
    }
    out.push(pad_row());
    out
}

/// Truncate a sequence of styled spans so the combined char count is
/// at most `max`, appending a single-character ellipsis if any span
/// was clipped. Each span keeps its own style; the ellipsis inherits
/// the style of the span it lands on.
fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    let mut span_iter = spans.into_iter().peekable();
    while let Some(s) = span_iter.next() {
        let len = s.content.chars().count();
        if used + len <= max {
            used += len;
            out.push(s);
            continue;
        }
        // Need to clip this span (and drop any remaining).
        let avail = max.saturating_sub(used);
        if avail > 0 {
            let cut: String = s.content.chars().take(avail.saturating_sub(1)).collect();
            out.push(Span::styled(format!("{cut}\u{2026}"), s.style));
        } else if let Some(last) = out.last_mut() {
            // Trim one char from the previous span to make room for `…`.
            let mut chars: Vec<char> = last.content.chars().collect();
            if !chars.is_empty() {
                chars.pop();
            }
            chars.push('\u{2026}');
            last.content = chars.into_iter().collect::<String>().into();
        }
        // Drain remaining spans — they don't fit.
        for _ in span_iter.by_ref() {}
        break;
    }
    out
}

fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_owned(), style)))
        .collect()
}

/// Render a paired `ToolCall` + `ToolResult` as one composite block.
///
/// Layout (folded → just the header):
/// ```text
/// > read README.md                    <- header
///                                     <- blank
///   ... (12 earlier lines)            <- truncation hint
///   matching first visible line       <- body (tail-truncated)
///   matching second visible line
///                                     <- blank
///   Took 23ms · 1.2 KB                <- dim footer
/// ```
fn tool_pair_to_lines(call: &Block, result: &Block, width: u16) -> Vec<Line<'static>> {
    let (name, input_summary, input_pretty, folded) = match call {
        Block::ToolCall {
            name,
            input_summary,
            input_pretty,
            folded,
            ..
        } => (name, input_summary, input_pretty, *folded),
        _ => return Vec::new(),
    };
    let (output, is_error, duration_ms) = match result {
        Block::ToolResult {
            output,
            is_error,
            duration_ms,
            ..
        } => (output, *is_error, *duration_ms),
        _ => return Vec::new(),
    };

    let style = tool_call_style();
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut content: Vec<Line<'static>> = Vec::new();

    // Header: `<fold> <name> <summary>  <size>  Took <ms>` packs the
    // most-useful at-a-glance info on a single row regardless of fold
    // state. The body preview grows below it.
    let mut header_spans = vec![
        Span::styled(
            format!("{} ", fold_indicator(folded)),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.to_owned(), style.add_modifier(Modifier::BOLD)),
    ];
    if !input_summary.is_empty() {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(input_summary.to_owned(), style));
    }
    header_spans.push(Span::raw("  "));
    if is_error {
        header_spans.push(Span::styled(
            "ERROR".to_owned(),
            tool_error_style().add_modifier(Modifier::BOLD),
        ));
    } else {
        header_spans.push(Span::styled(human_size(output.len()), dim));
    }
    if let Some(footer) = duration_footer(duration_ms) {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(footer, dim));
    }
    content.push(Line::from(header_spans));

    // Body: tail-truncated. Folded gets a small preview window;
    // unfolded shows much more. Unfolded with hundreds of huge tool
    // outputs hurts frame time so the cap is intentional in both.
    let (cap_lines, cap_bytes) = if folded {
        (FOLDED_PREVIEW_LINES, FOLDED_PREVIEW_BYTES)
    } else {
        (MAX_BODY_LINES, MAX_BODY_BYTES)
    };
    let body_style = if is_error {
        tool_error_style()
    } else {
        tool_result_style()
    };
    let body = tail_truncated_body(output, body_style, cap_lines, cap_bytes);
    if !body.is_empty() {
        content.push(Line::raw(""));
        for line in body {
            content.push(line);
        }
    }
    if !folded && input_recap_worth_showing(name, input_summary, input_pretty) {
        content.push(Line::raw(""));
        content.push(Line::from(Span::styled("input:".to_owned(), dim)));
        for body_line in plain_lines(input_pretty, style) {
            content.push(body_line);
        }
    }
    wrap_in_bubble(content, Color::Yellow, TOOL_BUBBLE_BG, width)
}

/// Lines and bytes shown in a folded tool block's preview. Trades
/// completeness for screen real estate; the user expands with `zo` to
/// see more.
const FOLDED_PREVIEW_LINES: usize = 6;
/// Byte cap that complements [`FOLDED_PREVIEW_LINES`].
const FOLDED_PREVIEW_BYTES: usize = 2 * 1024;

/// Heuristic: should we show the pretty-printed input above the output
/// body? Skip it when the header summary already conveys the call (the
/// common case for `read README.md`, `find *.rs`, etc.) and only show
/// it when the user might genuinely want to inspect arguments
/// (multi-line bash, complex JSON inputs).
fn input_recap_worth_showing(name: &str, summary: &str, pretty: &str) -> bool {
    // Bash commands often span multiple lines via embedded newlines;
    // showing the full pretty version is useful there.
    if matches!(name, "bash" | "shell") && summary.contains('\n') {
        return true;
    }
    // For any other tool, skip the recap when the pretty form is just
    // the same JSON we already summarized to. The summary covers it.
    let pretty_compact = pretty.replace([' ', '\n'], "");
    pretty_compact.len() > 80 && !pretty_compact.contains(summary.replace(' ', "").as_str())
}

/// `Took 12ms` style timing string, or `None` when timing is unknown.
#[allow(clippy::cast_precision_loss)]
fn duration_footer(ms: Option<u64>) -> Option<String> {
    let ms = ms?;
    if ms < 1000 {
        Some(format!("Took {ms}ms"))
    } else {
        let secs = ms as f64 / 1000.0;
        Some(format!("Took {secs:.1}s"))
    }
}

/// Render `output` showing its **last** N lines (with a `... ({n}
/// earlier lines)` marker on top). Tools like `find`, `grep`, and
/// `bash` typically have the most relevant content near the tail; we
/// follow pi's convention of preserving that.
fn tail_truncated_body(
    output: &str,
    style: Style,
    cap_lines: usize,
    cap_bytes: usize,
) -> Vec<Line<'static>> {
    if output.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = output.split('\n').collect();
    let total = lines.len();
    let mut bytes = 0usize;
    let mut shown = Vec::new();
    for line in lines.iter().rev() {
        if shown.len() >= cap_lines || bytes >= cap_bytes {
            break;
        }
        bytes += line.len() + 1;
        shown.push(*line);
    }
    shown.reverse();
    let elided = total - shown.len();
    let mut out = Vec::new();
    if elided > 0 {
        out.push(Line::from(Span::styled(
            format!("... ({elided} earlier lines, zo to expand)"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )));
    }
    for line in shown {
        out.push(Line::from(Span::styled(line.to_owned(), style)));
    }
    out
}

/// Header for a tool-call block: `{indicator} {name} {summary}` with no
/// bracketed tag. The summary is bold so the tool name and the salient
/// argument both pop, but the surrounding line stays compact.
fn tool_header_line(indicator: char, name: &str, summary: &str, style: Style) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{indicator} "), style.add_modifier(Modifier::BOLD)),
        Span::styled(name.to_owned(), style.add_modifier(Modifier::BOLD)),
    ];
    if !summary.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(summary.to_owned(), style));
    }
    Line::from(spans)
}

/// Header for a tool-result block. Folded results inline a size pill
/// (or `ERROR` glyph) and a one-line preview of the output so the user
/// sees the gist without expanding. Unfolded results keep just the
/// name + size and rely on the body for detail.
fn tool_result_header_line(
    folded: bool,
    name: &str,
    output: &str,
    is_error: bool,
) -> Line<'static> {
    let indicator = if folded { '<' } else { 'v' };
    let style = if is_error {
        tool_error_style()
    } else {
        tool_result_style()
    };
    let mut spans = vec![
        Span::styled(format!("{indicator} "), style.add_modifier(Modifier::BOLD)),
        Span::styled(name.to_owned(), style.add_modifier(Modifier::BOLD)),
    ];
    spans.push(Span::raw("  "));
    if is_error {
        spans.push(Span::styled(
            "ERROR".to_owned(),
            tool_error_style().add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            human_size(output.len()),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    if folded && let Some(preview) = first_line_preview(output, 60) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("· {preview}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// Render a byte count as a short human-readable string. Used for the
/// `(1.2 KB)` style annotation in tool result headers.
#[must_use]
#[allow(clippy::cast_precision_loss)]
fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// Cap on the rendered body of a tool result. Beyond either limit the
/// body is truncated and a one-line marker tells the user how many
/// rows were elided. Tool outputs from `find`, `grep`, or large file
/// reads otherwise dominate the screen and slow each frame down.
const MAX_BODY_LINES: usize = 200;
/// Byte cap that complements [`MAX_BODY_LINES`] for outputs with very
/// long lines (e.g., a single-line JSON dump).
const MAX_BODY_BYTES: usize = 16 * 1024;

/// Render `output` as a list of styled lines, capping at
/// [`MAX_BODY_LINES`] / [`MAX_BODY_BYTES`] and appending a
/// `... (N more lines)` marker when content was elided. The full text
/// stays in the buffer's `Block` so a future "expand fully" gesture
/// can show the rest without rerunning the tool.
fn truncated_body_lines(output: &str, style: Style) -> Vec<Line<'static>> {
    if output.is_empty() {
        return Vec::new();
    }
    let total_lines = output.split('\n').count();
    let mut bytes = 0usize;
    let mut shown = 0usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in output.split('\n') {
        if shown >= MAX_BODY_LINES || bytes >= MAX_BODY_BYTES {
            break;
        }
        bytes += line.len() + 1;
        out.push(Line::from(Span::styled(line.to_owned(), style)));
        shown += 1;
    }
    if shown < total_lines {
        let remaining = total_lines - shown;
        out.push(Line::from(Span::styled(
            format!("... ({remaining} more lines)"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )));
    }
    out
}

/// First non-empty line of `text`, trimmed and truncated to `max`
/// characters. Returns `None` when there is no non-empty content.
fn first_line_preview(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let trimmed = line.trim();
    if trimmed.chars().count() <= max {
        return Some(trimmed.to_owned());
    }
    let cut: String = trimmed.chars().take(max.saturating_sub(3)).collect();
    Some(format!("{cut}..."))
}

fn header_line(indicator: char, tag: &str, detail: Option<String>, style: Style) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{indicator} "), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("[{tag}]"), style.add_modifier(Modifier::BOLD)),
    ];
    if let Some(d) = detail {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(d, style));
    }
    Line::from(spans)
}

fn prefix_line(prefix: &str, line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix.to_owned()));
    spans.extend(line.spans);
    Line::from(spans)
}

fn fold_indicator(folded: bool) -> char {
    if folded { '>' } else { 'v' }
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "NOR",
        Mode::Insert => "INS",
        Mode::Visual => "VIS",
    }
}

fn mode_style(mode: Mode) -> Style {
    let bg = match mode {
        Mode::Normal => Color::Blue,
        Mode::Insert => Color::Green,
        Mode::Visual => Color::Magenta,
    };
    Style::default()
        .fg(Color::White)
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

fn assistant_style() -> Style {
    Style::default().fg(Color::White)
}

fn thinking_style() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::DIM | Modifier::ITALIC)
}

fn tool_call_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn tool_result_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn tool_error_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

fn custom_style() -> Style {
    Style::default().fg(Color::Magenta)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::*;
    use crate::buffer::Buffer;

    fn snapshot_lines(buffer: &Buffer, input: &InputState, area: Rect) -> Vec<String> {
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let regions = crate::layout::split(frame.area(), 1);
                render(frame, regions, buffer, input, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn folded_thinking_renders_one_line() {
        let mut buffer = Buffer::new();
        buffer.append_thinking_delta("step 1\nstep 2");
        buffer.finish_streaming();
        // Thinking starts unfolded; fold it so we can assert the body
        // doesn't make it to the screen.
        assert!(buffer.toggle_fold(0));
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 6));
        assert!(lines.iter().any(|l| l.contains("[thinking]")));
        assert!(!lines.iter().any(|l| l.contains("step 1")));
    }

    #[test]
    fn unfolded_thinking_includes_body() {
        let mut buffer = Buffer::new();
        buffer.append_thinking_delta("step 1\nstep 2");
        buffer.finish_streaming();
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 8));
        assert!(lines.iter().any(|l| l.contains("[thinking]")));
        assert!(lines.iter().any(|l| l.contains("step 1")));
        assert!(lines.iter().any(|l| l.contains("step 2")));
    }

    #[test]
    fn assistant_text_renders_without_header() {
        let mut buffer = Buffer::new();
        buffer.append_assistant_delta("hi there");
        buffer.finish_streaming();
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 5));
        assert!(lines.iter().any(|l| l.contains("hi there")));
        // No `[assistant]` header tag.
        assert!(!lines.iter().any(|l| l.contains("[assistant]")));
    }

    #[test]
    fn user_block_renders_with_padded_bubble() {
        let mut buffer = Buffer::new();
        buffer.push_user("hello");
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 5));
        // Bubble keeps the prompt text intact; trailing whitespace is
        // trimmed by the test's snapshot helper.
        assert!(lines.iter().any(|l| l.contains("hello")));
        assert!(!lines.iter().any(|l| l.contains("> hello")));
    }

    #[test]
    fn folded_tool_call_renders_name_then_summary_without_brackets() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "bash", "ls -la", "{\n  \"cmd\": \"ls -la\"\n}");
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 60, 6));
        let header = lines
            .iter()
            .find(|l| l.contains("bash"))
            .expect("tool header present");
        assert!(header.contains("bash ls -la"));
        assert!(!header.contains("[tool]"));
        assert!(!header.contains('('));
        assert!(!lines.iter().any(|l| l.contains("\"cmd\"")));
    }

    #[test]
    fn unfolded_tool_call_shows_full_input_body() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "bash", "ls -la", "{\n  \"cmd\": \"ls -la\"\n}");
        assert!(buffer.toggle_fold(0));
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 60, 12));
        assert!(lines.iter().any(|l| l.contains("bash")));
        assert!(lines.iter().any(|l| l.contains("\"cmd\"")));
    }

    #[test]
    fn folded_merged_pair_inlines_status_and_preview() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "bash", "false", "{}");
        buffer.push_tool_result("c1", "exit 1", true);
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 80, 12));
        let header = lines
            .iter()
            .find(|l| l.contains("> bash"))
            .expect("merged tool header");
        assert!(header.contains("ERROR"));
        // Old standalone-result tag should be gone.
        assert!(!lines.iter().any(|l| l.contains("[result]")));
    }

    #[test]
    fn folded_merged_pair_shows_size_pill_and_body_preview() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "read", "README.md", "{}");
        buffer.push_tool_result("c1", "first line of file\nsecond line\nthird line", false);
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 90, 12));
        let header = lines
            .iter()
            .find(|l| l.contains("> read"))
            .expect("merged folded read header");
        assert!(header.contains(" B"), "expected size pill, got: {header}");
        assert!(
            lines.iter().any(|l| l.contains("first line of file")),
            "expected body preview line"
        );
        assert!(
            lines.iter().any(|l| l.contains("third line")),
            "expected body preview line"
        );
    }

    #[test]
    fn unfolded_merged_pair_shows_body_and_inline_status() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "ls", ".", "{}");
        buffer.push_tool_result("c1", "a.rs\nb.rs\nc.rs", false);
        // Toggling either half flips both, so unfolding via the call
        // (idx 0) leaves the merged renderer with full body visible.
        assert!(buffer.toggle_fold(0));
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 60, 16));
        // Unfolded fold indicator is `v`.
        let header = lines
            .iter()
            .find(|l| l.contains("v ls"))
            .expect("unfolded ls header");
        // Header carries the size + Took inline.
        assert!(header.contains(" B"), "expected size pill, got: {header}");
        assert!(header.contains("Took"), "expected Took, got: {header}");
        assert!(lines.iter().any(|l| l.contains("a.rs")));
        assert!(lines.iter().any(|l| l.contains("c.rs")));
    }

    #[test]
    fn toggling_either_half_of_a_pair_flips_both() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "ls", ".", "{}");
        buffer.push_tool_result("c1", "a", false);
        assert!(matches!(
            buffer.blocks()[0],
            Block::ToolCall { folded: true, .. }
        ));
        assert!(matches!(
            buffer.blocks()[1],
            Block::ToolResult { folded: true, .. }
        ));
        // Toggle the result; the call should flip too.
        assert!(buffer.toggle_fold(1));
        assert!(matches!(
            buffer.blocks()[0],
            Block::ToolCall { folded: false, .. }
        ));
        assert!(matches!(
            buffer.blocks()[1],
            Block::ToolResult { folded: false, .. }
        ));
    }

    #[test]
    fn small_tool_output_is_not_truncated() {
        let style = Style::default();
        let lines = super::truncated_body_lines("a\nb\nc", style);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn over_200_line_output_is_capped_with_marker() {
        let style = Style::default();
        let raw: String = (0..250)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = super::truncated_body_lines(&raw, style);
        // 200 capped lines + 1 marker.
        assert_eq!(lines.len(), 201);
        let last = format!("{}", lines.last().unwrap().spans[0].content);
        assert!(last.contains("more lines"), "got: {last}");
        assert!(last.contains("50"));
    }

    #[test]
    fn many_short_lines_past_byte_budget_are_capped() {
        let style = Style::default();
        // 5000 lines of 8 chars each = ~45 KB, exceeds the 16 KB cap.
        let raw: String = (0..5000)
            .map(|i| format!("line{i:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = super::truncated_body_lines(&raw, style);
        // We hit MAX_BODY_BYTES well before MAX_BODY_LINES; the body
        // ends with a "... (N more lines)" marker.
        let last = format!("{}", lines.last().unwrap().spans[0].content);
        assert!(last.contains("more lines"), "got: {last}");
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(super::human_size(512), "512 B");
        assert_eq!(super::human_size(2048), "2.0 KB");
        assert_eq!(super::human_size(1_500_000), "1.4 MB");
    }

    #[test]
    fn first_line_preview_skips_empty_leading_lines_and_truncates() {
        assert_eq!(
            super::first_line_preview("\n\nhello world", 20).as_deref(),
            Some("hello world")
        );
        assert_eq!(
            super::first_line_preview(&"a".repeat(80), 20).as_deref(),
            Some(&*format!("{}...", "a".repeat(17)))
        );
        assert_eq!(super::first_line_preview("\n\n  \n", 10), None);
    }

    #[test]
    fn cursor_position_advances_with_typed_text() {
        let area = Rect::new(0, 4, 40, 4);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        for c in "hello".chars() {
            input.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        let pos = super::input_cursor_position(&input, area, 0).unwrap();
        // Inner row = area.y + 1 = 5; cursor column = 5 chars into row.
        assert_eq!(pos, (5, 5));
    }

    #[test]
    fn cursor_position_walks_to_next_row_on_newline() {
        let area = Rect::new(0, 0, 20, 5);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        // Paste pre-builds multi-line content cheaply.
        input.paste("ab\ncd");
        let pos = super::input_cursor_position(&input, area, 0).unwrap();
        // Two rows below the title border -> y = 0 + 1 + 1 = 2; col = 2.
        assert_eq!(pos, (2, 2));
    }

    #[test]
    fn input_scrolls_when_cursor_row_exceeds_visible_height() {
        // Area height = 4 rows -> 1 border + 3 content rows.
        let area = Rect::new(0, 0, 40, 4);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        // Five rows of content; cursor lands on row 4 (last line).
        input.paste("a\nb\nc\nd\ne");
        let off = super::input_scroll_offset(&input, area);
        // cursor_row=4, max_visible_row=2 -> scroll by 2.
        assert_eq!(off, 2);
        // Cursor renders on the last visible row of the area (y = 3).
        let pos = super::input_cursor_position(&input, area, off).unwrap();
        assert_eq!(pos.1, 3);
    }

    #[test]
    fn input_does_not_scroll_when_text_fits() {
        let area = Rect::new(0, 0, 40, 6);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        input.paste("a\nb\nc");
        assert_eq!(super::input_scroll_offset(&input, area), 0);
    }

    #[test]
    fn status_bar_shows_mode_label() {
        let buffer = Buffer::new();
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 20, 4));
        assert!(lines[0].contains("INS"));
    }
}
