//! The transcript's row model: the pure logic that turns one
//! session's transcript items into the rows the transcript renders,
//! plus the view state those rows are read with.
//!
//! [`RowModel`] is built once per session by [`row_model`]: it walks
//! `session.items`, folds consecutive same-family tool calls into one
//! [`Row::Group`], drops the turn boundaries tool calls follow, and
//! appends the changes run and the working row where the design puts
//! them. The model carries no measurement, no element ids and no
//! colors; the renderer owns those and reads what it needs from here.
//!
//! [`RowKey`] is the stable identity of a row, so the expansion
//! choices in [`UiState`] and the matches in [`FindMarks`] stay with
//! their row across rebuilds.

use std::collections::{HashMap, HashSet};

use kage_client::wire::{
    ContentBlock, StopReason, ToolCallContent, ToolCallStatus, ToolKind, TurnReason,
};
use kage_client::{Session, ToolCallItem, TranscriptItem};

use crate::store::{EXIT_PLAN_TOOL, PlanChoice};

/// How many characters of the newest thinking the live peek shows.
pub const PEEK_CHARS: usize = 60;

/// The newest words of a thinking text, for the live peek: the tail of
/// the sentence being written, so the line moves as text streams in
/// even while one long sentence is still open.
#[must_use]
pub fn last_sentence(text: &str) -> String {
    let trimmed = text.trim();
    let last = trimmed
        .split(['.', '!', '?', '\u{3002}', '\u{ff01}', '\u{ff1f}'])
        .map(str::trim)
        .rfind(|sentence| !sentence.is_empty())
        .unwrap_or(trimmed);
    let count = last.chars().count();
    if count <= PEEK_CHARS {
        return last.to_owned();
    }
    let tail: String = last.chars().skip(count - PEEK_CHARS).collect();
    format!("\u{2026}{}", tail.trim_start())
}

/// Where one row came from, for element identity, expansion state and
/// the find bar's per-row marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// The transcript item at this index.
    Item(usize),
    /// The collapse group whose first member sits at this index.
    Group(usize),
    /// The files the run ending at this turn-end index changed.
    Changes(usize),
    /// The working row at the end of a run in flight.
    Waiting,
}

/// Why the run stopped, when the session state says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The run was cancelled.
    Interrupted,
    /// The model refused.
    Failed,
}

/// Why the run stopped, when the session state says so.
#[must_use]
fn turn_outcome(stop: Option<StopReason>) -> Option<Outcome> {
    match stop {
        Some(StopReason::Cancelled) => Some(Outcome::Interrupted),
        Some(StopReason::Refusal) => Some(Outcome::Failed),
        _ => None,
    }
}

/// The collapse family of a tool title. Only these titles group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// read, grep, find and ls calls.
    Explore,
    /// shell calls.
    Shell,
}

/// The collapse family of a tool title, when it has one.
fn family_of(title: &str) -> Option<Family> {
    match title {
        "read" | "grep" | "find" | "ls" => Some(Family::Explore),
        "shell" => Some(Family::Shell),
        _ => None,
    }
}

/// The summary label of one collapsed group, over its member titles:
/// the first part capitalized, the rest lower case, as one sentence.
#[must_use]
pub fn group_label(family: Family, titles: &[&str], running: bool) -> String {
    match family {
        Family::Shell => {
            let n = titles.len();
            let verb = if running { "Running" } else { "Ran" };
            format!("{verb} {n} command{}", if n == 1 { "" } else { "s" })
        }
        Family::Explore => {
            let mut reads = 0;
            let mut searches = 0;
            let mut listings = 0;
            for title in titles {
                match *title {
                    "read" => reads += 1,
                    "grep" | "find" => searches += 1,
                    _ => listings += 1,
                }
            }
            let mut parts: Vec<String> = Vec::new();
            let mut part = |verb: &str, n: usize, one: &str, many: &str| {
                if n == 0 {
                    return;
                }
                let verb = if parts.is_empty() {
                    verb.to_owned()
                } else {
                    verb.to_lowercase()
                };
                parts.push(format!("{verb} {n} {}", if n == 1 { one } else { many }));
            };
            part("Read", reads, "file", "files");
            part("Searched", searches, "pattern", "patterns");
            part("Listed", listings, "directory", "directories");
            parts.join(", ")
        }
    }
}

/// One row the transcript renders.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A user message with its copy and edit actions.
    User {
        /// The item index.
        ix: usize,
        /// The message text.
        text: String,
    },
    /// Assistant reply text.
    Assistant {
        /// The item index.
        ix: usize,
        /// Whether chunks still merge into this item.
        live: bool,
    },
    /// Media the assistant produced, such as an image.
    Media {
        /// The item index.
        ix: usize,
    },
    /// Assistant reasoning, collapsed once it ends.
    Thinking {
        /// The item index.
        ix: usize,
        /// Whether chunks still merge into this item.
        live: bool,
        /// Whether the body is expanded.
        expanded: bool,
    },
    /// One tool call.
    Tool {
        /// The item index.
        ix: usize,
        /// Whether the detail is expanded.
        expanded: bool,
    },
    /// A collapsed run of same-family tool calls.
    Group {
        /// The group key.
        key: RowKey,
        /// The summary label.
        label: String,
        /// The collapse family, naming the icon.
        family: Family,
        /// The indexes of the member items.
        members: Vec<usize>,
        /// How many members failed.
        failed: usize,
        /// Whether a member is still pending or running.
        running: bool,
        /// Whether the nested rows are listed.
        expanded: bool,
    },
    /// The files a run changed, just before the run's end.
    Changes {
        /// The turn-end item index the run ends at.
        end: usize,
    },
    /// The end of a run: the turn boundary no tool calls follow.
    TurnEnd {
        /// The item index.
        ix: usize,
        /// Why the run stopped, when the session carries it.
        outcome: Option<Outcome>,
    },
    /// A message for the user outside the conversation.
    Notice {
        /// The item index.
        ix: usize,
    },
    /// Older context turns were summarized.
    Compaction {
        /// The item index.
        ix: usize,
    },
    /// The agent's plan.
    Plan {
        /// The item index.
        ix: usize,
    },
    /// A permission ask was answered: one line with the chosen label.
    Decision {
        /// The item index.
        ix: usize,
    },
    /// A run is in flight and nothing above moves yet: the model has
    /// not answered, or works out its next step after a call.
    Waiting,
}

impl Row {
    /// The row's element identity and expansion key.
    #[must_use]
    pub fn key(&self) -> RowKey {
        match self {
            Row::Group { key, .. } => *key,
            Row::Changes { end } => RowKey::Changes(*end),
            Row::Waiting => RowKey::Waiting,
            Row::User { ix, .. }
            | Row::Assistant { ix, .. }
            | Row::Thinking { ix, .. }
            | Row::Tool { ix, .. }
            | Row::TurnEnd { ix, .. }
            | Row::Media { ix }
            | Row::Notice { ix }
            | Row::Compaction { ix }
            | Row::Plan { ix }
            | Row::Decision { ix } => RowKey::Item(*ix),
        }
    }

    /// The row kind, for sequence assertions.
    #[cfg(test)]
    fn kind(&self) -> &'static str {
        match self {
            Row::User { .. } => "user",
            Row::Assistant { .. } => "assistant",
            Row::Media { .. } => "media",
            Row::Thinking { .. } => "thinking",
            Row::Tool { .. } => "tool",
            Row::Group { .. } => "group",
            Row::Changes { .. } => "changes",
            Row::TurnEnd { .. } => "turn-end",
            Row::Notice { .. } => "notice",
            Row::Compaction { .. } => "compaction",
            Row::Plan { .. } => "plan",
            Row::Decision { .. } => "decision",
            Row::Waiting => "waiting",
        }
    }
}

