//! Input area rendering.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

/// Width in cells of the prompt column: a space, the glyph and a
/// space. The glyph is painted on the first logical line only, so it
/// scrolls away with it; every other row stays blank here.
pub(crate) const INPUT_GLYPH_WIDTH: u16 = 3;

/// Prompt glyph painted at the start of the draft.
const INPUT_GLYPH: &str = ">";
/// Prompt glyph while shell-escape mode is armed.
const INPUT_GLYPH_SHELL: &str = "!";

/// Placeholder of the empty draft in insert mode and the modeless
/// editor.
const INPUT_PLACEHOLDER_INSERT: &str = "Ask kage anything";
/// Placeholder of the empty draft in vim normal mode.
const INPUT_PLACEHOLDER_NORMAL: &str = "Press i to type";
/// Placeholder of the empty draft while shell-escape mode is armed.
const INPUT_PLACEHOLDER_SHELL: &str = "Run a shell command (Backspace leaves shell mode)";

/// The rule glyph of the input's top and bottom rules.
const RULE: &str = "\u{2500}";
/// Rule cells kept outside a title on either end of a rule.
const RULE_LEAD: usize = 2;

/// Pending prompts listed above the top rule before the rest fold
/// into a `+N more` row.
const PENDING_MAX_ROWS: usize = 3;
/// Lead of a pending row, lined up with the working row.
const PENDING_LEAD: &str = "  > ";

/// Live agents pinned above the pending prompts before the rest fold
/// into a `+N more` row.
pub(crate) const AGENT_MAX_ROWS: usize = 4;
/// Lead of a pinned agent row, lined up with the working row.
const AGENT_LEAD: &str = "  ";
/// Extra indent of a pinned agent per level below the main session's
/// own agents.
const AGENT_INDENT: &str = "  ";
/// Glyph of a pinned agent waiting for approval.
const AGENT_WAITING_GLYPH: &str = "!";

/// Where a pinned agent is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRowState {
    /// Announced, but held back by the running limit.
    Queued,
    /// Running a turn or a tool.
    Running,
    /// Running, and waiting for an approval.
    Waiting,
}

/// A live agent in the pinned list above the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    /// The agent's session.
    pub session: kage_core::SessionId,
    /// Nesting under the main session: 1 for its own agents.
    pub depth: usize,
    /// Name of the agent definition.
    pub agent: String,
    /// The task description the model wrote.
    pub description: String,
    /// Where the agent is.
    pub state: AgentRowState,
    /// What a running agent does now, described like a tool row.
    pub activity: String,
    /// Time since its run started. `None` while queued.
    pub elapsed_ms: Option<u64>,
}

/// A prompt sent during a run that the engine has not delivered yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPrompt {
    /// The prompt's first non-blank line.
    pub text: String,
    /// `true` when it waits for the run to end, `false` when it steers
    /// into the run at the next turn boundary.
    pub queued: bool,
}

/// Rows the pending prompts take above the input's top rule.
pub(crate) fn pending_height(count: usize) -> u16 {
    list_height(count, PENDING_MAX_ROWS)
}

/// Rows the pinned agents take above the pending prompts.
pub(crate) fn agents_height(count: usize) -> u16 {
    list_height(count, AGENT_MAX_ROWS)
}

/// Rows of a list of `count` entries that shows at most `max`, then a
/// `+N more` row.
fn list_height(count: usize, max: usize) -> u16 {
    let rows = count.min(max) + usize::from(count > max);
    u16::try_from(rows).unwrap_or(u16::MAX)
}

