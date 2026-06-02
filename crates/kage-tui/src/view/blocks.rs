//! Tool-pair and tool block line builders and styles.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Render a paired `ToolCall` + `ToolResult` as one composite block.
///
/// Layout (folded -> just the header):
/// ```text
/// > read README.md                    <- header
///                                     <- blank
///   ... (12 earlier lines)            <- truncation hint
///   matching first visible line       <- body (tail-truncated)
///   matching second visible line
///                                     <- blank
///   Took 23ms | 1.2 KB                <- dim footer
/// ```
pub(crate) fn tool_pair_to_lines(
    call: &Block,
    result: &Block,
    width: u16,
    emphasis: Emphasis,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
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
    let dim = Style::default().fg(crate::theme::current().muted_fg);
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
        (UNFOLDED_MAX_LINES, UNFOLDED_MAX_BYTES)
    };
    let body_style = if is_error {
        tool_error_style()
    } else {
        tool_result_style()
    };
    let body = truncated_body(
        output,
        body_style,
        cap_lines,
        cap_bytes,
        body_trim_for(name),
    );
    if !body.is_empty() {
        content.push(Line::raw(""));
        let highlight_limit = row_budget.map(|b| b.saturating_sub(3));
        let highlighted = highlight_read_body_if_applicable(
            name,
            input_summary,
            &body,
            body_style,
            highlight_limit,
        );
        for line in highlighted {
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
    let theme = crate::theme::current();
    let bg = if is_error {
        theme.tool_error_bg
    } else {
        theme.tool_bg
    };
    let rule = if is_error {
        theme.tool_error_rule
    } else {
        theme.tool_rule
    };
    wrap_in_bubble_focused(content, rule, bg, width, emphasis, None)
}

/// Lines and bytes shown in a folded tool block's preview. Trades
/// completeness for screen real estate; the user expands with `zo` to
/// see more.
const FOLDED_PREVIEW_LINES: usize = 6;
/// Byte cap that complements [`FOLDED_PREVIEW_LINES`].
const FOLDED_PREVIEW_BYTES: usize = 2 * 1024;
/// Max body lines shown for an unfolded tool block. Bounds the
/// worst-case line construction cost without affecting typical
/// outputs (most are well under this). The height estimator uses the
/// same cap so scroll geometry stays consistent.
const UNFOLDED_MAX_LINES: usize = 500;
/// Byte cap for unfolded tool output body.
const UNFOLDED_MAX_BYTES: usize = 256 * 1024;

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
/// Whether a tool's body preview should keep the head (start) or
/// tail (end) of the output when truncated. Reading a file means the
/// top is most useful; running `find`/`grep`/`bash` means the most
/// recent / final lines carry the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyTrim {
    Head,
    Tail,
}

fn body_trim_for(tool: &str) -> BodyTrim {
    match tool {
        "read" | "view" => BodyTrim::Head,
        _ => BodyTrim::Tail,
    }
}