/// The view-local UI state a row model renders with: the expansion
/// choices of one session.
#[derive(Debug, Default)]
pub struct UiState {
    /// Row keys whose detail, body or nested list is expanded.
    pub expanded: HashSet<RowKey>,
    /// Shell calls whose whole output shows, not only its tail.
    pub full_output: HashSet<usize>,
    /// Swarm cards whose prompt template shows.
    pub templates: HashSet<usize>,
}

/// The find state the transcript tints rows with: the matching row
/// keys and which of them the counter points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindMarks {
    /// The keys of the rows the query matched, in transcript order.
    pub keys: Vec<RowKey>,
    /// The index into `keys` the find bar's counter names.
    pub current: usize,
}

/// The rows a transcript renders, in order.
#[derive(Debug, Default)]
pub struct RowModel {
    /// The rows, oldest first.
    pub rows: Vec<Row>,
    /// How many transcript items the rows were built from.
    pub items: usize,
}

impl RowModel {
    /// The row kinds, for sequence assertions.
    #[cfg(test)]
    fn kinds(&self) -> Vec<&'static str> {
        self.rows.iter().map(Row::kind).collect()
    }
}

/// One rendered line of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// A line only the new text has.
    Add(String),
    /// A line only the old text has.
    Del(String),
    /// A hunk or file header.
    Head(String),
    /// A line both texts share.
    Ctx(String),
}

impl DiffLine {
    /// The text the line renders after its marker.
    #[must_use]
    pub fn body(&self) -> &str {
        match self {
            DiffLine::Add(text) | DiffLine::Del(text) | DiffLine::Ctx(text) => text,
            DiffLine::Head(text) => text,
        }
    }

    /// The marker column the line renders with.
    #[must_use]
    pub fn marker(&self) -> &'static str {
        match self {
            DiffLine::Add(_) => "+",
            DiffLine::Del(_) => "-",
            _ => " ",
        }
    }
}

/// Builds the unified lines of one diff between two texts. The common
/// head and tail are context and the middle is a replacement, so the
/// result always transforms `old` into `new`.
fn diff_of_texts(path: &str, old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let head = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let tail = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(old_lines.len() - head)
        .min(new_lines.len() - head);
    let mut lines = vec![DiffLine::Head(format!("@@ {path}"))];
    for line in &old_lines[..head] {
        lines.push(DiffLine::Ctx((*line).to_owned()));
    }
    for line in &old_lines[head..old_lines.len() - tail] {
        lines.push(DiffLine::Del((*line).to_owned()));
    }
    for line in &new_lines[head..new_lines.len() - tail] {
        lines.push(DiffLine::Add((*line).to_owned()));
    }
    for line in &old_lines[old_lines.len() - tail..] {
        lines.push(DiffLine::Ctx((*line).to_owned()));
    }
    lines
}

/// Reads one text the wire delivered as a unified diff, line by line.
/// Text without any `+` or `-` prefixed line is not a diff.
fn diff_of_text(text: &str) -> Option<Vec<DiffLine>> {
    let marked = text
        .lines()
        .filter(|line| line.starts_with('+') || line.starts_with('-'))
        .count();
    if marked == 0 {
        return None;
    }
    Some(
        text.lines()
            .map(|line| match line.chars().next() {
                Some('+') => DiffLine::Add(line[1..].to_owned()),
                Some('-') => DiffLine::Del(line[1..].to_owned()),
                _ if line.starts_with("@@") => DiffLine::Head(line.to_owned()),
                _ => DiffLine::Ctx(line.strip_prefix(' ').unwrap_or(line).to_owned()),
            })
            .collect(),
    )
}

/// The unified lines of one tool call's diff, when the call carries
/// or implies one: every structured diff it delivered, one hunk each,
/// else its diff-marked text. Shared with the workbench's changes pane.
#[must_use]
pub fn diff_lines(call: &ToolCallItem) -> Option<Vec<DiffLine>> {
    let hunks: Vec<DiffLine> = call
        .content
        .iter()
        .filter_map(|content| match content {
            ToolCallContent::Diff(diff) => Some(diff_of_texts(
                &diff.path,
                diff.old_text.as_deref().unwrap_or(""),
                &diff.new_text,
            )),
            _ => None,
        })
        .flatten()
        .collect();
    if hunks.is_empty() {
        diff_of_text(&call.text())
    } else {
        Some(hunks)
    }
}

/// The added and removed line counts of a unified diff.
#[must_use]
fn diff_stat(lines: &[DiffLine]) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in lines {
        match line {
            DiffLine::Add(_) => added += 1,
            DiffLine::Del(_) => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// The tone a chip renders in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipTone {
    /// Neutral information.
    Neutral,
    /// An addition or a success.
    Good,
    /// A removal or a failure.
    Bad,
}

/// One honest chip: a fact computed from delivered output only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// The chip text, such as `3 lines`.
    pub text: String,
    /// How the chip is colored.
    pub tone: ChipTone,
}

impl Chip {
    fn neutral(text: String) -> Self {
        Self {
            text,
            tone: ChipTone::Neutral,
        }
    }
}

/// One result a `web_search` call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// The results a finished `web_search` call carried as data.
fn search_results(call: &ToolCallItem) -> Option<Vec<SearchResult>> {
    if call.title != "web_search" {
        return None;
    }
    let results = call.raw_output.as_ref()?.get("results")?.as_array()?;
    Some(
        results
            .iter()
            .map(|result| {
                let text = |key: &str| result[key].as_str().unwrap_or_default().to_owned();
                SearchResult {
                    title: text("title"),
                    url: text("url"),
                    snippet: text("snippet"),
                }
            })
            .collect(),
    )
}

/// The chips of one tool call. A call that has not ended shows none,
/// and a fact the output does not show stays hidden.
#[must_use]
pub fn chips_for(call: &ToolCallItem) -> Vec<Chip> {
    if call.status != ToolCallStatus::Completed {
        return Vec::new();
    }
    let output = call.text();
    match call.title.as_str() {
        "read" => {
            if output.is_empty() {
                Vec::new()
            } else {
                vec![Chip::neutral(format!("{} lines", output.lines().count()))]
            }
        }
        "grep" => {
            let lines: Vec<&str> = output.lines().filter(|line| !line.is_empty()).collect();
            if lines.is_empty() || !lines.iter().all(|line| line.contains(':')) {
                return Vec::new();
            }
            let mut files: Vec<&str> = lines
                .iter()
                .filter_map(|line| line.split(':').next())
                .collect();
            files.sort_unstable();
            files.dedup();
            vec![Chip::neutral(format!(
                "{} match{} in {} file{}",
                lines.len(),
                if lines.len() == 1 { "" } else { "es" },
                files.len(),
                if files.len() == 1 { "" } else { "s" },
            ))]
        }
        "web_search" => search_results(call)
            .map(|results| {
                let n = results.len();
                Chip::neutral(format!("{n} result{}", if n == 1 { "" } else { "s" }))
            })
            .into_iter()
            .collect(),
        "find" | "ls" => {
            let entries = output.lines().filter(|line| !line.is_empty()).count();
            if entries == 0 {
                Vec::new()
            } else {
                vec![Chip::neutral(format!("{entries} entries"))]
            }
        }
        "shell" => call
            .raw_output
            .as_ref()
            .and_then(|output| output.get("exit_code"))
            .and_then(serde_json::Value::as_i64)
            .map(|code| Chip {
                text: format!("exit {code}"),
                tone: if code == 0 {
                    ChipTone::Neutral
                } else {
                    ChipTone::Bad
                },
            })
            .into_iter()
            .collect(),
        "edit" | "write" => match diff_lines(call) {
            Some(lines) => {
                let (added, removed) = diff_stat(&lines);
                let mut chips = Vec::new();
                if added > 0 {
                    chips.push(Chip {
                        text: format!("+{added}"),
                        tone: ChipTone::Good,
                    });
                }
                if removed > 0 {
                    chips.push(Chip {
                        text: format!("-{removed}"),
                        tone: ChipTone::Bad,
                    });
                }
                chips
            }
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The file name of a path argument, for row targets.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A string argument of the tool input.
fn input_str<'a>(input: Option<&'a serde_json::Value>, key: &str) -> &'a str {
    input
        .and_then(|input| input.get(key))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// The first non-blank line of `text`, with an ellipsis when more
/// follow, so a multi-line command reads as one row.
#[must_use]
fn one_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} \u{2026}")
    } else {
        first.to_owned()
    }
}

