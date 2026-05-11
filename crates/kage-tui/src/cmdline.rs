//! `:` ex-style command line shown on the status row.
//!
//! [`CommandLine`] is a small text field opened by [`crate::App`] when
//! the user presses `:` in [`crate::Mode::Normal`]. It owns its own text
//! and cursor, runs the [`crate::cmdparse`] completion engine on every
//! edit, and reports user actions via [`CommandLineEvent`]: pending
//! (still typing), cancelled (Esc / Ctrl+C), or submit (Enter).
//!
//! Tab completion behaves like vim's `wildmode=longest:full,full`:
//! the first Tab extends the prefix to the longest common prefix
//! across candidates and opens the popup; the next Tab cycles. Down/Up
//! cycle while the popup is open. A single-candidate match completes
//! immediately and closes the popup. Esc dismisses the popup first;
//! when no popup is open it cancels the command line.
//!
//! Rendering lives in [`crate::view`]; navigation and editing here. The
//! widget intentionally stays separate from [`crate::InputState`] so
//! the modal state machine doesn't have to grow another mode for what
//! is conceptually a transient overlay.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::cmdparse::{Completions, Resolver, complete};
use crate::command::CommandSpec;

/// Outcome of [`CommandLine::handle_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandLineEvent {
    /// Keystroke was handled but no decision yet.
    Pending,
    /// User pressed Esc or Ctrl+C; close without running.
    Cancelled,
    /// User pressed Enter; carries the typed command (without the
    /// leading `:`). Empty input is reported as cancelled, not submit.
    Submit(String),
}

/// Single-line text field with completion popup state.
#[derive(Debug, Default)]
pub struct CommandLine {
    text: String,
    cursor: usize,
    completions: Completions,
    popup_open: bool,
    selected: Option<usize>,
    /// Inline validation error displayed below the command line.
    /// Set by the host after a failed submit; cleared on the next
    /// keystroke that changes the text.
    error: Option<String>,
}

impl CommandLine {
    /// Construct an empty command line.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current command text (without the leading `:`).
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor inside [`Self::text`].
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Current completion candidates, computed against the registry
    /// and resolver passed to the last [`Self::handle_key`] call.
    /// Empty until the user types or presses a key that triggers a
    /// refresh.
    #[must_use]
    pub fn completions(&self) -> &Completions {
        &self.completions
    }

    /// Whether the completion popup should be visible.
    #[must_use]
    pub fn popup_open(&self) -> bool {
        self.popup_open
    }

