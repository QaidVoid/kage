//! Agents overlay (`Ctrl+T`, `/agents`), the tree panel.
//!
//! A modal list of the main session and every agent under it, live or
//! finished, as a tree in spawn order. Enter opens the selected agent,
//! or the main view from the main session's row. A row with agents under
//! it folds: Left or `h` folds it (or moves to its parent), Right or `l`
//! unfolds it (or moves into it), Space toggles. Page Up and Page Down
//! move a page, the mouse wheel a few rows. `x` stops the selected agent
//! with the agents under it, and Esc closes. Other keys propagate,
//! so an approval panel under the overlay keeps its answers. The App hands in
//! fresh rows every frame, so states and times stay live, and the
//! selection follows its session when rows move.

use std::collections::HashSet;

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
const HINT: &str =
    " enter to open \u{b7} \u{2190}\u{2192} to fold \u{b7} x to stop \u{b7} esc to close ";
/// Rows the mouse wheel moves per notch.
const WHEEL_ROWS: usize = 3;

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
    /// The item a `swarm` call gave this agent. Empty otherwise.
    pub item: String,
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
    /// The selected row, as an index into `rows`; always a shown row.
    selected: usize,
    /// Agents whose subtree is folded away.
    folded: HashSet<SessionId>,
    /// Rows the body showed at the last paint, the size of a page.
    page: usize,
}

impl AgentsOverlay {
    /// Build the overlay over `rows` with the row of `focus` selected
    /// (the main session's row for `None`).
    #[must_use]
    pub fn new(rows: Vec<AgentsRow>, focus: Option<SessionId>) -> Self {
        let selected = rows.iter().position(|r| r.session == focus).unwrap_or(0);
        Self {
            rows,
            selected,
            folded: HashSet::new(),
            page: 10,
        }
    }

    /// How many rows sit under row `ix` in the tree.
    fn descendants(&self, ix: usize) -> usize {
        let depth = self.rows[ix].depth;
        self.rows[ix + 1..]
            .iter()
            .take_while(|row| row.depth > depth)
            .count()
    }

    /// Whether row `ix` is folded away.
    fn is_folded(&self, ix: usize) -> bool {
        self.rows[ix]
            .session
            .is_some_and(|session| self.folded.contains(&session))
    }

    /// The rows shown, as indexes into `rows`: every row but those
    /// under a folded one.
    fn shown(&self) -> Vec<usize> {
        let mut shown = Vec::with_capacity(self.rows.len());
        let mut ix = 0;
        while ix < self.rows.len() {
            shown.push(ix);
            ix += if self.is_folded(ix) {
                1 + self.descendants(ix)
            } else {
                1
            };
        }
        shown
    }

    /// Moves the selection by `delta` shown rows, stopping at the ends.
    fn step(&mut self, delta: isize) {
        let shown = self.shown();
        let at = shown
            .iter()
            .position(|&ix| ix == self.selected)
            .unwrap_or(0);
        let to = at
            .saturating_add_signed(delta)
            .min(shown.len().saturating_sub(1));
        if let Some(&ix) = shown.get(to) {
            self.selected = ix;
        }
    }

    /// Moves the selection by the mouse wheel: `down` a few rows down,
    /// else up.
    pub fn wheel(&mut self, down: bool) {
        let rows = isize::try_from(WHEEL_ROWS).unwrap_or(1);
        self.step(if down { rows } else { -rows });
    }

    /// Folds the selected row when it has agents under it and is open;
    /// otherwise moves to its parent row.
    fn fold(&mut self) {
        let ix = self.selected;
        if let Some(session) = self.rows[ix].session
            && self.descendants(ix) > 0
            && !self.folded.contains(&session)
        {
            self.folded.insert(session);
            return;
        }
        let depth = self.rows[ix].depth;
        if let Some(parent) = self.rows[..ix].iter().rposition(|row| row.depth < depth) {
            self.selected = parent;
        }
    }