/// The verb and target one tool call renders with: past tense once the
/// call ended, otherwise in progress.
#[must_use]
pub fn tool_verb(call: &ToolCallItem) -> (String, String) {
    let done = call.status == ToolCallStatus::Completed || call.status == ToolCallStatus::Failed;
    let input = call.input.as_ref();
    let (done_word, doing_word, target) = match call.title.as_str() {
        "read" => (
            "Read",
            "Reading",
            basename(input_str(input, "path")).to_owned(),
        ),
        "grep" => (
            "Searched",
            "Searching",
            format!(
                "\"{}\" in {}",
                input_str(input, "pattern"),
                input_str(input, "path")
            ),
        ),
        "find" => (
            "Found",
            "Finding",
            format!(
                "{} in {}",
                input_str(input, "pattern"),
                input_str(input, "path")
            ),
        ),
        "ls" => ("Listed", "Listing", input_str(input, "path").to_owned()),
        "shell" => ("Ran", "Running", one_line(input_str(input, "command"))),
        "edit" => (
            "Edited",
            "Editing",
            basename(input_str(input, "path")).to_owned(),
        ),
        "write" => (
            "Created",
            "Creating",
            basename(input_str(input, "path")).to_owned(),
        ),
        "todo_list" => ("Updated todos", "Updating todos", String::new()),
        "agent" => (
            "Delegated",
            "Delegating",
            input_str(input, "description").to_owned(),
        ),
        "swarm" => {
            let items = input
                .and_then(|input| input["items"].as_array())
                .map_or(0, Vec::len);
            (
                "Swarmed",
                "Swarming",
                format!(
                    "{} ({items} {})",
                    input_str(input, "description"),
                    if items == 1 { "item" } else { "items" }
                ),
            )
        }
        "ask_user_question" => (
            "Asked",
            "Asking",
            input
                .and_then(|input| input["questions"].as_array())
                .map(|questions| {
                    questions
                        .iter()
                        .filter_map(|q| q["header"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
        ),
        "web_search" => (
            "Searched the web",
            "Searching the web",
            input_str(input, "query").to_owned(),
        ),
        "web_fetch" => (
            "Fetched",
            "Fetching",
            input_str(input, "url")
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_owned(),
        ),
        other => (
            other,
            other,
            input
                .map(serde_json::Value::to_string)
                .unwrap_or_default()
                .chars()
                .take(60)
                .collect(),
        ),
    };
    let verb = if done { done_word } else { doing_word };
    (verb.to_owned(), target)
}

/// The text of a prompt: its text blocks in order, a blank line apart.
fn prompt_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(ContentBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The text one tool call contributes to find: the verb, the target,
/// the tool title and the delivered output.
#[must_use]
pub fn tool_search_text(call: &ToolCallItem) -> String {
    let (verb, target) = tool_verb(call);
    format!("{verb} {target} {} {}", call.title, call.text())
}

/// The text one row contributes to find: what it renders, with tool
/// titles, targets and notice text included.
#[must_use]
pub fn row_search_text(session: &Session, row: &Row) -> String {
    let item_text = |ix: usize| match session.items.get(ix) {
        Some(TranscriptItem::Assistant { text } | TranscriptItem::Thinking { text, .. }) => {
            text.clone()
        }
        Some(TranscriptItem::Notice { text, .. }) => text.clone(),
        Some(TranscriptItem::User { content, .. }) => prompt_text(content),
        _ => String::new(),
    };
    match row {
        Row::User { text, .. } => text.clone(),
        Row::Assistant { ix, .. } | Row::Thinking { ix, .. } | Row::Notice { ix } => item_text(*ix),
        Row::Tool { ix, .. } => match session.items.get(*ix) {
            Some(TranscriptItem::ToolCall(call)) => tool_search_text(call),
            _ => String::new(),
        },
        Row::Group { label, members, .. } => {
            let mut text = label.clone();
            for ix in members {
                if let Some(TranscriptItem::ToolCall(call)) = session.items.get(*ix) {
                    text.push(' ');
                    text.push_str(&tool_search_text(call));
                }
            }
            text
        }
        Row::TurnEnd { .. } => "turn ended".to_owned(),
        Row::Changes { end } => run_changes(session, *end)
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        Row::Compaction { .. } => "context compacted".to_owned(),
        Row::Media { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Media { mime, .. }) => mime.clone(),
            _ => String::new(),
        },
        Row::Waiting => String::new(),
        Row::Plan { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Plan { entries }) => entries
                .iter()
                .filter_map(|entry| entry.get("content").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        },
        Row::Decision { ix } => match session.items.get(*ix) {
            Some(TranscriptItem::Decision {
                subject,
                label,
                feedback,
                ..
            }) => format!(
                "{label} {subject} {}",
                feedback.as_deref().unwrap_or_default()
            ),
            _ => String::new(),
        },
    }
}

/// What a file change row reports, collected from one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
    /// The tool call the entry came from, so the pane can select it.
    pub call_id: String,
    /// The file path the change names.
    pub path: String,
    /// Lines only the new text has.
    pub add: usize,
    /// Lines only the old text has.
    pub del: usize,
    /// Whether the first change made the file: its diff named no old
    /// text.
    pub created: bool,
    /// The subagent that made the change, when one did.
    pub by: Option<String>,
}

/// The path one change names: the diff content's path first, then the
/// tool input's `path`, then the call title.
#[must_use]
pub fn change_path(call: &ToolCallItem) -> String {
    for content in &call.content {
        if let ToolCallContent::Diff(diff) = content {
            return diff.path.clone();
        }
    }
    if let Some(path) = call
        .input
        .as_ref()
        .and_then(|input| input.get("path"))
        .and_then(|path| path.as_str())
    {
        return path.to_owned();
    }
    call.title.clone()
}

/// The changed files the transcript holds: the edit, delete and move
/// calls, in transcript order, folded onto their paths.
#[must_use]
pub fn change_entries(items: &[TranscriptItem]) -> Vec<ChangeEntry> {
    let mut out: Vec<ChangeEntry> = Vec::new();
    for item in items {
        let TranscriptItem::ToolCall(call) = item else {
            continue;
        };
        if !matches!(
            call.kind,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move
        ) {
            continue;
        }
        let (add, del) = diff_lines(call)
            .map(|lines| {
                lines.iter().fold((0, 0), |(add, del), line| match line {
                    DiffLine::Add(_) => (add + 1, del),
                    DiffLine::Del(_) => (add, del + 1),
                    _ => (add, del),
                })
            })
            .unwrap_or((0, 0));
        let path = change_path(call);
        if let Some(entry) = out.iter_mut().find(|entry| entry.path == path) {
            entry.add += add;
            entry.del += del;
            continue;
        }
        let created = call.content.iter().any(
            |content| matches!(content, ToolCallContent::Diff(diff) if diff.old_text.is_none()),
        );
        out.push(ChangeEntry {
            call_id: call.tool_call_id.clone(),
            path,
            add,
            del,
            created,
            by: None,
        });
    }
    out
}

/// The files the run ending at the turn end `end` changed: the edits
/// after the run's prompt, folded onto their paths.
#[must_use]
pub fn run_changes(session: &Session, end: usize) -> Vec<ChangeEntry> {
    let start = session.items[..end]
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::User { steered: false, .. }))
        .map_or(0, |ix| ix + 1);
    change_entries(&session.items[start..end])
        .into_iter()
        .filter(|entry| entry.add + entry.del > 0)
        .collect()
}

/// What a rail tick is: the kind of moment a marked row holds. The
/// colors are the palette's, so only the kind travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// A user turn: the model's runs answer these.
    Turn,
    /// A file the agent edited, alone or in a run.
    Edit,
    /// A failed call, refusal or error notice.
    Fail,
    /// A permission ask was approved.
    Approval,
    /// A swarm run started.
    Swarm,
    /// A subagent ran.
    Agent,
    /// A plan the agent proposed.
    Plan,
}