    /// Currently highlighted completion index, or `None` when the
    /// popup is showing the LCP-only state with no specific row
    /// selected.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Current inline validation error, if any. Set by the host after
    /// a failed submit attempt; cleared on the next editing keystroke.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Set an inline validation error. The cmdline should remain open
    /// (the host does not set `self.cmdline = None`) so the user can
    /// fix the input.
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }

    /// Clear any inline validation error.
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Close the completion popup without changing the text.
    pub fn dismiss_completions(&mut self) {
        self.popup_open = false;
        self.selected = None;
    }

    /// Recompute completions against `registry` and `resolver` using
    /// the current text. Callers use this when they need to populate
    /// the popup without driving through a keystroke, e.g. the slash
    /// palette wants the full list visible on open.
    pub fn refresh_completions(&mut self, registry: &[&CommandSpec], resolver: &dyn Resolver) {
        self.refresh(registry, resolver);
    }

    /// Drive the widget by one key press. `registry` and `resolver`
    /// are consulted on every edit to refresh completions and on Tab
    /// to apply them.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        registry: &[&CommandSpec],
        resolver: &dyn Resolver,
    ) -> CommandLineEvent {
        if key.kind != KeyEventKind::Press {
            return CommandLineEvent::Pending;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return CommandLineEvent::Cancelled;
        }
        match key.code {
            KeyCode::Esc => {
                if self.popup_open {
                    self.dismiss_completions();
                    CommandLineEvent::Pending
                } else {
                    CommandLineEvent::Cancelled
                }
            }
            KeyCode::Enter => {
                let trimmed = self.text.trim().to_owned();
                if trimmed.is_empty() {
                    CommandLineEvent::Cancelled
                } else {
                    CommandLineEvent::Submit(trimmed)
                }
            }
            KeyCode::Tab => {
                self.error = None;
                if self.completions.items.is_empty() {
                    self.refresh(registry, resolver);
                }
                self.tab(true);
                CommandLineEvent::Pending
            }
            KeyCode::BackTab => {
                self.error = None;
                if self.completions.items.is_empty() {
                    self.refresh(registry, resolver);
                }
                self.tab(false);
                CommandLineEvent::Pending
            }
            KeyCode::Down if self.popup_open => {
                self.cycle(true);
                CommandLineEvent::Pending
            }
            KeyCode::Up if self.popup_open => {
                self.cycle(false);
                CommandLineEvent::Pending
            }
            KeyCode::Backspace => {
                self.error = None;
                self.backspace();
                self.refresh(registry, resolver);
                CommandLineEvent::Pending
            }
            KeyCode::Left => {
                self.error = None;
                self.move_cursor(-1);
                self.refresh(registry, resolver);
                CommandLineEvent::Pending
            }
            KeyCode::Right => {
                self.error = None;
                self.move_cursor(1);
                self.refresh(registry, resolver);
                CommandLineEvent::Pending
            }
            KeyCode::Home => {
                self.error = None;
                self.cursor = 0;
                self.refresh(registry, resolver);
                CommandLineEvent::Pending
            }
            KeyCode::End => {
                self.error = None;
                self.cursor = self.text.len();
                self.refresh(registry, resolver);
                CommandLineEvent::Pending
            }
            KeyCode::Char(c) => {
                self.error = None;
                self.insert_char(c);
                self.refresh(registry, resolver);
                CommandLineEvent::Pending
            }
            _ => CommandLineEvent::Pending,
        }
    }

    /// Apply the completion engine's current candidates. Matches
    /// vim's `wildmode=longest:full,full`:
    ///
    /// - Single match: insert it and close the popup.
    /// - Popup closed: insert the longest common prefix (if it extends
    ///   what the user has typed) and open the popup with no row
    ///   selected. The user sees the candidate list and can browse
    ///   before committing to one.
    /// - Popup already open: cycle selection forward or backward.
    pub fn tab(&mut self, forward: bool) {
        if self.completions.items.is_empty() {
            return;
        }
        if self.completions.items.len() == 1 {
            let value = self.completions.items[0].value.clone();
            self.replace_at_anchor(&value);
            self.dismiss_completions();
            return;
        }
        if self.popup_open {
            self.cycle(forward);
            return;
        }
        let lcp = longest_common_prefix(self.completions.items.iter().map(|i| i.value.as_str()));
        let anchor = self.completions.anchor;
        let current_prefix = self.text.get(anchor..self.cursor).unwrap_or("");
        if lcp.len() > current_prefix.len() && lcp.starts_with(current_prefix) {
            self.replace_at_anchor(&lcp);
        }
        self.popup_open = true;
        self.selected = None;
        let _ = forward;
    }

    fn cycle(&mut self, forward: bool) {
        let n = self.completions.items.len();
        if n == 0 {
            return;
        }
        let next = match self.selected {
            None => {
                if forward {
                    0
                } else {
                    n - 1
                }
            }
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        };
        self.selected = Some(next);
        let value = self.completions.items[next].value.clone();
        self.replace_at_anchor(&value);
    }

    fn refresh(&mut self, registry: &[&CommandSpec], resolver: &dyn Resolver) {
        self.popup_open = false;
        self.selected = None;
        self.completions = complete(registry, &self.text, self.cursor, resolver);
    }

    fn replace_at_anchor(&mut self, value: &str) {
        let anchor = self.completions.anchor.min(self.text.len());
        let end = self.cursor.max(anchor).min(self.text.len());
        self.text.drain(anchor..end);
        self.text.insert_str(anchor, value);
        self.cursor = anchor + value.len();
    }

    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    fn move_cursor(&mut self, delta: i32) {
        let target = i64::try_from(self.cursor).unwrap_or(0) + i64::from(delta);
        if target < 0 {
            self.cursor = 0;
        } else if let Ok(pos) = usize::try_from(target) {
            self.cursor = pos.min(self.text.len());
        }
    }
}

