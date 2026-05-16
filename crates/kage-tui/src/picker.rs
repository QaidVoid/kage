//! Tiny interactive picker for short menus run outside the main TUI.
//!
//! [`pick`] takes ownership of the terminal in raw mode for the
//! duration of the call, draws a search-as-you-type list, and returns
//! the selected item (or `None` for Esc / Ctrl+C). Drop guards restore
//! the terminal state on every exit path so a panic does not strand
//! the tty.
//!
//! Designed for one-shot prompts like `kage auth login` rather than
//! the persistent main loop in [`crate::App`].

use std::io::{self, Write};

use ratatui::crossterm::{
    cursor,
    event::{self as ct_event, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal,
};

use crate::error::TuiError;

/// One row in a [`pick`] menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickItem {
    /// Identifier returned to the caller when this row is selected.
    pub value: String,
    /// User-visible label shown in the list.
    pub label: String,
    /// Optional one-character status ('*' for "ready", '-' for "missing").
    pub badge: Option<char>,
    /// Optional section this row belongs to. When set, the overlay
    /// picker draws a non-selectable header each time the group
    /// changes (e.g. a date for sessions, a provider for models).
    /// Rows must already be ordered so a group is contiguous; the
    /// caller owns that ordering.
    pub group: Option<String>,
    /// Optional right-aligned trailing column (e.g. a session time).
    /// The overlay picker flushes it to the right edge and truncates
    /// the label if needed so it always fits, so callers must not pad
    /// the label themselves.
    pub right: Option<String>,
}

impl PickItem {
    /// Construct a row with `value` used for both the value and label,
    /// and no badge.
    #[must_use]
    pub fn simple(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            badge: None,
            group: None,
            right: None,
        }
    }

    /// Add or replace the row's status badge.
    #[must_use]
    pub fn with_badge(mut self, badge: char) -> Self {
        self.badge = Some(badge);
        self
    }

    /// Replace the label without touching the value.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Assign the section this row belongs to (see [`Self::group`]).
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Set the right-aligned trailing column (see [`Self::right`]).
    #[must_use]
    pub fn with_right(mut self, right: impl Into<String>) -> Self {
        self.right = Some(right.into());
        self
    }
}

/// Show `items` in a search-as-you-type picker with `prompt` as the
/// header. Items are sorted alphabetically by label so unfamiliar
/// lists feel predictable. Returns the chosen item's value, or
/// `None` for cancel.
pub fn pick(prompt: &str, items: &[PickItem]) -> Result<Option<String>, TuiError> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| a.label.cmp(&b.label));
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    let _restore = RawModeGuard;
    execute!(stdout, cursor::Hide)?;
    let result = run(&mut stdout, prompt, &sorted);
    let _ = execute!(stdout, cursor::Show);
    result
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn run(out: &mut io::Stdout, prompt: &str, items: &[PickItem]) -> Result<Option<String>, TuiError> {
    let mut selected: usize = 0;
    let mut search = String::new();
    let mut scroll_offset: usize = 0;
    let mut prev_lines: usize = 0;

    loop {
        let filtered = filter(items, &search);
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }
        let max_visible = max_visible_rows();
        let (offset, window) = compute_window(scroll_offset, selected, filtered.len(), max_visible);
        scroll_offset = offset;

        clear_lines(out, prev_lines)?;
        prev_lines = render(
            out,
            prompt,
            &search,
            items,
            &filtered,
            selected,
            scroll_offset,
            window,
        )?;

        let CtEvent::Key(key) = ct_event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            clear_lines(out, prev_lines)?;
            return Ok(None);
        }
        match key.code {
            KeyCode::Esc => {
                clear_lines(out, prev_lines)?;
                return Ok(None);
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down if selected + 1 < filtered.len() => selected += 1,
            KeyCode::PageUp => selected = selected.saturating_sub(max_visible),
            KeyCode::PageDown => {
                selected = (selected + max_visible).min(filtered.len().saturating_sub(1));
            }
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = filtered.len().saturating_sub(1),
            KeyCode::Enter => {
                if let Some(&idx) = filtered.get(selected) {
                    clear_lines(out, prev_lines)?;
                    return Ok(Some(items[idx].value.clone()));
                }
            }
            KeyCode::Backspace => {
                search.pop();
                selected = 0;
                scroll_offset = 0;
            }
            KeyCode::Char(c) => {
                search.push(c);
                selected = 0;
                scroll_offset = 0;
            }
            _ => {}
        }
    }
}