/// One tick of the turn timeline rail for `row`, with the label its
/// hover names, or `None` for a row the rail does not mark. The
/// mapping matches the old shell's rail.
#[must_use]
pub fn rail_tick(session: &Session, row: &Row) -> Option<(Tick, String)> {
    let item = |ix: usize| session.items.get(ix);
    let short = |text: &str| text.chars().take(60).collect::<String>();
    match row {
        Row::User { text, .. } if kage_client::agent_text::compaction_summary(text).is_some() => {
            None
        }
        Row::User { text, .. } => Some((Tick::Turn, format!("You: {}", short(text)))),
        Row::Tool { ix, .. } => {
            let Some(TranscriptItem::ToolCall(call)) = item(*ix) else {
                return None;
            };
            if call.status == ToolCallStatus::Failed {
                return Some((Tick::Fail, format!("Failed: {}", short(&call.title))));
            }
            match call.title.as_str() {
                "swarm" => return Some((Tick::Swarm, "Swarm".to_owned())),
                "agent" => {
                    let task = {
                        let task = input_str(call.input.as_ref(), "description");
                        if task.is_empty() { "subagent" } else { task }
                    };
                    return Some((Tick::Agent, format!("Agent: {}", short(task))));
                }
                _ => {}
            }
            let (add, del) = diff_lines(call)?
                .iter()
                .fold((0, 0), |(a, d), line| match line {
                    DiffLine::Add(_) => (a + 1, d),
                    DiffLine::Del(_) => (a, d + 1),
                    _ => (a, d),
                });
            let path = input_str(call.input.as_ref(), "path");
            Some((
                Tick::Edit,
                format!("Edited {}  +{add} -{del}", basename(path)),
            ))
        }
        Row::Group {
            failed, members, ..
        } if *failed > 0 => Some((
            Tick::Fail,
            format!("{failed} of {} calls failed", members.len()),
        )),
        Row::Changes { end } => {
            let count = run_changes(session, *end).len();
            Some((Tick::Edit, format!("{count} files changed")))
        }
        Row::Decision { ix } => match item(*ix) {
            Some(TranscriptItem::Decision {
                subject, allowed, ..
            }) => Some(if *allowed {
                (Tick::Approval, format!("Approved: {}", short(subject)))
            } else {
                (Tick::Fail, format!("Rejected: {}", short(subject)))
            }),
            _ => None,
        },
        Row::Plan { .. } => Some((Tick::Plan, "Plan".to_owned())),
        Row::Notice { ix } => match item(*ix) {
            Some(TranscriptItem::Notice {
                tone: kage_client::wire::NoticeTone::Error,
                text,
            }) => Some((Tick::Fail, short(text))),
            _ => None,
        },
        _ => None,
    }
}

/// What the reviewer answered to the plan the `exit_plan` call at `ix`
/// presented: the first review decision this client recorded after it.
#[must_use]
pub fn plan_decision(session: &Session, ix: usize) -> Option<PlanChoice> {
    session.items[ix..].iter().find_map(|item| match item {
        TranscriptItem::Decision { subject, label, .. } if subject == EXIT_PLAN_TOOL => {
            Some(match label.to_lowercase().as_str() {
                "approve" => PlanChoice::Approve,
                "revise" => PlanChoice::Revise,
                _ => PlanChoice::Reject,
            })
        }
        _ => None,
    })
}

/// Builds the row model of one session over the view's UI state.
///
/// Consecutive tool calls of one collapse family become one group row,
/// across the turn boundaries between them, which render nothing; only
/// the boundary that ends the run gets a row. A `todo_list` call is the
/// todos row itself, so the plan update it produced is not drawn twice.
#[must_use]
pub fn row_model(session: &Session, ui: &UiState) -> RowModel {
    let items = &session.items;
    let last = items.len().saturating_sub(1);
    let mut rows: Vec<Row> = Vec::with_capacity(items.len());
    let mut index = 0;
    while index < items.len() {
        let family = match &items[index] {
            TranscriptItem::ToolCall(call) => family_of(&call.title),
            _ => None,
        };
        let Some(family) = family else {
            if let Some(row) = plain_row(session, index, ui, last) {
                if matches!(row, Row::TurnEnd { .. }) && !run_changes(session, index).is_empty() {
                    rows.push(Row::Changes { end: index });
                }
                rows.push(row);
            }
            index += 1;
            continue;
        };
        let mut run = vec![index];
        let mut next = index + 1;
        while let Some(item) = items.get(next) {
            match item {
                TranscriptItem::ToolCall(call) if family_of(&call.title) == Some(family) => {
                    run.push(next);
                }
                TranscriptItem::TurnEnd {
                    reason: Some(TurnReason::ToolCalls),
                    ..
                } => {}
                _ => break,
            }
            next += 1;
        }
        // A trailing boundary belongs to the run only when another member
        // followed it.
        let end = run[run.len() - 1] + 1;
        if run.len() > 1 {
            let key = RowKey::Group(index);
            let calls: Vec<&ToolCallItem> = run
                .iter()
                .filter_map(|ix| match &items[*ix] {
                    TranscriptItem::ToolCall(call) => Some(call),
                    _ => None,
                })
                .collect();
            let titles: Vec<&str> = calls.iter().map(|call| call.title.as_str()).collect();
            let failed = calls
                .iter()
                .filter(|call| call.status == ToolCallStatus::Failed)
                .count();
            let running = calls.iter().any(|call| {
                matches!(
                    call.status,
                    ToolCallStatus::Pending | ToolCallStatus::InProgress
                )
            });
            rows.push(Row::Group {
                key,
                label: group_label(family, &titles, running),
                family,
                members: run.clone(),
                failed,
                running,
                expanded: ui.expanded.contains(&key),
            });
        } else {
            rows.push(Row::Tool {
                ix: index,
                expanded: ui.expanded.contains(&RowKey::Item(index)),
            });
        }
        index = end;
    }
    if waiting(session, rows.last()) {
        rows.push(Row::Waiting);
    }
    RowModel {
        rows,
        items: items.len(),
    }
}

