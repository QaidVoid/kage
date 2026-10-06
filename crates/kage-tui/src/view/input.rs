//! Input area rendering.

use super::*;

use ratatui::widgets::{Block, BorderType, Borders};

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
const INPUT_PLACEHOLDER_SHELL: &str = "Run a shell command (backspace leaves shell mode)";

/// The rule glyph of the input's top and bottom rules.
const RULE: &str = "\u{2500}";
/// Rule cells kept outside a title on either end of a rule.
const RULE_LEAD: usize = 2;

/// Pending prompts listed above the top rule before the rest fold
/// into a `+N more` row.
const PENDING_MAX_ROWS: usize = 3;
/// Lead of a pending row, lined up with the working row.
const PENDING_LEAD: &str = "  > ";

/// Lead of a pinned todo row, lined up with the working row.
const TODO_LEAD: &str = "  ";
/// Glyph of the todo heading on the box's border. Decorative, so it
/// cannot read as a task row.
const MARK_HEADING: &str = "\u{273b}";
/// Marker of a finished task row: a check in the success tier.
const MARK_DONE: &str = "\u{2713}";
/// Markers of a pinned todo task row, one per status.
const MARK_RUNNING: &str = super::todo::MARK_RUNNING;
const MARK_PENDING: &str = super::todo::MARK_PENDING;
/// Longest heading bar, in cells. Longer lists cap the bar instead of
/// growing it.
const TODO_BAR_MAX: usize = 10;
/// Filled and hollow cells of the heading bar.
const BAR_FULL: &str = "\u{25b0}";
const BAR_EMPTY: &str = "\u{25b1}";

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
    /// The item a `swarm` call gave this agent. Empty for a plain
    /// `agent` call, whose description is unique anyway.
    pub item: String,
    /// Where the agent is.
    pub state: AgentRowState,
    /// What a running agent does now, described like a tool row.
    pub activity: String,
    /// Time since its run started. `None` while queued.
    pub elapsed_ms: Option<u64>,
    /// Whether it runs in the background, tagged `bg` after its name.
    pub background: bool,
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

/// How many rows the pinned area paints: the live agents' rows, or,
/// while none is live but the session still has finished ones, the
/// single summary row that names them and the key that lists them.
pub(crate) fn pinned_area_rows(agents: &[AgentRow], total: usize) -> usize {
    if agents.is_empty() {
        usize::from(total > 0)
    } else {
        usize::from(list_height(agents.len(), AGENT_MAX_ROWS))
    }
}

/// The agents the pinned area paints: capped at [`AGENT_MAX_ROWS`],
/// the cap the row heights share.
pub(crate) fn pinned_shown_agents(agents: &[AgentRow]) -> &[AgentRow] {
    &agents[..agents.len().min(AGENT_MAX_ROWS)]
}

/// How many live agents the [`AGENT_MAX_ROWS`] cap hid behind the
/// `+N more` row. Zero when all of them show.
pub(crate) fn pinned_more(agents: &[AgentRow]) -> usize {
    agents.len().saturating_sub(AGENT_MAX_ROWS)
}

/// Whether the single finished-agents summary row paints instead of
/// agent rows: none is live but the session still has finished ones.
pub(crate) fn pinned_summary_painted(agents: &[AgentRow], total: usize) -> bool {
    agents.is_empty() && total > 0
}

