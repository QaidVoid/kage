//! Scrollable keyboard-reference overlay (`?` / `/help`).
//!
//! [`HelpOverlay`] is the in-TUI shortcut reference: static grouped
//! rows (group header, key column, description), one set per editor
//! style, with plain scrolling.
//! It is a reading surface, not a command surface: every key either
//! scrolls or closes, so it never swallows something the user meant
//! for the session underneath.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// One rendered row of the reference: a group header or a key/desc
/// pair.
#[derive(Debug)]
enum Row {
    /// Bold section header.
    Header(&'static str),
    /// A chord (left column) and what it does (right column).
    Key(&'static str, &'static str),
}

/// Reference for the modeless editor: always insert-like, with Esc
/// cancelling the turn and `/`, `!`, `?` as empty-prompt prefixes.
fn modeless_rows() -> Vec<Row> {
    use Row::{Header, Key};
    vec![
        Header("prompt editing"),
        Key("Enter", "send the prompt"),
        Key("Shift+Enter", "insert a newline (Alt+Enter also works)"),
        Key("Ctrl+A / Ctrl+E", "line start / end"),
        Key("Ctrl+K / Ctrl+U", "kill to end / start of line"),
        Key("Ctrl+W", "kill previous word"),
        Key("Ctrl+Y", "yank last kill"),
        Key("Ctrl+/", "undo last edit"),
        Key("Up / Down", "previous / next prompt from history"),
        Key("Ctrl+G", "edit the prompt in $VISUAL or $EDITOR"),
        Key("Ctrl+V", "attach image from clipboard"),
        Header("empty prompt"),
        Key("/", "command palette"),
        Key("!", "run a shell command"),
        Key("?", "this reference"),
        Header("conversation"),
        Key("PageUp / PageDown", "scroll ten lines"),
        Key("Ctrl+Up / Down", "scroll one line"),
        Key("Ctrl+Home / End", "jump to top / bottom"),
        Key("Alt+P / Alt+N", "focus previous / next block"),
        Key("Ctrl+O", "fold / unfold the focused block"),
        Key("F3", "jump to a message"),
        Header("pickers & overlays"),
        Key("Ctrl+P", "model picker"),
        Key("Ctrl+S", "session picker"),
        Key("Shift+Tab", "cycle thinking level"),
        Key("/settings", "theme, model, mouse, thinking"),
        Header("during a turn"),
        Key("Esc / Ctrl+C", "cancel the running turn"),
        Key("/compact", "compact history now"),
        Header("leave"),
        Key("Ctrl+Q", "quit kage (cancels a running turn)"),
        Key("/quit", "quit kage"),
    ]
}

/// Reference for the vim-style modal editor.
fn vim_rows() -> Vec<Row> {
    use Row::{Header, Key};
    vec![
        Header("modes"),
        Key("i", "edit the prompt (from normal mode)"),
        Key("Esc", "normal mode / close popups / cancel turn"),
        Key("Ctrl+W (normal)", "cycle pane focus (input / buffer)"),
        Key("?", "this reference (normal mode)"),
        Header("buffer (normal mode)"),
        Key("j / k", "scroll one line"),
        Key("gg / G", "jump to top / bottom"),
        Key("[ / ]", "focus previous / next block"),
        Key("Ctrl+O", "fold / unfold the focused block"),
        Key("zM / zR", "fold all / unfold all"),
        Key("/ (normal)", "search the buffer (n / N walk matches)"),
        Key("y / v", "yank selection / start visual select"),
        Header("prompt editing"),
        Key("Enter", "submit"),
        Key("Shift+Enter", "insert a newline (Alt+Enter also works)"),
        Key("Ctrl+A / Ctrl+E", "line start / end"),
        Key("Ctrl+K / Ctrl+U", "kill to end / start of line"),
        Key("Ctrl+W (insert)", "kill previous word"),
        Key("Ctrl+Y", "yank last kill"),
        Key("Ctrl+/", "undo last edit"),
        Key("Ctrl+G", "edit the prompt in $VISUAL or $EDITOR"),
        Key("/ (empty prompt)", "command palette"),
        Key("! (empty prompt)", "run a shell command"),
        Header("pickers & overlays"),
        Key("F3", "jump to a message"),
        Key("Ctrl+P", "model picker"),
        Key("Ctrl+S", "session picker"),
        Key("Shift+Tab", "cycle thinking level"),
        Key(":", "command line (normal mode)"),
        Key(":settings", "theme, model, mouse, thinking"),
        Key("Ctrl+V", "attach image from clipboard"),
        Header("during a turn"),
        Key("Ctrl+C", "cancel the running turn"),
        Key(":compact", "compact history now"),
        Header("leave"),
        Key("Ctrl+Q", "quit kage"),
        Key(":q", "quit kage"),
    ]
}

/// The `?` keyboard reference modal.
#[derive(Debug)]
pub struct HelpOverlay {
    title: String,
    rows: Vec<Row>,
    /// First visible row index.
    scroll: usize,
    /// Inner height from the last paint, used to clamp scrolling
    /// before the next frame and to size page jumps.
    viewport_rows: u16,
}

impl HelpOverlay {
    /// Build the reference for the active editor style: the modeless
    /// set when `modeless` is true, the vim set otherwise.
    #[must_use]
    pub fn new(modeless: bool) -> Self {
        Self {
            title: " keyboard shortcuts ".to_owned(),
            rows: if modeless {
                modeless_rows()
            } else {
                vim_rows()
            },
            scroll: 0,
            viewport_rows: 1,
        }
    }

