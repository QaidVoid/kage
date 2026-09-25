//! Inline approval panel.
//!
//! Shown in place of the input box while a tool call waits for the
//! user's decision. The title names the action, the summary shows what
//! the call does (a command, a diff, a file head, or its arguments),
//! and five numbered options answer it. Option 5 turns the panel into
//! a one-line field whose text goes to the model with the denial. A
//! request from an agent carries the agent's name and task in the title
//! and its name in option 5, since the text goes to that agent.
//!
//! Keys typed in the first [`TYPE_AHEAD_GUARD`] after the panel opens
//! are dropped, so type-ahead meant for the prompt cannot answer it.

use std::time::{Duration, Instant};

use kage_core::protocol::PermissionDecision;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cmdline::{CommandLine, CommandLineEvent};
use crate::cmdparse::EmptyResolver;
use crate::theme::Theme;
use crate::view::tool_view::{self, EditDiff, LineKind};

/// How long keys are dropped after a panel opens.
pub const TYPE_AHEAD_GUARD: Duration = Duration::from_millis(400);

const RULE: &str = "\u{2500}";
const RULE_LEAD: usize = 2;
const INDENT: &str = "   ";
const INDENT_WIDTH: u16 = 3;
const BASH_LINES: usize = 6;
const DIFF_LINES: usize = 10;
const WRITE_LINES: usize = 6;
const OPTIONS: usize = 5;
const NO: usize = 3;
const TELL: usize = 4;
const SEP: &str = " \u{B7} ";
/// Fewest cells of an agent's task the title shows before dropping it.
const MIN_TASK_WIDTH: usize = 8;

/// What a key did to the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The panel stays open.
    Stay,
    /// Answer the request with this decision.
    Decide(PermissionDecision),
    /// Deny the request, then send this text to the model.
    Feedback(String),
}

/// The approval prompt for one gated tool call, painted into the input
/// region.
#[derive(Debug)]
pub struct ApprovalPanel {
    tool: String,
    input: Value,
    question: String,
    /// The asking agent's name and task. `None` when the main session
    /// asks.
    agent: Option<(String, String)>,
    selected: usize,
    feedback: Option<CommandLine>,
    /// The feedback field closed with `Esc`, restored when option 5
    /// opens it again.
    parked: Option<CommandLine>,
    opened_at: Instant,
    /// An edit's change as the file shows it, in place of the one its
    /// input describes.
    diff: Option<EditDiff>,
}

impl ApprovalPanel {
    /// Build the panel for a call to `tool` with `input`, asked by the
    /// agent `agent` (its name and task) or, with `None`, by the main
    /// session. "Yes" starts selected.
    #[must_use]
    pub fn new(tool: &str, input: &Value, agent: Option<(&str, &str)>, opened_at: Instant) -> Self {
        Self {
            tool: tool.to_owned(),
            input: input.clone(),
            question: tool_view::question(tool, input),
            agent: agent.map(|(name, task)| (name.to_owned(), task.to_owned())),
            selected: 0,
            feedback: None,
            parked: None,
            opened_at,
            diff: None,
        }
    }

    /// Show `diff` as an edit's change instead of the one its input
    /// describes, such as the lines of the file it would change.
    #[must_use]
    pub fn with_diff(mut self, diff: Option<EditDiff>) -> Self {
        self.diff = diff;
        self
    }

    /// Who reads option 5's text: the asking agent, else `kage`.
    fn asker(&self) -> &str {
        self.agent.as_ref().map_or("kage", |(name, _)| name)
    }

    /// The title in `room` cells: the question, after the asking
    /// agent's name and its task cut to fit. The task is left out when
    /// too little of it would show.
    fn title(&self, room: usize) -> String {
        let Some((name, task)) = &self.agent else {
            return self.question.clone();
        };
        let short = format!("{name}{SEP}{}", self.question);
        let task_room = room.saturating_sub(short.width() + 2);
        if task.is_empty() || task_room < MIN_TASK_WIDTH.min(task.width()) {
            return short;
        }
        let task = crate::view::truncate_to_width(task, task_room, "...");
        format!("{name}: {task}{SEP}{}", self.question)
    }

    /// Whether the feedback field is open.
    #[must_use]
    pub fn in_feedback(&self) -> bool {
        self.feedback.is_some()
    }

