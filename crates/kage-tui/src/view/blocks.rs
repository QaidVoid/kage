//! Tool row and block line builders and styles.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

use serde_json::Value;

use super::modeline::spinner_frame;
use super::tool_view::{
    AgentEnd, BashExit, BodyLine, EditDiff, LineKind, ToolBody, ToolLabel, ToolPhase, agent_output,
    arg_rows, bash_output, describe, edit_diff, format_elapsed, format_seconds, group_summary,
};

/// Output lines a folded row shows for bash, errors and unknown tools.
const FOLDED_BODY_LINES: usize = 5;
/// Diff lines a folded edit row shows.
const FOLDED_DIFF_LINES: usize = 10;
/// Reply lines a folded finished agent row shows, so a finished card
/// stays as tall as the live one.
const FOLDED_AGENT_LINES: usize = 1;
/// Max body lines shown for an unfolded tool row. Bounds the
/// worst-case line construction cost without affecting typical
/// outputs.
const UNFOLDED_MAX_LINES: usize = 500;
/// Byte cap that complements [`UNFOLDED_MAX_LINES`].
const UNFOLDED_MAX_BYTES: usize = 256 * 1024;
/// Indent of body lines, two cells past the verb.
const BODY_INDENT: &str = "    ";

/// One tool call as the row renderer sees it.
pub(crate) struct ToolRow<'a> {
    /// Tool name as the model invoked it.
    pub(crate) name: &'a str,
    /// Parsed input.
    pub(crate) input: &'a Value,
    /// Where the call is in its lifecycle.
    pub(crate) phase: ToolPhase,
    /// Whether the body is collapsed.
    pub(crate) folded: bool,
    /// Elapsed time while running, or the recorded duration once
    /// finished.
    pub(crate) elapsed_ms: Option<u64>,
    /// The result text once finished, otherwise the latest progress.
    pub(crate) output: &'a str,
    /// An edit's change as whole lines of its file, when known.
    pub(crate) diff: Option<&'a EditDiff>,
}

/// Render one tool call as a verb-first row on its state-tinted band:
///
/// ```text
/// * Ran cargo build -p kage-cli                              4.9s
///     ... 9 earlier lines
///        Finished `dev` profile target(s) in 4.8s
/// ```
///
/// The header carries a state bullet, the verb, the target, any stats
/// and a right-aligned duration, exit code or state word. Folded rows
/// show a short body by kind: nothing for read-only tools, the output
/// tail for bash, the diff for edits, and the first lines for errors
/// and other tools. A running call shows its latest progress line.
/// Unfolded rows show the full body up to
/// [`UNFOLDED_MAX_LINES`].
pub(crate) fn tool_row_lines(
    row: &ToolRow<'_>,
    width: u16,
    emphasis: Emphasis,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let theme = crate::theme::current();
    let mut label = describe(row.name, row.input);
    if let Some(diff) = row.diff {
        label.stats = format!("(+{} -{})", diff.added, diff.removed);
    }
    let max = bubble_content_width(width);
    let (output, exit, end) = match row.name {
        "bash" => {
            let (output, exit) = bash_output(row.output);
            (output, exit, None)
        }
        "agent" => {
            let (output, end) = agent_output(row.output);
            (output, None, end)
        }
        _ => (text_lines(row.output), None, None),
    };
    // A stopped agent reads like an interrupted call, not a failure.
    let look = if end == Some(AgentEnd::Stopped) {
        ToolPhase::Interrupted
    } else {
        row.phase
    };
    let (bullet, bullet_style) = phase_bullet(look, &theme);
    let verb = label.verb_for(row.phase, exit.is_some());
    let right = right_text(row, exit, end);
    let right_style = if look == ToolPhase::Failed {
        tool_error_style()
    } else {
        Style::default().fg(theme.muted_fg)
    };
    let mut content = vec![header_row(
        (bullet, bullet_style),
        verb,
        &label.target,
        label.stats_for(row.phase),
        (&right, right_style),
        max,
    )];
    let body = if row.folded && row.name == "agent" {
        folded_agent_body(row.phase, end, output)
    } else if row.folded {
        folded_body(row, &label, output)
    } else {
        unfolded_body(row, &label, output, row_budget)
    };
    let clip = row.folded.then_some(max.saturating_sub(BODY_INDENT.len()));
    content.extend(body.into_iter().map(|line| indent_body(line, clip)));
    let (rule, bg) = match look {
        ToolPhase::Failed => (theme.tool_error_rule, theme.tool_error_bg),
        ToolPhase::Streaming
        | ToolPhase::Queued
        | ToolPhase::Waiting
        | ToolPhase::Approved
        | ToolPhase::Running => (theme.tool_pending_rule, theme.tool_pending_bg),
        ToolPhase::Done | ToolPhase::Denied | ToolPhase::Interrupted => {
            (theme.tool_rule, theme.tool_bg)
        }
    };
    wrap_in_bubble_focused(content, rule, bg, width, emphasis)
}