    /// Clamp the scroll offset so the last row stays visible.
    fn clamp_scroll(&mut self) {
        let visible = usize::from(self.viewport_rows.max(1));
        let max = self.rows.len().saturating_sub(visible);
        self.scroll = self.scroll.min(max);
    }

    /// Move the selection by `delta` rows in either direction.
    fn scroll_by(&mut self, delta: isize) {
        let current = isize::try_from(self.scroll).unwrap_or(0);
        let moved = current + delta;
        self.scroll = usize::try_from(moved.max(0)).unwrap_or(0);
        self.clamp_scroll();
    }
}

impl OverlayWidget for HelpOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = 76.min(available.width.saturating_sub(2));
        let wanted = u16::try_from(self.rows.len()).unwrap_or(u16::MAX) + 2;
        let height = wanted.min(available.height.saturating_sub(2));
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        Widget::render(crate::opaque::OpaqueClear, area, buf);
        let theme = crate::theme::current();
        let block = Block::default()
            .title(self.title.clone())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.overlay_border));
        let inner = block.inner(area);
        Widget::render(block, area, buf);
        self.viewport_rows = inner.height;
        self.clamp_scroll();

        let key_style = Style::default()
            .fg(theme.overlay_fg)
            .add_modifier(Modifier::BOLD);
        let desc_style = Style::default().fg(theme.overlay_fg);
        let header_style = Style::default()
            .fg(theme.focus_color)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(theme.muted_fg);

        let lines: Vec<Line> = self
            .rows
            .iter()
            .map(|row| match row {
                Row::Header(title) => Line::from(Span::styled((*title).to_owned(), header_style)),
                Row::Key(keys, desc) => Line::from(vec![
                    Span::styled(format!("  {keys:<18}"), key_style),
                    Span::styled((*desc).to_owned(), desc_style),
                ]),
            })
            .collect();

        let body = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(self.scroll).unwrap_or(u16::MAX), 0));
        Widget::render(body, inner, buf);

        // Scroll hint in the bottom-right corner of the box, over the
        // border row, so the affordance is visible without a footer.
        let more_below = self.scroll + usize::from(inner.height) < self.rows.len();
        if more_below && area.width > 12 {
            let hint_width =
                u16::try_from(crate::view::UnicodeWidthStr::width(" more: j/k ")).unwrap_or(11);
            let x = area.x + area.width.saturating_sub(hint_width + 1);
            let y = area.y + area.height.saturating_sub(1);
            let hint_area = Rect::new(x, y, hint_width, 1);
            Widget::render(
                Paragraph::new(Line::from(Span::styled(" more: j/k ", muted_style))),
                hint_area,
                buf,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        let page = isize::from(i16::try_from(self.viewport_rows.max(1)).unwrap_or(i16::MAX)) - 1;
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'Q') => OverlayAction::Close,
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_by(1);
                OverlayAction::Stay
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_by(-1);
                OverlayAction::Stay
            }
            KeyCode::PageDown => {
                self.scroll_by(page);
                OverlayAction::Stay
            }
            KeyCode::PageUp => {
                self.scroll_by(-page);
                OverlayAction::Stay
            }
            KeyCode::Home => {
                self.scroll = 0;
                OverlayAction::Stay
            }
            KeyCode::End => {
                self.scroll = usize::MAX;
                self.clamp_scroll();
                OverlayAction::Stay
            }
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

    #[test]
    fn esc_enter_and_q_close() {
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('q')] {
            let mut h = HelpOverlay::new(false);
            assert_eq!(h.handle_key(key(code)), OverlayAction::Close);
        }
    }

    #[test]
    fn scrolling_clamps_at_both_ends() {
        let mut h = HelpOverlay::new(false);
        let bottom = h.rows.len();
        for _ in 0..(bottom + 50) {
            h.handle_key(key(KeyCode::Down));
        }
        let max_scroll = h.rows.len() - usize::from(h.viewport_rows.max(1));
        assert!(
            h.scroll <= max_scroll,
            "scroll must clamp, got {}",
            h.scroll
        );
        for _ in 0..100 {
            h.handle_key(key(KeyCode::Up));
        }
        assert_eq!(h.scroll, 0);
    }

    #[test]
    fn page_down_jumps_by_viewport() {
        let mut h = HelpOverlay::new(false);
        h.viewport_rows = 10;
        h.handle_key(key(KeyCode::PageDown));
        assert_eq!(h.scroll, 9);
        h.handle_key(key(KeyCode::PageUp));
        assert_eq!(h.scroll, 0);
    }

    #[test]
    fn measure_centers_and_caps_height() {
        let h = HelpOverlay::new(false);
        let area = Rect::new(0, 0, 200, 100);
        let m = h.measure(area);
        assert!(m.width < area.width, "must not span the full width");
        let wanted_height = u16::try_from(h.rows.len()).unwrap_or(u16::MAX) + 2;
        assert!(m.height <= wanted_height);
        let tiny = Rect::new(0, 0, 30, 6);
        let m = h.measure(tiny);
        assert!(m.width <= tiny.width && m.height <= tiny.height);
    }

    #[test]
    fn other_keys_propagate() {
        let mut h = HelpOverlay::new(false);
        assert_eq!(
            h.handle_key(key(KeyCode::Char('x'))),
            OverlayAction::PropagateKey
        );
    }

    fn key_labels(rows: &[Row]) -> Vec<&'static str> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Key(keys, _) => Some(*keys),
                Row::Header(_) => None,
            })
            .collect()
    }

    #[test]
    fn modeless_rows_cover_modeless_keys_and_skip_vim_motions() {
        let labels = key_labels(&modeless_rows());
        for wanted in ["Shift+Enter", "!", "Ctrl+G", "Shift+Tab"] {
            assert!(labels.contains(&wanted), "missing {wanted}");
        }
        assert!(!labels.contains(&"gg / G"));
    }

    #[test]
    fn key_labels_are_unique_in_both_sets() {
        for rows in [modeless_rows(), vim_rows()] {
            let labels = key_labels(&rows);
            let mut deduped = labels.clone();
            deduped.sort_unstable();
            deduped.dedup();
            assert_eq!(
                deduped.len(),
                labels.len(),
                "duplicate key label in {labels:?}"
            );
        }
    }
}