pub(super) fn render_input(
    frame: &mut Frame,
    regions: Regions,
    input: &InputState,
    sources: &super::slot::Sources<'_>,
) {
    let status = sources.status;
    let (agents_area, pending_area, area) =
        split_input(regions.input, status.agents.len(), status.pending.len());
    if area.height < crate::layout::INPUT_CHROME_LINES || area.width == 0 {
        return;
    }
    paint_agents(frame, agents_area, status.agents);
    paint_pending(frame, pending_area, status.pending);
    let theme = crate::theme::current();
    let mode = input.mode();
    let shell = input.shell_armed();
    let pane_focused = input.focused_pane() == Pane::Input;
    let body_area = Rect::new(
        area.x.saturating_add(INPUT_GLYPH_WIDTH.min(area.width)),
        area.y + 1,
        input_body_width(area.width),
        area.height - crate::layout::INPUT_CHROME_LINES,
    );
    let scroll_off = input_scroll_offset(input, body_area);
    paint_rules(frame, area, body_area, scroll_off, input, sources);

    if body_area.height == 0 {
        return;
    }
    if scroll_off == 0 && area.width >= INPUT_GLYPH_WIDTH {
        let glyph = if shell {
            INPUT_GLYPH_SHELL
        } else {
            INPUT_GLYPH
        };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(glyph.to_owned(), Style::default().fg(theme.input_glyph_fg)),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, body_area.y, INPUT_GLYPH_WIDTH, 1),
        );
    }

    let placeholder = match mode {
        _ if shell => Some(INPUT_PLACEHOLDER_SHELL),
        Mode::Insert => Some(status.placeholder.unwrap_or(INPUT_PLACEHOLDER_INSERT)),
        Mode::Normal => Some(INPUT_PLACEHOLDER_NORMAL),
        Mode::Visual => None,
    };
    if input.text().is_empty() {
        if let Some(text) = placeholder {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(theme.input_placeholder_fg)
                    .add_modifier(Modifier::ITALIC),
            )));
            frame.render_widget(placeholder, body_area);
        }
    } else if body_area.width > 0 {
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
        // Lines are pre-wrapped at the body width to match
        // input_visual_cursor exactly; no Paragraph::wrap needed.
        let lines = build_input_body_lines(input.text(), range, highlight, body_area.width);
        let body = Paragraph::new(lines).scroll((scroll_off, 0));
        frame.render_widget(body, body_area);
    }

    // A visual selection in the buffer hides the input cursor.
    let input_visual_active = mode == Mode::Visual && input.input_visual_range().is_some();
    let show_cursor =
        pane_focused && (matches!(mode, Mode::Normal | Mode::Insert) || input_visual_active);
    if show_cursor && let Some(pos) = input_cursor_position(input, body_area, scroll_off) {
        frame.set_cursor_position(pos);
    }
}

/// Split the input region into the pinned rows of `agents`, the rows
/// of `pending` prompts and the input box, top to bottom. A short
/// region keeps the box's two rules first, then the agents.
pub(crate) fn split_input(input: Rect, agents: usize, pending: usize) -> (Rect, Rect, Rect) {
    let room = input
        .height
        .saturating_sub(crate::layout::INPUT_CHROME_LINES);
    let agent_rows = agents_height(agents).min(room);
    let pending_rows = pending_height(pending).min(room - agent_rows);
    let agents = Rect {
        height: agent_rows,
        ..input
    };
    let pending = Rect {
        y: input.y + agent_rows,
        height: pending_rows,
        ..input
    };
    let rest = Rect {
        y: pending.bottom(),
        height: input.height - agent_rows - pending_rows,
        ..input
    };
    (agents, pending, rest)
}

