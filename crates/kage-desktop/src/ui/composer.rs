//! The composer: the input surface, its key table and its token logic.
//!
//! The composer holds one plain-text draft. A leading `/command` and an
//! `@mention` are detected against the session's available commands and
//! the filesystem listing, and the active token draws in the accent
//! color in place, but the stored value stays byte-identical to what
//! was typed: the token is a rendering, never an edit.
//!
//! The key table: Enter sends by default (or adds a newline, with the
//! preference's Ctrl+Enter send), Shift+Enter adds a newline, Ctrl+Enter
//! sends, Up and Down walk the history or the rows as today, and Esc
//! arms an interrupt inside a 1.5 second window.
//!
//! The suggestion menu: a leading `/token` lists the commands, and an
//! `@token` before the caret lists the session workdir's entries. A
//! picked row inserts plain text, exactly as the old `pick_slash` and
//! `pick_mention` did, so the stored value stays byte-identical.

use crate::icons::PaintSnapped as _;
use crate::ui::forms::ShowCrisp as _;
use std::time::Duration;
use web_time::Instant;

use serde_json::Value;

/// The field's height floor, its insets included, as the reference
/// composer's textarea sits.
const FIELD_H: f32 = 60.0;

/// The field's top inset: the reference's padding plus the textarea's
/// own, so the first line sits where the reference draws it.
const FIELD_TOP: f32 = 26.0;
/// How long the first Esc of an interrupt gesture stays armed.
pub const ESC_WINDOW: Duration = Duration::from_millis(1500);

/// What one Esc press means, given the interrupt window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscStep {
    /// Open the window and show the hint.
    Arm,
    /// The second press inside the window: interrupt the run.
    Cancel,
    /// Nothing to interrupt or the window closed: drop the hint.
    Clear,
}

/// Classifies one Esc press. Idle presses only ever clear the hint.
#[must_use]
pub fn esc_step(armed: Option<Instant>, now: Instant, running: bool) -> EscStep {
    if !running {
        return EscStep::Clear;
    }
    match armed {
        Some(at) if now.duration_since(at) < ESC_WINDOW => EscStep::Cancel,
        _ => EscStep::Arm,
    }
}

/// Where the typed text points a suggestion: a slash command or an
/// at-mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suggest {
    /// The text before the caret is a bare `/token`.
    Slash {
        /// The text after the slash.
        query: String,
    },
    /// The text before the caret ends in `@token`.
    Mention {
        /// The text after the at.
        query: String,
        /// Byte offset of the at in the text.
        at: usize,
    },
}

/// The suggestion the value and caret point at, if any.
#[must_use]
pub fn suggest_for(value: &str, cursor: usize) -> Option<Suggest> {
    let before = value.get(..cursor).unwrap_or(value);
    if before.starts_with('/') && !before.contains(char::is_whitespace) {
        return Some(Suggest::Slash {
            query: before[1..].to_owned(),
        });
    }
    let at = before.rfind('@')?;
    if at > 0 && !before[..at].ends_with(char::is_whitespace) {
        return None;
    }
    let token = &before[at + 1..];
    if token.contains(char::is_whitespace) {
        return None;
    }
    Some(Suggest::Mention {
        query: token.to_owned(),
        at,
    })
}

/// The commands the client runs itself, ahead of the agent's: name,
/// argument hint and what it does.
const BUILTINS: [(&str, &str, &str); 4] = [
    (
        "swarm",
        "on | off | <task>",
        "Toggle swarm mode, or run one task as a swarm",
    ),
    ("plan", "on | off", "Toggle plan mode"),
    (
        "goal",
        "<text> | clear",
        "Set a goal the agent keeps pursuing",
    ),
    ("new", "", "Start a new session"),
];

/// A built-in command as typed: its name and the rest of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builtin<'a> {
    /// The command's name.
    pub name: &'a str,
    /// The rest of the line after the name.
    pub args: &'a str,
}

/// What became of a built-in command the composer recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinRun {
    /// It ran.
    Ran,
    /// It needs a session and none is active; the text stays.
    NeedsSession,
    /// The composer does not run it; the agent gets the text.
    NotMine,
}

/// The built-in command `text` runs, if it names one. A command with
/// no arguments is a command with empty arguments, so a bare `/goal`
/// names the goal command rather than falling through to the agent.
#[must_use]
pub fn builtin(text: &str) -> Option<Builtin<'_>> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    BUILTINS
        .iter()
        .find(|(builtin, ..)| *builtin == name)
        .map(|_| Builtin { name, args })
}

/// One slash menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashItem {
    /// The command name, without the leading slash.
    pub name: String,
    /// What the command does, as the agent described it.
    pub description: String,
    /// The input shape the command wants.
    pub hint: Option<String>,
    /// The badge: `agent` for the agent's own commands, none for the
    /// server commands whose name carries the `server:` prefix.
    pub badge: Option<&'static str>,
}

/// The byte length of the leading `/command` token, when the text names
/// a command the client runs or the agent advertised.
#[must_use]
pub fn command_len(text: &str, commands: &[Value]) -> Option<usize> {
    let rest = text.strip_prefix('/')?;
    let (name, _) = rest.split_once(char::is_whitespace)?;
    let known = BUILTINS.iter().any(|(builtin, ..)| *builtin == name)
        || commands
            .iter()
            .any(|command| command.get("name").and_then(Value::as_str) == Some(name));
    (!name.is_empty() && known).then_some(1 + name.len())
}

/// The byte span of the active `/command` or the `@mention` under
/// `caret` in `text`, the span [`Draft::token_range`] reports. The
/// caret clamps to the text, so a caret left stale by a bulk edit
/// never slices past the end.
#[must_use]
fn token_span(text: &str, caret: usize, commands: &[Value]) -> Option<std::ops::Range<usize>> {
    if let Some(len) = command_len(text, commands) {
        return Some(0..len);
    }
    let caret = caret.min(text.len());
    let at = match suggest_for(text, caret)? {
        Suggest::Mention { at, .. } => at,
        Suggest::Slash { .. } => return None,
    };
    let end = text[caret..]
        .find(char::is_whitespace)
        .map_or(text.len(), |off| caret + off);
    Some(at..end)
}

