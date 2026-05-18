//! Right-click context menu.
//!
//! A small floating menu opened by a right mouse press over a
//! conversation block. It owns selection and painting; the host owns
//! what each action does and which block it targets. For now the only
//! row is Copy (yank the whole block under the cursor as raw source);
//! the design leaves room for Revert and friends later.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// What a context-menu row does. The host interprets it against the
/// block the menu was opened over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    /// Copy the whole target block's raw source to the clipboard.
    Copy,
}

/// One selectable row.
#[derive(Debug, Clone, Copy)]
struct ContextItem {
    label: &'static str,
    action: ContextAction,
}

/// Outcome of routing a key into an open context menu.
#[derive(Debug, PartialEq, Eq)]
pub enum ContextMenuOutcome {
    /// Selection moved (or the key was swallowed); the menu stays
    /// open. Key consumed.
    Navigated,
    /// The menu was dismissed (Esc). Key consumed, no action.
    Dismissed,
    /// A row was activated. Key consumed; the host runs the action.
    Activated(ContextAction),
}

/// A floating right-click menu acting on one conversation block.
#[derive(Debug)]
pub struct ContextMenu {
    items: Vec<ContextItem>,
    selected: usize,
    /// Screen cell `(col, row)` the right-click landed on; the menu's
    /// top-left anchor before viewport clamping.
    anchor: (u16, u16),
    /// Index of the block the menu acts on, captured at open time so
    /// a later focus change cannot retarget it.
    block_idx: usize,
}

impl ContextMenu {
    /// Open a menu for `block_idx`, anchored at screen `(col, row)`.
    #[must_use]
    pub fn new(col: u16, row: u16, block_idx: usize) -> Self {
        Self {
            items: vec![ContextItem {
                label: "Copy",
                action: ContextAction::Copy,
            }],
            selected: 0,
            anchor: (col, row),
            block_idx,
        }
    }

    /// The block this menu acts on.
    #[must_use]
    pub fn block_idx(&self) -> usize {
        self.block_idx
    }

