//! External-editor support (`Ctrl+G`).
//!
//! Opens the current prompt draft in the user's editor, suspending
//! the TUI for the duration: the alternate screen is left, raw mode
//! is dropped, and the editor inherits the real terminal. On return
//! the TUI repaints and the (possibly edited) text replaces the
//! prompt. `$VISUAL` wins over `$EDITOR`; without either, `Ctrl+G`
//! toasts instead of launching anything.

use std::process::Command;

use super::Tui;
/// Resolve the editor command: `$VISUAL` first, then `$EDITOR`.
/// Empty strings count as unset.
fn editor_command() -> Option<String> {
    for var in ["VISUAL", "EDITOR"] {
        match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => return Some(v),
            _ => {}
        }
    }
    None
}

/// Why the editor run failed and how to fix it, from the shell's exit
/// `code`: 127 means the shell found no such command.
fn editor_failure(editor: &str, code: Option<i32>) -> String {
    match code {
        Some(127) => {
            format!("editor `{editor}` not found. Set $VISUAL or $EDITOR to an installed editor.")
        }
        Some(code) => {
            format!("editor `{editor}` exited with status {code}. The prompt is unchanged.")
        }
        None => format!("editor `{editor}` was stopped by a signal. The prompt is unchanged."),
    }
}

/// Normalize editor output before it lands in the prompt: strip one
/// trailing newline (editors always append one) and any carriage
/// returns a Windows-ish editor left behind.
#[must_use]
pub(crate) fn normalize_edited_text(text: &str) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    text.replace('\r', "")
}

impl super::App {
    /// Whether `key` is the external-editor chord and no modal layer
    /// would otherwise own the keyboard. A mapping on the chord from
    /// `init.lua` or `config.toml` wins, mirroring the
    /// `Ctrl+C`/`Ctrl+Q` hatches.
    pub(crate) fn external_edit_key(&self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if !key.modifiers.contains(KeyModifiers::CONTROL)
            || !matches!(key.code, KeyCode::Char('g'))
            || self.user_mapped(&key)
        {
            return false;
        }
        !self.keyboard_modal_open()
    }

    /// Whether any keyboard-consuming modal layer is on screen. A
    /// superset of the mouse-oriented [`App::modal_open`]: the help
    /// overlay, approval panel, and context menu also own keys.
    pub(crate) fn keyboard_modal_open(&self) -> bool {
        self.modal_open()
            || self.help_overlay.is_some()
            || self.approval_panel.is_some()
            || self.context_menu.is_some()
    }

    /// Open the prompt draft in the external editor. Suspends the
    /// TUI so the editor gets a sane terminal; failures to resolve
    /// or run an editor toast and leave the prompt untouched.
    pub(crate) fn edit_in_external_editor(&mut self, tui: &mut Tui) {
        let Some(editor) = editor_command() else {
            self.notify("no editor: set $VISUAL or $EDITOR");
            return;
        };
        // A per-process unique name is enough: only one editor runs
        // at a time, and the file is removed on every path out.
        let path = std::env::temp_dir().join(format!(
            "kage-edit-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        if let Err(e) = std::fs::write(&path, self.input.text()) {
            self.notify(format!("editor: {e}"));
            return;
        }

        tui.suspend();
        // Route through a shell so editors with arguments
        // (`code -w`, `subl --wait`) work as-is.
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{} \"$@\"", editor.trim()))
            .arg("sh")
            .arg(&path)
            .status();
        tui.resume();

        let Ok(status) = status else {
            self.notify(format!("editor: could not run `{}`", editor.trim()));
            let _ = std::fs::remove_file(&path);
            return;
        };
        if !status.success() {
            self.notify(editor_failure(editor.trim(), status.code()));
            let _ = std::fs::remove_file(&path);
            return;
        }
        let edited = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        let Ok(edited) = edited else {
            self.notify("editor: could not read back the temp file");
            return;
        };
        let edited = normalize_edited_text(&edited);
        let old_len = self.input.text().len();
        self.input.splice(0, old_len, &edited);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn normalize_strips_one_trailing_newline_and_carriage_returns() {
        assert_eq!(normalize_edited_text("hello\n"), "hello");
        assert_eq!(normalize_edited_text("hello\r\n"), "hello");
        assert_eq!(normalize_edited_text("a\nb\n"), "a\nb");
        assert_eq!(normalize_edited_text("a\r\nb\r\n"), "a\nb");
        assert_eq!(normalize_edited_text(""), "");
        assert_eq!(normalize_edited_text("\n"), "");
    }

    #[test]
    fn a_missing_editor_says_how_to_fix_it() {
        let msg = editor_failure("nosuchedit", Some(127));
        assert!(msg.contains("`nosuchedit` not found"), "{msg}");
        assert!(msg.contains("$EDITOR"), "{msg}");
        assert!(editor_failure("vi", Some(1)).contains("status 1"));
    }

    #[test]
    fn ctrl_g_requests_external_edit_outside_modals() {
        use super::super::App;
        let buffer = crate::events::shared_buffer();
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::new(buffer, tx);
        let chord = key(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(app.external_edit_key(chord));
        let plain = key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert!(!app.external_edit_key(plain));
    }

    #[test]
    fn ctrl_g_is_suppressed_while_a_modal_is_open() {
        use super::super::App;
        let buffer = crate::events::shared_buffer();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.open_help();
        let chord = key(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(app.keyboard_modal_open());
        assert!(!app.external_edit_key(chord));
    }
}