    /// The footer hint for the panel's keys.
    #[must_use]
    pub fn hint(&self) -> String {
        let parts: &[&str] = if self.in_feedback() {
            &["enter to send", "esc to go back"]
        } else {
            &["y/s/a/n/t or 1-5", "enter", "esc no"]
        };
        parts.join(SEP)
    }

    /// Handle `key` pressed at `now`. Keys within [`TYPE_AHEAD_GUARD`]
    /// of opening are dropped.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) -> ApprovalOutcome {
        if key.kind != KeyEventKind::Press
            || now.saturating_duration_since(self.opened_at) < TYPE_AHEAD_GUARD
        {
            return ApprovalOutcome::Stay;
        }
        if let Some(field) = self.feedback.as_mut() {
            if key.code == KeyCode::Esc {
                self.parked = self.feedback.take();
                return ApprovalOutcome::Stay;
            }
            return match field.handle_key(key, &[], &EmptyResolver) {
                CommandLineEvent::Submit(text) => ApprovalOutcome::Feedback(text),
                CommandLineEvent::Pending | CommandLineEvent::Cancelled => ApprovalOutcome::Stay,
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ApprovalOutcome::Stay;
        }
        let choice = match key.code {
            KeyCode::Esc => NO,
            KeyCode::Up => {
                self.selected = (self.selected + OPTIONS - 1) % OPTIONS;
                return ApprovalOutcome::Stay;
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % OPTIONS;
                return ApprovalOutcome::Stay;
            }
            KeyCode::Enter => self.selected,
            KeyCode::Char(c) => match c.to_ascii_lowercase() {
                '1' | 'y' => 0,
                '2' | 's' => 1,
                '3' | 'a' => 2,
                '4' | 'n' => NO,
                '5' | 't' => TELL,
                _ => return ApprovalOutcome::Stay,
            },
            _ => return ApprovalOutcome::Stay,
        };
        self.selected = choice;
        match choice {
            0 => ApprovalOutcome::Decide(PermissionDecision::AllowOnce),
            1 => ApprovalOutcome::Decide(PermissionDecision::AllowSession),
            2 => ApprovalOutcome::Decide(PermissionDecision::AllowAlways),
            TELL => {
                self.feedback = Some(self.parked.take().unwrap_or_default());
                ApprovalOutcome::Stay
            }
            _ => ApprovalOutcome::Decide(PermissionDecision::Deny),
        }
    }

    /// Insert pasted text into the feedback field, when it is open.
    pub fn paste(&mut self, text: &str) {
        if let Some(field) = self.feedback.as_mut() {
            field.paste_str(text, &[], &EmptyResolver);
        }
    }

