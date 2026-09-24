//! Scrollable keyboard-reference overlay (`?` / `/help`).
//!
//! [`HelpOverlay`] is the in-TUI shortcut reference: the live keymap
//! groups (group header, key column, description), then a static
//! "editing (built in)" section for the keys the Rust editor grammar
//! handles, one set per editor style, with plain scrolling.
//! It is a reading surface, not a command surface: every key either
//! scrolls or closes, so it never swallows something the user meant
//! for the session underneath.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

use crate::keymap::HelpGroup;
use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// Title of the static section listing the editor grammar keys.
pub const BUILTIN_SECTION: &str = "editing (built in)";

/// One rendered row of the reference: a group header or a key/desc
/// pair.
#[derive(Debug)]
enum Row {
    /// Bold section header.
    Header(String),
    /// Keys (left column) and what they do (right column).
    Key(String, String),
}

/// Grammar keys of the modeless editor: always insert-like, with Esc
/// cancelling the turn and `/`, `!`, `?` as empty-prompt prefixes.
const MODELESS_BUILTIN: &[(&str, &str)] = &[
    ("Enter", "send the prompt"),
    ("Shift+Enter", "insert a newline (Alt+Enter also works)"),
    ("Up / Down", "previous / next prompt from history"),
    ("Ctrl+A / Ctrl+E", "line start / end"),
    ("Ctrl+K / Ctrl+U", "kill to end / start of line"),
    ("Ctrl+W / Alt+BS", "kill previous word"),
    ("Ctrl+Y", "yank last kill"),
    ("Ctrl+/", "undo last edit"),
    (
        "Ctrl+O",
        "expand a collapsed paste, or fold the focused block",
    ),
    ("Ctrl+G", "edit the prompt in $VISUAL or $EDITOR"),
    ("/ (empty prompt)", "command palette"),
    ("! (empty prompt)", "run a shell command"),
    ("? (empty prompt)", "this reference"),
    ("Esc / Ctrl+C", "cancel the running turn"),
    ("Ctrl+Q", "quit kage (cancels a running turn)"),
];