/// The field's layout job: `text` in the body font with `ink`, the
/// token's bytes in `accent`, wrapped at `wrap_width` and keeping
/// trailing whitespace as the field's default layouter does. The line
/// height stays unset; the layouter sets it from the live fonts.
fn token_job(
    text: &str,
    wrap_width: f32,
    font_id: egui::FontId,
    ink: egui::Color32,
    accent: egui::Color32,
    token: Option<std::ops::Range<usize>>,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font_id, ink, wrap_width);
    job.keep_trailing_whitespace = true;
    // The span can outlive the text it came from when the field edits
    // mid-frame, so a span past the end lays out plain.
    let Some(range) = token.filter(|range| range.end <= text.len() && range.start < range.end)
    else {
        return job;
    };
    let base = job.sections[0].format.clone();
    let mut marked = base.clone();
    marked.color = accent;
    let section = |start: usize, end: usize, format: egui::TextFormat| egui::text::LayoutSection {
        leading_space: 0.0,
        byte_range: egui::text::ByteIndex(start)..egui::text::ByteIndex(end),
        format,
    };
    job.sections = if range.start == 0 {
        vec![
            section(0, range.end, marked),
            section(range.end, text.len(), base),
        ]
    } else {
        vec![
            section(0, range.start, base.clone()),
            section(range.start, range.end, marked),
            section(range.end, text.len(), base),
        ]
    };
    job
}

/// The slash menu rows the session's available commands deliver for
/// `query`, in the order the agent listed them.
#[must_use]
pub fn slash_items(commands: &[Value], query: &str) -> Vec<SlashItem> {
    let builtins = BUILTINS
        .iter()
        .filter(|(name, ..)| name.starts_with(query))
        .map(|(name, hint, description)| SlashItem {
            name: (*name).to_owned(),
            description: (*description).to_owned(),
            hint: (!hint.is_empty()).then(|| (*hint).to_owned()),
            badge: None,
        });
    let agent = commands.iter().filter_map(|command| {
        let name = command.get("name")?.as_str()?;
        if !name.starts_with(query) {
            return None;
        }
        Some(SlashItem {
            name: name.to_owned(),
            description: command
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            hint: None,
            badge: Some("agent"),
        })
    });
    builtins.chain(agent).collect()
}

/// One mention menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionItem {
    /// The path relative to the session workdir.
    pub path: String,
    /// Whether the entry is a directory.
    pub directory: bool,
}

/// The mention rows a `_kage/fs` listing delivers for `query`, case
/// insensitively. No listing yields no rows: the menu says so itself.
#[must_use]
pub fn mention_items(
    listing: Option<&kage_client::wire::FsListResult>,
    query: &str,
) -> Vec<MentionItem> {
    let query = query.to_lowercase();
    listing
        .into_iter()
        .flat_map(|listing| listing.entries.iter())
        .filter(|entry| entry.path.to_lowercase().contains(&query))
        .map(|entry| MentionItem {
            path: entry.path.clone(),
            directory: entry.kind == kage_client::wire::FsKind::Directory,
        })
        .collect()
}

/// The composer's draft and the state its input reads.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    /// The typed text, exactly as the keys put it.
    pub text: String,
    /// The byte offset of the caret.
    pub caret: usize,
    /// When the first Esc of an interrupt gesture landed.
    pub esc_armed: Option<Instant>,
    /// Whether code moved the caret since the field last drew, so the
    /// field takes the caret and the keyboard on its next frame.
    placed: bool,
}

