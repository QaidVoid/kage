//! Acquire and release the terminal in raw alt-screen mode.
//!
//! [`Tui`] wraps [`ratatui::DefaultTerminal`] with a small lifecycle
//! helper that also enables bracketed paste, opts into the kitty
//! keyboard protocol (so `Shift+Enter` and other modified keys are
//! transmitted reliably), and installs a panic hook so a crashing run
//! never strands the user's tty in raw mode. Drop reverses every state
//! change in the right order.
//!
//! Tests render against [`ratatui::backend::TestBackend`] directly; the
//! lifecycle wrapper is only meaningful with a real tty.
//!
//! [`forward_typed_lines`] reads lines from the terminal while the TUI
//! is suspended and stops on request, so a host flow that waits for
//! either typed input or something else can hand the terminal back
//! without asking for a key press.

use std::io::{self, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, EnterAlternateScreen};

use crate::error::TuiError;
use crate::theme::ColorDepth;

static PANIC_HOOK: Once = Once::new();

/// The keyboard-enhancement flags we request. We only ask for the bare
/// minimum that buys `Shift+Enter` and `Ctrl+I` vs `Tab` disambiguation.
/// Pushing more aggressive flags (`REPORT_ALL_KEYS_AS_ESCAPE_CODES`,
/// `REPORT_EVENT_TYPES`) caused some terminals to ignore the entire
/// request, dropping Shift+Enter back to plain Enter.
const KITTY_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;

/// How often [`forward_typed_lines`] checks its stop flag.
const LINE_POLL: Duration = Duration::from_millis(50);

/// Owns the terminal while the TUI is running. Restoring is automatic on
/// drop and via a panic hook so a crashing run never strands the tty.
// The three `*_active` flags model independent terminal features
// (paste, kitty keyboard, mouse); folding them into a bitmask or
// options buys nothing.
#[allow(clippy::struct_excessive_bools)]
pub struct Tui {
    terminal: DefaultTerminal,
    bracketed_paste_active: bool,
    kitty_flags_active: bool,
    mouse_capture_active: bool,
    /// Between a [`Self::suspend`] and [`Self::resume`] pair.
    suspended: bool,
    /// Colors the terminal shows, detected once on entry.
    color_depth: ColorDepth,
}

impl std::fmt::Debug for Tui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tui")
            .field("bracketed_paste_active", &self.bracketed_paste_active)
            .field("kitty_flags_active", &self.kitty_flags_active)
            .field("mouse_capture_active", &self.mouse_capture_active)
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
        let bracketed_paste_active = match execute!(io::stdout(), EnableBracketedPaste) {
            Ok(()) => true,
            Err(err) => {
                ratatui::restore();
                return Err(err.into());
            }
        };
        let kitty_flags_active =
            execute!(io::stdout(), PushKeyboardEnhancementFlags(KITTY_FLAGS)).is_ok();
        let mouse_capture_active = execute!(io::stdout(), EnableMouseCapture).is_ok();
        Ok(Self {
            terminal,
            bracketed_paste_active,
            kitty_flags_active,
            mouse_capture_active,
            suspended: false,
            color_depth: ColorDepth::detect(),
        })
    }

    /// Colors the terminal shows, read from `COLORTERM` and `TERM`
    /// when the TUI started.
    #[must_use]
    pub fn color_depth(&self) -> ColorDepth {
        self.color_depth
    }

    /// Borrow the wrapped ratatui terminal so the caller can `draw` to it.
    pub fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }

    /// Toggle mouse capture at runtime. With capture off the host
    /// receives no [`crossterm`] mouse events, but the terminal's
    /// native selection (drag to highlight, double-click word, etc.)
    /// becomes available again - the user can copy any visible text
    /// via the terminal's own clipboard binding without the TUI
    /// having to map row/col into block coordinates. Returns the new
    /// state.
    pub fn set_mouse_capture(&mut self, enable: bool) -> bool {
        if enable
            && !self.mouse_capture_active
            && execute!(io::stdout(), EnableMouseCapture).is_ok()
        {
            self.mouse_capture_active = true;
        } else if !enable
            && self.mouse_capture_active
            && execute!(io::stdout(), DisableMouseCapture).is_ok()
        {
            self.mouse_capture_active = false;
        }
        self.mouse_capture_active
    }

    /// Whether mouse capture is currently on.
    #[must_use]
    pub fn mouse_capture(&self) -> bool {
        self.mouse_capture_active
    }

    /// Suspend the TUI so an external program can own the tty:
    /// leaves the alternate screen, disables raw mode, and tears
    /// down paste/keyboard/mouse state this struct enabled. The
    /// remembered enablement flags stay untouched so [`Self::resume`]
    /// can restore exactly what was active. Idempotent; if resume
    /// never runs, [`Drop`](Self::drop) still restores everything.
    pub fn suspend(&mut self) {
        if self.suspended {
            return;
        }
        let _ = self.terminal.flush();
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        if self.mouse_capture_active {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        if self.kitty_flags_active {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.bracketed_paste_active {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        let _ = io::stdout().flush();
        ratatui::restore();
        self.suspended = true;
    }

    /// Re-enter the TUI after [`Self::suspend`]: raw mode, alternate
    /// screen, and the remembered paste/keyboard/mouse state come
    /// back, and the next frame repaints from scratch. Idempotent.
    pub fn resume(&mut self) {
        if !self.suspended {
            return;
        }
        if ratatui::crossterm::terminal::enable_raw_mode().is_err() {
            self.suspended = false;
            return;
        }
        let _ = execute!(io::stdout(), EnterAlternateScreen);
        if self.bracketed_paste_active {
            let _ = execute!(io::stdout(), EnableBracketedPaste);
        }
        if self.kitty_flags_active {
            let _ = execute!(io::stdout(), PushKeyboardEnhancementFlags(KITTY_FLAGS));
        }
        if self.mouse_capture_active {
            let _ = execute!(io::stdout(), EnableMouseCapture);
        }
        let _ = self.terminal.clear();
        self.suspended = false;
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        // Reset the DECSCUSR cursor shape we may have pushed (block
        // in Normal, bar in Insert) so the user's shell prompt
        // doesn't inherit it.
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        if self.mouse_capture_active {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            self.mouse_capture_active = false;
        }
        if self.kitty_flags_active {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
            self.kitty_flags_active = false;
        }
        if self.bracketed_paste_active {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
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
            let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
            let _ = execute!(io::stdout(), DisableMouseCapture);
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            ratatui::restore();
            prev(info);
        }));
    });
}

