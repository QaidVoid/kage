//! Agents overlay (`Ctrl+T`, `/agents`), the tree panel.
//!
//! A modal list of the main session and every agent under it, live or
//! finished, as a tree in spawn order. Enter opens the selected agent,
//! or the main view from the main session's row. `x` stops the selected
//! agent with the agents under it, and Esc closes. Other keys propagate,
//! so an approval panel under the overlay keeps its answers. The App hands in
//! fresh rows every frame, so states and times stay live, and the
//! selection follows its session when rows move.

use kage_core::SessionId;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::view::{UnicodeWidthStr, pad_to_width, truncate_to_width};

/// Widest the overlay grows, in cells.
const MAX_WIDTH: u16 = 100;
/// Columns between the name, description, activity, time and token
/// columns.
const GAP: usize = 2;
/// Extra indent per level below the main session's own agents.
const INDENT: &str = "  ";
/// The footer hint in the bottom border.
const HINT: &str = " enter to open \u{b7} x to stop \u{b7} esc to close ";

/// Where a row's session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentsRowState {
    /// The main session with no run in flight.
    Idle,
    /// Announced, but held back by the running limit.
    Queued,
    /// Running a turn or a tool.
    Running,
    /// Running, and waiting for an approval.
    Waiting,
    /// The last run completed.
    Done,
    /// The last run stopped on an error.
    Failed,
    /// The last run was cancelled.
    Stopped,
}

impl AgentsRowState {
    /// Whether the session is queued or running.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }
}

/// One row of the agents overlay.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentsRow {
    /// The agent's session. `None` for the main session.
    pub session: Option<SessionId>,
    /// Nesting: 0 for the main session, 1 for its own agents.
    pub depth: usize,
    /// Name of the agent definition, or `kage` for the main session.
    pub name: String,
    /// The task description, or the main session's title.
    pub title: String,
    /// Where the session is.
    pub state: AgentsRowState,
    /// What a running agent does now, described like a tool row.
    pub activity: String,
    /// How long the current run has taken, or the last run took.
    pub elapsed_ms: Option<u64>,
    /// Input and output tokens the session used.
    pub tokens: u64,
    /// Dollars the session cost. `0.0` without pricing.
    pub cost: f64,
}

/// The agents overlay.
#[derive(Debug)]
pub struct AgentsOverlay {
    rows: Vec<AgentsRow>,
    selected: usize,
}

impl AgentsOverlay {
    /// Build the overlay over `rows` with the row of `focus` selected
    /// (the main session's row for `None`).
    #[must_use]
    pub fn new(rows: Vec<AgentsRow>, focus: Option<SessionId>) -> Self {
        let selected = rows.iter().position(|r| r.session == focus).unwrap_or(0);
        Self { rows, selected }
    }

    /// Replace the rows, keeping the selection on the same session
    /// when it is still listed.
    pub fn set_rows(&mut self, rows: Vec<AgentsRow>) {
        let current = self.rows.get(self.selected).map(|r| r.session);
        self.selected = current
            .and_then(|s| rows.iter().position(|r| r.session == s))
            .unwrap_or_else(|| self.selected.min(rows.len().saturating_sub(1)));
        self.rows = rows;
    }

    /// The selected row's session. `None` for the main session.
    #[must_use]
    pub fn selected(&self) -> Option<SessionId> {
        self.rows.get(self.selected).and_then(|r| r.session)
    }

    /// Inherent render wrapper, matching the other overlays, so the
    /// App's draw closure can pass a `Frame` directly.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let modal = OverlayWidget::measure(self, area);
        frame.render_widget(crate::opaque::OpaqueClear, modal);
        let theme = crate::theme::current();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        OverlayWidget::render(self, modal, frame.buffer_mut(), &ctx);
    }

    /// The top border's summary: agents per state, then the tokens and
    /// cost of every listed session.
    fn summary(&self) -> String {
        let agents = || self.rows.iter().filter(|r| r.session.is_some());
        let mut parts: Vec<String> = [
            (AgentsRowState::Running, "running"),
            (AgentsRowState::Waiting, "waiting"),
            (AgentsRowState::Queued, "queued"),
            (AgentsRowState::Done, "done"),
            (AgentsRowState::Failed, "failed"),
            (AgentsRowState::Stopped, "stopped"),
        ]
        .into_iter()
        .filter_map(|(state, word)| {
            let n = agents().filter(|r| r.state == state).count();
            (n > 0).then(|| format!("{n} {word}"))
        })
        .collect();
        let tokens: u64 = self.rows.iter().map(|r| r.tokens).sum();
        if tokens > 0 {
            parts.push(tokens_label(tokens));
        }
        let cost: f64 = self.rows.iter().map(|r| r.cost).sum();
        if cost > 0.0 {
            parts.push(format!("${cost:.2}"));
        }
        if parts.is_empty() {
            return String::new();
        }
        format!(" {} ", parts.join(" \u{b7} "))
    }
}