impl Draft {
    /// A draft with `text` and the caret at its end.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let caret = text.len();
        Self {
            text,
            caret,
            esc_armed: None,
            placed: false,
        }
    }

    /// Replaces the text with `text`, the caret at its end, and hands
    /// the field the keyboard.
    pub fn set(&mut self, text: String) {
        self.caret = text.len();
        self.text = text;
        self.placed = true;
    }

    /// Inserts `text` at the caret and moves the caret past it.
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.caret, text);
        self.caret += text.len();
    }

    /// Deletes the character before the caret, when there is one.
    pub fn backspace(&mut self) {
        let Some(prev) = self
            .text
            .get(..self.caret)
            .and_then(|head| head.chars().next_back())
        else {
            return;
        };
        self.caret -= prev.len_utf8();
        self.text
            .replace_range(self.caret..self.caret + prev.len_utf8(), "");
    }

    /// Takes the text out, leaving an empty draft with the caret home.
    pub fn take(&mut self) -> String {
        self.caret = 0;
        self.placed = true;
        std::mem::take(&mut self.text)
    }

    /// The suggestion the draft points at.
    #[must_use]
    pub fn suggest(&self) -> Option<Suggest> {
        suggest_for(&self.text, self.caret)
    }

    /// The active token's label: the leading `/command` or `@mention`
    /// under the caret, or `None` when no token is active.
    #[must_use]
    pub fn token(&self, commands: &[Value]) -> Option<String> {
        self.token_range(commands)
            .map(|range| self.text[range].to_owned())
    }

    /// The byte range of the active `/command` or the `@mention` under
    /// the caret, the text `token` names.
    #[must_use]
    pub fn token_range(&self, commands: &[Value]) -> Option<std::ops::Range<usize>> {
        token_span(&self.text, self.caret, commands)
    }

    /// The byte offset the mention token under the caret starts at,
    /// or the caret itself when no token is active.
    #[must_use]
    fn token_at(&self) -> usize {
        match self.suggest() {
            Some(Suggest::Mention { at, .. }) => at,
            _ => self.caret,
        }
    }

    /// Replaces the slash token before the caret with the picked
    /// command, ready for its arguments, as `pick_slash` does.
    pub fn pick_slash(&mut self, name: &str) {
        let after = self.text[self.caret..].to_owned();
        self.text = format!("/{name} {after}");
        self.caret = 1 + name.len() + 1;
        self.placed = true;
    }

    /// Replaces the at-mention before the caret with the picked path,
    /// as `pick_mention` does.
    pub fn pick_mention(&mut self, at: usize, path: &str) {
        let before = self.text[..self.caret].to_owned();
        let after = self.text[self.caret..].to_owned();
        let head = before.get(..at).unwrap_or(&before).to_owned();
        self.text = format!("{head}@{path} {after}");
        self.caret = head.len() + 1 + path.len() + 1;
        self.placed = true;
    }

    /// Inserts an at-mention at the caret: a bare `@` when `path` is
    /// `None`, else `@path` and a space. A space goes in front only
    /// when the caret sits against a word, so the mention never glues
    /// to it.
    pub fn insert_mention(&mut self, path: Option<&str>) {
        let caret = self.caret.min(self.text.len());
        let (before, after) = self.text.split_at(caret);
        let spaced = before.is_empty()
            || before.ends_with(char::is_whitespace)
            || after.starts_with(char::is_whitespace);
        let mention = match path {
            Some(path) => format!("{}@{path} ", if spaced { "" } else { " " }),
            None => format!("{}@", if spaced { "" } else { " " }),
        };
        self.text.insert_str(caret, &mention);
        self.caret = caret + mention.len();
        self.placed = true;
    }

    /// Fills the draft with the bare slash that opens the command menu.
    pub fn seed_slash(&mut self) {
        self.set("/".to_owned());
    }

    /// Classifies one Esc press against the interrupt window.
    pub fn esc(&mut self, now: Instant, running: bool) -> EscStep {
        let step = esc_step(self.esc_armed, now, running);
        match step {
            EscStep::Arm => self.esc_armed = Some(now),
            EscStep::Cancel | EscStep::Clear => self.esc_armed = None,
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_steps_through_the_interrupt_window() {
        let start = Instant::now();
        let mut draft = Draft::default();
        assert_eq!(draft.esc(start, false), EscStep::Clear, "idle clears");
        assert_eq!(draft.esc(start, true), EscStep::Arm, "running arms");
        assert_eq!(
            draft.esc(start + Duration::from_millis(100), true),
            EscStep::Cancel,
            "the second press inside the window cancels"
        );
        assert_eq!(
            draft.esc(start + ESC_WINDOW + Duration::from_millis(1), true),
            EscStep::Arm,
            "an expired window arms again"
        );
    }

    #[test]
    fn the_command_len_names_the_known_commands_only() {
        let commands = vec![serde_json::json!({"name": "review"})];
        assert_eq!(command_len("/plan on", &commands), Some(5));
        assert_eq!(command_len("/review the diff", &commands), Some(7));
        assert_eq!(command_len("/swarm off", &commands), Some(6));
        assert_eq!(command_len("/unknown thing", &commands), None);
        assert_eq!(command_len("no slash", &commands), None);
        assert_eq!(
            command_len("/", &commands),
            None,
            "a bare slash is not a command"
        );
    }

    #[test]
    fn the_token_span_never_slices_past_the_text() {
        let commands = vec![serde_json::json!({"name": "review"})];
        // A caret a bulk edit left past the text's end clamps.
        assert_eq!(token_span("/goal ", 99, &commands), Some(0..5));
        assert_eq!(token_span("", 99, &commands), None);
        // No slash, no span, whatever the caret.
        assert_eq!(token_span("just words", 4, &commands), None);
    }

    #[test]
    fn the_token_job_lays_a_stale_span_out_plain() {
        let font = egui::FontId::monospace(12.0);
        let ink = egui::Color32::WHITE;
        let accent = egui::Color32::RED;
        // The text shrank under the span: plain text, no reversed or
        // overlong section, no panic.
        let job = token_job("", 200.0, font.clone(), ink, accent, Some(0..6));
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, ink);
        // An empty span marks nothing.
        let job = token_job("/plan", 200.0, font, ink, accent, Some(2..2));
        assert_eq!(job.sections.len(), 1);
    }

    #[test]
    fn the_token_job_marks_only_the_tokens_bytes() {
        let font = egui::FontId::monospace(12.0);
        let ink = egui::Color32::WHITE;
        let accent = egui::Color32::RED;
        let job = token_job("/plan on", 200.0, font.clone(), ink, accent, Some(0..5));
        let spans: Vec<(std::ops::Range<usize>, egui::Color32)> = job
            .sections
            .iter()
            .map(|s| (s.byte_range.start.0..s.byte_range.end.0, s.format.color))
            .collect();
        assert_eq!(spans, vec![(0..5, accent), (5..8, ink)]);

        let job = token_job("fix @a/b now", 200.0, font.clone(), ink, accent, Some(4..8));
        let spans: Vec<(std::ops::Range<usize>, egui::Color32)> = job
            .sections
            .iter()
            .map(|s| (s.byte_range.start.0..s.byte_range.end.0, s.format.color))
            .collect();
        assert_eq!(
            spans,
            vec![(0..4, ink), (4..8, accent), (8..12, ink)],
            "a mention mid-line splits three ways"
        );

        let job = token_job("just words", 200.0, font, ink, accent, None);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, ink);
    }

    #[test]
    fn the_slash_menu_lists_builtins_first_then_the_agents() {
        let commands = vec![
            serde_json::json!({"name": "review", "description": "review the diff"}),
            serde_json::json!({"name": "server:deploy"}),
        ];
        let items = slash_items(&commands, "");
        assert_eq!(items[0].name, "swarm", "builtins come first");
        assert_eq!(
            items.len(),
            6,
            "four builtins plus the agent's two, one of them server-prefixed"
        );
        assert_eq!(items[4].badge, Some("agent"));
        let narrowed = slash_items(&commands, "pl");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].name, "plan");
    }

    #[test]
    fn the_suggestion_points_at_the_token_before_the_caret() {
        assert_eq!(
            suggest_for("/pla", 4),
            Some(Suggest::Slash {
                query: "pla".to_owned()
            })
        );
        assert_eq!(
            suggest_for("fix @src/ma", 11),
            Some(Suggest::Mention {
                query: "src/ma".to_owned(),
                at: 4,
            })
        );
        assert_eq!(suggest_for("no token here", 14), None);
        assert_eq!(
            suggest_for("word@src/ma", 12),
            None,
            "an at glued to a word is not a mention"
        );
        assert_eq!(
            suggest_for("word @src/ma", 12),
            Some(Suggest::Mention {
                query: "src/ma".to_owned(),
                at: 5,
            }),
            "an at after a space is a mention"
        );
        assert_eq!(
            suggest_for("fix @src/main.rs now", 19),
            None,
            "a closed mention is not a suggestion"
        );
        assert_eq!(
            suggest_for("fix @src/ma now", 11),
            Some(Suggest::Mention {
                query: "src/ma".to_owned(),
                at: 4,
            }),
            "the caret inside the token keeps the suggestion"
        );
    }

    #[test]
    fn a_builtin_names_itself_and_its_args() {
        let found = builtin("/plan on").expect("plan is built in");
        assert_eq!(found.name, "plan");
        assert_eq!(found.args, "on");
        assert!(
            builtin("/review the diff").is_none(),
            "the agent's is not built in"
        );
        assert!(builtin("plan on").is_none(), "no slash, no command");
    }

    #[test]
    fn the_draft_edits_at_the_caret_and_takes_its_text() {
        let mut draft = Draft::new("hello");
        draft.insert(" world");
        assert_eq!(draft.text, "hello world");
        assert_eq!(draft.caret, 11);
        draft.backspace();
        assert_eq!(draft.text, "hello worl");
        assert_eq!(draft.take(), "hello worl");
        assert_eq!(draft.text, "");
        assert_eq!(draft.caret, 0);
    }

    #[test]
    fn the_active_token_names_the_command_or_mention() {
        let commands = vec![serde_json::json!({"name": "review"})];
        let draft = Draft::new("/plan on");
        assert_eq!(draft.token(&commands).as_deref(), Some("/plan"));
        assert_eq!(draft.token_range(&commands), Some(0..5));
        let mut draft = Draft::new("fix @src/main.rs now");
        draft.caret = 15;
        assert_eq!(draft.token(&commands).as_deref(), Some("@src/main.rs"));
        assert_eq!(draft.token_range(&commands), Some(4..16));
        let draft = Draft::new("just words");
        assert_eq!(draft.token(&commands), None);
        assert_eq!(draft.token_range(&commands), None);
    }

    #[test]
    fn picking_a_command_replaces_its_token_and_keeps_the_rest() {
        // The pick replaces the token before the caret and keeps what
        // follows it, exactly as the old `pick_slash` did: the text
        // from the caret on, so a caret inside the token keeps its
        // tail including the separating space.
        let mut draft = Draft::new("/pl after");
        draft.caret = 3;
        draft.pick_slash("plan");
        assert_eq!(draft.text, "/plan  after");
        assert_eq!(
            draft.caret, 6,
            "the caret sits after the command and its space"
        );
        let mut draft = Draft::new("/pl");
        draft.pick_slash("plan");
        assert_eq!(draft.text, "/plan ");
    }

    #[test]
    fn an_inserted_mention_never_glues_to_a_word() {
        let mut draft = Draft::default();
        draft.insert_mention(None);
        assert_eq!((draft.text.as_str(), draft.caret), ("@", 1));
        let mut draft = Draft::new("fix");
        draft.insert_mention(Some("src/main.rs"));
        assert_eq!(draft.text, "fix @src/main.rs ");
        assert_eq!(draft.caret, draft.text.len());
        let mut draft = Draft::new("fix ");
        draft.insert_mention(None);
        assert_eq!(draft.text, "fix @", "a space already separates it");
    }

    #[test]
    fn picking_a_mention_replaces_it_and_keeps_the_rest() {
        let mut draft = Draft::new("fix @src/ma now");
        draft.caret = 11;
        draft.pick_mention(4, "src/main.rs");
        assert_eq!(draft.text, "fix @src/main.rs  now");
        assert_eq!(
            draft.caret, 17,
            "the caret sits after the path and its space"
        );
    }
}

