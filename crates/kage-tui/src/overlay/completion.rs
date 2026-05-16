//! Prompt-input autocomplete popup.
//!
//! A light, non-modal suggestion list floated just above the input
//! card (like the slash palette), driven by the plugin autocomplete
//! provider stack (`kage.add_autocomplete_provider`). Unlike the slash
//! palette it never takes full keyboard ownership: only Up / Down /
//! Ctrl-p / Ctrl-n (navigate), Tab (accept), and Esc (dismiss) are
//! consumed. Every other key passes through to normal text editing,
//! after which the host re-queries the provider stack and rebuilds the
//! popup. The host computes the splice range when a candidate is
//! accepted; this widget only owns selection and painting.

use kage_plugin::AutocompleteItem;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::layout::Regions;

/// Most visible rows before `... N more` indicators kick in.
const MAX_VISIBLE: usize = 8;

/// Outcome of routing a key into an open completion popup.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionAction {
    /// Key consumed; the selection moved and the popup stays open.
    Navigated,
    /// User dismissed the popup (Esc); key consumed, no edit.
    Dismissed,
    /// User accepted a candidate (Tab); key consumed. The host applies
    /// it via [`crate::input::InputState::splice`].
    Accepted(AutocompleteItem),
    /// The popup does not own this key; the host routes it to the
    /// input and then re-queries the provider stack.
    PassThrough,
}

/// Inline autocomplete popup for the prompt input.
#[derive(Debug)]
pub struct InputCompletion {
    items: Vec<AutocompleteItem>,
    selected: usize,
}

impl InputCompletion {
    /// Build a popup for `items`, or `None` when there is nothing to
    /// show so the host can clear any open popup uniformly.
    #[must_use]
    pub fn new(items: Vec<AutocompleteItem>) -> Option<Self> {
        if items.is_empty() {
            None
        } else {
            Some(Self { items, selected: 0 })
        }
    }

    /// Index of the highlighted row.
    #[cfg(test)]
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Number of candidates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the popup has no candidates. Always `false` for a popup
    /// built through [`Self::new`]; present so clippy does not nag
    /// about a `len` without an `is_empty`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Route a key. See [`CompletionAction`].
    pub fn handle_key(&mut self, key: KeyEvent) -> CompletionAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down => {
                self.select_next();
                CompletionAction::Navigated
            }
            KeyCode::Up => {
                self.select_prev();
                CompletionAction::Navigated
            }
            KeyCode::Char('n') if ctrl => {
                self.select_next();
                CompletionAction::Navigated
            }
            KeyCode::Char('p') if ctrl => {
                self.select_prev();
                CompletionAction::Navigated
            }
            KeyCode::Tab => CompletionAction::Accepted(self.items[self.selected].clone()),
            KeyCode::Esc => CompletionAction::Dismissed,
            _ => CompletionAction::PassThrough,
        }
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

    /// Paint the popup above the input card. The host draws the rest
    /// of the frame first, then calls this when the popup is open.
    pub fn render(&self, frame: &mut Frame, regions: Regions) {
        let Some(area) = popup_area(regions, self.items.len(), self.selected) else {
            return;
        };
        frame.render_widget(Clear, area);
        let lines = self.rows(usize::from(area.width));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn rows(&self, width: usize) -> Vec<Line<'static>> {
        let theme = crate::theme::current();
        let bg = theme.modeline_bg;
        let row_style = Style::default().fg(Color::White).bg(bg);
        let dim_style = Style::default().fg(theme.status_dim_fg).bg(bg);
        let sel_style = Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);

        let total = self.items.len();
        let max_visible = MAX_VISIBLE.min(total);
        let (offset, window) = scroll_window(self.selected, total, max_visible);
        let above = offset;
        let below = total.saturating_sub(offset + window);

        let label_col = self
            .items
            .iter()
            .skip(offset)
            .take(window)
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(0);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if above > 0 {
            lines.push(Line::from(Span::styled(
                pad(&format!("  ... {above} more above"), width),
                dim_style,
            )));
        }
        for (idx, item) in self.items.iter().enumerate().skip(offset).take(window) {
            let selected = idx == self.selected;
            let main = if selected { sel_style } else { row_style };
            let aside = if selected { sel_style } else { dim_style };
            lines.push(row(item, label_col, width, main, aside));
        }
        if below > 0 {
            lines.push(Line::from(Span::styled(
                pad(&format!("  ... {below} more below"), width),
                dim_style,
            )));
        }
        lines
    }
}