/// The terminal row of the finished-agents summary row inside the
/// pinned `area`, when one is painted there. A click on it opens the
/// agents overlay.
pub(crate) fn pinned_summary_hit(agents: &[AgentRow], total: usize, area: Rect) -> Option<u16> {
    (pinned_summary_painted(agents, total) && area.height > 0)
        .then(|| area.bottom().saturating_sub(1))
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
    let summary = pinned_area_rows(status.agents, status.agents_total);
    let (agents_area, pending_area, area) =
        split_input(regions.input, summary, status.pending.len());
    if area.height < crate::layout::INPUT_CHROME_LINES || area.width == 0 {
        return;
    }
    paint_agents(
        frame,
        agents_area,
        status.agents,
        status.agents_total,
        status.agents_key,
    );
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
        let command = status
            .command_end
            .map(|end| (end, theme.group_style("KageInputCommand")));
        let lines =
            build_input_body_lines(input.text(), range, highlight, command, body_area.width);
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
/// region keeps the box's two rules first, then the agents. The todo
/// box pins under the working row, outside this region.
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
        y: agents.bottom(),
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

/// Paint the todo box pinned above the input, under the working row:
/// a rounded frame
/// whose top border carries the progress title and, when there is
/// room, a bar, with the tasks inside. The heading lifts onto the
/// border, so the frame costs no extra row for it. An unfinished list
/// takes the working accent; a finished one checks out in the success
/// tier. The running task is the one accented row, with a bright
/// title; pending tasks recede to the muted tier. Painting every row
/// in one tier made a list with nothing in progress a single flat
/// block.
pub(crate) fn render_todo_box(frame: &mut Frame, area: Rect, strip: &super::todo::TodoStrip) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let inner_width = usize::from(area.width).saturating_sub(2);
    let painted = super::todo::rows(strip, inner_width);
    let title = match painted.first() {
        Some((text, super::todo::RowKind::Heading)) => Some(text.as_str()),
        _ => None,
    };
    let lines: Vec<Line<'static>> = painted
        .iter()
        .skip(1)
        .map(|(text, kind)| task_line(text, *kind, &theme))
        .collect();
    let block = todo_block(title, strip, area.width, &theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One task row inside the box. The running task is the one accented
/// row, with a bright title; pending tasks recede to the body tier;
/// finished tasks check out in the success tier, struck through.
fn task_line(text: &str, kind: super::todo::RowKind, theme: &crate::theme::Theme) -> Line<'static> {
    let (glyph, glyph_fg, text_fg, strike) = match kind {
        super::todo::RowKind::Running => (
            MARK_RUNNING,
            theme.tool_pending_rule,
            theme.assistant_fg,
            false,
        ),
        super::todo::RowKind::Pending => {
            (MARK_PENDING, theme.muted_fg, theme.tool_result_fg, false)
        }
        super::todo::RowKind::Done => (MARK_DONE, theme.success_fg, theme.muted_fg, true),
        super::todo::RowKind::Heading => (MARK_HEADING, theme.muted_fg, theme.muted_fg, false),
    };
    let mut text_style = Style::default().fg(text_fg);
    if strike {
        text_style = text_style.add_modifier(Modifier::CROSSED_OUT);
    }
    Line::from(vec![
        Span::raw(TODO_LEAD),
        Span::styled(
            glyph,
            Style::default().fg(glyph_fg).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(text.to_owned(), text_style),
    ])
}

/// The todo box frame: rounded, muted, with the progress title on the
/// left of its top border and the bar on the right, both in the
/// working accent. Only a list with outstanding work paints, so the
/// frame never needs a finished state.
fn todo_block(
    title: Option<&str>,
    strip: &super::todo::TodoStrip,
    width: u16,
    theme: &crate::theme::Theme,
) -> Block<'static> {
    let (done, total) = strip.progress().unwrap_or((0, 0));
    let muted = Style::default().fg(theme.muted_fg);
    let (count, label) = title
        .and_then(|text| text.split_once(' '))
        .unwrap_or((title.unwrap_or(""), ""));
    let title_spans = vec![
        Span::styled(" ", muted),
        Span::styled(
            MARK_HEADING,
            Style::default()
                .fg(theme.tool_pending_rule)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            count.to_owned(),
            Style::default()
                .fg(theme.tool_result_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label} "), muted),
    ];
    let title_width: usize = title_spans.iter().map(Span::width).sum();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(muted)
        .title(Line::from(title_spans));
    let bar_len = total.min(TODO_BAR_MAX);
    let bar_width = bar_len + 2;
    // The bar rides the right end of the border when the title leaves
    // room for it beside the corners.
    if total > 0 && title_width + bar_width + 2 <= usize::from(width) {
        let filled = (done * bar_len + total / 2) / total;
        let bar = Line::from(vec![
            Span::styled(" ", muted),
            Span::styled(
                BAR_FULL.repeat(filled),
                Style::default().fg(theme.tool_pending_rule),
            ),
            Span::styled(
                BAR_EMPTY.repeat(bar_len - filled),
                Style::default().fg(theme.muted_fg),
            ),
            Span::styled(" ", muted),
        ])
        .right_aligned();
        block = block.title(bar);
    }
    block
}

/// Paint the pinned agents in tree order, with names in one column
/// and what each does right-aligned. The `+N more` row names `key`,
/// the key that lists them all. While `agents` is empty but the
/// session still holds `total` finished ones, one summary row takes
/// their place and names the same key.
fn paint_agents(
    frame: &mut Frame,
    area: Rect,
    agents: &[AgentRow],
    total: usize,
    key: Option<&str>,
) {
    if area.height == 0 {
        return;
    }
    let shown = pinned_shown_agents(agents);
    let name_column = shown
        .iter()
        .map(|row| agent_name_offset(row) + name_width(row))
        .max()
        .unwrap_or(0);
    let width = usize::from(area.width);
    let mut lines: Vec<Line<'static>> = shown
        .iter()
        .map(|row| agent_line(row, name_column, width))
        .collect();
    let muted = Style::default().fg(crate::theme::current().muted_fg);
    let more = pinned_more(agents);
    if more > 0 {
        let more = match key {
            Some(key) => format!("  +{more} more \u{b7} {key} for agents"),
            None => format!("  +{more} more"),
        };
        lines.push(Line::from(Span::styled(more, muted)));
    } else if pinned_summary_painted(agents, total) {
        let noun = if total == 1 { "agent" } else { "agents" };
        let row = match key {
            Some(key) => format!("  {total} {noun} \u{b7} {key} for agents"),
            None => format!("  {total} {noun}"),
        };
        lines.push(Line::from(Span::styled(row, muted)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Cells a pinned agent's name takes, its `bg` tag included.
fn name_width(row: &AgentRow) -> usize {
    row.agent.width() + if row.background { BG_TAG.len() } else { 0 }
}

/// The tag after a background agent's name.
const BG_TAG: &str = " bg";

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
    let pad = name_column + 2 - agent_name_offset(row) - name_width(row);
    let tag = if row.background { BG_TAG } else { "" };
    let room = width.saturating_sub(AGENT_LEAD.len() + name_column + 2 + 1);
    // A swarm child names its item instead of the batch description
    // every sibling repeats.
    let label = if row.item.is_empty() {
        row.description.as_str()
    } else {
        row.item.as_str()
    };
    let right_width = doing.width() + time.width();
    let description = label.width();
    let description_room = description
        .min(room.saturating_sub(right_width + 2))
        .max(description.min(room / 3));
    let description = truncate_to_width(label, description_room, "...");
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
        Span::styled(
            row.agent.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(tag, muted),
        Span::raw(" ".repeat(pad)),
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
    command: Option<(usize, Style)>,
    body_width: u16,
) -> Vec<Line<'static>> {
    wrap_input_rows(text, body_width)
        .into_iter()
        .map(|(start, end)| input_row(text, start, end, highlight_range, highlight, command))
        .collect()
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

/// One wrapped visual row spanning `text[start..end]`, split into
/// spans where `visual_range` or the command ending at `command`
/// cross the slice.
fn input_row(
    text: &str,
    start: usize,
    end: usize,
    visual_range: Option<(usize, usize)>,
    highlight: Style,
    command: Option<(usize, Style)>,
) -> Line<'static> {
    let mut cuts = vec![start, end];
    cuts.extend(
        visual_range
            .into_iter()
            .flat_map(|(from, to)| [from, to])
            .chain(command.map(|(command_end, _)| command_end))
            .map(|at| at.clamp(start, end)),
    );
    cuts.sort_unstable();
    cuts.dedup();
    let spans: Vec<Span<'static>> = cuts
        .windows(2)
        .map(|piece| {
            let (from, to) = (piece[0], piece[1]);
            let mut style = Style::default();
            if let Some((command_end, command_style)) = command
                && from < command_end
            {
                style = command_style;
            }
            if visual_range.is_some_and(|(vs, ve)| vs <= from && to <= ve) {
                style = style.patch(highlight);
            }
            Span::styled(text[from..to].to_owned(), style)
        })
        .collect();
    if spans.is_empty() {
        return Line::from(Span::raw(String::new()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_paints_apart_and_keeps_the_selection_over_it() {
        let command = Style::default().fg(Color::Blue);
        let select = Style::default().bg(Color::Red);
        let lines =
            build_input_body_lines("/model opus", Some((4, 8)), select, Some((6, command)), 40);
        let spans: Vec<(&str, Style)> = lines[0]
            .spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style))
            .collect();
        assert_eq!(
            spans,
            [
                ("/mod", command),
                ("el", command.patch(select)),
                (" o", select),
                ("pus", Style::default()),
            ]
        );
    }

    #[test]
    fn a_command_reaches_only_the_first_row() {
        let command = Style::default().fg(Color::Blue);
        let lines = build_input_body_lines(
            "/plan\nnext",
            None,
            Style::default(),
            Some((5, command)),
            40,
        );
        assert_eq!(lines[0].spans[0].style, command);
        assert_eq!(lines[1].spans[0].style, Style::default());
    }

    fn agent_row() -> AgentRow {
        AgentRow {
            session: kage_core::SessionId::new(),
            depth: 1,
            agent: "general".to_owned(),
            description: "a task".to_owned(),
            item: String::new(),
            state: AgentRowState::Running,
            activity: String::new(),
            elapsed_ms: None,
            background: false,
        }
    }

    #[test]
    fn finished_only_sessions_reserve_one_summary_row() {
        assert_eq!(pinned_area_rows(&[], 3), 1);
        assert_eq!(pinned_area_rows(&[], 0), 0);
        assert!(pinned_summary_painted(&[], 3));
        assert!(!pinned_summary_painted(&[], 0));
        assert!(!pinned_summary_painted(&[agent_row()], 3));
    }

    #[test]
    fn the_summary_hit_is_the_pinned_area_row_itself() {
        let area = ratatui::layout::Rect {
            x: 0,
            y: 10,
            width: 40,
            height: 1,
        };
        assert_eq!(pinned_summary_hit(&[], 3, area), Some(10));
        assert_eq!(pinned_summary_hit(&[agent_row()], 3, area), None);
        assert_eq!(pinned_summary_hit(&[], 0, area), None);
        assert_eq!(
            pinned_summary_hit(&[], 3, ratatui::layout::Rect::default()),
            None
        );
    }

    #[test]
    fn split_agent_rows_never_exceed_the_chrome_height() {
        let counts = [0, 1, 3, AGENT_MAX_ROWS, AGENT_MAX_ROWS + 2];
        for count in counts {
            let demand = pinned_area_rows(
                &std::iter::repeat_with(agent_row)
                    .take(count)
                    .collect::<Vec<_>>(),
                count,
            );
            let input = ratatui::layout::Rect {
                x: 0,
                y: 0,
                width: 60,
                height: crate::layout::input_height_for(1).saturating_add(agents_height(demand)),
            };
            let (agents, _, _) = split_input(input, demand, 0);
            assert!(
                agents.height <= agents_height(demand),
                "count {count}: split {} exceeds chrome {}",
                agents.height,
                agents_height(demand)
            );
        }
    }
}