/// The rows the suggestion menu lists: the slash items, the mention
/// items, or `None` when the draft points at no menu. An empty slash
/// list closes the menu, as the old panel did; a mention with no
/// listing yet keeps a single explanatory row.
fn suggestion_rows(
    suggestion: Option<&Suggest>,
    commands: &[Value],
    listing: Option<&kage_client::wire::FsListResult>,
) -> Option<Vec<MenuRow>> {
    match suggestion? {
        Suggest::Slash { query } => {
            let items = slash_items(commands, query);
            if items.is_empty() {
                return None;
            }
            Some(
                items
                    .into_iter()
                    .map(|item| MenuRow {
                        icon: if item.badge.is_none() {
                            crate::icons::Icon::Server
                        } else {
                            crate::icons::Icon::Slash
                        },
                        title: format!("/{}", item.name),
                        detail: item.hint.unwrap_or_default(),
                        description: item.description,
                        badge: item.badge.map(str::to_owned),
                        pick: MenuPick::Slash(item.name),
                    })
                    .collect(),
            )
        }
        Suggest::Mention { query, at } => {
            if listing.is_none() {
                return Some(vec![MenuRow {
                    icon: crate::icons::Icon::AtSign,
                    title: "no file listing yet; the session workdir was asked".to_owned(),
                    detail: String::new(),
                    description: String::new(),
                    badge: None,
                    pick: MenuPick::None,
                }]);
            }
            let items = mention_items(listing, query);
            if items.is_empty() {
                return None;
            }
            Some(
                items
                    .into_iter()
                    .map(|item| MenuRow {
                        icon: if item.directory {
                            crate::icons::Icon::Folder
                        } else {
                            crate::icons::Icon::File
                        },
                        title: item.path.clone(),
                        detail: String::new(),
                        description: String::new(),
                        badge: Some(if item.directory { "dir" } else { "file" }.to_owned()),
                        pick: MenuPick::Mention {
                            at: *at,
                            path: item.path,
                        },
                    })
                    .collect(),
            )
        }
    }
}

/// One suggestion menu row.
struct MenuRow {
    /// The row's leading icon.
    icon: crate::icons::Icon,
    /// The row's title, in the mono family.
    title: String,
    /// The row's second line, when it carries one.
    detail: String,
    /// The row's description, under the title.
    description: String,
    /// The row's trailing badge.
    badge: Option<String>,
    /// What picking the row does.
    pick: MenuPick,
}

/// What one suggestion row does when it is picked.
enum MenuPick {
    /// Insert the named command.
    Slash(String),
    /// Replace the mention at `at` with `path`.
    Mention { at: usize, path: String },
    /// Nothing: the row only reports a state.
    None,
}