/// Grammar keys of the vim-style modal editor.
const VIM_BUILTIN: &[(&str, &str)] = &[
    (
        "i / a / I / A",
        "insert mode (from the conversation: focus input)",
    ),
    ("Esc", "normal mode, clear the selection"),
    ("h j k l w b e", "move in the prompt (normal mode)"),
    ("0 $ ^ gg G", "line start / end, prompt start / end"),
    (
        "d c y + motion",
        "operators, doubled for lines (dd), counts (3dw)",
    ),
    ("x X r D C o O", "single-character and line edits"),
    ("p P u Ctrl+R", "paste, undo, redo"),
    ("v", "visual select in the prompt"),
    ("Enter", "submit"),
    ("Shift+Enter", "insert a newline (Alt+Enter also works)"),
    ("Up / Down", "prompt history (insert mode)"),
    ("Ctrl+A / Ctrl+E", "line start / end (insert mode)"),
    (
        "Ctrl+K / Ctrl+U",
        "kill to end / start of line (insert mode)",
    ),
    (
        "Ctrl+W / Ctrl+Y",
        "kill previous word / yank it back (insert mode)",
    ),
    (
        "Ctrl+O (insert)",
        "expand a collapsed paste, or fold the focused block",
    ),
    ("Ctrl+G", "edit the prompt in $VISUAL or $EDITOR"),
    ("/ (empty prompt)", "command palette (insert mode)"),
    ("! (empty prompt)", "run a shell command (insert mode)"),
    ("Ctrl+C", "cancel the running turn"),
    ("Ctrl+Q", "quit kage"),
];

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
    /// Build the reference from `groups`, the live keymap rows for the
    /// active editor style, followed by the built-in editing keys of
    /// that style: the modeless set when `modeless` is true, the vim
    /// set otherwise.
    #[must_use]
    pub fn new(groups: &[HelpGroup], modeless: bool) -> Self {
        let mut rows = Vec::new();
        for group in groups {
            rows.push(Row::Header(group.name.clone()));
            for row in &group.rows {
                rows.push(Row::Key(row.lhs.clone(), row.desc.clone()));
            }
        }
        rows.push(Row::Header(BUILTIN_SECTION.to_owned()));
        let builtin = if modeless {
            MODELESS_BUILTIN
        } else {
            VIM_BUILTIN
        };
        for (keys, desc) in builtin {
            rows.push(Row::Key((*keys).to_owned(), (*desc).to_owned()));
        }
        Self {
            title: " keyboard shortcuts ".to_owned(),
            rows,
            scroll: 0,
            viewport_rows: 1,
        }
    }

    /// The `(keys, desc)` rows above the built-in section.
    #[cfg(test)]
    pub(crate) fn mapped_rows(&self) -> Vec<(&str, &str)> {
        self.rows
            .iter()
            .take_while(|row| !matches!(row, Row::Header(name) if name == BUILTIN_SECTION))
            .filter_map(|row| match row {
                Row::Key(keys, desc) => Some((keys.as_str(), desc.as_str())),
                Row::Header(_) => None,
            })
            .collect()
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
                Row::Header(title) => Line::from(Span::styled(title.clone(), header_style)),
                Row::Key(keys, desc) => Line::from(vec![
                    Span::styled(format!("  {keys:<18}"), key_style),
                    Span::styled(desc.clone(), desc_style),
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
    use crate::keymap::HelpRow;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_enter_and_q_close() {
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('q')] {
            let mut h = HelpOverlay::new(&[], false);
            assert_eq!(h.handle_key(key(code)), OverlayAction::Close);
        }
    }

    #[test]
    fn scrolling_clamps_at_both_ends() {
        let mut h = HelpOverlay::new(&[], false);
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
        let mut h = HelpOverlay::new(&[], false);
        h.viewport_rows = 10;
        h.handle_key(key(KeyCode::PageDown));
        assert_eq!(h.scroll, 9);
        h.handle_key(key(KeyCode::PageUp));
        assert_eq!(h.scroll, 0);
    }

    #[test]
    fn measure_centers_and_caps_height() {
        let h = HelpOverlay::new(&[], false);
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
        let mut h = HelpOverlay::new(&[], false);
        assert_eq!(
            h.handle_key(key(KeyCode::Char('x'))),
            OverlayAction::PropagateKey
        );
    }

    fn key_labels(rows: &[Row]) -> Vec<&str> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Key(keys, _) => Some(keys.as_str()),
                Row::Header(_) => None,
            })
            .collect()
    }

    #[test]
    fn live_groups_come_first_then_the_builtin_section() {
        let groups = [HelpGroup {
            name: "general".to_owned(),
            rows: vec![HelpRow {
                lhs: "<C-p>".to_owned(),
                desc: "model picker".to_owned(),
            }],
        }];
        let h = HelpOverlay::new(&groups, true);
        assert!(matches!(&h.rows[0], Row::Header(name) if name == "general"));
        assert!(
            matches!(&h.rows[1], Row::Key(keys, desc) if keys == "<C-p>" && desc == "model picker")
        );
        assert!(matches!(&h.rows[2], Row::Header(name) if name == BUILTIN_SECTION));
        assert_eq!(h.rows.len(), 3 + MODELESS_BUILTIN.len());
    }

    #[test]
    fn builtin_sections_cover_each_style() {
        let modeless = HelpOverlay::new(&[], true);
        let labels = key_labels(&modeless.rows);
        for wanted in ["Shift+Enter", "! (empty prompt)", "Ctrl+G", "Ctrl+Q"] {
            assert!(labels.contains(&wanted), "missing {wanted}");
        }
        assert!(!labels.iter().any(|l| l.contains("gg")));
        let vim = HelpOverlay::new(&[], false);
        assert!(key_labels(&vim.rows).contains(&"0 $ ^ gg G"));
    }

    #[test]
    fn builtin_labels_are_unique_in_both_sets() {
        for rows in [MODELESS_BUILTIN, VIM_BUILTIN] {
            let mut labels: Vec<&str> = rows.iter().map(|(keys, _)| *keys).collect();
            let count = labels.len();
            labels.sort_unstable();
            labels.dedup();
            assert_eq!(labels.len(), count, "duplicate key label in {rows:?}");
        }
    }
}