    /// Index of the highlighted row.
    #[cfg(test)]
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    /// Route a key. The menu is modal while open: navigation and
    /// activation are consumed, Esc dismisses, and every other key is
    /// swallowed (Esc is always available, so it cannot wedge the UI).
    pub fn handle_key(&mut self, key: KeyEvent) -> ContextMenuOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                ContextMenuOutcome::Navigated
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev();
                ContextMenuOutcome::Navigated
            }
            KeyCode::Char('n') if ctrl => {
                self.select_next();
                ContextMenuOutcome::Navigated
            }
            KeyCode::Char('p') if ctrl => {
                self.select_prev();
                ContextMenuOutcome::Navigated
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                ContextMenuOutcome::Activated(self.items[self.selected].action)
            }
            KeyCode::Esc => ContextMenuOutcome::Dismissed,
            _ => ContextMenuOutcome::Navigated,
        }
    }

    /// Route a left click at screen `(col, row)`. `Some(action)` when
    /// it landed on a row (the host runs it, then closes the menu);
    /// `None` when it fell outside or on the border (the host closes
    /// the menu and consumes the click so it does not also start a
    /// drag selection).
    pub fn handle_click(&mut self, viewport: Rect, col: u16, row: u16) -> Option<ContextAction> {
        let area = self.area(viewport);
        let inside = col >= area.x
            && col < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height);
        if !inside {
            return None;
        }
        let first_row = area.y.saturating_add(1);
        let rel = usize::from(row.checked_sub(first_row)?);
        let item = self.items.get(rel)?;
        self.selected = rel;
        Some(item.action)
    }

    /// The clamped on-screen rect: a box at the click anchor, shifted
    /// so it stays fully inside `viewport`.
    fn area(&self, viewport: Rect) -> Rect {
        let label_w = self
            .items
            .iter()
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(0);
        // borders (2) + a space of padding on each side (2).
        let want_w = u16::try_from(label_w.saturating_add(4)).unwrap_or(u16::MAX);
        let want_h = u16::try_from(self.items.len().saturating_add(2)).unwrap_or(u16::MAX);
        let width = want_w.min(viewport.width.max(1));
        let height = want_h.min(viewport.height.max(1));
        let max_x = viewport
            .x
            .saturating_add(viewport.width)
            .saturating_sub(width);
        let max_y = viewport
            .y
            .saturating_add(viewport.height)
            .saturating_sub(height);
        let (ax, ay) = self.anchor;
        let x = ax.clamp(viewport.x, max_x.max(viewport.x));
        let y = ay.clamp(viewport.y, max_y.max(viewport.y));
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Paint the menu over `viewport`. The host draws the frame first,
    /// then calls this when the menu is open.
    pub fn render(&self, frame: &mut Frame, viewport: Rect) {
        let area = self.area(viewport);
        if area.width < 3 || area.height < 3 {
            return;
        }
        let theme = crate::theme::current();
        frame.render_widget(crate::opaque::OpaqueClear, area);
        let border = Style::default().fg(theme.focus_color).bg(theme.bg);
        let normal = Style::default().fg(Color::White).bg(theme.bg);
        let sel = Style::default()
            .fg(theme.bg)
            .bg(theme.selection_color)
            .add_modifier(Modifier::BOLD);
        let inner_w = usize::from(area.width).saturating_sub(2);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.items.len());
        for (idx, item) in self.items.iter().enumerate() {
            let style = if idx == self.selected { sel } else { normal };
            let mut text = format!(" {}", item.label);
            let pad = inner_w.saturating_sub(text.chars().count());
            text.push_str(&" ".repeat(pad));
            lines.push(Line::from(Span::styled(text, style)));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .style(Style::default().bg(theme.bg));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn new_has_one_copy_row_targeting_the_block() {
        let m = ContextMenu::new(4, 7, 3);
        assert_eq!(m.block_idx(), 3);
        assert_eq!(m.selected_index(), 0);
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.items[0].action, ContextAction::Copy);
    }

    #[test]
    fn enter_activates_copy_and_esc_dismisses() {
        let mut m = ContextMenu::new(0, 0, 0);
        assert_eq!(
            m.handle_key(key(KeyCode::Enter)),
            ContextMenuOutcome::Activated(ContextAction::Copy)
        );
        assert_eq!(
            m.handle_key(key(KeyCode::Esc)),
            ContextMenuOutcome::Dismissed
        );
    }

    #[test]
    fn unrelated_keys_are_swallowed_not_passed_through() {
        let mut m = ContextMenu::new(0, 0, 0);
        // No PassThrough variant exists: a stray key keeps the menu
        // open rather than leaking into the editor behind it.
        assert_eq!(
            m.handle_key(key(KeyCode::Char('x'))),
            ContextMenuOutcome::Navigated
        );
    }

    #[test]
    fn click_on_the_copy_row_returns_the_action() {
        let mut m = ContextMenu::new(2, 2, 0);
        let viewport = Rect::new(0, 0, 40, 20);
        let area = m.area(viewport);
        // First row inside the top border.
        let hit = m.handle_click(viewport, area.x + 1, area.y + 1);
        assert_eq!(hit, Some(ContextAction::Copy));
    }

    #[test]
    fn click_outside_or_on_border_returns_none() {
        let mut m = ContextMenu::new(2, 2, 0);
        let viewport = Rect::new(0, 0, 40, 20);
        let area = m.area(viewport);
        // Far outside.
        assert_eq!(m.handle_click(viewport, area.x + 30, area.y + 10), None);
        // On the top border row (no item there).
        assert_eq!(m.handle_click(viewport, area.x + 1, area.y), None);
    }

    #[test]
    fn area_is_clamped_into_the_viewport_from_a_corner_anchor() {
        // Anchored past the bottom-right corner: the box must shift
        // back so it stays fully on screen.
        let m = ContextMenu::new(60, 30, 0);
        let viewport = Rect::new(0, 0, 20, 10);
        let area = m.area(viewport);
        assert!(area.x + area.width <= viewport.width, "got {area:?}");
        assert!(area.y + area.height <= viewport.height, "got {area:?}");
    }
}