    /// Rows the panel wants at terminal `width`: the two rules, the
    /// summary and the options or the feedback field.
    #[must_use]
    pub fn height(&self, width: u16) -> u16 {
        let theme = crate::theme::current();
        let summary = self.summary(&theme, width);
        let rows = 2 + summary.len() + self.body(&theme, width, !summary.is_empty()).len();
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    /// Paint the panel into `area`. `waiting` is the number of requests
    /// queued behind this one, which the count adds to this one. When
    /// `area` is short, summary lines are dropped first.
    pub fn render(&self, frame: &mut Frame, area: Rect, waiting: usize) {
        if area.height < 2 || area.width == 0 {
            return;
        }
        let theme = crate::theme::current();
        let rule = theme.group_style("KageApproval");
        let count = (waiting > 0).then(|| format!("1 of {}", waiting + 1));
        let right = count.as_ref().map_or(0, |c| c.width() + 2 + RULE_LEAD);
        let room = usize::from(area.width).saturating_sub(right + RULE_LEAD + 2);
        let mut lines = vec![rule_line(area.width, &self.title(room), count, rule)];
        let summary = self.summary(&theme, area.width);
        let body = self.body(&theme, area.width, !summary.is_empty());
        let room = usize::from(area.height - 2).saturating_sub(body.len());
        lines.extend(summary.into_iter().take(room));
        lines.extend(body);
        lines.truncate(usize::from(area.height - 1));
        let field_row = u16::try_from(lines.len() - 1).unwrap_or(u16::MAX);
        lines.push(rule_line(area.width, "", None, rule));
        frame.render_widget(Paragraph::new(lines), area);

        if let Some(field) = &self.feedback {
            let (_, cursor) = field_view(field, area.width);
            let x = area.x + INDENT_WIDTH + u16::try_from(cursor).unwrap_or(u16::MAX);
            if x < area.right() {
                frame.set_cursor_position((x, area.y + field_row));
            }
        }
    }

    fn summary(&self, theme: &Theme, width: u16) -> Vec<Line<'static>> {
        let text = Style::default().fg(theme.assistant_fg);
        let muted = theme.group_style("KageMuted");
        let field = |key| self.input.get(key).and_then(Value::as_str).unwrap_or("");
        let plain = |s: String| Line::from(Span::styled(s, text));
        let lines = match self.tool.as_str() {
            "bash" => {
                let wrap = width.saturating_sub(INDENT_WIDTH + 2);
                let rows = field("command")
                    .trim()
                    .lines()
                    .enumerate()
                    .flat_map(|(i, line)| {
                        crate::view::wrap_input_rows(line, wrap)
                            .into_iter()
                            .enumerate()
                            .map(|(row, (s, e))| {
                                let lead = if i == 0 && row == 0 { "$ " } else { "  " };
                                format!("{lead}{}", &line[s..e])
                            })
                            .collect::<Vec<_>>()
                    })
                    .map(plain)
                    .collect();
                capped(rows, BASH_LINES, muted)
            }
            "edit" => {
                let diff = self
                    .diff
                    .clone()
                    .unwrap_or_else(|| tool_view::edit_diff(&self.input));
                let rows = diff
                    .lines
                    .into_iter()
                    .map(|l| match l.kind {
                        LineKind::Add => {
                            Line::styled(format!("+ {}", l.text), theme.group_style("KageDiffAdd"))
                        }
                        LineKind::Delete => Line::styled(
                            format!("- {}", l.text),
                            theme.group_style("KageDiffDelete"),
                        ),
                        LineKind::Context => Line::styled(format!("  {}", l.text), muted),
                        LineKind::Marker => Line::styled(l.text, muted),
                        LineKind::Text => plain(l.text),
                    })
                    .collect();
                capped(rows, DIFF_LINES, muted)
            }
            "write" => {
                let mut rows = vec![plain(field("path").to_owned())];
                let content = field("content").lines().map(|l| plain(l.to_owned()));
                rows.extend(capped(content.collect(), WRITE_LINES, muted));
                rows
            }
            "read" => vec![plain(field("path").to_owned())],
            _ => tool_view::arg_rows(&self.input)
                .into_iter()
                .map(|(key, value)| {
                    Line::from(vec![
                        Span::styled(key, muted),
                        Span::styled(format!("   {value}"), text),
                    ])
                })
                .collect(),
        };
        lines
            .into_iter()
            .map(|mut line| {
                line.spans.insert(0, Span::raw(INDENT));
                line
            })
            .collect()
    }

    fn body(&self, theme: &Theme, width: u16, gap: bool) -> Vec<Line<'static>> {
        let text = Style::default().fg(theme.assistant_fg);
        let approval = theme.group_style("KageApproval");
        let mut lines = if gap {
            vec![Line::default()]
        } else {
            Vec::new()
        };
        if let Some(field) = &self.feedback {
            let what = if self.tool == "bash" {
                "command"
            } else {
                "call"
            };
            let help = format!(
                "Tell {} what to do instead. Enter sends it and denies the {what}, esc goes back.",
                self.asker()
            );
            let muted = theme.group_style("KageMuted");
            let wrap = width.saturating_sub(INDENT_WIDTH);
            lines.extend(
                crate::view::wrap_input_rows(&help, wrap)
                    .into_iter()
                    .map(|(s, e)| Line::styled(format!("{INDENT}{}", &help[s..e]), muted)),
            );
            let (visible, _) = field_view(field, width);
            lines.push(Line::from(vec![
                Span::styled(" > ", approval),
                Span::styled(visible, text),
            ]));
            return lines;
        }
        let tool = &self.tool;
        let labels = [
            "Yes".to_owned(),
            format!("Yes, and allow {tool} for the rest of this session"),
            format!("Yes, and always allow {tool} (saved to config.toml)"),
            "No".to_owned(),
            format!("No, and tell {} what to do instead", self.asker()),
        ];
        lines.extend(labels.into_iter().enumerate().map(|(idx, label)| {
            let label = format!("{}. {label}", idx + 1);
            if idx == self.selected {
                let strong = approval.add_modifier(Modifier::BOLD);
                Line::from(vec![
                    Span::styled(" > ", strong),
                    Span::styled(label, strong),
                ])
            } else {
                Line::from(vec![Span::raw(INDENT), Span::styled(label, text)])
            }
        }));
        lines
    }
}

