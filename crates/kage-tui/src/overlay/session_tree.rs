//! `:tree` session-forest browser overlay.
//!
//! Sessions are flat JSONL files, but each header's `parent_session`
//! links forks into a forest. This overlay rebuilds that forest from a
//! flat node list, renders it as an indented tree with the active
//! session marked, and lets the user resume (`Enter`), fork (`f`), or
//! delete (`d`) the selected session. It implements [`OverlayWidget`]
//! and resolves with `{ action, path }` JSON the host maps onto the
//! matching `RunRequest`.
//!
//! ASCII only: depth is shown with indentation and a `+` child marker,
//! never box-drawing glyphs.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// Source of session rows, mirroring [`crate::app::SessionLister`].
/// Returns every session with its parent link so the overlay can
/// rebuild the fork forest.
pub type SessionTreeSource = Box<dyn Fn() -> Vec<SessionNode> + Send + 'static>;

/// One session in the forest. `id` and `parent` are session ids as
/// strings; `parent` is `None` for a root (or when the parent is not
/// in the listed set).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionNode {
    /// This session's id (string form).
    pub id: String,
    /// Absolute path to the session file (the resolve payload).
    pub path: String,
    /// Parent session id, if forked.
    pub parent: Option<String>,
    /// One-line summary the row renders (id + prompt + time).
    pub label: String,
    /// Whether this is the session currently loaded in the TUI.
    pub is_current: bool,
}

/// The `:tree` overlay.
#[derive(Debug)]
pub struct SessionTreeOverlay {
    nodes: Vec<SessionNode>,
    /// `(node_index, depth)` in pre-order, the visible row order.
    order: Vec<(usize, usize)>,
    selected: usize,
}

impl SessionTreeOverlay {
    /// Build the overlay, computing the forest layout once. Roots are
    /// nodes with no parent (or whose parent is absent from the set);
    /// children follow their parent in the input's order (the host
    /// lists newest-first). Selection starts on the active session.
    #[must_use]
    pub fn new(nodes: Vec<SessionNode>) -> Self {
        let order = forest_order(&nodes);
        let selected = order
            .iter()
            .position(|&(idx, _)| nodes[idx].is_current)
            .unwrap_or(0);
        Self {
            nodes,
            order,
            selected,
        }
    }

    /// Whether there is nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Number of visible rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// The path of the selected session, or empty when there are none.
    #[must_use]
    pub fn selected_path(&self) -> &str {
        self.order
            .get(self.selected)
            .map_or("", |&(idx, _)| self.nodes[idx].path.as_str())
    }