/// Whether a run is in flight with nothing on screen moving: no text
/// or thinking streams in, no call runs and no ask waits on the user,
/// so the end of the transcript shows that the run goes on.
fn waiting(session: &Session, last: Option<&Row>) -> bool {
    let moving = last.is_some_and(|row| match row {
        Row::Assistant { live, .. } | Row::Thinking { live, .. } => *live,
        Row::Tool { ix, .. } => matches!(
            session.items.get(*ix),
            Some(TranscriptItem::ToolCall(call))
                if matches!(call.status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
        ),
        Row::Group { running, .. } => *running,
        _ => false,
    });
    (session.running || session.compacting) && session.permissions.is_empty() && !moving
}

/// The row one non-grouped transcript item renders as, if any: a turn
/// boundary tool calls follow renders nothing, and neither does the
/// plan update a `todo_list` call just produced.
fn plain_row(session: &Session, ix: usize, ui: &UiState, last: usize) -> Option<Row> {
    let live = ix == last && session.running;
    Some(match &session.items[ix] {
        TranscriptItem::User { content, .. } => Row::User {
            ix,
            text: prompt_text(content),
        },
        TranscriptItem::Assistant { .. } => Row::Assistant { ix, live },
        TranscriptItem::Media { .. } => Row::Media { ix },
        TranscriptItem::Thinking { .. } => Row::Thinking {
            ix,
            live,
            expanded: ui.expanded.contains(&RowKey::Item(ix)),
        },
        TranscriptItem::ToolCall(_) => Row::Tool {
            ix,
            expanded: ui.expanded.contains(&RowKey::Item(ix)),
        },
        TranscriptItem::TurnEnd {
            reason: Some(TurnReason::ToolCalls),
            ..
        } => return None,
        TranscriptItem::TurnEnd { .. } => Row::TurnEnd {
            ix,
            outcome: if ix == last {
                turn_outcome(session.last_stop)
            } else {
                None
            },
        },
        TranscriptItem::Notice { .. } => Row::Notice { ix },
        TranscriptItem::Compaction { .. } => Row::Compaction { ix },
        TranscriptItem::Plan { .. } => {
            // The update lands after the turn boundary the call's turn
            // closed with, since the tool runs once the model is done.
            let from_todo_tool = session.items[..ix]
                .iter()
                .rev()
                .find(|item| {
                    !matches!(
                        item,
                        TranscriptItem::TurnEnd {
                            reason: Some(TurnReason::ToolCalls),
                            ..
                        }
                    )
                })
                .is_some_and(|item| {
                    matches!(item, TranscriptItem::ToolCall(call) if call.title == "todo_list")
                });
            if from_todo_tool {
                return None;
            }
            Row::Plan { ix }
        }
        TranscriptItem::Decision { subject, .. } if subject == EXIT_PLAN_TOOL => return None,
        TranscriptItem::Decision { .. } => Row::Decision { ix },
    })
}

/// The activity row's vertical margin, collapsed with its neighbors'.
const ROW_MARGIN: f32 = 2.0;
/// The detail box's bottom margin.
const DETAIL_BOTTOM: f32 = 10.0;

/// The design's vertical margins around one row. An expanded row
/// carries the margin of the detail or body that closes it.
pub fn margins(row: &Row) -> (f32, f32) {
    match row {
        Row::User { .. } | Row::Assistant { .. } | Row::Media { .. } => (18.0, 18.0),
        Row::Thinking { expanded, .. } => (
            ROW_MARGIN,
            if *expanded { DETAIL_BOTTOM } else { ROW_MARGIN },
        ),
        Row::Tool { expanded, .. } => (
            ROW_MARGIN,
            if *expanded { DETAIL_BOTTOM } else { ROW_MARGIN },
        ),
        Row::Group { expanded, .. } => (
            ROW_MARGIN,
            if *expanded { DETAIL_BOTTOM } else { ROW_MARGIN },
        ),
        Row::TurnEnd { .. } => (6.0, 22.0),
        Row::Changes { .. } => (10.0, 10.0),
        Row::Notice { .. } | Row::Compaction { .. } => (10.0, 10.0),
        Row::Plan { .. } => (ROW_MARGIN, 12.0),
        Row::Decision { .. } => (4.0, 4.0),
        Row::Waiting => (ROW_MARGIN, ROW_MARGIN),
    }
}

/// The space two adjacent rows render with: the design's collapsed
/// margins. The first row carries only its own top margin.
pub fn gap(prev: Option<&Row>, row: &Row) -> f32 {
    let (top, _) = margins(row);
    match prev {
        Some(prev) => f32::max(margins(prev).1, top),
        None => top,
    }
}

/// One signature per row: what decides its laid-out height. A row whose
/// signature is unchanged keeps its measurement.
#[must_use]
pub fn signatures(
    session: &Session,
    rows: &[Row],
    ui: &UiState,
    revealed: &HashMap<usize, usize>,
) -> Vec<u64> {
    use std::hash::{Hash as _, Hasher as _};

    let last = rows.len().saturating_sub(1);
    rows.iter()
        .enumerate()
        .map(|(ix, row)| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            format!("{row:?}").hash(&mut hasher);
            let prev = ix.checked_sub(1).and_then(|p| rows.get(p));
            gap(prev, row).to_bits().hash(&mut hasher);
            (ix == last).hash(&mut hasher);
            let members: &[usize] = match row {
                Row::Group { members, .. } => members,
                _ => &[],
            };
            match row.key() {
                RowKey::Item(item) => {
                    item_fingerprint(session, item, &mut hasher);
                    ui.full_output.contains(&item).hash(&mut hasher);
                    revealed.get(&item).hash(&mut hasher);
                }
                RowKey::Changes(end) => {
                    format!("{:?}", run_changes(session, end)).hash(&mut hasher);
                }
                RowKey::Waiting => {}
                RowKey::Group(_) => {
                    for item in members {
                        item_fingerprint(session, *item, &mut hasher);
                        ui.expanded.contains(&RowKey::Item(*item)).hash(&mut hasher);
                        ui.full_output.contains(item).hash(&mut hasher);
                    }
                }
            }
            hasher.finish()
        })
        .collect()
}

