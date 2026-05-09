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
}

/// Show `items` in a search-as-you-type picker with `prompt` as the
/// header. Returns the chosen item's value, or `None` for cancel.
pub fn pick(prompt: &str, items: &[PickItem]) -> Result<Option<String>, TuiError> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    let _restore = RawModeGuard;
    execute!(stdout, cursor::Hide)?;
    let result = run(&mut stdout, prompt, items);
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
    let mut prev_lines: usize = 0;

    loop {
        let filtered = filter(items, &search);
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }

        clear_lines(out, prev_lines)?;
        prev_lines = render(out, prompt, &search, items, &filtered, selected)?;

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
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down if selected + 1 < filtered.len() => selected += 1,
            KeyCode::Enter => {
                if let Some(&idx) = filtered.get(selected) {
                    clear_lines(out, prev_lines)?;
                    return Ok(Some(items[idx].value.clone()));
                }
            }
            KeyCode::Backspace => {
                search.pop();
                selected = 0;
            }
            KeyCode::Char(c) => {
                search.push(c);
                selected = 0;
            }
            _ => {}
        }
    }
}

fn filter(items: &[PickItem], search: &str) -> Vec<usize> {
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

fn render(
    out: &mut io::Stdout,
    prompt: &str,
    search: &str,
    items: &[PickItem],
    filtered: &[usize],
    selected: usize,
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
        for (vi, &idx) in filtered.iter().enumerate() {
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
    }

    queue!(
        out,
        SetForegroundColor(Color::DarkGrey),
        Print("  up/down select, enter confirm, type to filter, esc cancel\r\n"),
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
}