    fn resolve(&self, action: &str) -> OverlayAction {
        let path = self.selected_path();
        if path.is_empty() {
            return OverlayAction::Stay;
        }
        OverlayAction::Resolve(serde_json::json!({
            "action": action,
            "path": path,
        }))
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
}

/// Pre-order `(node_index, depth)` for the forest in `nodes`. A node
/// is a root when it has no parent or its parent id is not present.
/// Cycles (a corrupt parent chain) cannot loop: each node is emitted
/// at most once via the `seen` set.
fn forest_order(nodes: &[SessionNode]) -> Vec<(usize, usize)> {
    let mut children: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    let ids: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    for (i, n) in nodes.iter().enumerate() {
        if let Some(p) = n.parent.as_deref()
            && ids.contains(p)
        {
            children.entry(p).or_default().push(i);
        }
    }
    let mut order = Vec::new();
    let mut seen = vec![false; nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        let is_root = n.parent.as_deref().is_none_or(|p| !ids.contains(p));
        if is_root {
            push_subtree(i, 0, nodes, &children, &mut seen, &mut order);
        }
    }
    // Any node not reached (e.g. part of a parent cycle) is appended
    // flat so it is never hidden.
    for (i, s) in seen.iter_mut().enumerate() {
        if !*s {
            *s = true;
            order.push((i, 0));
        }
    }
    order
}

fn push_subtree(
    idx: usize,
    depth: usize,
    nodes: &[SessionNode],
    children: &std::collections::HashMap<&str, Vec<usize>>,
    seen: &mut [bool],
    order: &mut Vec<(usize, usize)>,
) {
    if seen[idx] {
        return;
    }
    seen[idx] = true;
    order.push((idx, depth));
    if let Some(kids) = children.get(nodes[idx].id.as_str()) {
        for &k in kids {
            push_subtree(k, depth + 1, nodes, children, seen, order);
        }
    }
}

fn center(available: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (available.width * pct_w / 100).max(1);
    let h = (available.height * pct_h / 100).max(1);
    let x = available.x + (available.width.saturating_sub(w)) / 2;
    let y = available.y + (available.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

impl OverlayWidget for SessionTreeOverlay {
    fn measure(&self, available: Rect) -> Rect {
        center(available, 80, 70)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>) {
        let accent = ctx.theme.focus_color;
        let block = Block::default()
            .title(" sessions ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        if self.order.is_empty() {
            buf.set_line(
                rows[0].x,
                rows[0].y,
                &Line::from(Span::styled(
                    "  no sessions yet",
                    Style::default().fg(ctx.theme.status_dim_fg),
                )),
                rows[0].width,
            );
        } else {
            let height = usize::from(rows[0].height);
            let offset = self
                .selected
                .saturating_sub(height / 2)
                .min(self.order.len().saturating_sub(height));
            for (row, &(idx, depth)) in self.order.iter().enumerate().skip(offset).take(height) {
                let node = &self.nodes[idx];
                let selected = row == self.selected;
                let indent = "  ".repeat(depth);
                let branch = if depth == 0 { "" } else { "+ " };
                let mark = if node.is_current { "* " } else { "  " };
                let text = format!("{mark}{indent}{branch}{}", node.label);
                let style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(accent)
                        .add_modifier(Modifier::BOLD)
                } else if node.is_current {
                    Style::default()
                        .fg(ctx.theme.focus_color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(ctx.theme.assistant_fg)
                };
                buf.set_line(
                    rows[0].x,
                    rows[0].y + u16::try_from(row - offset).unwrap_or(0),
                    &Line::from(Span::styled(text, style)),
                    rows[0].width,
                );
            }
        }

        buf.set_line(
            rows[1].x,
            rows[1].y,
            &Line::from(Span::styled(
                "Up/Down move - Enter resume - f fork - d delete - Esc close",
                Style::default().fg(ctx.theme.status_dim_fg),
            )),
            rows[1].width,
        );
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
                if self.selected + 1 < self.order.len() {
                    self.selected += 1;
                }
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.selected = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.selected = self.order.len().saturating_sub(1);
                OverlayAction::Stay
            }
            KeyCode::Enter => self.resolve("resume"),
            KeyCode::Char('f') => self.resolve("fork"),
            KeyCode::Char('d') => self.resolve("delete"),
            _ => OverlayAction::Stay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn node(id: &str, parent: Option<&str>, current: bool) -> SessionNode {
        SessionNode {
            id: id.to_owned(),
            path: format!("/s/{id}.jsonl"),
            parent: parent.map(str::to_owned),
            label: id.to_owned(),
            is_current: current,
        }
    }

    #[test]
    fn forest_orders_children_under_parents() {
        // root -> child -> grandchild, plus a second root.
        let nodes = vec![
            node("root", None, false),
            node("child", Some("root"), false),
            node("grand", Some("child"), false),
            node("other", None, false),
        ];
        let t = SessionTreeOverlay::new(nodes);
        let ids: Vec<(&str, usize)> = t
            .order
            .iter()
            .map(|&(i, d)| (t.nodes[i].id.as_str(), d))
            .collect();
        assert_eq!(
            ids,
            vec![("root", 0), ("child", 1), ("grand", 2), ("other", 0)]
        );
    }

    #[test]
    fn orphan_parent_is_treated_as_root() {
        // parent id "ghost" is not in the set -> "lonely" is a root.
        let nodes = vec![node("lonely", Some("ghost"), false)];
        let t = SessionTreeOverlay::new(nodes);
        assert_eq!(t.order, vec![(0, 0)]);
    }

    #[test]
    fn parent_cycle_does_not_loop_and_shows_every_node() {
        let nodes = vec![node("a", Some("b"), false), node("b", Some("a"), false)];
        let t = SessionTreeOverlay::new(nodes);
        assert_eq!(t.len(), 2, "both nodes still listed despite the cycle");
    }

    #[test]
    fn selection_starts_on_current_session() {
        let nodes = vec![
            node("a", None, false),
            node("b", None, true),
            node("c", None, false),
        ];
        let t = SessionTreeOverlay::new(nodes);
        assert_eq!(t.selected_path(), "/s/b.jsonl");
    }

    #[test]
    fn enter_f_d_resolve_with_action_and_path() {
        let mut t = SessionTreeOverlay::new(vec![node("a", None, true)]);
        for (k, want) in [
            (KeyCode::Enter, "resume"),
            (KeyCode::Char('f'), "fork"),
            (KeyCode::Char('d'), "delete"),
        ] {
            match t.handle_key(key(k)) {
                OverlayAction::Resolve(v) => {
                    assert_eq!(v["action"], serde_json::json!(want));
                    assert_eq!(v["path"], serde_json::json!("/s/a.jsonl"));
                }
                other => panic!("expected Resolve for {k:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn navigation_clamps_and_esc_closes() {
        let nodes = vec![node("a", None, false), node("b", None, false)];
        let mut t = SessionTreeOverlay::new(nodes);
        t.handle_key(key(KeyCode::Up)); // already at 0, clamps
        assert_eq!(t.selected, 0);
        t.handle_key(key(KeyCode::Down));
        assert_eq!(t.selected, 1);
        t.handle_key(key(KeyCode::Down)); // clamps at last
        assert_eq!(t.selected, 1);
        assert_eq!(t.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
    }

    #[test]
    fn empty_tree_resolves_to_stay() {
        let mut t = SessionTreeOverlay::new(Vec::new());
        assert!(t.is_empty());
        assert_eq!(t.handle_key(key(KeyCode::Enter)), OverlayAction::Stay);
    }
}