/// Pick a visible-row count that leaves room for the header (1),
/// search line (1), help line (1), and one safety row, and floors at
/// 3 so we always show at least three items even on tiny terminals.
fn max_visible_rows() -> usize {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    (rows as usize).saturating_sub(4).max(3)
}

/// Place the visible window for the next frame.
///
/// Returns `(scroll_offset, window_size)`. `window_size` already
/// accounts for any "N more above"/"N more below" indicator rows the
/// renderer will draw, so the caller can iterate exactly that many
/// items without going past the visible region.
#[must_use]
pub fn compute_window(
    scroll: usize,
    selected: usize,
    total: usize,
    max_visible: usize,
) -> (usize, usize) {
    if total == 0 || max_visible == 0 {
        return (0, 0);
    }
    if total <= max_visible {
        return (0, total);
    }
    // Overflow: reserve up to two rows for indicators. Start with a
    // pessimistic window assuming both indicators show, place the
    // window, then expand if either indicator turns out to be hidden
    // (selected at the very top or very bottom).
    let reserved = max_visible.saturating_sub(2).max(1);
    let max_offset = total.saturating_sub(reserved);
    let mut s = scroll.min(max_offset);
    if selected < s {
        s = selected;
    } else if selected >= s + reserved {
        s = selected + 1 - reserved;
    }
    let above = s > 0;
    let below_after = s + reserved < total;
    let extra = usize::from(!above) + usize::from(!below_after);
    let window = (reserved + extra).min(max_visible).min(total - s);
    (s, window)
}