/// Cells before a row's name: the selection marker, its indent and
/// its state glyph. The main session's name sits where the glyphs do.
fn name_offset(row: &AgentsRow) -> usize {
    match row.depth {
        0 => 2,
        depth => 2 + INDENT.len() * (depth - 1) + 2,
    }
}

/// The token column, empty while nothing is known, as for the agents
/// of a resumed session.
fn tokens_label(tokens: u64) -> String {
    if tokens == 0 {
        return String::new();
    }
    format!("{} tok", crate::view::format_token_count(tokens))
}

fn time_label(row: &AgentsRow) -> String {
    row.elapsed_ms
        .map(crate::view::tool_view::format_seconds)
        .unwrap_or_default()
}

/// What the activity column says for `row`.
fn doing(row: &AgentsRow) -> &str {
    match row.state {
        AgentsRowState::Idle => "idle",
        AgentsRowState::Queued => "queued",
        AgentsRowState::Running if row.activity.is_empty() => "running",
        AgentsRowState::Running => &row.activity,
        AgentsRowState::Waiting => "waiting for approval",
        AgentsRowState::Done => "done",
        AgentsRowState::Failed => "failed",
        AgentsRowState::Stopped => "stopped",
    }
}

/// Column widths shared by every row.
struct Columns {
    name_end: usize,
    title: usize,
    doing: usize,
    time: usize,
    tokens: usize,
}

impl Columns {
    /// Fit the columns of `rows` into `width`. The activity gets what
    /// it needs up to a third of the room and the description the rest,
    /// so agents with similar tasks stay apart on narrow screens.
    fn fit(rows: &[AgentsRow], width: usize) -> Self {
        let widest = |f: &dyn Fn(&AgentsRow) -> usize| rows.iter().map(f).max().unwrap_or(0);
        let name_end = widest(&|r| name_offset(r) + r.name.width());
        let time = widest(&|r| time_label(r).width());
        let tokens = widest(&|r| tokens_label(r.tokens).width());
        let room = width.saturating_sub(name_end + 4 * GAP + time + tokens);
        let doing_share = widest(&|r| doing(r).width()).min(room / 3);
        let title = widest(&|r| r.title.width()).min(room.saturating_sub(doing_share));
        Self {
            name_end,
            title,
            doing: room.saturating_sub(title),
            time,
            tokens,
        }
    }
}