/// Hashes what of one item can change its row's height.
fn item_fingerprint(session: &Session, ix: usize, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash as _;

    match session.items.get(ix) {
        Some(TranscriptItem::Assistant { text } | TranscriptItem::Thinking { text, .. }) => {
            text.len().hash(hasher);
        }
        Some(TranscriptItem::User { content, .. }) => content.len().hash(hasher),
        Some(TranscriptItem::ToolCall(call)) => {
            if call.title == "agent" || call.title == "swarm" {
                // The card follows the children's records and their runs.
                for (id, agent) in &session.agents {
                    if agent.tool_call_id.as_deref() == Some(call.tool_call_id.as_str()) {
                        id.hash(hasher);
                        format!("{:?}", agent.state).hash(hasher);
                        agent.reason.hash(hasher);
                    }
                }
            }
            if call.title == EXIT_PLAN_TOOL {
                // The card follows the open ask and the decision after it.
                session.permissions.len().hash(hasher);
                session.items.len().hash(hasher);
            }
            format!("{:?}", call.status).hash(hasher);
            call.title.hash(hasher);
            call.content.len().hash(hasher);
            call.text().len().hash(hasher);
            call.raw_output.is_some().hash(hasher);
            call.input.as_ref().map(ToString::to_string).hash(hasher);
        }
        Some(TranscriptItem::Notice { text, .. }) => text.hash(hasher),
        Some(TranscriptItem::Plan { entries }) => format!("{entries:?}").hash(hasher),
        Some(item) => format!("{item:?}").hash(hasher),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        ChipTone, Outcome, PEEK_CHARS, Row, RowKey, UiState, chips_for, diff_lines, diff_stat,
        last_sentence, row_model, tool_verb,
    };
    use crate::store::{Command, Store};
    use crate::transport::State;
    use crate::transport::replay;
    use kage_client::wire::{
        ContentBlock, DiffContent, MessageChunk, TextContent, ToolCallContent, ToolCallStatus,
        ToolKind, TurnReason,
    };
    use kage_client::{Session, ToolCallItem, TranscriptItem};

    /// A session holding exactly `items`.
    fn session_with(items: Vec<TranscriptItem>) -> Session {
        let mut session = Session::new("s1");
        session.items = items;
        session
    }

    /// A completed tool call with the given fields.
    fn call(
        id: &str,
        title: &str,
        kind: ToolKind,
        status: ToolCallStatus,
        content: Vec<ToolCallContent>,
        raw_output: Option<serde_json::Value>,
        input: Option<serde_json::Value>,
    ) -> TranscriptItem {
        TranscriptItem::ToolCall(ToolCallItem {
            tool_call_id: id.into(),
            name: None,
            locations: None,
            title: title.into(),
            kind,
            status,
            input,
            swarm: None,
            content,
            raw_output,
            took_ms: None,
        })
    }

    /// The tool call inside an item.
    fn as_call(item: &TranscriptItem) -> &ToolCallItem {
        match item {
            TranscriptItem::ToolCall(call) => call,
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// Text content holding one chunk.
    fn text_chunk(text: &str) -> ToolCallContent {
        ToolCallContent::Content(MessageChunk {
            content: ContentBlock::text(text),
            meta: None,
        })
    }

    #[test]
    fn read_chips_count_the_delivered_lines() {
        let read = call(
            "r1",
            "read",
            ToolKind::Read,
            ToolCallStatus::Completed,
            vec![text_chunk("a\nb\nc")],
            None,
            None,
        );
        let chips = chips_for(as_call(&read));
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].text, "3 lines");

        let empty = call(
            "r2",
            "read",
            ToolKind::Read,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            None,
        );
        assert!(chips_for(as_call(&empty)).is_empty());
    }

    #[test]
    fn grep_chips_count_matches_and_files_and_refuse_unparseable_output() {
        let parseable = call(
            "g1",
            "grep",
            ToolKind::Search,
            ToolCallStatus::Completed,
            vec![text_chunk("src/a.rs:1:x\nsrc/b.rs:2:y\nsrc/a.rs:3:z")],
            None,
            None,
        );
        let chips = chips_for(as_call(&parseable));
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].text, "3 matches in 2 files");

        let single = call(
            "g2",
            "grep",
            ToolKind::Search,
            ToolCallStatus::Completed,
            vec![text_chunk("src/a.rs:1:x")],
            None,
            None,
        );
        assert_eq!(chips_for(as_call(&single))[0].text, "1 match in 1 file");

        let unparseable = call(
            "g3",
            "grep",
            ToolKind::Search,
            ToolCallStatus::Completed,
            vec![text_chunk("no colons here\nnor there")],
            None,
            None,
        );
        assert!(chips_for(as_call(&unparseable)).is_empty());
    }

    #[test]
    fn shell_chips_come_only_from_the_delivered_exit_code() {
        let exited = call(
            "s1",
            "shell",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![text_chunk("done")],
            Some(serde_json::json!({"exit_code": 3})),
            None,
        );
        let chips = chips_for(as_call(&exited));
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].text, "exit 3");
        assert_eq!(chips[0].tone, ChipTone::Bad);

        let clean = call(
            "s2",
            "shell",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![text_chunk("done")],
            Some(serde_json::json!({"exit_code": 0})),
            None,
        );
        assert_eq!(chips_for(as_call(&clean))[0].tone, ChipTone::Neutral);

        let no_code = call(
            "s3",
            "shell",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![text_chunk("done")],
            Some(serde_json::json!({})),
            None,
        );
        assert!(chips_for(as_call(&no_code)).is_empty());
    }

    #[test]
    fn edit_chips_match_the_diff_stat_of_the_delivered_diff() {
        let old = "fn a() {\n    one\n    two\n}\n";
        let new = "fn a() {\n    one\n    three\n}\n";
        let edit = call(
            "e1",
            "edit",
            ToolKind::Edit,
            ToolCallStatus::Completed,
            vec![ToolCallContent::Diff(DiffContent {
                path: "src/a.rs".into(),
                old_text: Some(old.into()),
                new_text: new.into(),
            })],
            None,
            None,
        );
        let lines = diff_lines(as_call(&edit)).expect("the diff content parses");
        let (added, removed) = diff_stat(&lines);
        assert_eq!(added, 1, "one new line");
        assert_eq!(removed, 1, "one replaced line");
        let chips = chips_for(as_call(&edit));
        assert_eq!(chips.len(), 2);
        assert_eq!(chips[0].text, format!("+{added}"));
        assert_eq!(chips[1].text, format!("-{removed}"));
        assert_eq!(chips[0].tone, ChipTone::Good);
        assert_eq!(chips[1].tone, ChipTone::Bad);
    }

    #[test]
    fn diff_marked_text_parses_line_by_line() {
        let edit = call(
            "e2",
            "edit",
            ToolKind::Edit,
            ToolCallStatus::Completed,
            vec![text_chunk("@@ src/a.rs\n context\n-old\n+new")],
            None,
            None,
        );
        let lines = diff_lines(as_call(&edit)).expect("marked text parses");
        assert_eq!(diff_stat(&lines), (1, 1));
        let plain = call(
            "e3",
            "edit",
            ToolKind::Edit,
            ToolCallStatus::Completed,
            vec![text_chunk("patched src/main.rs")],
            None,
            None,
        );
        assert!(diff_lines(as_call(&plain)).is_none_or(|lines| lines.is_empty()));
    }

    #[test]
    fn pending_running_and_failed_calls_show_no_computed_chip() {
        for status in [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Failed,
        ] {
            let item = call(
                "x",
                "read",
                ToolKind::Read,
                status,
                vec![text_chunk("a\nb")],
                Some(serde_json::json!({"exit_code": 0})),
                None,
            );
            assert!(chips_for(as_call(&item)).is_empty(), "{status:?}");
        }
    }

    #[test]
    fn verbs_flip_to_past_tense_when_the_call_ends() {
        let running = call(
            "r",
            "read",
            ToolKind::Read,
            ToolCallStatus::InProgress,
            Vec::new(),
            None,
            Some(serde_json::json!({"path": "src/main.rs"})),
        );
        assert_eq!(
            tool_verb(as_call(&running)),
            ("Reading".to_owned(), "main.rs".to_owned())
        );
        let done = call(
            "r",
            "read",
            ToolKind::Read,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            Some(serde_json::json!({"path": "src/main.rs"})),
        );
        assert_eq!(
            tool_verb(as_call(&done)),
            ("Read".to_owned(), "main.rs".to_owned())
        );
    }

    #[test]
    fn three_reads_collapse_into_one_summary() {
        let read = |id: &str| {
            call(
                id,
                "read",
                ToolKind::Read,
                ToolCallStatus::Completed,
                vec![text_chunk("a\nb")],
                None,
                Some(serde_json::json!({"path": "src/a.rs"})),
            )
        };
        let session = session_with(vec![read("1"), read("2"), read("3")]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["group"]);
        match &model.rows[0] {
            Row::Group { label, members, .. } => {
                assert_eq!(label, "Read 3 files");
                assert_eq!(members.len(), 3);
            }
            other => panic!("expected a group, got {other:?}"),
        }
    }

    #[test]
    fn two_shells_collapse_into_ran_two_commands() {
        let shell = |id: &str| {
            call(
                id,
                "shell",
                ToolKind::Execute,
                ToolCallStatus::Completed,
                vec![text_chunk("out")],
                Some(serde_json::json!({"exit_code": 0})),
                Some(serde_json::json!({"command": "cargo test"})),
            )
        };
        let session = session_with(vec![shell("1"), shell("2")]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["group"]);
        match &model.rows[0] {
            Row::Group { label, .. } => assert_eq!(label, "Ran 2 commands"),
            other => panic!("expected a group, got {other:?}"),
        }
    }

    #[test]
    fn expanding_a_group_lists_the_nested_rows() {
        let read = |id: &str| {
            call(
                id,
                "read",
                ToolKind::Read,
                ToolCallStatus::Completed,
                vec![text_chunk("a")],
                None,
                Some(serde_json::json!({"path": "src/a.rs"})),
            )
        };
        let session = session_with(vec![read("1"), read("2"), read("3")]);
        let ui = UiState {
            expanded: HashSet::from([RowKey::Group(0)]),
            ..UiState::default()
        };
        let model = row_model(&session, &ui);
        assert_eq!(model.kinds(), vec!["group"], "the members render inside it");
        assert!(matches!(
            &model.rows[0],
            Row::Group { expanded: true, members, .. } if members == &[0, 1, 2]
        ));
    }

    #[test]
    fn a_run_with_a_running_member_groups_under_a_running_label() {
        let read = |status| {
            call(
                "r",
                "read",
                ToolKind::Read,
                status,
                vec![text_chunk("a")],
                None,
                Some(serde_json::json!({"path": "src/a.rs"})),
            )
        };
        let session = session_with(vec![
            read(ToolCallStatus::Completed),
            read(ToolCallStatus::Completed),
            read(ToolCallStatus::InProgress),
        ]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["group"]);
        assert!(matches!(
            &model.rows[0],
            Row::Group { running: true, label, .. } if label == "Read 3 files"
        ));
    }

    #[test]
    fn a_run_groups_across_the_turn_boundaries_between_its_calls() {
        let shell = |id: &str| {
            call(
                id,
                "shell",
                ToolKind::Execute,
                ToolCallStatus::Completed,
                Vec::new(),
                None,
                Some(serde_json::json!({"command": "ls"})),
            )
        };
        let boundary = || TranscriptItem::TurnEnd {
            reason: Some(TurnReason::ToolCalls),
            at: None,
            took_ms: None,
        };
        let session = session_with(vec![shell("a"), boundary(), shell("b"), boundary()]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["group"]);
        assert!(matches!(
            &model.rows[0],
            Row::Group { members, label, .. } if members == &[0, 2] && label == "Ran 2 commands"
        ));
    }

    #[test]
    fn group_labels_capitalize_only_their_first_part() {
        assert_eq!(
            super::group_label(
                super::Family::Explore,
                &["read", "grep", "ls", "read"],
                false
            ),
            "Read 2 files, searched 1 pattern, listed 1 directory"
        );
        assert_eq!(
            super::group_label(super::Family::Shell, &["shell", "shell"], true),
            "Running 2 commands"
        );
    }

    #[test]
    fn other_items_and_other_families_break_runs() {
        let item = |title: &str, kind: ToolKind| {
            call(
                "x",
                title,
                kind,
                ToolCallStatus::Completed,
                vec![text_chunk("a")],
                None,
                None,
            )
        };
        let session = session_with(vec![
            item("read", ToolKind::Read),
            TranscriptItem::Assistant {
                text: "text between".into(),
            },
            item("read", ToolKind::Read),
            item("shell", ToolKind::Execute),
        ]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["tool", "assistant", "tool", "tool"]);
    }

    #[test]
    fn only_the_run_end_gets_a_row_and_it_carries_the_outcome() {
        let mut session = session_with(vec![
            TranscriptItem::TurnEnd {
                reason: Some(TurnReason::ToolCalls),
                at: None,
                took_ms: None,
            },
            TranscriptItem::TurnEnd {
                reason: Some(TurnReason::NoToolCalls),
                at: None,
                took_ms: None,
            },
        ]);
        session.last_stop = Some(kage_client::wire::StopReason::Cancelled);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["turn-end"], "tools followed the first");
        match &model.rows[0] {
            Row::TurnEnd { ix, outcome } => {
                assert_eq!(*ix, 1);
                assert_eq!(*outcome, Some(Outcome::Interrupted));
            }
            other => panic!("expected a turn end, got {other:?}"),
        }
    }

    #[test]
    fn a_run_shows_it_works_until_something_moves() {
        let mut session = session_with(vec![TranscriptItem::User {
            content: vec![ContentBlock::text("go")],
            steered: false,
        }]);
        let kinds = |session: &Session| row_model(session, &UiState::default()).kinds();
        assert_eq!(
            kinds(&session),
            vec!["user"],
            "an idle session waits on no one"
        );
        session.running = true;
        assert_eq!(kinds(&session), vec!["user", "waiting"]);
        session.items.push(TranscriptItem::Assistant {
            text: "on it".into(),
        });
        assert_eq!(kinds(&session), vec!["user", "assistant"], "text streams");
        session.items.push(call(
            "c1",
            "read",
            ToolKind::Read,
            ToolCallStatus::InProgress,
            vec![],
            None,
            None,
        ));
        assert_eq!(kinds(&session), vec!["user", "assistant", "tool"]);
        if let Some(TranscriptItem::ToolCall(call)) = session.items.last_mut() {
            call.status = ToolCallStatus::Completed;
        }
        assert_eq!(
            kinds(&session),
            vec!["user", "assistant", "tool", "waiting"],
            "the model works out its next step"
        );
    }

    #[test]
    fn compaction_rows_keep_the_delivered_token_counts() {
        let session = session_with(vec![TranscriptItem::Compaction {
            kept: 4,
            before: Some(90_000),
            after: Some(12_000),
            summarized: Some(12),
            summary: Some("framed".to_owned()),
        }]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["compaction"]);
        match &session.items[0] {
            TranscriptItem::Compaction {
                kept,
                before,
                after,
                summarized,
                summary,
            } => {
                assert_eq!(*kept, 4);
                assert_eq!((*before, *after), (Some(90_000), Some(12_000)));
                assert_eq!(*summarized, Some(12));
                assert_eq!(summary.as_deref(), Some("framed"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_decision_is_a_one_line_row_carrying_label_subject_and_feedback() {
        let read = call(
            "r",
            "read",
            ToolKind::Read,
            ToolCallStatus::Completed,
            vec![text_chunk("a")],
            None,
            Some(serde_json::json!({"path": "src/a.rs"})),
        );
        let session = session_with(vec![
            read,
            TranscriptItem::Decision {
                subject: "shell".into(),
                label: "Reject shell".into(),
                allowed: false,
                feedback: Some("use rustfmt first".into()),
            },
        ]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["tool", "decision"]);
        match &model.rows[1] {
            Row::Decision { ix } => assert_eq!(*ix, 1),
            other => panic!("expected a decision row, got {other:?}"),
        }
        match &session.items[1] {
            TranscriptItem::Decision {
                subject,
                label,
                allowed,
                feedback,
            } => {
                assert_eq!(
                    (subject.as_str(), label.as_str(), *allowed),
                    ("shell", "Reject shell", false)
                );
                assert_eq!(feedback.as_deref(), Some("use rustfmt first"));
            }
            other => panic!("expected the decision item, got {other:?}"),
        }
        let bare = session_with(vec![TranscriptItem::Decision {
            subject: "ls".into(),
            label: "Allow ls".into(),
            allowed: true,
            feedback: None,
        }]);
        let model = row_model(&bare, &UiState::default());
        assert_eq!(model.kinds(), vec!["decision"]);
    }

    #[test]
    fn a_plan_review_reads_its_answer_from_the_decision_it_hides() {
        let exit_plan = call(
            "call_plan",
            "exit_plan",
            ToolKind::Other,
            ToolCallStatus::Completed,
            Vec::new(),
            None,
            Some(serde_json::json!({"plan": "# Retry budget\n\n1. Do it"})),
        );
        let session = session_with(vec![
            exit_plan,
            TranscriptItem::Decision {
                subject: "exit_plan".into(),
                label: "Revise".into(),
                allowed: false,
                feedback: Some("cover the tests".into()),
            },
        ]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(
            model.kinds(),
            vec!["tool"],
            "the card speaks for the decision"
        );
        assert_eq!(
            super::plan_decision(&session, 0),
            Some(crate::store::PlanChoice::Revise)
        );
    }

    #[test]
    fn a_run_that_changed_files_lists_them_before_its_end() {
        let edit = call(
            "call_edit",
            "edit",
            ToolKind::Edit,
            ToolCallStatus::Completed,
            vec![ToolCallContent::Diff(DiffContent {
                path: "src/retry.rs".into(),
                old_text: Some("a".into()),
                new_text: "b".into(),
            })],
            None,
            Some(serde_json::json!({"path": "src/retry.rs"})),
        );
        let session = session_with(vec![
            TranscriptItem::User {
                content: vec![ContentBlock::text("fix")],
                steered: false,
            },
            edit,
            TranscriptItem::TurnEnd {
                reason: Some(TurnReason::NoToolCalls),
                at: None,
                took_ms: None,
            },
        ]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["user", "tool", "changes", "turn-end"]);
        let entries = super::run_changes(&session, 2);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            (entries[0].add, entries[0].del, entries[0].created),
            (1, 1, false)
        );
    }

    #[test]
    fn a_streaming_delta_moves_only_its_own_row_signature() {
        let mut session = session_with(vec![
            TranscriptItem::User {
                content: vec![ContentBlock::text("go")],
                steered: false,
            },
            TranscriptItem::Assistant { text: "a".into() },
        ]);
        let ui = UiState::default();
        let before = super::signatures(
            &session,
            &row_model(&session, &ui).rows,
            &ui,
            &std::collections::HashMap::new(),
        );
        session.items[1] = TranscriptItem::Assistant {
            text: "a longer reply".into(),
        };
        let after = super::signatures(
            &session,
            &row_model(&session, &ui).rows,
            &ui,
            &std::collections::HashMap::new(),
        );
        assert_eq!(before[0], after[0], "the prompt row keeps its measurement");
        assert_ne!(before[1], after[1], "the growing reply is measured again");
    }

    #[test]
    fn a_multi_line_command_reads_as_its_first_line() {
        assert_eq!(super::one_line("cargo test"), "cargo test");
        assert_eq!(super::one_line("\n  cd /w\n  echo hi\n"), "cd /w \u{2026}");
    }

    #[test]
    fn the_peek_is_the_last_sentence_one_line_long() {
        assert_eq!(
            last_sentence("First idea. Second thought."),
            "Second thought"
        );
        assert_eq!(last_sentence("no terminator"), "no terminator");
        let long = format!("{} newest", "word ".repeat(40));
        let peek = last_sentence(&long);
        assert!(peek.chars().count() <= PEEK_CHARS + 1);
        assert!(peek.starts_with('\u{2026}'));
        assert!(peek.ends_with("newest"), "the peek follows the stream");
        assert_eq!(
            last_sentence("\u{7b2c}\u{4e00}\u{3002}\u{7b2c}\u{4e8c}"),
            "\u{7b2c}\u{4e8c}"
        );
    }

    /// Carries out `commands` the way the shell does.
    fn run_commands(store: &mut Store) {
        for command in store.take_commands() {
            match command {
                Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                Command::NewSession => store.new_session(),
                Command::ReplayPrompt => {
                    let _ = store.prompt("fix the null check");
                }
            }
        }
    }

    /// Drives the recording through a store the way the shell does:
    /// handshake, session, scripted prompt, then every later frame.
    fn replayed_store() -> Store {
        let mut store = Store::new("/w", true);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        let frames = replay::transcript();
        store.absorb(frames[0].clone());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(frames[1].clone());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        for frame in &frames[2..] {
            store.absorb(frame.clone());
        }
        store
    }

    #[test]
    fn the_replay_renders_the_recorded_row_sequence() {
        let store = replayed_store();
        let session = store.active_session().expect("the recording opened one");
        let model = row_model(session, &UiState::default());
        assert_eq!(
            model.kinds(),
            vec![
                "user",
                "tool",
                "thinking",
                "assistant",
                "tool",
                "tool",
                "assistant",
                "turn-end",
            ],
            "boundaries tools follow render nothing, and the todo call is the plan"
        );
        let tools: Vec<(String, Vec<String>)> = model
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Tool { ix, .. } => {
                    let delivered = as_call(&session.items[*ix]);
                    Some((
                        tool_verb(delivered).0,
                        chips_for(delivered)
                            .into_iter()
                            .map(|chip| chip.text)
                            .collect(),
                    ))
                }
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].0, "Read");
        assert_eq!(tools[0].1, vec!["4 lines"]);
        assert_eq!(
            tools[1].0, "Edited",
            "the fixture edit has no diff, so no chip"
        );
        assert!(tools[1].1.is_empty());
        assert_eq!(
            tools[2].0, "Updated todos",
            "the todo call reads as its row"
        );
        let outcomes: Vec<Option<Outcome>> = model
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::TurnEnd { outcome, .. } => Some(*outcome),
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            vec![None],
            "the recording ended cleanly, so no chip"
        );
        assert!(
            model
                .rows
                .iter()
                .any(|row| matches!(row, Row::Thinking { live: false, .. })),
            "the recorded thinking is done"
        );
    }

    #[test]
    fn the_rail_marks_user_turns_and_skips_the_rest_of_the_prose() {
        let session = session_with(vec![
            TranscriptItem::User {
                content: vec![ContentBlock::Text(TextContent {
                    text: "fix the null check in the parser".to_owned(),
                })],
                steered: false,
            },
            TranscriptItem::Assistant {
                text: "Done. The check moved behind the borrow.".to_owned(),
            },
        ]);
        let model = row_model(&session, &UiState::default());
        let ticks: Vec<_> = model
            .rows
            .iter()
            .filter_map(|row| super::rail_tick(&session, row))
            .collect();
        assert_eq!(
            ticks,
            vec![(
                super::Tick::Turn,
                "You: fix the null check in the parser".to_owned()
            )],
            "the user turn marks; the assistant prose does not"
        );
    }

    #[test]
    fn the_rail_names_failed_calls_and_swarm_and_plans() {
        let session = session_with(vec![
            call(
                "t1",
                "edit_file",
                ToolKind::Other,
                ToolCallStatus::Failed,
                vec![],
                None,
                Some(serde_json::json!({"path": "/w/src/main.rs"})),
            ),
            call(
                "t2",
                "swarm",
                ToolKind::Other,
                ToolCallStatus::Completed,
                vec![],
                None,
                None,
            ),
            TranscriptItem::Plan {
                entries: vec![serde_json::json!({"content": "step one"})],
            },
            TranscriptItem::Notice {
                tone: kage_client::wire::NoticeTone::Error,
                text: "the provider timed out".to_owned(),
            },
            TranscriptItem::Notice {
                tone: kage_client::wire::NoticeTone::Info,
                text: "nothing broke".to_owned(),
            },
        ]);
        let model = row_model(&session, &UiState::default());
        let ticks: Vec<_> = model
            .rows
            .iter()
            .filter_map(|row| super::rail_tick(&session, row))
            .collect();
        let kinds: Vec<_> = ticks.iter().map(|(tick, _)| *tick).collect();
        assert_eq!(
            kinds,
            vec![
                super::Tick::Fail,
                super::Tick::Swarm,
                super::Tick::Plan,
                super::Tick::Fail,
            ],
            "the failed call, the swarm, the plan and the error notice mark; the info notice does not"
        );
        assert_eq!(ticks[0].1, "Failed: edit_file");
        assert_eq!(ticks[1].1, "Swarm");
        assert_eq!(ticks[2].1, "Plan");
        assert_eq!(ticks[3].1, "the provider timed out");
    }

    #[test]
    fn an_edit_tick_names_the_file_and_its_diff_weight() {
        let diff = ToolCallContent::Diff(DiffContent {
            path: "/w/src/parse.rs".to_owned(),
            old_text: Some("a\nb\n".to_owned()),
            new_text: "a\nc\nd\n".to_owned(),
        });
        let session = session_with(vec![call(
            "t1",
            "edit_file",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![diff],
            None,
            Some(serde_json::json!({"path": "/w/src/parse.rs"})),
        )]);
        let model = row_model(&session, &UiState::default());
        let tick = model
            .rows
            .iter()
            .find_map(|row| super::rail_tick(&session, row))
            .expect("an edit marks the rail");
        assert_eq!(tick.0, super::Tick::Edit);
        assert_eq!(tick.1, "Edited parse.rs  +2 -1");
    }
}