/// Indexes of items whose value or label contains `search`
/// (case-insensitive). Public so the in-TUI overlay picker can
/// reuse the exact same matcher.
#[must_use]
pub fn filter(items: &[PickItem], search: &str) -> Vec<usize> {
    if search.is_empty() {
        return (0..items.len()).collect();
    }
    let q = search.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.value.to_lowercase().contains(&q) || item.label.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

fn clear_lines(out: &mut io::Stdout, count: usize) -> io::Result<()> {
    if count == 0 {
        return Ok(());
    }
    for _ in 0..count {
        queue!(
            out,
            cursor::MoveUp(1),
            terminal::Clear(terminal::ClearType::CurrentLine)
        )?;
    }
    queue!(out, cursor::MoveToColumn(0))?;
    out.flush()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render(
    out: &mut io::Stdout,
    prompt: &str,
    search: &str,
    items: &[PickItem],
    filtered: &[usize],
    selected: usize,
    scroll_offset: usize,
    window: usize,
) -> io::Result<usize> {
    let mut lines = 0;
    queue!(out, cursor::MoveToColumn(0))?;
    queue!(
        out,
        SetForegroundColor(Color::Blue),
        SetAttribute(Attribute::Bold),
        Print("  "),
        Print(prompt),
        ResetColor,
        Print("\r\n"),
    )?;
    lines += 1;

    queue!(
        out,
        SetForegroundColor(Color::DarkGrey),
        Print("  search: "),
        ResetColor,
    )?;
    if search.is_empty() {
        queue!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("_"),
            ResetColor,
            Print("\r\n"),
        )?;
    } else {
        queue!(out, Print(search), Print("\r\n"))?;
    }
    lines += 1;

    if filtered.is_empty() {
        queue!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("  (no matches)\r\n"),
            ResetColor,
        )?;
        lines += 1;
    } else {
        let total = filtered.len();
        let end = (scroll_offset + window).min(total);
        let above = scroll_offset;
        let below = total.saturating_sub(end);

        if above > 0 {
            queue!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(format!("  ... {above} more above\r\n")),
                ResetColor,
            )?;
            lines += 1;
        }

        for (vi, &idx) in filtered.iter().enumerate().take(end).skip(scroll_offset) {
            let item = &items[idx];
            let is_sel = vi == selected;
            let marker = if is_sel { '>' } else { ' ' };
            let badge = item.badge.unwrap_or(' ');
            let color = if is_sel {
                Color::Blue
            } else if item.badge == Some('*') {
                Color::Green
            } else {
                Color::DarkGrey
            };
            queue!(
                out,
                SetForegroundColor(color),
                Print(format!("  {marker} {badge} ")),
                ResetColor,
            )?;
            if is_sel {
                queue!(
                    out,
                    SetForegroundColor(Color::White),
                    SetAttribute(Attribute::Bold),
                    Print(&item.label),
                    ResetColor,
                )?;
            } else {
                queue!(out, Print(&item.label))?;
            }
            queue!(out, Print("\r\n"))?;
            lines += 1;
        }

        if below > 0 {
            queue!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(format!("  ... {below} more below\r\n")),
                ResetColor,
            )?;
            lines += 1;
        }
    }

    queue!(
        out,
        SetForegroundColor(Color::DarkGrey),
        Print("  up/down select, page up/down jump, enter confirm, esc cancel\r\n"),
        ResetColor,
    )?;
    lines += 1;

    out.flush()?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_item_simple_uses_value_as_label() {
        let it = PickItem::simple("zai");
        assert_eq!(it.value, "zai");
        assert_eq!(it.label, "zai");
        assert!(it.badge.is_none());
    }

    #[test]
    fn pick_item_with_badge_and_label() {
        let it = PickItem::simple("z").with_label("ZAI").with_badge('*');
        assert_eq!(it.label, "ZAI");
        assert_eq!(it.badge, Some('*'));
    }

    #[test]
    fn filter_matches_value_case_insensitive() {
        let items = vec![
            PickItem::simple("Anthropic"),
            PickItem::simple("OpenAI"),
            PickItem::simple("ZAI"),
        ];
        let f = filter(&items, "ai");
        assert_eq!(f, vec![1, 2]);
    }

    #[test]
    fn filter_empty_search_returns_all() {
        let items = vec![PickItem::simple("a"), PickItem::simple("b")];
        assert_eq!(filter(&items, ""), vec![0, 1]);
    }

    #[test]
    fn filter_no_match_is_empty() {
        let items = vec![PickItem::simple("anthropic")];
        assert!(filter(&items, "zzz").is_empty());
    }

    #[test]
    fn compute_window_no_overflow() {
        let (off, win) = compute_window(0, 4, 5, 10);
        assert_eq!((off, win), (0, 5));
    }

    #[test]
    fn compute_window_at_top_hides_above_indicator_only() {
        // 100 items, visible 5: 4 items + 1 below indicator on first frame.
        let (off, win) = compute_window(0, 0, 100, 5);
        assert_eq!(off, 0);
        // window includes 4 items; "below" indicator rendered separately.
        assert_eq!(win, 4);
        // Selected stays inside.
        assert!(0 < off + win);
    }

    #[test]
    fn compute_window_in_middle_reserves_two_indicator_rows() {
        // Visible 5, selected 50, list of 100: both indicators show.
        let (off, win) = compute_window(48, 50, 100, 5);
        assert_eq!(win, 3);
        assert!(50 >= off && 50 < off + win);
    }

    #[test]
    fn compute_window_at_bottom_hides_below_indicator_only() {
        // At the very end, the offset can only fit 3 items above
        // (97/98/99). Above indicator is still on; below is hidden.
        let (off, win) = compute_window(95, 99, 100, 5);
        assert_eq!((off, win), (97, 3));
    }

    #[test]
    fn compute_window_keeps_selected_in_view_after_scrolling() {
        // Move down through a 100-item list with 5 visible rows. The
        // selected index must always satisfy off <= selected < off + win.
        let mut off = 0usize;
        for selected in 0..100 {
            let (next_off, win) = compute_window(off, selected, 100, 5);
            assert!(
                selected >= next_off && selected < next_off + win,
                "selected={selected} off={next_off} win={win}",
            );
            off = next_off;
        }
    }

    #[test]
    fn compute_window_handles_empty_and_zero() {
        assert_eq!(compute_window(0, 0, 0, 5), (0, 0));
        assert_eq!(compute_window(0, 0, 5, 0), (0, 0));
    }
}