/// Draws the suggestion menu over the composer at `at`. A picked row
/// writes plain text into the draft, as the old `pick_slash` and
/// `pick_mention` did.
fn show_suggestion_menu(
    ctx: &egui::Context,
    palette: &crate::theme::Palette,
    rows: Vec<MenuRow>,
    at: egui::Pos2,
    draft: &mut Draft,
) {
    let width = SUGGEST_W;
    let content: f32 = rows.iter().map(MenuRow::height).sum();
    let height = (content + 2.0 * SUGGEST_PAD + 2.0).min(SUGGEST_MAX_H);
    // The menu opens above the composer, and a composer near the top
    // of a short window would push it off screen, so the position is
    // clamped to the viewport.
    let mut at = at - egui::vec2(0.0, height + 6.0);
    let screen = ctx.content_rect();
    at.x = at.x.clamp(
        screen.left() + 8.0,
        (screen.right() - width - 8.0).max(screen.left() + 8.0),
    );
    at.y = at.y.max(screen.top() + 8.0);
    egui::Area::new(egui::Id::new("composer-suggest"))
        .order(egui::Order::Foreground)
        .fixed_pos(at)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(palette.menu)
                .stroke(egui::Stroke::new(1.0, palette.line))
                .corner_radius(egui::CornerRadius::same(crate::theme::R_LG as u8))
                .inner_margin(egui::Margin::same(SUGGEST_PAD as i8))
                .show_shadowed(ui, &palette.shadow_menu, |ui| {
                    // An area's bounds are last frame's size, so the
                    // height is set outright or a menu that once shrank
                    // to one row never grows back.
                    ui.set_width(width - 2.0 * SUGGEST_PAD - 2.0);
                    ui.set_height(height - 2.0 * SUGGEST_PAD - 2.0);
                    egui::ScrollArea::vertical()
                        .max_height(SUGGEST_MAX_H - 2.0 * SUGGEST_PAD - 2.0)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for row in &rows {
                                if suggest_row(ui, row, palette).clicked() {
                                    match &row.pick {
                                        MenuPick::Slash(name) => draft.pick_slash(name),
                                        MenuPick::Mention { at, path } => {
                                            draft.pick_mention(*at, path);
                                        }
                                        MenuPick::None => {}
                                    }
                                }
                            }
                        });
                });
        });
}

impl MenuRow {
    /// The row's height: the padding around its title line, and the
    /// description line under it when it has one.
    fn height(&self) -> f32 {
        let description = if self.description.is_empty() {
            0.0
        } else {
            crate::theme::FS_XS * SUGGEST_LINE
        };
        2.0 * 7.0 + 12.5 * SUGGEST_LINE + description
    }
}

/// One suggestion row, painted at its fixed height: the icon centered
/// on the text, the mono title with its hint, the description under
/// them, and the badge at the right.
fn suggest_row(
    ui: &mut egui::Ui,
    row: &MenuRow,
    palette: &crate::theme::Palette,
) -> egui::Response {
    use crate::theme::{FAMILY_BODY_REGULAR, FAMILY_MONO_REGULAR, family};
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row.height()),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &row.title));
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(crate::theme::R_MD as u8),
            palette.selected,
        );
    }
    let mid = rect.center().y;
    crate::icons::themed_image(row.icon, 16.0, palette.muted).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 9.0, mid - 8.0),
            egui::vec2(16.0, 16.0),
        ),
    );
    let x = rect.left() + 9.0 + 16.0 + 10.0;
    let mut right = rect.right() - 9.0;
    if let Some(badge) = &row.badge {
        let font = egui::FontId::new(10.5, family(crate::theme::FAMILY_BODY_MEDIUM));
        let galley = ui
            .painter()
            .layout_no_wrap(badge.clone(), font, palette.muted);
        let width = galley.size().x + 16.0;
        let at = egui::pos2(right - width, mid);
        right = crate::ui::settings::paint_badge(ui, at, badge, 10.5, palette.muted, palette)
            .left()
            - 10.0;
    }
    let title_h = 12.5 * SUGGEST_LINE;
    let top = rect.top() + 7.0;
    let mono = |size: f32| egui::FontId::new(size, family(FAMILY_MONO_REGULAR));
    let title = crate::ui::transcript::head::one_line(
        ui,
        row.title.clone(),
        egui::TextFormat::simple(mono(12.5), palette.ink),
        right - x,
    );
    let title_w = title.size().x;
    ui.painter().galley(
        egui::pos2(x, top + (title_h - title.size().y) / 2.0),
        title,
        palette.ink,
    );
    if !row.detail.is_empty() {
        let hint_x = x + title_w + 6.0;
        let hint = crate::ui::transcript::head::one_line(
            ui,
            row.detail.clone(),
            egui::TextFormat::simple(mono(11.0), palette.faint),
            right - hint_x,
        );
        ui.painter().galley(
            egui::pos2(hint_x, top + (title_h - hint.size().y) / 2.0),
            hint,
            palette.faint,
        );
    }
    if !row.description.is_empty() {
        let line_h = crate::theme::FS_XS * SUGGEST_LINE;
        let description = crate::ui::transcript::head::one_line(
            ui,
            row.description.clone(),
            egui::TextFormat::simple(
                egui::FontId::new(crate::theme::FS_XS, family(FAMILY_BODY_REGULAR)),
                palette.muted,
            ),
            right - x,
        );
        ui.painter().galley(
            egui::pos2(x, top + title_h + (line_h - description.size().y) / 2.0),
            description,
            palette.muted,
        );
    }
    response
}

/// The suggestion surface's inner padding.
const SUGGEST_PAD: f32 = 5.0;

/// The design's line height over its font size.
const SUGGEST_LINE: f32 = 1.618;

/// The suggestion surface's width.
const SUGGEST_W: f32 = 460.0;

/// The suggestion surface's max height, before it scrolls.
const SUGGEST_MAX_H: f32 = 300.0;

/// The composer's input surface: the text field, its token color and
/// its send or stop control.
///
/// The field holds one [`Draft`]. Enter sends, or adds a newline when
/// the preference says so; Shift+Enter adds a newline; Ctrl+Enter
/// always sends. The token chip names the leading command or the
/// mention under the caret, and the send button carries the store's
/// running state.
pub struct Composer {
    /// The draft the field edits.
    pub draft: Draft,
    /// The placeholder the field shows, re-set only on a real change.
    placeholder: String,
    /// The session workdir listing the mention menu reads, cloned out
    /// of the store each frame it is asked for.
    listing: Option<kage_client::wire::FsListResult>,
    /// The toolbar under the field: pickers, context ring, send.
    toolbar: crate::ui::toolbar::Toolbar,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    /// A composer over an empty draft.
    #[must_use]
    pub fn new() -> Self {
        Self {
            draft: Draft::default(),
            placeholder: String::new(),
            listing: None,
            toolbar: crate::ui::toolbar::Toolbar::new(),
        }
    }

    /// The prompt the draft holds, if it is not empty.
    #[must_use]
    pub fn prompt(&self) -> Option<&str> {
        let text = self.draft.text.trim();
        (!text.is_empty()).then_some(text)
    }

    /// Takes the draft out for a send, leaving the field empty.
    pub fn take_prompt(&mut self) -> Option<String> {
        let prompt = self.prompt().map(str::to_owned)?;
        self.draft.take();
        Some(prompt)
    }