/// Send each line typed on the terminal to `tx` until `stop` is set,
/// `tx` closes or reading the terminal fails. Call it only while the
/// TUI is suspended: the terminal is then in cooked mode, so it echoes
/// and edits the line itself and crossterm reports the finished line
/// as keys ending in Enter. Input typed but not submitted when it
/// returns is discarded, so none of it reaches the TUI.
pub fn forward_typed_lines(tx: &mpsc::Sender<String>, stop: &AtomicBool) {
    let mut line = String::new();
    while !stop.load(Ordering::SeqCst) {
        match event::poll(LINE_POLL) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => break,
        }
        let Ok(event) = event::read() else { break };
        if let Some(done) = edit_line(&mut line, event)
            && tx.send(done).is_err()
        {
            break;
        }
    }
    discard_typed_input();
}

/// Apply one terminal event to the line being typed. Returns the line
/// once Enter, or a line break inside a paste, finishes it.
fn edit_line(line: &mut String, event: Event) -> Option<String> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Enter => Some(std::mem::take(line)),
            KeyCode::Backspace => {
                line.pop();
                None
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                line.push(c);
                None
            }
            _ => None,
        },
        Event::Paste(text) => {
            if let Some((head, _)) = text.split_once(['\r', '\n']) {
                line.push_str(head);
                Some(std::mem::take(line))
            } else {
                line.push_str(&text);
                None
            }
        }
        _ => None,
    }
}

/// Drop what is left in the terminal's input. A line still being edited
/// in cooked mode only becomes readable in raw mode, so this switches
/// to raw mode while it drains and back afterwards.
fn discard_typed_input() {
    if terminal::enable_raw_mode().is_err() {
        return;
    }
    while matches!(event::poll(Duration::ZERO), Ok(true)) {
        if event::read().is_err() {
            break;
        }
    }
    let _ = terminal::disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::widgets::Paragraph;

    use super::edit_line;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::from(code))
    }

    #[test]
    fn typed_characters_become_a_line_on_enter() {
        let mut line = String::new();
        for c in "http://x/cb?code=c".chars() {
            assert_eq!(edit_line(&mut line, key(KeyCode::Char(c))), None);
        }
        assert_eq!(
            edit_line(&mut line, key(KeyCode::Enter)).as_deref(),
            Some("http://x/cb?code=c")
        );
        assert!(line.is_empty());
        assert_eq!(
            edit_line(&mut line, key(KeyCode::Enter)).as_deref(),
            Some("")
        );
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut line = "abc".to_owned();
        assert_eq!(edit_line(&mut line, key(KeyCode::Backspace)), None);
        assert_eq!(line, "ab");
        line.clear();
        assert_eq!(edit_line(&mut line, key(KeyCode::Backspace)), None);
        assert!(line.is_empty());
    }

    #[test]
    fn releases_control_keys_and_other_events_are_ignored() {
        let mut line = "a".to_owned();
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for event in [
            Event::Key(release),
            Event::Key(ctrl_c),
            key(KeyCode::Tab),
            key(KeyCode::Up),
            key(KeyCode::Esc),
            Event::Resize(80, 24),
            Event::FocusGained,
        ] {
            assert_eq!(edit_line(&mut line, event), None);
        }
        assert_eq!(line, "a");
        let release_enter =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(edit_line(&mut line, Event::Key(release_enter)), None);
    }

    #[test]
    fn a_paste_joins_the_line_and_a_line_break_finishes_it() {
        let mut line = "> ".to_owned();
        assert_eq!(
            edit_line(&mut line, Event::Paste("http://x".to_owned())),
            None
        );
        assert_eq!(line, "> http://x");
        let done = edit_line(&mut line, Event::Paste("/cb?code=c\nrest".to_owned()));
        assert_eq!(done.as_deref(), Some("> http://x/cb?code=c"));
        assert!(line.is_empty());
    }

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
