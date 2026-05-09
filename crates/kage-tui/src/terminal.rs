//! Acquire and release the terminal in raw alt-screen mode.
//!
//! [`Tui`] wraps [`ratatui::DefaultTerminal`] with a small lifecycle
//! helper that also enables bracketed paste and installs a panic hook so
//! a crashing run never strands the user's tty in raw mode. Drop reverses
//! every state change in the right order.
//!
//! Tests render against [`ratatui::backend::TestBackend`] directly; the
//! lifecycle wrapper is only meaningful with a real tty.

use std::io::{self, Write};
use std::sync::Once;

use ratatui::DefaultTerminal;
use ratatui::crossterm::{event, execute};

use crate::error::TuiError;

static PANIC_HOOK: Once = Once::new();

/// Owns the terminal while the TUI is running. Restoring is automatic on
/// drop and via a panic hook so a crashing run never strands the tty.
pub struct Tui {
    terminal: DefaultTerminal,
    bracketed_paste_active: bool,
}

impl std::fmt::Debug for Tui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tui")
            .field("bracketed_paste_active", &self.bracketed_paste_active)
            .finish_non_exhaustive()
    }
}

impl Tui {
    /// Enter raw alt-screen mode against process stdout and arm the
    /// panic-hook restore. On success the caller is the sole owner of
    /// the terminal until the returned [`Tui`] is dropped.
    pub fn enter() -> Result<Self, TuiError> {
        install_panic_hook();
        let terminal = ratatui::try_init()?;
        let bracketed_paste_active = match execute!(io::stdout(), event::EnableBracketedPaste) {
            Ok(()) => true,
            Err(err) => {
                ratatui::restore();
                return Err(err.into());
            }
        };
        Ok(Self {
            terminal,
            bracketed_paste_active,
        })
    }

    /// Borrow the wrapped ratatui terminal so the caller can `draw` to it.
    pub fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.bracketed_paste_active {
            let _ = execute!(io::stdout(), event::DisableBracketedPaste);
            self.bracketed_paste_active = false;
        }
        let _ = io::stdout().flush();
        ratatui::restore();
    }
}

/// Install a panic hook that restores raw-mode terminals before the
/// default handler prints the backtrace. Idempotent across runs.
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(io::stdout(), event::DisableBracketedPaste);
            ratatui::restore();
            prev(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    /// Sanity check: ratatui can render a widget through `TestBackend`.
    /// Production `Tui` requires a tty, so renderer tests in this crate
    /// always go through `TestBackend` rather than [`Tui`].
    #[test]
    fn ratatui_test_backend_renders_a_paragraph() {
        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("hello"), frame.area());
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let line: String = (0..5).map(|x| buf[(x, 0)].symbol().to_owned()).collect();
        assert_eq!(line, "hello");
    }
}