/// The part of the field's text that fits after the indent at `width`,
/// scrolled so the cursor stays visible, and the cursor's column in it.
fn field_view(field: &CommandLine, width: u16) -> (String, usize) {
    let room = usize::from(width.saturating_sub(INDENT_WIDTH + 1)).max(1);
    let text = field.text();
    let before = text[..field.cursor()].width();
    let mut skip = (before + 1).saturating_sub(room);
    let mut start = 0;
    for (idx, c) in text.char_indices() {
        if skip == 0 {
            break;
        }
        start = idx + c.len_utf8();
        skip = skip.saturating_sub(UnicodeWidthChar::width(c).unwrap_or(0));
    }
    let visible = crate::view::truncate_to_width(&text[start..], room, "");
    (visible, before - text[..start].width())
}

/// The first `max` of `lines`, then a muted note for the rest.
fn capped(mut lines: Vec<Line<'static>>, max: usize, muted: Style) -> Vec<Line<'static>> {
    if lines.len() > max {
        let more = lines.len() - max;
        lines.truncate(max);
        let noun = if more == 1 { "line" } else { "lines" };
        lines.push(Line::styled(format!("... +{more} more {noun}"), muted));
    }
    lines
}

/// A rule row `width` cells wide with `title` after the lead and
/// `count` before the tail.
fn rule_line(width: u16, title: &str, count: Option<String>, rule: Style) -> Line<'static> {
    let width = usize::from(width);
    let lead = RULE.repeat(RULE_LEAD);
    let right = count.map_or_else(String::new, |c| format!(" {c} {lead}"));
    let mut used = right.width();
    let mut spans = Vec::with_capacity(5);
    if !title.is_empty() {
        let budget = width.saturating_sub(used + RULE_LEAD + 2);
        let title = crate::view::truncate_to_width(title, budget, "\u{2026}");
        used += RULE_LEAD + 2 + title.width();
        spans.push(Span::styled(format!("{lead} "), rule));
        spans.push(Span::styled(title, rule.add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(" ", rule));
    }
    spans.push(Span::styled(RULE.repeat(width.saturating_sub(used)), rule));
    spans.push(Span::styled(right, rule));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn opened() -> (ApprovalPanel, Instant) {
        let at = Instant::now();
        (
            ApprovalPanel::new("bash", &json!({"command": "ls"}), None, at),
            at + Duration::from_millis(500),
        )
    }

    fn rows(panel: &ApprovalPanel, width: u16, waiting: usize) -> Vec<String> {
        let height = panel.height(width);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area(), waiting))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn bash_shows_the_command_and_five_options() {
        let at = Instant::now();
        let panel = ApprovalPanel::new("bash", &json!({"command": "cargo test"}), None, at);
        let rows = rows(&panel, 80, 0);
        assert!(rows[0].contains("Run this command?"), "{rows:#?}");
        assert_eq!(rows[1], "   $ cargo test");
        assert_eq!(rows[3], " > 1. Yes");
        assert_eq!(
            rows[4],
            "   2. Yes, and allow bash for the rest of this session"
        );
        assert_eq!(
            rows[5],
            "   3. Yes, and always allow bash (saved to config.toml)"
        );
        assert_eq!(rows[7], "   5. No, and tell kage what to do instead");
        assert!(rows[8].chars().all(|c| c == '\u{2500}'), "{rows:#?}");
        assert!(rows.iter().all(|r| !r.contains('{')), "{rows:#?}");
    }

    #[test]
    fn a_wrapped_command_hangs_under_its_first_word() {
        let at = Instant::now();
        let command = "cargo test --workspace --all-features --no-fail-fast";
        let panel = ApprovalPanel::new("bash", &json!({ "command": command }), None, at);
        let rows = rows(&panel, 30, 0);
        assert_eq!(rows[1], "   $ cargo test --workspace");
        assert_eq!(rows[2], "     --all-features");
        assert_eq!(rows[3], "     --no-fail-fast");
    }

    #[test]
    fn long_bash_commands_wrap_to_six_lines() {
        let at = Instant::now();
        let command = (0..10).map(|i| format!("step{i}")).collect::<Vec<_>>();
        let panel = ApprovalPanel::new("bash", &json!({"command": command.join("\n")}), None, at);
        let rows = rows(&panel, 40, 0);
        assert_eq!(rows[1], "   $ step0");
        assert_eq!(rows[6], "     step5");
        assert_eq!(rows[7], "   ... +4 more lines");
    }

    #[test]
    fn edit_shows_diff_lines_in_the_diff_groups() {
        let at = Instant::now();
        let input = json!({"path": "a.rs", "old_str": "old", "new_str": "new"});
        let panel = ApprovalPanel::new("edit", &input, None, at);
        assert!(rows(&panel, 60, 0)[0].contains("Edit a.rs?"));
        let groups = crate::theme::groups_for("default", None).unwrap();
        let theme = Theme::from_groups(&groups.into_highlights("default"));
        let summary = panel.summary(&theme, 60);
        let line = |idx: usize| (summary[idx].to_string(), summary[idx].style);
        assert_eq!(
            line(0),
            ("   - old".to_owned(), theme.group_style("KageDiffDelete"))
        );
        assert_eq!(
            line(1),
            ("   + new".to_owned(), theme.group_style("KageDiffAdd"))
        );
        assert_ne!(
            theme.group_style("KageDiffAdd"),
            theme.group_style("KageDiffDelete")
        );
    }

    #[test]
    fn edit_diff_is_capped_at_ten_lines() {
        let at = Instant::now();
        let new: Vec<String> = (0..14).map(|i| format!("n{i}")).collect();
        let input = json!({"path": "a", "old_str": "x", "new_str": new.join("\n")});
        let rows = rows(&ApprovalPanel::new("edit", &input, None, at), 60, 0);
        assert_eq!(rows[10], "   + n8");
        assert_eq!(rows[11], "   ... +5 more lines");
    }

    #[test]
    fn write_shows_the_path_and_its_head() {
        let at = Instant::now();
        let input = json!({"path": "b.txt", "content": "one\ntwo"});
        let rows = rows(&ApprovalPanel::new("write", &input, None, at), 60, 0);
        assert_eq!(&rows[1..4], ["   b.txt", "   one", "   two"]);
    }

    #[test]
    fn mcp_tools_show_key_value_rows() {
        let at = Instant::now();
        let input = json!({"repo": "qaidvoid/kage", "title": "Palette"});
        let panel = ApprovalPanel::new("github__create_issue", &input, None, at);
        let rows = rows(&panel, 80, 0);
        assert!(rows[0].contains("Allow github.create_issue?"), "{rows:#?}");
        assert_eq!(rows[1], "   repo    qaidvoid/kage");
        assert_eq!(rows[2], "   title   Palette");
    }

    #[test]
    fn keys_inside_the_guard_are_dropped() {
        let at = Instant::now();
        let mut panel = ApprovalPanel::new("bash", &json!({"command": "ls"}), None, at);
        let early = at + Duration::from_millis(100);
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Char('y')), early),
            ApprovalOutcome::Stay
        );
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), early),
            ApprovalOutcome::Stay
        );
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Char('y')), at + Duration::from_millis(500)),
            ApprovalOutcome::Decide(PermissionDecision::AllowOnce)
        );
    }

    #[test]
    fn shortcuts_map_to_decisions() {
        let cases = [
            ('1', PermissionDecision::AllowOnce),
            ('y', PermissionDecision::AllowOnce),
            ('2', PermissionDecision::AllowSession),
            ('s', PermissionDecision::AllowSession),
            ('3', PermissionDecision::AllowAlways),
            ('a', PermissionDecision::AllowAlways),
            ('4', PermissionDecision::Deny),
            ('n', PermissionDecision::Deny),
        ];
        for (c, decision) in cases {
            let (mut panel, now) = opened();
            assert_eq!(
                panel.handle_key_at(key(KeyCode::Char(c)), now),
                ApprovalOutcome::Decide(decision),
                "{c}"
            );
        }
        let (mut panel, now) = opened();
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Esc), now),
            ApprovalOutcome::Decide(PermissionDecision::Deny)
        );
    }

    #[test]
    fn yes_is_preselected_and_arrows_move() {
        let (mut panel, now) = opened();
        panel.handle_key_at(key(KeyCode::Up), now);
        assert!(!panel.in_feedback());
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), now),
            ApprovalOutcome::Stay
        );
        assert!(panel.in_feedback());

        let (mut panel, now) = opened();
        panel.handle_key_at(key(KeyCode::Down), now);
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), now),
            ApprovalOutcome::Decide(PermissionDecision::AllowSession)
        );

        let (mut panel, now) = opened();
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), now),
            ApprovalOutcome::Decide(PermissionDecision::AllowOnce)
        );
    }

    #[test]
    fn feedback_field_sends_its_text() {
        let (mut panel, now) = opened();
        panel.handle_key_at(key(KeyCode::Char('t')), now);
        assert!(panel.in_feedback());
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), now),
            ApprovalOutcome::Stay
        );
        for c in "use ls -a".chars() {
            panel.handle_key_at(key(KeyCode::Char(c)), now);
        }
        let rows = rows(&panel, 100, 0);
        assert_eq!(
            rows[3],
            "   Tell kage what to do instead. Enter sends it and denies the command, esc goes back."
        );
        assert_eq!(rows[4], " > use ls -a");
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), now),
            ApprovalOutcome::Feedback("use ls -a".to_owned())
        );
    }

    #[test]
    fn esc_in_feedback_returns_to_the_options() {
        let (mut panel, now) = opened();
        panel.handle_key_at(key(KeyCode::Char('5')), now);
        panel.handle_key_at(key(KeyCode::Char('x')), now);
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Esc), now),
            ApprovalOutcome::Stay
        );
        assert!(!panel.in_feedback());
        assert!(
            rows(&panel, 80, 0)
                .iter()
                .any(|r| r.contains("5. No, and tell"))
        );
        panel.handle_key_at(key(KeyCode::Char('5')), now);
        panel.handle_key_at(key(KeyCode::Char('y')), now);
        assert_eq!(
            panel.handle_key_at(key(KeyCode::Enter), now),
            ApprovalOutcome::Feedback("xy".to_owned())
        );
    }

    #[test]
    fn the_count_shows_the_pending_requests_only_with_a_queue() {
        let at = Instant::now();
        let panel = ApprovalPanel::new("bash", &json!({"command": "ls"}), None, at);
        assert!(rows(&panel, 80, 1)[0].ends_with(" 1 of 2 \u{2500}\u{2500}"));
        assert!(!rows(&panel, 80, 0)[0].contains(" of "));
    }

    #[test]
    fn an_agent_request_names_the_agent() {
        let at = Instant::now();
        let input = json!({"command": "cargo test"});
        let mut panel = ApprovalPanel::new("bash", &input, Some(("explore", "map src")), at);
        let options = rows(&panel, 80, 0);
        assert!(
            options[0]
                .starts_with("\u{2500}\u{2500} explore: map src \u{B7} Run this command? \u{2500}"),
            "{options:#?}"
        );
        assert_eq!(options[7], "   5. No, and tell explore what to do instead");
        panel.handle_key_at(key(KeyCode::Char('t')), at + Duration::from_millis(500));
        let field = rows(&panel, 100, 0);
        assert!(
            field[3].starts_with("   Tell explore what to do instead."),
            "{field:#?}"
        );
    }

    #[test]
    fn a_long_task_is_cut_to_fit_and_dropped_when_too_narrow() {
        let at = Instant::now();
        let input = json!({"path": "src/components/Button.tsx"});
        let task = "map the named exports of every file under src/components";
        let panel = ApprovalPanel::new("read", &input, Some(("explore", task)), at);
        let wide = rows(&panel, 80, 1);
        assert!(wide[0].contains("explore: map the named"), "{wide:#?}");
        assert!(wide[0].contains("... \u{B7} Read"), "{wide:#?}");
        assert!(wide[0].ends_with(" 1 of 2 \u{2500}\u{2500}"), "{wide:#?}");
        assert!(wide[0].width() <= 80, "{wide:#?}");
        let narrow = rows(&panel, 44, 0);
        assert!(narrow[0].contains("explore \u{B7} Read"), "{narrow:#?}");
    }

    #[test]
    fn a_short_area_drops_summary_lines_first() {
        let at = Instant::now();
        let command = (0..6).map(|i| format!("s{i}")).collect::<Vec<_>>();
        let panel = ApprovalPanel::new("bash", &json!({"command": command.join("\n")}), None, at);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area(), 0))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol()).collect() };
        assert!(row(1).starts_with("   $ s0"));
        assert!(row(2).starts_with("     s1"));
        assert!(row(4).starts_with(" > 1. Yes"));
        assert!(row(8).starts_with("   5. No"));
    }

    #[test]
    fn hint_follows_the_mode() {
        let (mut panel, now) = opened();
        assert_eq!(panel.hint(), "y/s/a/n/t or 1-5 \u{B7} enter \u{B7} esc no");
        panel.handle_key_at(key(KeyCode::Char('t')), now);
        assert!(panel.hint().contains("esc to go back"));
    }
}