/// Render a run of finished read-only calls as one `Explored` row,
/// listing the targets below it with consecutive calls of one verb
/// joined on a line.
pub(crate) fn tool_group_lines(
    calls: &[&Block],
    width: u16,
    emphasis: Emphasis,
) -> Vec<Line<'static>> {
    let theme = crate::theme::current();
    let max = bubble_content_width(width);
    let labels: Vec<ToolLabel> = calls
        .iter()
        .filter_map(|b| match b {
            Block::ToolCall { name, input, .. } => Some(describe(name, input)),
            _ => None,
        })
        .collect();
    let mut content = vec![header_row(
        phase_bullet(ToolPhase::Done, &theme),
        "Explored",
        &group_summary(&labels),
        "",
        ("", Style::default()),
        max,
    )];
    let mut lines: Vec<(&str, Vec<&str>)> = Vec::new();
    for label in &labels {
        match lines.last_mut() {
            Some((verb, targets)) if *verb == label.verb_done => targets.push(&label.target),
            _ => lines.push((label.verb_done, vec![&label.target])),
        }
    }
    let style = tool_result_style();
    for (verb, targets) in lines {
        let line = Line::from(Span::styled(
            format!("{verb} {}", targets.join(", ")),
            style,
        ));
        content.push(indent_body(
            line,
            Some(max.saturating_sub(BODY_INDENT.len())),
        ));
    }
    wrap_in_bubble_focused(content, theme.tool_rule, theme.tool_bg, width, emphasis)
}

/// The state bullet of a tool row and its style.
fn phase_bullet(phase: ToolPhase, theme: &crate::theme::Theme) -> (&'static str, Style) {
    let fg = |c| Style::default().fg(c);
    match phase {
        ToolPhase::Streaming | ToolPhase::Queued | ToolPhase::Approved => {
            ("\u{2022}", fg(theme.tool_pending_rule))
        }
        ToolPhase::Running => (spinner_frame(), fg(theme.tool_pending_rule)),
        ToolPhase::Waiting => ("\u{2022}", theme.group_style("KageApproval")),
        ToolPhase::Done => ("\u{2022}", fg(theme.success_fg)),
        ToolPhase::Failed => ("\u{2717}", fg(theme.tool_error_rule)),
        ToolPhase::Denied | ToolPhase::Interrupted => ("\u{2298}", fg(theme.muted_fg)),
    }
}

/// The right-aligned part of a tool header: the duration, a failed
/// command's exit status, or the state word. A finished agent shows
/// how it ended before its duration. Agents count whole seconds, like
/// the other views of an agent, and a queued agent's card says so in
/// its body.
fn right_text(row: &ToolRow<'_>, exit: Option<BashExit>, end: Option<AgentEnd>) -> String {
    let agent = row.name == "agent";
    let format = if agent {
        format_seconds
    } else {
        format_elapsed
    };
    let elapsed = row.elapsed_ms.map(format);
    if let Some(end) = end
        && matches!(row.phase, ToolPhase::Done | ToolPhase::Failed)
    {
        return [Some(end.word().to_owned()), elapsed]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
    }
    match row.phase {
        ToolPhase::Streaming => String::new(),
        ToolPhase::Queued if agent => String::new(),
        ToolPhase::Queued => "queued".to_owned(),
        ToolPhase::Waiting => "waiting".to_owned(),
        ToolPhase::Approved => "approved".to_owned(),
        ToolPhase::Denied => "denied".to_owned(),
        ToolPhase::Interrupted => "interrupted".to_owned(),
        ToolPhase::Running | ToolPhase::Done => elapsed.unwrap_or_default(),
        ToolPhase::Failed => {
            let status = match exit {
                Some(BashExit::Code(code)) => Some(format!("exit {code}")),
                Some(BashExit::Signal) => Some("signal".to_owned()),
                None => None,
            };
            [status, elapsed]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" \u{b7} ")
        }
    }
}