/// One row: the marker, the indent, the glyph, the name, the title,
/// what the session does, its time and its tokens.
fn row_line(
    row: &AgentsRow,
    selected: bool,
    cols: &Columns,
    width: usize,
    ctx: &OverlayCtx<'_>,
) -> Line<'static> {
    let theme = ctx.theme;
    let muted = Style::default().fg(theme.muted_fg);
    let text = Style::default().fg(theme.overlay_fg);
    let approval = theme.group_style("KageApproval");
    let (glyph, glyph_style) = match row.state {
        AgentsRowState::Idle => ("", text),
        AgentsRowState::Queued => ("\u{2022}", Style::default().fg(theme.tool_pending_rule)),
        AgentsRowState::Running => (
            crate::view::spinner_frame(),
            Style::default().fg(theme.tool_pending_rule),
        ),
        AgentsRowState::Waiting => ("!", approval),
        AgentsRowState::Done => ("\u{2022}", Style::default().fg(theme.success_fg)),
        AgentsRowState::Failed => ("\u{2717}", Style::default().fg(theme.tool_error_rule)),
        AgentsRowState::Stopped => ("\u{2298}", muted),
    };
    let glyph = if row.depth == 0 {
        String::new()
    } else {
        format!("{glyph} ")
    };
    let doing_style = if row.state == AgentsRowState::Waiting {
        approval
    } else {
        muted
    };
    let fit = |s: &str, w: usize| pad_to_width(&truncate_to_width(s, w, "..."), w);
    let name_room = cols.name_end - name_offset(row) + GAP;
    let time = time_label(row);
    let tokens = tokens_label(row.tokens);
    let mut line = Line::from(vec![
        Span::styled(if selected { "> " } else { "  " }, text),
        Span::raw(INDENT.repeat(row.depth.saturating_sub(1))),
        Span::styled(glyph, glyph_style.add_modifier(Modifier::BOLD)),
        Span::styled(fit(&row.name, name_room), text.add_modifier(Modifier::BOLD)),
        Span::styled(fit(&row.title, cols.title), text),
        Span::raw(" ".repeat(GAP)),
        Span::styled(fit(doing(row), cols.doing), doing_style),
        Span::raw(" ".repeat(GAP)),
        Span::styled(format!("{time:>w$}", w = cols.time), muted),
        Span::raw(" ".repeat(GAP)),
        Span::styled(format!("{tokens:>w$}", w = cols.tokens), muted),
    ]);
    if selected {
        let used = line.width();
        line.spans
            .push(Span::raw(" ".repeat(width.saturating_sub(used))));
        line = line.patch_style(
            Style::default()
                .fg(theme.overlay_selected_fg)
                .bg(theme.overlay_selected_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

impl OverlayWidget for AgentsOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = available
            .width
            .saturating_sub(4)
            .clamp(available.width.min(40), MAX_WIDTH);
        let rows = u16::try_from(self.rows.len().max(1)).unwrap_or(u16::MAX);
        let height = rows.saturating_add(2).min(available.height);
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>) {
        let theme = ctx.theme;
        let muted = Style::default().fg(theme.muted_fg);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.overlay_border))
            .title(Line::styled(
                " agents ",
                Style::default()
                    .fg(theme.overlay_fg)
                    .add_modifier(Modifier::BOLD),
            ))
            .title(Line::styled(self.summary(), muted).right_aligned())
            .title_bottom(Line::styled(HINT, muted));
        let inner = block.inner(area);
        Widget::render(block, area, buf);
        let body = Rect {
            x: inner.x.saturating_add(1),
            width: inner.width.saturating_sub(2),
            ..inner
        };
        if self.rows.is_empty() {
            Widget::render(
                Paragraph::new(Line::styled("  no agents yet", muted)),
                body,
                buf,
            );
            return;
        }
        let width = usize::from(body.width);
        let cols = Columns::fit(&self.rows, width);
        let height = usize::from(body.height);
        let offset = crate::view::scroll_offset_centered(self.selected, self.rows.len(), height);
        let lines: Vec<Line<'static>> = self
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(i, row)| row_line(row, i == self.selected, &cols, width, ctx))
            .collect();
        Widget::render(Paragraph::new(lines), body, buf);
    }

    fn footer_hint(&self) -> &'static str {
        "enter to open \u{b7} x to stop \u{b7} esc to close"
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        match key.code {
            KeyCode::Esc => OverlayAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                OverlayAction::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.rows.len() {
                    self.selected += 1;
                }
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.selected = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.selected = self.rows.len().saturating_sub(1);
                OverlayAction::Stay
            }
            KeyCode::Enter if !self.rows.is_empty() => OverlayAction::Resolve("open".into()),
            KeyCode::Char('x')
                if self
                    .rows
                    .get(self.selected)
                    .is_some_and(|r| r.state.is_live()) =>
            {
                OverlayAction::Resolve("stop".into())
            }
            KeyCode::Char('x') => OverlayAction::Stay,
            _ => OverlayAction::PropagateKey,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(
        session: Option<SessionId>,
        depth: usize,
        name: &str,
        state: AgentsRowState,
    ) -> AgentsRow {
        AgentsRow {
            session,
            depth,
            name: name.to_owned(),
            title: format!("{name} task"),
            state,
            activity: String::new(),
            elapsed_ms: Some(41_000),
            tokens: 22_000,
            cost: 0.05,
        }
    }

    fn tree() -> (AgentsOverlay, [SessionId; 3]) {
        let ids = [SessionId::new(), SessionId::new(), SessionId::new()];
        let rows = vec![
            row(None, 0, "kage", AgentsRowState::Running),
            row(Some(ids[0]), 1, "general", AgentsRowState::Waiting),
            row(Some(ids[1]), 2, "test", AgentsRowState::Running),
            row(Some(ids[2]), 1, "explore", AgentsRowState::Done),
        ];
        (AgentsOverlay::new(rows, None), ids)
    }

    fn paint(overlay: &mut AgentsOverlay, width: u16, height: u16) -> Vec<String> {
        let theme = crate::theme::Theme::default();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let modal = overlay.measure(area);
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        OverlayWidget::render(overlay, modal, &mut buf, &ctx);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn selection_starts_on_the_focused_agent_and_follows_it() {
        let (overlay, ids) = tree();
        let mut overlay = AgentsOverlay::new(overlay.rows, Some(ids[1]));
        assert_eq!(overlay.selected(), Some(ids[1]));
        let mut rows = overlay.rows.clone();
        rows.remove(1);
        overlay.set_rows(rows);
        assert_eq!(overlay.selected(), Some(ids[1]));
    }

    #[test]
    fn keys_move_open_stop_and_close() {
        let (mut overlay, ids) = tree();
        assert_eq!(overlay.selected(), None);
        assert_eq!(
            overlay.handle_key(key(KeyCode::Enter)),
            OverlayAction::Resolve("open".into())
        );
        overlay.handle_key(key(KeyCode::Down));
        overlay.handle_key(key(KeyCode::Char('j')));
        assert_eq!(overlay.selected(), Some(ids[1]));
        assert_eq!(
            overlay.handle_key(key(KeyCode::Char('x'))),
            OverlayAction::Resolve("stop".into())
        );
        overlay.handle_key(key(KeyCode::End));
        overlay.handle_key(key(KeyCode::Down));
        assert_eq!(overlay.selected(), Some(ids[2]));
        assert_eq!(
            overlay.handle_key(key(KeyCode::Char('x'))),
            OverlayAction::Stay,
            "a finished agent has nothing to stop"
        );
        overlay.handle_key(key(KeyCode::Char('k')));
        assert_eq!(overlay.selected(), Some(ids[1]));
        assert_eq!(overlay.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
    }

    #[test]
    fn rows_paint_as_a_tree_with_a_summary() {
        let (mut overlay, _) = tree();
        let rows = paint(&mut overlay, 120, 10);
        let top = rows.iter().find(|r| r.contains(" agents ")).unwrap();
        assert!(
            top.contains(" 1 running \u{b7} 1 waiting \u{b7} 1 done \u{b7} 88k tok \u{b7} $0.20 "),
            "{rows:#?}"
        );
        let main = rows.iter().position(|r| r.contains("kage")).unwrap();
        assert!(rows[main].contains("> kage"), "{rows:#?}");
        assert!(rows[main].contains("running"), "{rows:#?}");
        assert!(rows[main + 1].contains("  ! general"), "{rows:#?}");
        assert!(rows[main + 1].contains("waiting for approval"), "{rows:#?}");
        assert!(rows[main + 2].contains("    "), "{rows:#?}");
        assert!(rows[main + 2].contains(" test "), "{rows:#?}");
        assert!(rows[main + 3].contains("\u{2022} explore"), "{rows:#?}");
        assert!(rows[main + 3].contains("done"), "{rows:#?}");
        assert!(rows[main + 3].contains("41s  22k tok"), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains(HINT.trim())), "{rows:#?}");
    }

    #[test]
    fn similar_tasks_stay_apart_at_80_columns() {
        let mut rows = tree().0.rows;
        for (row, dir) in rows[1..].iter_mut().zip(["components", "routes", "hooks"]) {
            row.state = AgentsRowState::Running;
            row.title = format!("map exports under src/{dir}");
            row.activity = format!("Searched \"export \" in src/{dir}");
        }
        let mut overlay = AgentsOverlay::new(rows, None);
        let painted = paint(&mut overlay, 80, 24);
        assert!(painted.iter().any(|r| r.contains("src/co")), "{painted:#?}");
        assert!(painted.iter().any(|r| r.contains("src/ro")), "{painted:#?}");
    }

    #[test]
    fn the_overlay_fits_80_columns() {
        let (mut overlay, _) = tree();
        let rows = paint(&mut overlay, 80, 24);
        let explore = rows.iter().find(|r| r.contains("explore")).unwrap();
        assert!(explore.width() <= 78, "{rows:#?}");
        assert!(explore.contains("22k tok"), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains(HINT.trim())), "{rows:#?}");
    }
}