/// Paint the pinned agents in tree order, with names in one column
/// and what each does right-aligned.
fn paint_agents(frame: &mut Frame, area: Rect, agents: &[AgentRow]) {
    if area.height == 0 {
        return;
    }
    let shown = &agents[..agents.len().min(AGENT_MAX_ROWS)];
    let name_column = shown
        .iter()
        .map(|row| agent_name_offset(row) + row.agent.width())
        .max()
        .unwrap_or(0);
    let width = usize::from(area.width);
    let mut lines: Vec<Line<'static>> = shown
        .iter()
        .map(|row| agent_line(row, name_column, width))
        .collect();
    if let Some(more) = agents.len().checked_sub(AGENT_MAX_ROWS).filter(|n| *n > 0) {
        let muted = Style::default().fg(crate::theme::current().muted_fg);
        lines.push(Line::from(Span::styled(format!("  +{more} more"), muted)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Cells before a pinned agent's name: its indent, the glyph and a
/// space.
fn agent_name_offset(row: &AgentRow) -> usize {
    AGENT_INDENT.len() * row.depth.saturating_sub(1) + 2
}

/// One pinned agent row: the lead, the glyph, the name padded to
/// `name_column`, the description, and what the agent does with its
/// time right-aligned one cell from the edge. The description yields
/// to the activity down to a third of the room.
fn agent_line(row: &AgentRow, name_column: usize, width: usize) -> Line<'static> {
    let theme = crate::theme::current();
    let muted = Style::default().fg(theme.muted_fg);
    let (glyph, glyph_style) = match row.state {
        AgentRowState::Queued => ("\u{2022}", Style::default().fg(theme.tool_pending_rule)),
        AgentRowState::Running => (
            super::modeline::spinner_frame(),
            Style::default().fg(theme.tool_pending_rule),
        ),
        AgentRowState::Waiting => (AGENT_WAITING_GLYPH, theme.group_style("KageApproval")),
    };
    let (doing, doing_style) = match row.state {
        AgentRowState::Queued => ("queued", muted),
        AgentRowState::Running => (row.activity.as_str(), muted),
        AgentRowState::Waiting => ("waiting for approval", theme.group_style("KageApproval")),
    };
    let time = row
        .elapsed_ms
        .map(|ms| format!(" \u{b7} {}", super::tool_view::format_seconds(ms)))
        .unwrap_or_default();
    let name = pad_to_width(&row.agent, name_column - agent_name_offset(row) + 2);
    let room = width.saturating_sub(AGENT_LEAD.len() + name_column + 2 + 1);
    let right_width = doing.width() + time.width();
    let description = row.description.width();
    let description_room = description
        .min(room.saturating_sub(right_width + 2))
        .max(description.min(room / 3));
    let description = truncate_to_width(&row.description, description_room, "...");
    let doing_room = room.saturating_sub(description.width() + 2 + time.width());
    let doing = if doing.width() <= doing_room {
        doing.to_owned()
    } else if doing_room > 3 {
        truncate_to_width(doing, doing_room, "...")
    } else {
        String::new()
    };
    let time = if doing.is_empty() {
        time.trim_start_matches(" \u{b7} ").to_owned()
    } else {
        time
    };
    let gap = room.saturating_sub(description.width() + doing.width() + time.width());
    Line::from(vec![
        Span::raw(AGENT_LEAD),
        Span::raw(AGENT_INDENT.repeat(row.depth.saturating_sub(1))),
        Span::styled(glyph, glyph_style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(description),
        Span::raw(" ".repeat(gap)),
        Span::styled(doing, doing_style),
        Span::styled(time, muted),
    ])
}

/// Paint the pending prompts, oldest first, each with when it will be
/// delivered.
fn paint_pending(frame: &mut Frame, area: Rect, pending: &[PendingPrompt]) {
    if area.height == 0 {
        return;
    }
    let muted = Style::default().fg(crate::theme::current().muted_fg);
    let width = usize::from(area.width);
    let mut lines: Vec<Line<'static>> = pending
        .iter()
        .take(PENDING_MAX_ROWS)
        .map(|p| {
            let when = if p.queued {
                "when this run ends"
            } else {
                "after the current tool call"
            };
            pending_line(&p.text, when, width, muted)
        })
        .collect();
    if let Some(more) = pending
        .len()
        .checked_sub(PENDING_MAX_ROWS)
        .filter(|n| *n > 0)
    {
        lines.push(Line::from(Span::styled(format!("  +{more} more"), muted)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// One pending row: the lead, the prompt clipped to fit, and `when`
/// right-aligned one cell from the edge, dropped when the row is too
/// narrow for both.
fn pending_line(text: &str, when: &str, width: usize, muted: Style) -> Line<'static> {
    const MIN_TEXT: usize = 8;
    let room = width.saturating_sub(PENDING_LEAD.len() + 1);
    let with_when = room.checked_sub(when.len() + 2).filter(|r| *r >= MIN_TEXT);
    let text = truncate_to_width(text, with_when.unwrap_or(room), "\u{2026}");
    let mut spans = vec![Span::styled(PENDING_LEAD, muted)];
    if with_when.is_some() {
        let gap = room - text.width() - when.len();
        spans.push(Span::raw(text));
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(when.to_owned(), muted));
    } else {
        spans.push(Span::raw(text));
    }
    Line::from(spans)
}

/// Paint the top rule with the input pill and the bottom rule with the
/// count of draft rows scrolled out of `body_area`.
fn paint_rules(
    frame: &mut Frame,
    area: Rect,
    body_area: Rect,
    scroll_off: u16,
    input: &InputState,
    sources: &super::slot::Sources<'_>,
) {
    let theme = crate::theme::current();
    let mode = input.mode();
    let shell = input.shell_armed();
    let pane_focused = input.focused_pane() == Pane::Input;
    // Buffer pane focused: recede the rules to the muted tier so the
    // eye tracks the focused buffer block.
    let rule_color = if shell {
        theme.warning_fg
    } else if pane_focused {
        mode_border_color(&theme, mode)
    } else {
        theme.muted_fg
    };
    let rule = Style::default().fg(rule_color);
    let strong = if pane_focused && !shell {
        mode_pill_style(&theme, mode)
    } else {
        rule.add_modifier(Modifier::BOLD)
    };
    let (left, right) = super::slot::pill_titles(sources, rule, strong);
    let top = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(
        Paragraph::new(rule_line(area.width, left, right, rule)),
        top,
    );
    let hidden = if input.text().is_empty() {
        None
    } else {
        let rows = wrap_input_rows(input.text(), body_area.width).len();
        let above = usize::from(scroll_off);
        let below = rows.saturating_sub(above + usize::from(body_area.height));
        overflow_label(above, below)
    };
    let label = hidden
        .map(|text| vec![Span::styled(text, rule)])
        .unwrap_or_default();
    let bottom = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    frame.render_widget(
        Paragraph::new(rule_line(area.width, Vec::new(), label, rule)),
        bottom,
    );
}

/// One rule row `width` cells wide: `left` after two rule cells and
/// `right` before the last two, each padded by a space, with rule
/// cells between. `right` is dropped when both do not fit.
fn rule_line(
    width: u16,
    left: Vec<Span<'static>>,
    mut right: Vec<Span<'static>>,
    rule: Style,
) -> Line<'static> {
    let titled = |spans: &[Span<'static>]| {
        if spans.is_empty() {
            0
        } else {
            RULE_LEAD + 2 + spans.iter().map(Span::width).sum::<usize>()
        }
    };
    let width = usize::from(width);
    let left_width = titled(&left);
    let mut right_width = titled(&right);
    if left_width + right_width > width {
        right.clear();
        right_width = 0;
    }
    let rules = |n: usize| Span::styled(RULE.repeat(n), rule);
    let space = || Span::styled(" ", rule);
    let mut spans = Vec::with_capacity(left.len() + right.len() + 7);
    if !left.is_empty() {
        spans.push(rules(RULE_LEAD));
        spans.push(space());
        spans.extend(left);
        spans.push(space());
    }
    spans.push(rules(width.saturating_sub(left_width + right_width)));
    if !right.is_empty() {
        spans.push(space());
        spans.extend(right);
        spans.push(space());
        spans.push(rules(RULE_LEAD));
    }
    Line::from(spans)
}

/// What the bottom rule says about draft rows scrolled out of view.
fn overflow_label(above: usize, below: usize) -> Option<String> {
    let lines = |n: usize| if n == 1 { "line" } else { "lines" };
    match (above, below) {
        (0, 0) => None,
        (n, 0) => Some(format!("{n} more {} above", lines(n))),
        (0, n) => Some(format!("{n} more {} below", lines(n))),
        (a, b) => Some(format!("{a} more above, {b} below")),
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
/// - Within a logical line, a row is filled greedily by display
///   width. When the next character would overflow `body_width`, the
///   row is cut at the most recent ASCII space (the space is consumed
///   and not painted on either side); if no break point exists in the
///   row, the cut is mid-character.
/// - A "word" longer than `body_width` is split at `body_width`
///   display-column boundaries until it fits.
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
    let mut row_width = 0usize;
    let mut last_space_abs: Option<usize> = None;
    let mut byte_pos = line_start_abs;
    for c in line.chars() {
        let c_len = c.len_utf8();
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if row_width + cw > bw {
            if let Some(sb) = last_space_abs.filter(|&s| s > row_start_abs) {
                rows.push((row_start_abs, sb));
                row_start_abs = sb + 1;
            } else {
                rows.push((row_start_abs, byte_pos));
                row_start_abs = byte_pos;
            }
            row_width = line[(row_start_abs - line_start_abs)..(byte_pos - line_start_abs)].width();
            last_space_abs = None;
        }
        if c == ' ' {
            last_space_abs = Some(byte_pos);
        }
        byte_pos += c_len;
        row_width += cw;
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