/// For `read`/`view` tool blocks, run the (already-truncated) body
/// through syntect using the syntax inferred from the file path's
/// extension. Other tools pass through unchanged.
///
/// Operates on already-rendered lines so it preserves the tail/head
/// truncation marker added by `truncated_body`. The marker line is
/// the only one whose first span style is the dim `DarkGray`; we
/// detect that and skip highlighting it.
fn highlight_read_body_if_applicable(
    tool_name: &str,
    input_summary: &str,
    body: &[Line<'static>],
    fallback: Style,
    highlight_limit: Option<usize>,
) -> Vec<Line<'static>> {
    if !matches!(tool_name, "read" | "view") {
        return body.to_vec();
    }
    let path = input_summary.split_whitespace().next().unwrap_or("");
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.is_empty() {
        return body.to_vec();
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(body.len());
    let mut highlighted_count = 0usize;
    for line in body {
        let original_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if original_text.trim_start().starts_with("...") {
            out.push(line.clone());
            continue;
        }
        let over_budget = highlight_limit.is_some_and(|limit| highlighted_count >= limit);
        if over_budget {
            out.push(line.clone());
        } else {
            let highlighted = crate::syntax::highlight_extension(&original_text, ext, fallback);
            highlighted_count += highlighted.len();
            for hl in highlighted {
                out.push(hl);
            }
        }
    }
    out
}

fn truncated_body(
    output: &str,
    style: Style,
    cap_lines: usize,
    cap_bytes: usize,
    trim: BodyTrim,
) -> Vec<Line<'static>> {
    if output.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = output.split('\n').collect();
    let total = lines.len();
    let mut bytes = 0usize;
    let take_iter: Box<dyn Iterator<Item = &&str>> = match trim {
        BodyTrim::Head => Box::new(lines.iter()),
        BodyTrim::Tail => Box::new(lines.iter().rev()),
    };
    let mut taken: Vec<&str> = Vec::new();
    for line in take_iter {
        if taken.len() >= cap_lines || bytes >= cap_bytes {
            break;
        }
        bytes += line.len() + 1;
        taken.push(*line);
    }
    if matches!(trim, BodyTrim::Tail) {
        taken.reverse();
    }
    let elided = total - taken.len();
    let dim = Style::default().fg(crate::theme::current().muted_fg);
    let elision = match trim {
        BodyTrim::Head => format!("... ({elided} more lines)"),
        BodyTrim::Tail => format!("... ({elided} earlier lines)"),
    };
    let mut out = Vec::new();
    if matches!(trim, BodyTrim::Tail) && elided > 0 {
        out.push(Line::from(Span::styled(elision.clone(), dim)));
    }
    for line in taken {
        out.push(Line::from(Span::styled(line.to_owned(), style)));
    }
    if matches!(trim, BodyTrim::Head) && elided > 0 {
        out.push(Line::from(Span::styled(elision, dim)));
    }
    out
}

/// Header for a tool-call block: `{indicator} {name} {summary}` with no
/// bracketed tag. The summary is bold so the tool name and the salient
/// argument both pop, but the surrounding line stays compact.
/// Header for a tool-result block. Folded results inline a size pill
/// (or `ERROR` glyph) and a one-line preview of the output so the user
/// sees the gist without expanding. Unfolded results keep just the
/// name + size and rely on the body for detail.
pub(crate) fn tool_result_header_line(
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
            Style::default().fg(crate::theme::current().muted_fg),
        ));
    }
    if folded && let Some(preview) = first_line_preview(output, 60) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("\u{b7} {preview}"),
            Style::default().fg(crate::theme::current().muted_fg),
        ));
    }
    Line::from(spans)
}

/// Render a byte count as a short human-readable string. Used for the
/// `(1.2 KB)` style annotation in tool result headers.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub(crate) fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    const GB: usize = MB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
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
pub(crate) fn truncated_body_lines(output: &str, style: Style) -> Vec<Line<'static>> {
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
pub(crate) fn first_line_preview(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let trimmed = line.trim();
    if trimmed.chars().count() <= max {
        return Some(trimmed.to_owned());
    }
    let cut: String = trimmed.chars().take(max.saturating_sub(3)).collect();
    Some(format!("{cut}..."))
}

pub(crate) fn header_line(
    indicator: char,
    tag: &str,
    detail: Option<String>,
    style: Style,
) -> Line<'static> {
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

pub(crate) fn prefix_line(prefix: &str, line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix.to_owned()));
    spans.extend(line.spans);
    Line::from(spans)
}

pub(crate) fn fold_indicator(folded: bool) -> char {
    if folded { '>' } else { 'v' }
}

pub(crate) fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "-",
        Mode::Insert => "*",
        Mode::Visual => "%",
    }
}

pub(crate) fn assistant_style() -> Style {
    Style::default().fg(crate::theme::current().assistant_fg)
}

pub(crate) fn thinking_style() -> Style {
    Style::default()
        .fg(crate::theme::current().thinking_fg)
        .add_modifier(Modifier::DIM | Modifier::ITALIC)
}

pub(crate) fn tool_call_style() -> Style {
    Style::default().fg(crate::theme::current().tool_rule)
}

pub(crate) fn tool_result_style() -> Style {
    Style::default().fg(crate::theme::current().tool_result_fg)
}

pub(crate) fn tool_error_style() -> Style {
    Style::default()
        .fg(crate::theme::current().tool_error_fg)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn custom_style() -> Style {
    Style::default().fg(crate::theme::current().custom_fg)
}