    /// Draws the composer and reports what the frame's keys asked for.
    /// `commands` are the session's available commands, for the
    /// colored token; `running` selects the stop button; `enter_sends`
    /// is the preference.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &mut crate::store::Store,
        commands: &[serde_json::Value],
        running: bool,
        enter_sends: bool,
    ) -> ComposerEvent {
        let palette = crate::theme::Palette::active_of(ui.ctx());
        let mut event = ComposerEvent::None;
        let field = egui::Id::new("composer-input");
        // The field keeps its own caret, so a caret code placed (a pick,
        // a seed, a send) is written into it, with the keyboard.
        if std::mem::take(&mut self.draft.placed) {
            let at = self
                .draft
                .text
                .get(..self.draft.caret)
                .map_or(0, |head| head.chars().count());
            let mut state = egui::TextEdit::load_state(ui.ctx(), field).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(at),
                )));
            state.store(ui.ctx(), field);
            ui.ctx().memory_mut(|memory| memory.request_focus(field));
        }
        let had_picker = self.toolbar.picker().is_some();
        // What the toolbar asked the shell for, carried out of the
        // frame's borrow of the store.
        let mut goal = false;
        let mut confirm_swarm = false;
        let has_text = !self.draft.text.trim().is_empty();
        self.sync_placeholder(store, running);

        // The suggestion menu opens over the field, so its position is
        // the card's own, taken before the card lays out.
        let menu_top = ui.next_widget_position().y - 6.0;
        let menu_left = ui.next_widget_position().x;

        // The card: the field over the toolbar row, the way the
        // reference composer stacks them. The border names the state:
        // plan, then swarm, then focus, then rest.
        let focused = ui.ctx().memory(|memory| memory.has_focus(field));
        let border = if store.plan_on() {
            palette.accent_bd
        } else if store.swarm_on() {
            palette.done_bd
        } else if focused {
            palette.composer_focus_line
        } else {
            palette.composer_line
        };
        // The field's layout: the body text as the default layouter
        // draws it, with the active `/command` or `@mention` recolored
        // in place.
        // The span reads inside the layouter, from the text it lays
        // out: the field edits mid-frame, so a span taken here would
        // outlive the text and split the job at bytes past its end.
        let caret = self.draft.caret;
        let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
            let body = text.as_str();
            let font_id = egui::TextStyle::Body.resolve(ui.style());
            let ink = ui.visuals().widgets.inactive.text_color();
            let mut job = token_job(
                body,
                wrap_width,
                font_id.clone(),
                ink,
                palette.accent,
                token_span(body, caret, commands),
            );
            let line_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id))
                + ui.spacing().extra_text_line_spacing;
            for section in &mut job.sections {
                section.format.line_height = Some(line_height);
            }
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        };
        let frame = egui::Frame::NONE
            .fill(palette.composer_bg)
            .stroke(egui::Stroke::new(1.0, border))
            .corner_radius(egui::CornerRadius::same(crate::theme::R_COMPOSER as u8))
            .outer_margin(egui::Margin::ZERO);
        frame.show_shadowed(ui, &palette.shadow_input, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 20,
                        right: 20,
                        top: FIELD_TOP as i8,
                        bottom: 4,
                    })
                    .show(ui, |ui| {
                        let width = ui.available_width();
                        // The field adds the newline only on the keys that
                        // ask for one, so Enter never lands in a prompt
                        // it sends; Escape stays with the field, which
                        // reads it as the interrupt gesture.
                        let newline = if enter_sends {
                            egui::Modifiers::SHIFT
                        } else {
                            egui::Modifiers::NONE
                        };
                        let edit = egui::TextEdit::multiline(&mut self.draft.text)
                            .id(field)
                            .hint_text(self.placeholder.as_str())
                            .desired_width(width)
                            .desired_rows(1)
                            .min_size(egui::vec2(width, FIELD_H - FIELD_TOP - 4.0))
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::ZERO)
                            .layouter(&mut layouter)
                            .return_key(egui::KeyboardShortcut::new(newline, egui::Key::Enter))
                            .event_filter(egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            });
                        let output = edit.show(ui);
                        if let Some(range) = output.cursor_range {
                            // The caret is a char index; the draft's caret
                            // is a byte offset, so it converts.
                            self.draft.caret = self
                                .draft
                                .text
                                .char_indices()
                                .nth(range.primary.index.0)
                                .map_or(self.draft.text.len(), |(at, _)| at);
                        }
                        output.response
                    });
                // The toolbar row under the field, the reference's
                // 6/8/8 padding around it.
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 8,
                        right: 8,
                        top: 6,
                        bottom: 8,
                    })
                    .show(ui, |ui| {
                        let toolbar_events = crate::ui::toolbar::show(
                            ui,
                            &mut self.toolbar,
                            store,
                            running,
                            has_text,
                        );
                        for asked in toolbar_events {
                            match asked {
                                crate::ui::toolbar::ToolbarEvent::Submit => {
                                    event = ComposerEvent::Send { steer: false };
                                }
                                crate::ui::toolbar::ToolbarEvent::Cancel => {
                                    event = ComposerEvent::Stop;
                                }
                                crate::ui::toolbar::ToolbarEvent::OpenGoal => {
                                    goal = true;
                                }
                                crate::ui::toolbar::ToolbarEvent::TogglePlan => {
                                    if store.plan_on() {
                                        store.exit_plan();
                                    } else {
                                        store.enter_plan();
                                    }
                                }
                                crate::ui::toolbar::ToolbarEvent::ConfirmSwarm => {
                                    confirm_swarm = true;
                                }
                                crate::ui::toolbar::ToolbarEvent::SwarmOff => {
                                    store.set_option("swarm", "off");
                                }
                                crate::ui::toolbar::ToolbarEvent::SeedMention => {
                                    self.draft.insert_mention(None);
                                }
                                crate::ui::toolbar::ToolbarEvent::SeedSlash => {
                                    self.draft.seed_slash();
                                }
                                crate::ui::toolbar::ToolbarEvent::Compact => {
                                    store.compact();
                                }
                                crate::ui::toolbar::ToolbarEvent::OpenProviders => {
                                    event = ComposerEvent::OpenProviders;
                                }
                            }
                        }
                    });
            })
        });

        // The hint line under the card: the key table on the left, the
        // session's cost on the right, as the reference draws it.
        let esc_armed = self
            .draft
            .esc_armed
            .is_some_and(|at| at.elapsed() < ESC_WINDOW);
        hint_line(ui, store, running, enter_sends, esc_armed);

        // The suggestion menu, over the field: the slash rows or the
        // mention rows the draft points at, read after the field took
        // the frame's keys, so the rows match the text it shows now.
        let suggestion = self.draft.suggest();
        let menu_rows = suggestion_rows(suggestion.as_ref(), commands, self.listing.as_ref());
        if let Some(rows) = menu_rows {
            show_suggestion_menu(
                ui.ctx(),
                palette,
                rows,
                egui::pos2(menu_left, menu_top),
                &mut self.draft,
            );
        }

        // The goal and swarm asks outrank a send, so the frame reports
        // the dialog rather than the prompt.
        if goal {
            event = ComposerEvent::OpenGoal;
        } else if confirm_swarm {
            event = ComposerEvent::ConfirmSwarm;
        }

        // The keys below are the field's own, so they answer only while
        // it holds the keyboard: an Enter in a dialog or a picker is not
        // a send.
        let focused = ui.ctx().memory(|memory| memory.has_focus(field));
        // Enter sends per the preference, and Ctrl+Enter steers while
        // Enter sends; the field itself adds the newlines. The
        // toolbar's own ask wins, so a click and a keypress in one
        // frame do not race. Ctrl+Enter also sends while no other field
        // holds the keys, as the reference's global binding does.
        let elsewhere = !focused && ui.ctx().memory(|memory| memory.focused()).is_some();
        if event == ComposerEvent::None
            && !elsewhere
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
        {
            let modifiers = ui.input(|i| i.modifiers);
            let secondary = modifiers.ctrl || modifiers.command;
            if enter_sends && !modifiers.shift && (focused || secondary) {
                event = ComposerEvent::Send { steer: secondary };
            } else if !enter_sends && secondary {
                event = ComposerEvent::Send { steer: false };
            }
        }
        // In vim mode Escape leaves the composer for normal mode,
        // where a second Escape interrupts a running turn. Otherwise
        // two presses inside the window interrupt it. A suggestion
        // menu or a popover takes the key first.
        if focused
            && event == ComposerEvent::None
            && ui.input(|i| i.key_pressed(egui::Key::Escape))
            && self.draft.suggest().is_none()
            && !self.toolbar.any_open()
        {
            if store.prefs().vim {
                event = ComposerEvent::LeaveInsert;
            } else {
                match self.draft.esc(Instant::now(), running) {
                    EscStep::Arm => ui.ctx().request_repaint_after(ESC_WINDOW),
                    EscStep::Cancel => event = ComposerEvent::Stop,
                    EscStep::Clear => {}
                }
            }
        }
        // A picker closing hands the keys back to the field, after the
        // keys above, so the Enter that chose a row does not also send.
        if had_picker && self.toolbar.picker().is_none() {
            ui.ctx().memory_mut(|memory| memory.request_focus(field));
        }
        event
    }

    /// Keeps the field's placeholder on the per-state phrasing: queueing
    /// while a run is in flight, then swarm, then plan, then an open
    /// ask, then the default.
    fn sync_placeholder(&mut self, store: &crate::store::Store, running: bool) {
        let wanted = if running {
            "Queue a follow-up, or Ctrl+Enter to steer the running turn"
        } else if store.swarm_on() {
            "Describe work to split across parallel agents..."
        } else if store.plan_on() {
            "Describe what to plan..."
        } else if store
            .active_session()
            .is_some_and(|session| !session.permissions.is_empty())
        {
            "Answer the request above first, or type to queue"
        } else {
            "Ask kage anything, @ to mention, / for commands"
        };
        self.set_placeholder(wanted);
    }

    /// Replaces the draft with `text`, the caret at its end. The dock's
    /// edit action uses this, so the caret stays a valid byte offset.
    pub fn set_draft(&mut self, text: String) {
        self.draft.set(text);
    }

    /// Inserts a `@path` mention of `path` at the caret, replacing any
    /// mention token already under it, so a file picked in the
    /// workbench lands as a typed one would.
    pub fn mention(&mut self, path: &str) {
        let at = self.draft.token_at();
        self.draft.pick_mention(at, path);
    }

    /// Sets the placeholder, when it changed.
    pub fn set_placeholder(&mut self, placeholder: &str) {
        if self.placeholder != placeholder {
            self.placeholder = placeholder.to_owned();
        }
    }

    /// The placeholder the field shows.
    #[must_use]
    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    /// Sets the session workdir listing the mention menu reads. The
    /// shell hands in the active session's listing each frame, so the
    /// menu follows the session.
    pub fn set_listing(&mut self, listing: Option<kage_client::wire::FsListResult>) {
        self.listing = listing;
    }
}