fn row(
    item: &AutocompleteItem,
    label_col: usize,
    width: usize,
    main: Style,
    aside: Style,
) -> Line<'static> {
    let leading = "  ";
    let label_chars = item.label.chars().count();
    let gap = label_col.saturating_sub(label_chars);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(leading.to_owned(), main),
        Span::styled(item.label.clone(), main),
    ];
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), main));
    }
    if let Some(detail) = item.detail.as_deref() {
        let used = leading.chars().count() + label_chars + gap;
        let room = width.saturating_sub(used).saturating_sub(2);
        if room > 0 {
            spans.push(Span::styled("  ".to_owned(), aside));
            spans.push(Span::styled(truncate(detail, room), aside));
        }
    }
    let painted: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if painted < width {
        spans.push(Span::styled(" ".repeat(width - painted), main));
    }
    Line::from(spans)
}

/// The rect just above the input card, clamped to the space between
/// the conversation buffer and the input. `None` when there is no
/// room or nothing to show.
fn popup_area(regions: Regions, total: usize, selected: usize) -> Option<Rect> {
    if total == 0 || regions.input.width == 0 {
        return None;
    }
    let max_visible = MAX_VISIBLE.min(total);
    let (offset, window) = scroll_window(selected, total, max_visible);
    let above = offset;
    let below = total.saturating_sub(offset + window);
    let rows = window + usize::from(above > 0) + usize::from(below > 0);
    let space_above = regions.input.y.saturating_sub(regions.buffer.y);
    let rows = rows.min(usize::from(space_above));
    if rows == 0 {
        return None;
    }
    let height = u16::try_from(rows).unwrap_or(u16::MAX);
    Some(Rect {
        x: regions.input.x,
        y: regions.input.y.saturating_sub(height),
        width: regions.input.width,
        height,
    })
}

/// `(offset, window)` such that `selected` is visible within at most
/// `max_visible` rows.
fn scroll_window(selected: usize, total: usize, max_visible: usize) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let half = max_visible / 2;
    let offset = selected
        .saturating_sub(half)
        .min(total.saturating_sub(max_visible));
    (offset, max_visible)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('\u{2026}');
    out
}

fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.chars().take(width).collect()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// The completion prefix: the run of non-whitespace characters
/// immediately before `cursor`. Providers also receive the full text
/// and cursor so they can tokenize differently (e.g. an `@file`
/// trigger); this is only the host's default replace span.
#[must_use]
pub fn prefix_before_cursor(text: &str, cursor: usize) -> &str {
    let head = &text[..cursor.min(text.len())];
    let start = head
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(idx, c)| idx + c.len_utf8());
    &head[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: &str) -> AutocompleteItem {
        AutocompleteItem {
            label: value.to_owned(),
            detail: None,
            value: value.to_owned(),
            range: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn new_is_none_when_empty() {
        assert!(InputCompletion::new(Vec::new()).is_none());
    }

    #[test]
    fn down_and_up_wrap_around() {
        let mut c = InputCompletion::new(vec![item("a"), item("b")]).unwrap();
        assert_eq!(c.selected_index(), 0);
        assert_eq!(
            c.handle_key(key(KeyCode::Down)),
            CompletionAction::Navigated
        );
        assert_eq!(c.selected_index(), 1);
        c.handle_key(key(KeyCode::Down));
        assert_eq!(c.selected_index(), 0);
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.selected_index(), 1);
    }

    #[test]
    fn ctrl_n_ctrl_p_navigate() {
        let mut c = InputCompletion::new(vec![item("a"), item("b")]).unwrap();
        let cn = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(c.handle_key(cn), CompletionAction::Navigated);
        assert_eq!(c.selected_index(), 1);
        let cp = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        c.handle_key(cp);
        assert_eq!(c.selected_index(), 0);
    }

    #[test]
    fn tab_accepts_selected_item() {
        let mut c = InputCompletion::new(vec![item("first"), item("second")]).unwrap();
        c.handle_key(key(KeyCode::Down));
        match c.handle_key(key(KeyCode::Tab)) {
            CompletionAction::Accepted(it) => assert_eq!(it.value, "second"),
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses_and_other_keys_pass_through() {
        let mut c = InputCompletion::new(vec![item("a")]).unwrap();
        assert_eq!(c.handle_key(key(KeyCode::Esc)), CompletionAction::Dismissed);
        assert_eq!(
            c.handle_key(key(KeyCode::Char('x'))),
            CompletionAction::PassThrough
        );
    }

    #[test]
    fn prefix_is_run_before_cursor() {
        assert_eq!(prefix_before_cursor("hello wor", 9), "wor");
        assert_eq!(prefix_before_cursor("hello wor", 5), "hello");
        assert_eq!(prefix_before_cursor("", 0), "");
        assert_eq!(prefix_before_cursor("@src/m", 6), "@src/m");
        assert_eq!(prefix_before_cursor("a b", 2), "");
    }
}