/// One header row: `bullet verb target stats`, then `right` pushed to
/// the right edge of `max` cells. The target is cut to fit, and the
/// stats are dropped when too little room is left for the target.
fn header_row(
    (bullet, bullet_style): (&str, Style),
    verb: &str,
    target: &str,
    stats: &str,
    (right, right_style): (&str, Style),
    max: usize,
) -> Line<'static> {
    let theme = crate::theme::current();
    let text = Style::default().fg(theme.tool_result_fg);
    let dim = Style::default().fg(theme.muted_fg);
    let reserve = if right.is_empty() {
        0
    } else {
        right.width() + 2
    };
    let avail = max.saturating_sub(bullet.width() + 1 + verb.width() + reserve);
    let stats_w = if stats.is_empty() {
        0
    } else {
        stats.width() + 1
    };
    let (target_room, stats) = if avail >= target.width() + 1 + stats_w || avail >= stats_w + 12 {
        (avail - stats_w, stats)
    } else {
        (avail, "")
    };
    let mut spans = vec![
        Span::styled(
            format!("{bullet} "),
            bullet_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(verb.to_owned(), text.add_modifier(Modifier::BOLD)),
    ];
    if !target.is_empty() && target_room > 1 {
        let cut = truncate_to_width(target, target_room - 1, "...");
        spans.push(Span::styled(format!(" {cut}"), text));
    }
    if !stats.is_empty() {
        spans.push(Span::styled(format!(" {stats}"), dim));
    }
    if !right.is_empty() {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let gap = max.saturating_sub(used + right.width()).max(2);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(right.to_owned(), right_style));
    }
    Line::from(spans)
}

/// The body of a folded row. A running call shows its latest progress
/// line whatever its kind.
fn folded_body(row: &ToolRow<'_>, label: &ToolLabel, output: Vec<BodyLine>) -> Vec<Line<'static>> {
    match (row.phase, label.body) {
        (ToolPhase::Denied, _) => Vec::new(),
        (_, ToolBody::Tail) => tail(output, FOLDED_BODY_LINES),
        (ToolPhase::Running, _) => output
            .into_iter()
            .rfind(|l| !l.text.trim().is_empty())
            .map(|l| body_line(l, tool_result_style()))
            .into_iter()
            .collect(),
        (ToolPhase::Failed, _) => head(output, FOLDED_BODY_LINES, tool_error_style()),
        (ToolPhase::Done, ToolBody::Diff) => {
            head(diff_lines(row), FOLDED_DIFF_LINES, tool_result_style())
        }
        (ToolPhase::Done, ToolBody::Head) => head(output, FOLDED_BODY_LINES, tool_result_style()),
        _ => Vec::new(),
    }
}

/// The body of a folded `agent` row: the live card the App writes as
/// progress while the agent works, else the head of its reply. An
/// interrupted card keeps only its stat line, since the activity above
/// it is stale.
fn folded_agent_body(
    phase: ToolPhase,
    end: Option<AgentEnd>,
    output: Vec<BodyLine>,
) -> Vec<Line<'static>> {
    match (phase, end) {
        (ToolPhase::Streaming | ToolPhase::Denied, _) => Vec::new(),
        (ToolPhase::Queued | ToolPhase::Waiting | ToolPhase::Approved | ToolPhase::Running, _) => {
            head(output, FOLDED_BODY_LINES, tool_result_style())
        }
        (ToolPhase::Interrupted, _) => head(
            output.into_iter().skip(1).collect(),
            FOLDED_BODY_LINES,
            tool_result_style(),
        ),
        (ToolPhase::Failed, Some(AgentEnd::Failed) | None) => {
            head(output, FOLDED_AGENT_LINES, tool_error_style())
        }
        (ToolPhase::Done | ToolPhase::Failed, _) => {
            head(output, FOLDED_AGENT_LINES, tool_result_style())
        }
    }
}

/// The body of an unfolded row: the diff for edits, or the arguments
/// for tools without a tailored summary or without output yet, then
/// the output.
fn unfolded_body(
    row: &ToolRow<'_>,
    label: &ToolLabel,
    output: Vec<BodyLine>,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if label.body == ToolBody::Diff {
        if row.phase != ToolPhase::Failed {
            out.extend(head(
                diff_lines(row),
                UNFOLDED_MAX_LINES,
                tool_result_style(),
            ));
        }
    } else if label.verb_done == "Called" || output.is_empty() {
        let key = Style::default().fg(crate::theme::current().muted_fg);
        out.extend(arg_rows(row.input).into_iter().map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k}  "), key),
                Span::styled(v, tool_result_style()),
            ])
        }));
    }
    let output = capped(output);
    match (row.phase, label.body) {
        (_, ToolBody::Tail) => out.extend(tail(output, UNFOLDED_MAX_LINES)),
        (ToolPhase::Failed, _) => out.extend(head(output, UNFOLDED_MAX_LINES, tool_error_style())),
        (_, ToolBody::Diff) => {}
        (phase, _) => match read_extension(row).filter(|_| phase == ToolPhase::Done) {
            Some(ext) => out.extend(highlighted_head(output, ext, row_budget)),
            None => out.extend(head(output, UNFOLDED_MAX_LINES, tool_result_style())),
        },
    }
    out
}