    /// Unfolds the selected row when folded; otherwise moves into its
    /// first agent.
    fn unfold(&mut self) {
        let ix = self.selected;
        if let Some(session) = self.rows[ix].session
            && self.folded.remove(&session)
        {
            return;
        }
        if self.descendants(ix) > 0 {
            self.selected = ix + 1;
        }
    }

    /// Folds or unfolds the selected row.
    fn toggle(&mut self) {
        let ix = self.selected;
        if let Some(session) = self.rows[ix].session
            && self.descendants(ix) > 0
            && !self.folded.remove(&session)
        {
            self.folded.insert(session);
        }
    }

    /// Replace the rows, keeping the selection on the same session
    /// when it is still listed.
    pub fn set_rows(&mut self, rows: Vec<AgentsRow>) {
        let current = self.rows.get(self.selected).map(|r| r.session);
        self.selected = current
            .and_then(|s| rows.iter().position(|r| r.session == s))
            .unwrap_or_else(|| self.selected.min(rows.len().saturating_sub(1)));
        self.rows = rows;
        // A selection inside a subtree that is folded now climbs to the
        // folded row, so it is always on screen.
        let shown = self.shown();
        if !shown.contains(&self.selected) {
            self.selected = shown
                .iter()
                .rev()
                .find(|&&ix| ix < self.selected)
                .copied()
                .unwrap_or(0);
        }
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
        depth => 2 + INDENT.len() * (depth - 1) + 4,
    }
}

/// How a row's subtree shows: no agents under it, open, or folded with
/// this many rows hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fold {
    Leaf,
    Open,
    Folded(usize),
}