/// A key cap as the hint line draws it: 16px tall, a hairline border,
/// mono at 10px, with 2px either side.
fn hint_kbd(ui: &mut egui::Ui, label: &str, palette: &crate::theme::Palette) {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(
            10.0,
            crate::theme::family(crate::theme::FAMILY_MONO_REGULAR),
        ),
        palette.faint,
    );
    // The reference's cap: 2px either side, then the 1px border and
    // 5px around the label, 16px tall on the hint's 18px line.
    let text_w = galley.size().x.ceil();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(text_w + 16.0, crate::theme::line_h(crate::theme::FS_2XS)),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    let cap = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 2.0, crate::theme::snap(rect.center().y - 8.0)),
        egui::vec2(text_w + 12.0, 16.0),
    );
    ui.painter().rect_stroke(
        cap,
        egui::CornerRadius::same(5),
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    let y = crate::theme::line_y(cap.top() + 1.0, 14.0, &galley);
    ui.painter()
        .galley(egui::pos2(cap.left() + 6.0, y), galley, palette.faint);
}

/// One word of the hint line, on its 18px line.
fn hint_word(ui: &mut egui::Ui, text: &str, palette: &crate::theme::Palette) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(
            crate::theme::FS_2XS,
            crate::theme::family(crate::theme::FAMILY_BODY_REGULAR),
        ),
        palette.faint,
    );
    let line = crate::theme::line_h(crate::theme::FS_2XS);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(galley.size().x.ceil(), line),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let y = crate::theme::line_y(rect.top(), line, &galley);
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, palette.faint);
}