/// The lines of an edit's change: from its file when known, else from
/// its input.
fn diff_lines(row: &ToolRow<'_>) -> Vec<BodyLine> {
    row.diff
        .map_or_else(|| edit_diff(row.input).lines, |diff| diff.lines.clone())
}

/// The file extension of a successful `read`, whose output is syntax
/// highlighted when unfolded.
fn read_extension<'a>(row: &ToolRow<'a>) -> Option<&'a str> {
    if !matches!(row.name, "read" | "view") {
        return None;
    }
    let path = row.input.get("path").and_then(Value::as_str)?;
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
}

/// The first [`UNFOLDED_MAX_LINES`] lines of a read, syntax
/// highlighted as far as the caller will display them.
fn highlighted_head(
    mut output: Vec<BodyLine>,
    ext: &str,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let more = output.len().saturating_sub(UNFOLDED_MAX_LINES);
    output.truncate(UNFOLDED_MAX_LINES);
    let plain = output.split_off(row_budget.map_or(output.len(), |b| b.min(output.len())));
    let code = output
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = if output.is_empty() {
        Vec::new()
    } else {
        crate::syntax::highlight_extension(&code, ext, tool_result_style())
    };
    out.extend(head(plain, usize::MAX, tool_result_style()));
    if more > 0 {
        out.push(elision(more_lines(more)));
    }
    out
}

/// `text` split into plain body lines.
fn text_lines(text: &str) -> Vec<BodyLine> {
    text.lines()
        .map(|l| BodyLine {
            kind: LineKind::Text,
            text: l.to_owned(),
        })
        .collect()
}

/// The longest prefix of `lines` within [`UNFOLDED_MAX_BYTES`].
fn capped(mut lines: Vec<BodyLine>) -> Vec<BodyLine> {
    let mut bytes = 0usize;
    if let Some(end) = lines.iter().position(|l| {
        bytes += l.text.len() + 1;
        bytes > UNFOLDED_MAX_BYTES
    }) {
        lines.truncate(end);
    }
    lines
}

/// The first `n` lines, with a marker for the rest.
fn head(lines: Vec<BodyLine>, n: usize, text: Style) -> Vec<Line<'static>> {
    let more = lines.len().saturating_sub(n);
    let mut out: Vec<Line<'static>> = lines
        .into_iter()
        .take(n)
        .map(|l| body_line(l, text))
        .collect();
    if more > 0 {
        out.push(elision(more_lines(more)));
    }
    out
}

/// The last `n` lines, after a marker for the earlier ones.
fn tail(lines: Vec<BodyLine>, n: usize) -> Vec<Line<'static>> {
    let skip = lines.len().saturating_sub(n);
    let mut out = Vec::with_capacity(n + 1);
    if skip > 0 {
        out.push(elision(if skip == 1 {
            "... 1 earlier line".to_owned()
        } else {
            format!("... {skip} earlier lines")
        }));
    }
    let text = tool_result_style();
    out.extend(lines.into_iter().skip(skip).map(|l| body_line(l, text)));
    out
}

fn body_line(line: BodyLine, text: Style) -> Line<'static> {
    let group = |name| crate::theme::current().group_style(name);
    let span = match line.kind {
        LineKind::Text => Span::styled(line.text, text),
        LineKind::Context => Span::styled(format!("  {}", line.text), group("KageMuted")),
        LineKind::Add => Span::styled(format!("+ {}", line.text), group("KageDiffAdd")),
        LineKind::Delete => Span::styled(format!("- {}", line.text), group("KageDiffDelete")),
        LineKind::Marker => Span::styled(line.text, group("KageMuted")),
    };
    Line::from(span)
}

fn elision(text: String) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default().fg(crate::theme::current().muted_fg),
    ))
}

/// Indent a body line under the verb, cutting it to `clip` cells when
/// given so a folded row stays one screen row per line.
fn indent_body(line: Line<'static>, clip: Option<usize>) -> Line<'static> {
    let mut spans = vec![Span::raw(BODY_INDENT)];
    let Some(mut room) = clip else {
        spans.extend(line.spans);
        return Line::from(spans);
    };
    for span in line.spans {
        if room == 0 {
            break;
        }
        let w = span.content.width();
        if w <= room {
            room -= w;
            spans.push(span);
        } else {
            spans.push(Span::styled(
                truncate_to_width(&span.content, room, "..."),
                span.style,
            ));
            room = 0;
        }
    }
    Line::from(spans)
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

fn more_lines(more: usize) -> String {
    if more == 1 {
        "... 1 more line".to_owned()
    } else {
        format!("... {more} more lines")
    }
}