/// The name a row shows: a folded row says how many agents it hides.
fn shown_name(row: &AgentsRow, fold: Fold) -> String {
    match fold {
        Fold::Folded(hidden) => format!("{} (+{hidden})", row.name),
        _ => row.name.clone(),
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
    fn fit(rows: &[(&AgentsRow, Fold)], width: usize) -> Self {
        let name_end = rows
            .iter()
            .map(|(r, fold)| name_offset(r) + shown_name(r, *fold).width())
            .max()
            .unwrap_or(0);
        let widest =
            |f: &dyn Fn(&AgentsRow) -> usize| rows.iter().map(|(r, _)| f(r)).max().unwrap_or(0);
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
    fold: Fold,
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
    // A swarm child names its item instead of the batch description
    // every sibling repeats.
    let title = if row.item.is_empty() {
        row.title.as_str()
    } else {
        row.item.as_str()
    };
    let name_room = cols.name_end - name_offset(row) + GAP;
    let marker = match (row.depth, fold) {
        (0, _) => "",
        (_, Fold::Leaf) => "  ",
        (_, Fold::Open) => "\u{25be} ",
        (_, Fold::Folded(_)) => "\u{25b8} ",
    };
    let time = time_label(row);
    let tokens = tokens_label(row.tokens);
    let mut line = Line::from(vec![
        Span::styled(if selected { "> " } else { "  " }, text),
        Span::raw(INDENT.repeat(row.depth.saturating_sub(1))),
        Span::styled(glyph, glyph_style.add_modifier(Modifier::BOLD)),
        Span::styled(marker, muted),
        Span::styled(
            fit(&shown_name(row, fold), name_room),
            text.add_modifier(Modifier::BOLD),
        ),
        Span::styled(fit(title, cols.title), text),
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
        let height = usize::from(body.height);
        self.page = height.max(1);
        let shown: Vec<(usize, &AgentsRow, Fold)> = self
            .shown()
            .into_iter()
            .map(|ix| {
                let hidden = self.descendants(ix);
                let fold = match (hidden, self.is_folded(ix)) {
                    (0, _) => Fold::Leaf,
                    (_, false) => Fold::Open,
                    (hidden, true) => Fold::Folded(hidden),
                };
                (ix, &self.rows[ix], fold)
            })
            .collect();
        let fitted: Vec<(&AgentsRow, Fold)> = shown.iter().map(|(_, r, f)| (*r, *f)).collect();
        let cols = Columns::fit(&fitted, width);
        let at = shown
            .iter()
            .position(|(ix, _, _)| *ix == self.selected)
            .unwrap_or(0);
        let offset = crate::view::scroll_offset_centered(at, shown.len(), height);
        let lines: Vec<Line<'static>> = shown
            .iter()
            .skip(offset)
            .take(height)
            .map(|(ix, row, fold)| row_line(row, *fold, *ix == self.selected, &cols, width, ctx))
            .collect();
        Widget::render(Paragraph::new(lines), body, buf);
    }

    fn footer_hint(&self) -> &'static str {
        "enter to open \u{b7} \u{2190}\u{2192} to fold \u{b7} x to stop \u{b7} esc to close"
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
            _ if self.rows.is_empty() && key.code != KeyCode::Enter => match key.code {
                KeyCode::Char('x') => OverlayAction::Stay,
                _ => OverlayAction::PropagateKey,
            },
            KeyCode::Up | KeyCode::Char('k') => {
                self.step(-1);
                OverlayAction::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.step(1);
                OverlayAction::Stay
            }
            KeyCode::PageUp => {
                self.step(-isize::try_from(self.page).unwrap_or(1));
                OverlayAction::Stay
            }
            KeyCode::PageDown => {
                self.step(isize::try_from(self.page).unwrap_or(1));
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.selected = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.selected = self.shown().last().copied().unwrap_or(0);
                OverlayAction::Stay
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.fold();
                OverlayAction::Stay
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.unfold();
                OverlayAction::Stay
            }
            KeyCode::Char(' ') => {
                self.toggle();
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
            item: String::new(),
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
        assert!(rows[main + 1].contains("  ! \u{25be} general"), "{rows:#?}");
        assert!(rows[main + 1].contains("waiting for approval"), "{rows:#?}");
        assert!(rows[main + 2].contains("    "), "{rows:#?}");
        assert!(rows[main + 2].contains(" test "), "{rows:#?}");
        assert!(rows[main + 3].contains("\u{2022}   explore"), "{rows:#?}");
        assert!(rows[main + 3].contains("done"), "{rows:#?}");
        assert!(rows[main + 3].contains("41s  22k tok"), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains(HINT.trim())), "{rows:#?}");
    }

    #[test]
    fn a_subtree_folds_and_unfolds_and_pages_skip_folded_rows() {
        let (mut overlay, ids) = tree();
        overlay.handle_key(key(KeyCode::Down));
        assert_eq!(overlay.selected(), Some(ids[0]));
        overlay.handle_key(key(KeyCode::Left));
        let painted = paint(&mut overlay, 120, 10);
        assert!(
            painted.iter().any(|r| r.contains("\u{25b8} general (+1)")),
            "{painted:#?}"
        );
        assert!(
            !painted.iter().any(|r| r.contains(" test ")),
            "{painted:#?}"
        );
        overlay.handle_key(key(KeyCode::Down));
        assert_eq!(
            overlay.selected(),
            Some(ids[2]),
            "the folded child is skipped"
        );
        overlay.handle_key(key(KeyCode::Up));
        overlay.handle_key(key(KeyCode::Right));
        overlay.handle_key(key(KeyCode::Right));
        assert_eq!(
            overlay.selected(),
            Some(ids[1]),
            "right on an open row steps in"
        );
        overlay.handle_key(key(KeyCode::Left));
        assert_eq!(overlay.selected(), Some(ids[0]), "left on a leaf climbs");
        overlay.handle_key(key(KeyCode::PageDown));
        assert_eq!(overlay.selected(), Some(ids[2]), "a page stops at the end");
        overlay.handle_key(key(KeyCode::PageUp));
        assert_eq!(overlay.selected(), None, "and at the top");
        overlay.wheel(true);
        assert_eq!(overlay.selected(), Some(ids[2]));
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
