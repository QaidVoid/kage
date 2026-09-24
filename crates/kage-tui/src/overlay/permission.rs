//! Tool-permission prompt overlay.
//!
//! Shown when the agent loop hits a tool configured `ask` under
//! `[permissions.tools.<name>]`. Mirrors the plugin-dialog hosting
//! pattern: the worker parks on a reply channel while this modal owns
//! the keyboard. Enter picks the highlighted row; `y` / `a` / `n` are
//! direct shortcuts; Esc denies. The overlay never closes without a
//! decision: every dismissal path resolves, so the parked worker
//! always resumes.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// The three rows the prompt offers, in display order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionChoice {
    /// Run this one call; ask again next time.
    AllowOnce,
    /// Run this call and persist an allow rule for the tool.
    AllowAlways,
    /// Refuse the call.
    Deny,
}

const CHOICES: [PermissionChoice; 3] = [
    PermissionChoice::AllowOnce,
    PermissionChoice::AllowAlways,
    PermissionChoice::Deny,
];

fn label(choice: PermissionChoice) -> &'static str {
    match choice {
        PermissionChoice::AllowOnce => "Allow once (y)",
        PermissionChoice::AllowAlways => "Always allow (a)",
        PermissionChoice::Deny => "Deny (n)",
    }
}

/// Modal allow / always / deny prompt for one tool call.
#[derive(Debug)]
pub struct PermissionOverlay {
    tool: String,
    subject: String,
    selected: usize,
}

impl PermissionOverlay {
    /// Build a prompt for `tool` with the `subject` preview (the
    /// command line or compact JSON the rules matched against).
    /// Defaults to [`PermissionChoice::Deny`] so a stray Enter cannot
    /// approve a destructive call.
    #[must_use]
    pub fn new(tool: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            subject: subject.into(),
            selected: CHOICES.len() - 1,
        }
    }

    /// Currently highlighted row. Exposed for tests.
    #[must_use]
    pub fn selected(&self) -> PermissionChoice {
        CHOICES[self.selected.min(CHOICES.len() - 1)]
    }

    fn move_sel(&mut self, forward: bool) {
        let len = CHOICES.len();
        self.selected = if forward {
            (self.selected + 1) % len
        } else {
            (self.selected + len - 1) % len
        };
    }
}

impl OverlayWidget for PermissionOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let width = available.width.clamp(40, 72);
        let height: u16 = 9;
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        Widget::render(crate::opaque::OpaqueClear, area, buf);
        let block = Block::default()
            .title(format!(" Allow {}? ", self.tool))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::theme::current().warning_fg));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(2),
                ratatui::layout::Constraint::Length(3),
            ])
            .split(inner);

        Widget::render(
            Paragraph::new(self.subject.clone())
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(crate::theme::current().overlay_fg)),
            chunks[0],
            buf,
        );

        for (idx, choice) in CHOICES.iter().enumerate() {
            let style = if idx == self.selected {
                Style::default()
                    .fg(crate::theme::current().overlay_selected_fg)
                    .bg(crate::theme::current().overlay_selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(crate::theme::current().muted_fg)
            };
            let marker = if idx == self.selected { ">" } else { " " };
            let row = Rect {
                y: chunks[1].y + u16::try_from(idx).unwrap_or(0),
                height: 1,
                ..chunks[1]
            };
            let line = Line::from(vec![
                Span::raw(" "),
                Span::raw(marker),
                Span::styled(format!(" {} ", label(*choice)), style),
            ]);
            Widget::render(Paragraph::new(line), row, buf);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl+C is intercepted by the App's global cancel hatch
            // before the overlay sees it; any other chord stays here.
            return OverlayAction::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => OverlayAction::Resolve(deny_value()),
            KeyCode::Up | KeyCode::Left => {
                self.move_sel(false);
                OverlayAction::Stay
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.move_sel(true);
                OverlayAction::Stay
            }
            KeyCode::Char('y' | 'Y') => {
                OverlayAction::Resolve(choice_value(PermissionChoice::AllowOnce))
            }
            KeyCode::Char('a' | 'A') => {
                OverlayAction::Resolve(choice_value(PermissionChoice::AllowAlways))
            }
            KeyCode::Enter => OverlayAction::Resolve(choice_value(self.selected())),
            _ => OverlayAction::Stay,
        }
    }
}