#[cfg(test)]
impl CommandLine {
    /// Build a `CommandLine` in a specific state. Used by `view`
    /// snapshot tests to exercise popup rendering without driving
    /// through a full keystroke sequence.
    pub(crate) fn for_test(
        text: &str,
        completions: Completions,
        popup_open: bool,
        selected: Option<usize>,
    ) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.len(),
            completions,
            popup_open,
            selected,
            error: None,
        }
    }

    /// Build a `CommandLine` with a validation error set. Used by
    /// view snapshot tests to exercise error rendering.
    pub(crate) fn for_test_with_error(text: &str, error: &str) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.len(),
            completions: Completions::default(),
            popup_open: false,
            selected: None,
            error: Some(error.to_owned()),
        }
    }
}

fn longest_common_prefix<'a, I: IntoIterator<Item = &'a str>>(values: I) -> String {
    let mut iter = values.into_iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix = first.to_owned();
    for v in iter {
        while !v.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return String::new();
            }
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmdparse::EmptyResolver;
    use crate::command::{ArgSource, ArgSpec, CommandCategory};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn empty_registry() -> Vec<&'static CommandSpec> {
        Vec::new()
    }

    fn send(cl: &mut CommandLine, k: KeyEvent) -> CommandLineEvent {
        cl.handle_key(k, &empty_registry(), &EmptyResolver)
    }

    fn send_with(
        cl: &mut CommandLine,
        k: KeyEvent,
        registry: &[&CommandSpec],
        resolver: &dyn Resolver,
    ) -> CommandLineEvent {
        cl.handle_key(k, registry, resolver)
    }

    const QUIT: CommandSpec = CommandSpec {
        name: "quit",
        aliases: &["q"],
        description: "leave",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    };

    const MODEL: CommandSpec = CommandSpec {
        name: "model",
        aliases: &[],
        description: "switch model",
        category: CommandCategory::Both,
        args: &[ArgSpec::DynamicChoice {
            name: "id",
            source: ArgSource::Models,
            optional: false,
        }],
        subcommands: &[],
    };

    const MOUSE: CommandSpec = CommandSpec {
        name: "mouse",
        aliases: &[],
        description: "toggle mouse",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "state",
            values: &["on", "off", "toggle"],
            optional: false,
        }],
        subcommands: &[],
    };

    const HELP: CommandSpec = CommandSpec {
        name: "help",
        aliases: &[],
        description: "list commands",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    };

    fn registry() -> Vec<&'static CommandSpec> {
        vec![&QUIT, &MODEL, &MOUSE, &HELP]
    }

    struct ModelResolver(Vec<String>);
    impl Resolver for ModelResolver {
        fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
            match source {
                ArgSource::Models => self.0.clone(),
                _ => Vec::new(),
            }
        }
    }

    #[test]
    fn typing_appends_to_text() {
        let mut cl = CommandLine::new();
        for c in "model".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        assert_eq!(cl.text(), "model");
        assert_eq!(cl.cursor(), 5);
    }

    #[test]
    fn enter_submits_trimmed_text() {
        let mut cl = CommandLine::new();
        for c in "  q  ".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        assert_eq!(
            send(&mut cl, key(KeyCode::Enter)),
            CommandLineEvent::Submit("q".into())
        );
    }

    #[test]
    fn enter_on_empty_text_cancels() {
        let mut cl = CommandLine::new();
        assert_eq!(
            send(&mut cl, key(KeyCode::Enter)),
            CommandLineEvent::Cancelled
        );
    }

    #[test]
    fn esc_and_ctrl_c_cancel_when_popup_closed() {
        let mut cl = CommandLine::new();
        send(&mut cl, key(KeyCode::Char('x')));
        assert_eq!(
            send(&mut cl, key(KeyCode::Esc)),
            CommandLineEvent::Cancelled
        );
        let mut cl2 = CommandLine::new();
        send(&mut cl2, key(KeyCode::Char('x')));
        assert_eq!(send(&mut cl2, ctrl('c')), CommandLineEvent::Cancelled);
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        send(&mut cl, key(KeyCode::Backspace));
        assert_eq!(cl.text(), "ab");
        assert_eq!(cl.cursor(), 2);
    }

    #[test]
    fn left_right_move_cursor() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        send(&mut cl, key(KeyCode::Left));
        send(&mut cl, key(KeyCode::Left));
        assert_eq!(cl.cursor(), 1);
        send(&mut cl, key(KeyCode::Char('X')));
        assert_eq!(cl.text(), "aXbc");
    }

    #[test]
    fn home_and_end_jump() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        send(&mut cl, key(KeyCode::Home));
        assert_eq!(cl.cursor(), 0);
        send(&mut cl, key(KeyCode::End));
        assert_eq!(cl.cursor(), 3);
    }

    #[test]
    fn typing_refreshes_completions() {
        let mut cl = CommandLine::new();
        let reg = registry();
        send_with(&mut cl, key(KeyCode::Char('m')), &reg, &EmptyResolver);
        let names: Vec<&str> = cl
            .completions()
            .items
            .iter()
            .map(|i| i.value.as_str())
            .collect();
        assert!(names.contains(&"model"));
        assert!(names.contains(&"mouse"));
        assert!(!cl.popup_open());
    }

    #[test]
    fn tab_with_single_match_completes_and_closes_popup() {
        let mut cl = CommandLine::new();
        let reg = registry();
        send_with(&mut cl, key(KeyCode::Char('h')), &reg, &EmptyResolver);
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.text(), "help");
        assert!(!cl.popup_open());
    }

    #[test]
    fn tab_extends_to_lcp_then_cycles() {
        let mut cl = CommandLine::new();
        let reg = registry();
        send_with(&mut cl, key(KeyCode::Char('m')), &reg, &EmptyResolver);
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.text(), "mo");
        assert!(cl.popup_open());
        assert_eq!(cl.selected(), None);
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(0));
        assert!(cl.text() == "model" || cl.text() == "mouse");
    }

    #[test]
    fn tab_cycles_forward_and_backward() {
        let mut cl = CommandLine::new();
        let reg = registry();
        for c in "mo".chars() {
            send_with(&mut cl, key(KeyCode::Char(c)), &reg, &EmptyResolver);
        }
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert!(cl.popup_open());
        assert_eq!(cl.selected(), None);
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(0));
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(1));
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(0));
        send_with(&mut cl, key(KeyCode::BackTab), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(1));
    }

    #[test]
    fn down_up_cycle_when_popup_open() {
        let mut cl = CommandLine::new();
        let reg = registry();
        for c in "mo".chars() {
            send_with(&mut cl, key(KeyCode::Char(c)), &reg, &EmptyResolver);
        }
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), None);
        send_with(&mut cl, key(KeyCode::Down), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(0));
        send_with(&mut cl, key(KeyCode::Down), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(1));
        send_with(&mut cl, key(KeyCode::Up), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(0));
    }

    #[test]
    fn tab_on_empty_input_opens_popup_without_cycling() {
        let mut cl = CommandLine::new();
        let reg = registry();
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert_eq!(cl.text(), "");
        assert!(cl.popup_open());
        assert_eq!(cl.selected(), None);
        // Down moves into the list after the popup is open.
        send_with(&mut cl, key(KeyCode::Down), &reg, &EmptyResolver);
        assert_eq!(cl.selected(), Some(0));
    }

    #[test]
    fn esc_dismisses_popup_first_then_cancels() {
        let mut cl = CommandLine::new();
        let reg = registry();
        for c in "mo".chars() {
            send_with(&mut cl, key(KeyCode::Char(c)), &reg, &EmptyResolver);
        }
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert!(cl.popup_open());
        let ev = send_with(&mut cl, key(KeyCode::Esc), &reg, &EmptyResolver);
        assert_eq!(ev, CommandLineEvent::Pending);
        assert!(!cl.popup_open());
        let ev = send_with(&mut cl, key(KeyCode::Esc), &reg, &EmptyResolver);
        assert_eq!(ev, CommandLineEvent::Cancelled);
    }

    #[test]
    fn editing_closes_popup_and_refreshes() {
        let mut cl = CommandLine::new();
        let reg = registry();
        for c in "mo".chars() {
            send_with(&mut cl, key(KeyCode::Char(c)), &reg, &EmptyResolver);
        }
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert!(cl.popup_open());
        send_with(&mut cl, key(KeyCode::Backspace), &reg, &EmptyResolver);
        assert!(!cl.popup_open());
    }

    #[test]
    fn dynamic_choice_uses_resolver() {
        let mut cl = CommandLine::new();
        let reg = registry();
        let res = ModelResolver(vec![
            "anthropic:claude".into(),
            "anthropic:opus".into(),
            "openai:gpt".into(),
        ]);
        for c in "model anth".chars() {
            send_with(&mut cl, key(KeyCode::Char(c)), &reg, &res);
        }
        send_with(&mut cl, key(KeyCode::Tab), &reg, &res);
        assert_eq!(cl.text(), "model anthropic:");
        assert!(cl.popup_open());
    }

    #[test]
    fn lcp_helper_returns_common_prefix() {
        assert_eq!(longest_common_prefix(["model", "mouse"]), "mo");
        assert_eq!(longest_common_prefix(["a", "b"]), "");
        assert_eq!(longest_common_prefix(["only"]), "only");
        let empty: [&str; 0] = [];
        assert_eq!(longest_common_prefix(empty), "");
    }

    // --- Inline error tests (PN.9) ---

    #[test]
    fn set_error_stores_message_accessible_via_error() {
        let mut cl = CommandLine::new();
        assert!(cl.error().is_none());
        cl.set_error("bad arg");
        assert_eq!(cl.error(), Some("bad arg"));
    }

    #[test]
    fn clear_error_removes_message() {
        let mut cl = CommandLine::new();
        cl.set_error("oops");
        cl.clear_error();
        assert!(cl.error().is_none());
    }

    #[test]
    fn typing_clears_error() {
        let mut cl = CommandLine::new();
        cl.set_error("fix me");
        assert_eq!(cl.error(), Some("fix me"));
        send(&mut cl, key(KeyCode::Char('x')));
        assert!(cl.error().is_none(), "typing should clear the error");
    }

    #[test]
    fn backspace_clears_error() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        cl.set_error("fix me");
        assert_eq!(cl.error(), Some("fix me"));
        send(&mut cl, key(KeyCode::Backspace));
        assert!(cl.error().is_none(), "backspace should clear the error");
    }

    #[test]
    fn left_right_clear_error() {
        let mut cl = CommandLine::new();
        for c in "abc".chars() {
            send(&mut cl, key(KeyCode::Char(c)));
        }
        cl.set_error("fix me");
        send(&mut cl, key(KeyCode::Left));
        assert!(cl.error().is_none(), "cursor left should clear the error");
    }

    #[test]
    fn esc_does_not_clear_error_when_popup_closed() {
        let mut cl = CommandLine::new();
        cl.set_error("fix me");
        // Esc cancels the cmdline when popup is closed; the error
        // state doesn't matter because the cmdline is about to be
        // destroyed. We just verify the event is Cancelled.
        assert_eq!(
            send(&mut cl, key(KeyCode::Esc)),
            CommandLineEvent::Cancelled
        );
    }

    #[test]
    fn tab_clears_error() {
        let mut cl = CommandLine::new();
        cl.set_error("fix me");
        let reg = registry();
        send_with(&mut cl, key(KeyCode::Tab), &reg, &EmptyResolver);
        assert!(cl.error().is_none(), "tab should clear the error");
    }
}