/// The hint line under the card: the key table on the left, the
/// session's cost on the right. The phrasing follows the run state and
/// the Enter-sends preference, as the reference's `hint_line` does.
fn hint_line(
    ui: &mut egui::Ui,
    store: &crate::store::Store,
    running: bool,
    enter_sends: bool,
    esc_armed: bool,
) {
    let palette = crate::theme::Palette::active_of(ui.ctx());
    let chip = |ui: &mut egui::Ui, keys: &str| hint_kbd(ui, keys, palette);
    let word = |ui: &mut egui::Ui, text: &str| hint_word(ui, text, palette);
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 6,
            right: 6,
            top: 6,
            bottom: 0,
        })
        .show(ui, |ui| {
            ui.set_min_height(16.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if esc_armed {
                    ui.label(
                        egui::RichText::new("Press Esc again to interrupt")
                            .size(11.0)
                            .color(palette.warn),
                    );
                } else if !enter_sends {
                    chip(ui, "Ctrl Enter");
                    word(ui, if running { "queue" } else { "send" });
                    chip(ui, "Enter");
                    word(ui, "newline");
                } else if running {
                    let steer = store.state().steer_available();
                    chip(ui, "Enter");
                    word(ui, "queue");
                    if steer {
                        chip(ui, "Ctrl Enter");
                        word(ui, "steer");
                    }
                    chip(ui, "Esc Esc");
                    word(ui, "interrupt");
                    if !steer {
                        ui.add_space(6.0);
                        word(ui, "steering not advertised");
                    }
                } else {
                    chip(ui, "Enter");
                    word(ui, "send");
                    chip(ui, "Shift Enter");
                    word(ui, "newline");
                }
                // The cost at the row's right edge, as the reference's
                // justify-between does.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let cost = store
                        .active_id()
                        .and_then(|id| crate::cost::tree_label(store, id, 4))
                        .unwrap_or_default();
                    if !cost.is_empty() {
                        word(ui, &cost);
                    }
                });
            });
        });
}

/// What the composer's frame asked the shell for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerEvent {
    /// Nothing.
    None,
    /// Send the draft, or steer the running turn when `steer` is set.
    Send {
        /// The ask came from Ctrl+Enter, so a running turn is steered
        /// rather than queued.
        steer: bool,
    },
    /// Stop the run in flight.
    Stop,
    /// Open the goal dialog.
    OpenGoal,
    /// Confirm swarm mode before it turns on.
    ConfirmSwarm,
    /// Leave the composer for vim normal mode: vim mode's Escape in
    /// the field.
    LeaveInsert,
    /// Open the providers section of settings, from the model picker.
    OpenProviders,
}

#[cfg(test)]
mod surface_tests {
    use super::{Composer, ComposerEvent, Suggest, mention_items, suggestion_rows};

    #[test]
    fn an_empty_draft_sends_nothing() {
        let mut composer = Composer::new();
        assert_eq!(composer.prompt(), None);
        assert_eq!(composer.take_prompt(), None);
    }

    #[test]
    fn the_mention_rows_come_from_the_listing_only() {
        let listing = kage_client::wire::FsListResult {
            entries: vec![
                kage_client::wire::FsEntry {
                    path: "src/main.rs".to_owned(),
                    kind: kage_client::wire::FsKind::File,
                    size: 12,
                },
                kage_client::wire::FsEntry {
                    path: "src/lib.rs".to_owned(),
                    kind: kage_client::wire::FsKind::File,
                    size: 9,
                },
                kage_client::wire::FsEntry {
                    path: "docs".to_owned(),
                    kind: kage_client::wire::FsKind::Directory,
                    size: 0,
                },
            ],
            truncated: false,
        };
        let items = mention_items(Some(&listing), "main");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "src/main.rs");
        assert!(!items[0].directory);
        let dirs = mention_items(Some(&listing), "docs");
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].directory, "a directory row says so");
        assert!(
            mention_items(Some(&listing), "MAIN").len() == 1,
            "the query is case insensitive"
        );
        assert!(
            mention_items(None, "").is_empty(),
            "no listing yields no rows"
        );
    }

    #[test]
    fn the_suggestion_menu_names_the_slash_and_mention_rows() {
        let commands = vec![serde_json::json!({"name": "review"})];
        let rows = suggestion_rows(
            Some(&Suggest::Slash {
                query: "pl".to_owned(),
            }),
            &commands,
            None,
        )
        .expect("the slash menu shows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "/plan");
        assert!(
            suggestion_rows(
                Some(&Suggest::Slash {
                    query: "zzz".to_owned(),
                }),
                &commands,
                None,
            )
            .is_none(),
            "no matching command closes the menu"
        );
        let listing = kage_client::wire::FsListResult {
            entries: vec![kage_client::wire::FsEntry {
                path: "src/main.rs".to_owned(),
                kind: kage_client::wire::FsKind::File,
                size: 0,
            }],
            truncated: false,
        };
        let rows = suggestion_rows(
            Some(&Suggest::Mention {
                query: "main".to_owned(),
                at: 4,
            }),
            &commands,
            Some(&listing),
        )
        .expect("the mention menu shows");
        assert_eq!(rows[0].title, "src/main.rs");
        assert_eq!(rows[0].badge.as_deref(), Some("file"));
        let rows = suggestion_rows(
            Some(&Suggest::Mention {
                query: "main".to_owned(),
                at: 4,
            }),
            &commands,
            None,
        )
        .expect("the menu says the listing is coming");
        assert!(rows[0].title.starts_with("no file listing yet"));
    }

    #[test]
    fn a_typed_prompt_is_taken_once() {
        let mut composer = Composer::new();
        composer.draft.insert("fix the flake");
        assert_eq!(composer.prompt(), Some("fix the flake"));
        assert_eq!(composer.take_prompt(), Some("fix the flake".to_owned()));
        assert_eq!(composer.prompt(), None, "the field is empty after the send");
    }

    #[test]
    fn the_placeholder_re_sets_only_on_a_real_change() {
        let mut composer = Composer::new();
        composer.set_placeholder("Ask anything");
        composer.set_placeholder("Ask anything");
        assert_eq!(composer.placeholder(), "Ask anything");
        composer.set_placeholder("Ask kage");
        assert_eq!(composer.placeholder(), "Ask kage", "a change re-sets it");
    }

    #[test]
    fn the_send_event_carries_whether_it_steers() {
        assert_eq!(
            ComposerEvent::Send { steer: false },
            ComposerEvent::Send { steer: false }
        );
        assert_ne!(
            ComposerEvent::Send { steer: true },
            ComposerEvent::Send { steer: false }
        );
        assert_ne!(ComposerEvent::Send { steer: false }, ComposerEvent::Stop);
        assert_ne!(ComposerEvent::None, ComposerEvent::Stop);
    }
}