fn choice_value(choice: PermissionChoice) -> serde_json::Value {
    serde_json::Value::String(match choice {
        PermissionChoice::AllowOnce => "allow_once".to_owned(),
        PermissionChoice::AllowAlways => "allow_always".to_owned(),
        PermissionChoice::Deny => "deny".to_owned(),
    })
}

fn deny_value() -> serde_json::Value {
    choice_value(PermissionChoice::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn defaults_to_deny() {
        let o = PermissionOverlay::new("bash", "rm -rf /tmp/x");
        assert_eq!(o.selected(), PermissionChoice::Deny);
    }

    #[test]
    fn enter_resolves_current_selection() {
        let mut o = PermissionOverlay::new("bash", "cargo test");
        assert_eq!(
            o.handle_key(key(KeyCode::Enter)),
            OverlayAction::Resolve(serde_json::json!("deny"))
        );
    }

    #[test]
    fn up_moves_selection_wrapping() {
        let mut o = PermissionOverlay::new("bash", "ls");
        o.handle_key(key(KeyCode::Up));
        assert_eq!(o.selected(), PermissionChoice::AllowAlways);
        o.handle_key(key(KeyCode::Up));
        assert_eq!(o.selected(), PermissionChoice::AllowOnce);
        o.handle_key(key(KeyCode::Up));
        assert_eq!(o.selected(), PermissionChoice::Deny);
    }

    #[test]
    fn down_moves_forward_wrapping() {
        let mut o = PermissionOverlay::new("bash", "ls");
        o.handle_key(key(KeyCode::Down));
        assert_eq!(o.selected(), PermissionChoice::AllowOnce);
        o.handle_key(key(KeyCode::Down));
        assert_eq!(o.selected(), PermissionChoice::AllowAlways);
        o.handle_key(key(KeyCode::Down));
        assert_eq!(o.selected(), PermissionChoice::Deny);
    }

    #[test]
    fn shortcuts_resolve_directly() {
        let mut o = PermissionOverlay::new("bash", "ls");
        assert_eq!(
            o.handle_key(key(KeyCode::Char('y'))),
            OverlayAction::Resolve(serde_json::json!("allow_once"))
        );
        assert_eq!(
            o.handle_key(key(KeyCode::Char('a'))),
            OverlayAction::Resolve(serde_json::json!("allow_always"))
        );
        assert_eq!(
            o.handle_key(key(KeyCode::Char('n'))),
            OverlayAction::Resolve(serde_json::json!("deny"))
        );
    }

    #[test]
    fn esc_resolves_deny() {
        let mut o = PermissionOverlay::new("bash", "ls");
        assert_eq!(
            o.handle_key(key(KeyCode::Esc)),
            OverlayAction::Resolve(serde_json::json!("deny"))
        );
    }

    fn snapshot(o: &mut PermissionOverlay, area: Rect) -> Vec<String> {
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::Theme::default();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        let modal = o.measure(area);
        o.render(modal, &mut buf, &ctx);
        let mut out = Vec::with_capacity(usize::from(area.height));
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn render_paints_title_wrapped_subject_and_options() {
        let mut o = PermissionOverlay::new("bash", "git commit -m 'a very long subject");
        let lines = snapshot(&mut o, Rect::new(0, 0, 80, 24));
        assert!(
            lines.iter().any(|l| l.contains("Allow bash?")),
            "title missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("git commit")),
            "subject missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Allow once")),
            "options missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Always allow")),
            "options missing: {lines:?}"
        );
    }

    #[test]
    fn render_narrow_viewport_still_paints_box() {
        let mut o = PermissionOverlay::new("write", "{\"path\":\"a\"}");
        let lines = snapshot(&mut o, Rect::new(0, 0, 40, 12));
        assert!(lines.iter().any(|l| l.contains("Allow write?")));
    }
}
