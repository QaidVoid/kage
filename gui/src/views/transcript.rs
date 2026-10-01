//! The main panel: the gate banner over the active session's
//! transcript.
//!
//! The transcript renders a row model built from kage-client session
//! items: a measured list with one row per model row. Labels, verbs
//! and chips are computed only from what the wire delivered; a chip
//! whose inputs are missing is not rendered. Each row carries a
//! signature of what decides its height, and only rows whose signature
//! moved are measured again. The list follows the bottom while a turn
//! streams, even as the last row grows, and offers
//! a jump control once the reader scrolls away from it. Every row
//! carries a stable element id, so element state such as the markdown
//! parse cache stays with its item across frames.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::text::TextView;
use gpui_kit::component::theme::{ActiveTheme, ThemeColor};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, Context, Div, ElementId, Entity, FollowMode,
    Hsla, InteractiveElement as _, IntoElement, ListAlignment, ListState, ParentElement as _,
    Render, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window, div, list, px, radians, relative,
};

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FS_2XS, FS_SM, FS_XS, R_FULL, R_LG, R_SM, SP_1, SP_2, SP_3, SP_4, SP_5};
use crate::timing::RunEnd;
use kage_client::wire::{NoticeTone, ToolCallContent, ToolCallStatus, TurnReason};
use kage_client::{Session, ToolCallItem, TranscriptItem};

/// The line height of the mono detail text: 12px at 1.55.
const DETAIL_LINE: f32 = 19.0;
/// The line height of the thinking body: 13px at 1.5.
const THINK_LINE: f32 = 20.0;
/// The activity row's minimum height.
const ROW_H: f32 = 28.0;
/// The turn-end row's height: the 24px action buttons it carries.
const TURN_END_H: f32 = 24.0;
/// The activity row's vertical margin, collapsed with its neighbors'.
const ROW_MARGIN: f32 = 2.0;
/// The design's 12px detail size.
const DETAIL_SIZE: f32 = 12.0;
/// How far past the viewport the list lays rows out, so a scroll never
/// shows a row before it was measured.
const LIST_OVERDRAW: f32 = 800.0;
/// The small icon size of the design (`.ico.sm`).
const ICON_SM: f32 = 14.0;
/// The extra small icon size of the design (`.ico.xs`).
const ICON_XS: f32 = 12.0;
/// The user bubble's corner radius.
const BUBBLE_R: f32 = 18.0;
/// The user bubble's horizontal padding.
const BUBBLE_PX: f32 = 14.0;
/// The user bubble's vertical padding.
const BUBBLE_PY: f32 = 9.0;
/// The bubble's widest share of the column.
const BUBBLE_MAX: f32 = 0.85;
/// The left indent of detail boxes and thinking bodies.
const DETAIL_INDENT: f32 = 22.0;
/// The detail head's vertical padding.
const DETAIL_HEAD_PY: f32 = 7.0;
/// The mono body's vertical padding inside a detail box.
const DETAIL_PRE_PY: f32 = 10.0;
/// The detail box's bottom margin.
const DETAIL_BOTTOM: f32 = 10.0;

/// The design's vertical margins around one row. An expanded row
/// carries the margin of the detail or body that closes it.
fn margins(row: &Row) -> (f32, f32) {
    match row {
        Row::User { .. } | Row::Assistant { .. } => (18.0, 18.0),
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
        Row::Notice { .. } | Row::Compaction { .. } => (10.0, 10.0),
        Row::Plan { .. } => (ROW_MARGIN, 12.0),
        Row::Decision { .. } => (4.0, 4.0),
    }
}

/// The space two adjacent rows render with: the design's collapsed
/// margins. The first row carries only its own top margin.
fn gap(prev: Option<&Row>, row: &Row) -> f32 {
    let (top, _) = margins(row);
    match prev {
        Some(prev) => f32::max(margins(prev).1, top),
        None => top,
    }
}

/// One rendered line of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffLine {
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
    fn body(&self) -> &str {
        match self {
            DiffLine::Add(text) | DiffLine::Del(text) | DiffLine::Ctx(text) => text,
            DiffLine::Head(text) => text,
        }
    }

    /// The marker column the line renders with.
    fn marker(&self) -> &'static str {
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
pub(crate) fn diff_lines(call: &ToolCallItem) -> Option<Vec<DiffLine>> {
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
enum ChipTone {
    /// Neutral information.
    Neutral,
    /// An addition or a success.
    Good,
    /// A removal or a failure.
    Bad,
}

/// One honest chip: a fact computed from delivered output only.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chip {
    /// The chip text, such as `3 lines`.
    text: String,
    /// How the chip is colored.
    tone: ChipTone,
}

impl Chip {
    fn neutral(text: String) -> Self {
        Self {
            text,
            tone: ChipTone::Neutral,
        }
    }
}

/// The chips of one tool call. A call that has not ended shows none,
/// and a fact the output does not show stays hidden.
fn chips_for(call: &ToolCallItem) -> Vec<Chip> {
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

/// The verb and target one tool call renders with: past tense once the
/// call ended, otherwise in progress.
fn tool_verb(call: &ToolCallItem) -> (String, String) {
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
        "shell" => ("Ran", "Running", input_str(input, "command").to_owned()),
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

/// The icon one tool title renders with.
fn tool_icon(title: &str) -> IconName {
    match title {
        "read" => IconName::File,
        "grep" | "find" => IconName::Search,
        "ls" => IconName::ListTree,
        "shell" => IconName::Terminal,
        "edit" => IconName::Pencil,
        "write" => IconName::FilePlus,
        "web_search" | "web_fetch" => IconName::Globe,
        "todo_list" => IconName::ListTodo,
        _ => IconName::Zap,
    }
}

/// A design icon at its small size in one color.
fn icon(name: IconName, color: Hsla) -> Icon {
    Icon::new(name).with_size(px(ICON_SM)).text_color(color)
}

/// The spinner the running states show, turning like the design's.
fn spinner(color: Hsla, key: usize) -> impl IntoElement {
    Icon::new(IconName::LoaderCircle)
        .with_size(px(ICON_SM))
        .text_color(color)
        .with_animation(
            ElementId::named_usize("spin", key),
            Animation::new(Duration::from_millis(900)).repeat(),
            |icon, delta| icon.rotate(radians(delta * std::f32::consts::PI * 2.0)),
        )
}

/// The row's edge chevron: pointing right when closed, down when open.
fn chevron(open: bool, color: Hsla) -> Icon {
    Icon::new(IconName::ChevronRight)
        .with_size(px(ICON_SM))
        .text_color(color)
        .rotate(radians(if open {
            std::f32::consts::FRAC_PI_2
        } else {
            0.0
        }))
}

/// A pill chip in one tone of the design's chip anatomy: 20px tall,
/// mono at the smallest size, on a soft tint.
fn chip(text: String, fg: Hsla, bg: Hsla, mono: SharedString) -> Div {
    div()
        .flex()
        .items_center()
        .flex_none()
        .h(px(20.0))
        .px(px(7.0))
        .rounded(px(R_FULL))
        .font_family(mono)
        .text_size(px(FS_2XS))
        .text_color(fg)
        .bg(bg)
        .whitespace_nowrap()
        .child(SharedString::from(text))
}

/// The collapse family of a tool title. Only these titles group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// read, grep, find and ls calls.
    Explore,
    /// shell calls.
    Shell,
}

fn family_of(title: &str) -> Option<Family> {
    match title {
        "read" | "grep" | "find" | "ls" => Some(Family::Explore),
        "shell" => Some(Family::Shell),
        _ => None,
    }
}

/// The summary label of one collapsed group, over its member titles:
/// the first part capitalized, the rest lower case, as one sentence.
fn group_label(family: Family, titles: &[&str], running: bool) -> String {
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

/// Why the run stopped, when the session state says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The run was cancelled.
    Interrupted,
    /// The model refused.
    Failed,
}

fn turn_outcome(stop: Option<kage_client::wire::StopReason>) -> Option<Outcome> {
    match stop {
        Some(kage_client::wire::StopReason::Cancelled) => Some(Outcome::Interrupted),
        Some(kage_client::wire::StopReason::Refusal) => Some(Outcome::Failed),
        _ => None,
    }
}

/// Where one row came from, for element identity, expansion state and
/// the find bar's per-row marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// The transcript item at this index.
    Item(usize),
    /// The collapse group whose first member sits at this index.
    Group(usize),
}

/// The last sentence of a thinking text, for the live peek.
fn last_sentence(text: &str) -> String {
    let trimmed = text.trim();
    let mut last = trimmed;
    for sentence in trimmed.split(['.', '!', '?']) {
        let sentence = sentence.trim();
        if !sentence.is_empty() {
            last = sentence;
        }
    }
    let mut peek: String = last.chars().take(120).collect();
    if last.chars().count() > 120 {
        peek.push('\u{2026}');
    }
    peek
}

/// One row the transcript renders.
#[derive(Debug, Clone, PartialEq)]
enum Row {
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
        /// Whether the row sits inside an expanded group.
        nested: bool,
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
}

impl Row {
    /// The row's element identity and expansion key.
    fn key(&self) -> RowKey {
        match self {
            Row::Group { key, .. } => *key,
            Row::User { ix, .. }
            | Row::Assistant { ix, .. }
            | Row::Thinking { ix, .. }
            | Row::Tool { ix, .. }
            | Row::TurnEnd { ix, .. }
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
            Row::Thinking { .. } => "thinking",
            Row::Tool { .. } => "tool",
            Row::Group { .. } => "group",
            Row::TurnEnd { .. } => "turn-end",
            Row::Notice { .. } => "notice",
            Row::Compaction { .. } => "compaction",
            Row::Plan { .. } => "plan",
            Row::Decision { .. } => "decision",
        }
    }
}

/// The view-local UI state a row model renders with: the expansion
/// choices of one session.
#[derive(Debug, Default)]
struct UiState {
    /// Row keys whose detail, body or nested list is expanded.
    expanded: HashSet<RowKey>,
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
struct RowModel {
    /// The rows, oldest first.
    rows: Vec<Row>,
}

impl RowModel {
    /// The row kinds, for sequence assertions.
    #[cfg(test)]
    fn kinds(&self) -> Vec<&'static str> {
        self.rows.iter().map(Row::kind).collect()
    }
}

/// Whether a running shell call shows its streamed tail unprompted.
fn live_shell_tail(call: &ToolCallItem) -> bool {
    call.title == "shell" && call.status == ToolCallStatus::InProgress && !call.text().is_empty()
}

/// The text one tool call contributes to find: the verb, the target,
/// the tool title and the delivered output.
fn tool_search_text(call: &ToolCallItem) -> String {
    let (verb, target) = tool_verb(call);
    format!("{verb} {target} {} {}", call.title, call.text())
}

/// The text one row contributes to find: what it renders, with tool
/// titles, targets and notice text included.
fn row_search_text(session: &Session, row: &Row) -> String {
    let item_text = |ix: usize| match session.items.get(ix) {
        Some(TranscriptItem::Assistant { text } | TranscriptItem::Thinking { text }) => {
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
        Row::Compaction { .. } => "context compacted".to_owned(),
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

/// The text of a prompt: its text blocks in order, a blank line apart.
fn prompt_text(content: &[kage_client::wire::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(kage_client::wire::ContentBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Builds the row model of one session over the view's UI state.
///
/// Consecutive tool calls of one collapse family become one group row,
/// across the turn boundaries between them, which render nothing; only
/// the boundary that ends the run gets a row. A `todo_list` call is the
/// todos row itself, so the plan update it produced is not drawn twice.
fn row_model(session: &Session, ui: &UiState) -> RowModel {
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
            if ui.expanded.contains(&key) {
                for ix in &run {
                    rows.push(Row::Tool {
                        ix: *ix,
                        nested: true,
                        expanded: ui.expanded.contains(&RowKey::Item(*ix)),
                    });
                }
            }
        } else {
            rows.push(Row::Tool {
                ix: index,
                nested: false,
                expanded: ui.expanded.contains(&RowKey::Item(index)),
            });
        }
        index = end;
    }
    RowModel { rows }
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
        TranscriptItem::Thinking { .. } => Row::Thinking {
            ix,
            live,
            expanded: ui.expanded.contains(&RowKey::Item(ix)),
        },
        TranscriptItem::ToolCall(_) => Row::Tool {
            ix,
            nested: false,
            expanded: ui.expanded.contains(&RowKey::Item(ix)),
        },
        TranscriptItem::TurnEnd {
            reason: Some(TurnReason::ToolCalls),
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
                            reason: Some(TurnReason::ToolCalls)
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
        TranscriptItem::Decision { .. } => Row::Decision { ix },
    })
}

/// The middle panel.
pub struct TranscriptView {
    store: Entity<Store>,
    composer: Entity<TextareaState>,
    /// The measured list the rows render in.
    list: ListState,
    /// The row model this view renders, rebuilt per frame.
    model: Rc<RowModel>,
    /// What each row of the list looked like when it was last laid out,
    /// so a frame re-measures only the rows that changed.
    signatures: Vec<u64>,
    /// The expansion state of the session on screen.
    ui: UiState,
    /// The expansion state of the other sessions, kept for their return.
    stashed: HashMap<String, UiState>,
    /// The session the UI state belongs to.
    session_key: Option<String>,
    /// Per-row render counts since this session opened.
    render_counts: HashMap<RowKey, u32>,
    /// The find marks the find bar last set, tinting matching rows.
    find: Option<FindMarks>,
}

impl TranscriptView {
    /// A transcript following `store`, filling `composer` on edit.
    #[must_use]
    pub fn new(
        store: Entity<Store>,
        composer: Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            composer,
            list: following_list(),
            model: Rc::default(),
            signatures: Vec::new(),
            ui: UiState::default(),
            stashed: HashMap::new(),
            session_key: None,
            render_counts: HashMap::new(),
            find: None,
        }
    }

    /// Whether the list follows the bottom as rows arrive and grow.
    #[must_use]
    pub fn following(&self) -> bool {
        self.list.is_following_tail()
    }

    /// Re-lays the rows whose signature moved since the last frame:
    /// the span between the unchanged head and the unchanged tail.
    fn sync_list(&mut self, signatures: Vec<u64>) {
        let old = &self.signatures;
        let head = old
            .iter()
            .zip(&signatures)
            .take_while(|(a, b)| a == b)
            .count();
        let tail = old[head..]
            .iter()
            .rev()
            .zip(signatures[head..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let changed = old.len() - head - tail;
        let fresh = signatures.len() - head - tail;
        if changed > 0 || fresh > 0 {
            self.list.splice(head..head + changed, fresh);
        }
        self.signatures = signatures;
    }

    /// Marks one row's detail, body or nested list expanded or collapsed.
    fn toggle(&mut self, key: RowKey) {
        if !self.ui.expanded.remove(&key) {
            self.ui.expanded.insert(key);
        }
    }

    /// The searchable text of every model row, in row order: what the
    /// rows render, with tool titles, targets and notice text
    /// included.
    #[must_use]
    pub fn searchable_rows(&self, cx: &App) -> Vec<(RowKey, String)> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        row_model(session, &self.ui)
            .rows
            .iter()
            .map(|row| (row.key(), row_search_text(session, row)))
            .collect()
    }

    /// Tints the rows the find bar matched; `None` clears the tint.
    pub fn set_find(&mut self, find: Option<FindMarks>, cx: &mut Context<Self>) {
        self.find = find;
        cx.notify();
    }

    /// The tint a row renders with under the find bar: the current
    /// match strongest, the other matches softer.
    fn row_tint(&self, key: RowKey, colors: ThemeColor) -> Option<Hsla> {
        let find = self.find.as_ref()?;
        if find.keys.get(find.current) == Some(&key) {
            return Some(colors.list_active);
        }
        find.keys.contains(&key).then_some(colors.list_hover)
    }

    /// Brings the row `key` into view and stops following the bottom.
    pub fn scroll_to_row(&mut self, key: RowKey, cx: &mut Context<Self>) {
        let Some(ix) = self.model.rows.iter().position(|row| row.key() == key) else {
            return;
        };
        self.list.scroll_to_reveal_item(ix);
        self.list.pause_following_tail();
        cx.notify();
    }

    /// Brings the plan card into view: the dock's review pill asks for
    /// this through the shell.
    pub fn scroll_to_plan(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.store.read(cx).active_session() else {
            return;
        };
        let Some(ix) = session
            .items
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::Plan { .. }))
        else {
            return;
        };
        self.scroll_to_row(RowKey::Item(ix), cx);
    }

    /// The dismissible banner the gate report raises.
    fn banner(&self, cx: &Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        if store.gate().is_clean() || store.gate_dismissed() {
            return None;
        }
        let theme = cx.theme().colors;
        let lines = store.gate().lines().join("\n");
        let store_handle = self.store.clone();
        Some(
            h_flex()
                .w_full()
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .justify_between()
                .bg(theme.warning)
                .border_b_1()
                .border_color(theme.warning)
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.warning_foreground)
                        .child(SharedString::from(format!(
                            "this agent is too old or missing capabilities: {lines}"
                        ))),
                )
                .child(
                    Button::new("dismiss-gate")
                        .label("Dismiss")
                        .on_click(move |_, _, cx| {
                            store_handle.update(cx, |store, cx| {
                                store.dismiss_gate();
                                cx.notify();
                            });
                        }),
                ),
        )
    }

    /// One transcript row, from the model and the session it names,
    /// seated on the design's collapsed margins.
    fn render_row(
        &self,
        cell_ix: usize,
        row: &Row,
        prev: Option<&Row>,
        last: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let session = store.active_session();
        let times = session.and_then(|session| store.timings(&session.id));
        let element = match row {
            Row::User { ix, text } => self.render_user(*ix, text, cx).into_any_element(),
            Row::Assistant { ix, live } => match session.and_then(|s| s.items.get(*ix)) {
                Some(TranscriptItem::Assistant { text }) => self
                    .render_assistant(*ix, text, *live, cx)
                    .into_any_element(),
                _ => blank_row(*ix).into_any_element(),
            },
            Row::Thinking { ix, live, expanded } => match session.and_then(|s| s.items.get(*ix)) {
                Some(TranscriptItem::Thinking { text }) => self
                    .render_thinking(
                        *ix,
                        text,
                        *live,
                        *expanded,
                        times.and_then(|t| t.thinking(*ix)),
                        cx,
                    )
                    .into_any_element(),
                _ => blank_row(*ix).into_any_element(),
            },
            Row::Tool { ix, nested, .. } => {
                let first_nested = matches!(prev, Some(Row::Group { .. }));
                match session.and_then(|s| s.items.get(*ix)) {
                    Some(TranscriptItem::ToolCall(call))
                        if call.status == ToolCallStatus::Completed
                            && let Some(todos) = todos_of_call(call) =>
                    {
                        render_todos(
                            *ix,
                            &todos,
                            self.ui.expanded.contains(&RowKey::Item(*ix)),
                            cx,
                        )
                        .into_any_element()
                    }
                    Some(TranscriptItem::ToolCall(call)) => self
                        .render_tool(
                            *ix,
                            call,
                            *nested,
                            first_nested,
                            times.and_then(|t| t.tool(&call.tool_call_id)),
                            cx,
                        )
                        .into_any_element(),
                    _ => blank_row(*ix).into_any_element(),
                }
            }
            Row::Group { .. } => self.render_group(row, cx).into_any_element(),
            Row::TurnEnd { ix, outcome } => match session {
                Some(session) => self
                    .render_turn_end(
                        session,
                        *ix,
                        *outcome,
                        times.and_then(|t| t.run_end(*ix)),
                        cx,
                    )
                    .into_any_element(),
                None => blank_row(*ix).into_any_element(),
            },
            Row::Notice { ix } => match session.and_then(|s| s.items.get(*ix)) {
                Some(TranscriptItem::Notice { tone, text }) => {
                    render_notice(*ix, *tone, text, cx).into_any_element()
                }
                _ => blank_row(*ix).into_any_element(),
            },
            Row::Compaction { ix } => match session.and_then(|s| s.items.get(*ix)) {
                Some(TranscriptItem::Compaction {
                    kept,
                    before,
                    after,
                }) => render_compaction(*ix, *kept, *before, *after, cx).into_any_element(),
                _ => blank_row(*ix).into_any_element(),
            },
            Row::Plan { ix } => match session.and_then(|s| s.items.get(*ix)) {
                Some(TranscriptItem::Plan { entries }) => render_todos(
                    *ix,
                    &todos_of_plan(entries),
                    self.ui.expanded.contains(&RowKey::Item(*ix)),
                    cx,
                )
                .into_any_element(),
                _ => blank_row(*ix).into_any_element(),
            },
            Row::Decision { ix } => match session.and_then(|s| s.items.get(*ix)) {
                Some(TranscriptItem::Decision {
                    subject,
                    label,
                    allowed,
                    feedback,
                }) => render_decision(*ix, subject, label, *allowed, feedback.as_deref(), cx),
                _ => blank_row(*ix).into_any_element(),
            },
        };
        let mut cell = div()
            .id(ElementId::named_usize("row-cell", cell_ix))
            .test_support()
            .w_full()
            .pt(px(gap(prev, row)));
        if last {
            cell = cell.pb(px(margins(row).1));
        }
        cell.child(element).into_any_element()
    }

    /// A user message: a right-aligned bubble over hover actions.
    fn render_user(&self, ix: usize, text: &str, cx: &Context<Self>) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let composer = self.composer.clone();
        let edit_text = text.to_owned();
        let copy_text = edit_text.clone();
        div()
            .id(ElementId::named_usize("row-user", ix))
            .w_full()
            .flex()
            .flex_col()
            .items_end()
            .group("user-msg")
            .child(
                div()
                    .max_w(relative(BUBBLE_MAX))
                    .rounded(px(BUBBLE_R))
                    .px(px(BUBBLE_PX))
                    .py(px(BUBBLE_PY))
                    .bg(theme.secondary)
                    .text_color(theme.secondary_foreground)
                    .child(SharedString::from(text.to_owned())),
            )
            .child(
                h_flex()
                    .mt(px(SP_2))
                    .gap(px(SP_1))
                    .opacity(0.)
                    .group_hover("user-msg", |style| style.opacity(1.))
                    .child(Clipboard::new("copy").value(copy_text).tooltip("Copy"))
                    .child(
                        Button::new("edit")
                            .icon(IconName::Pencil)
                            .xsmall()
                            .ghost()
                            .tooltip("Edit and resend")
                            .on_click(move |_, window, cx| {
                                let text = edit_text.clone();
                                composer.update(cx, |state, cx| {
                                    state.set_value(&text, window, cx);
                                    state.focus(window, cx);
                                });
                            }),
                    ),
            )
    }

    /// Assistant reply text as markdown, with a caret while it streams.
    fn render_assistant(
        &self,
        ix: usize,
        text: &str,
        live: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let mut row = div()
            .id(ElementId::named_usize("row-assistant", ix))
            .w_full()
            .text_color(theme.foreground)
            .child(
                TextView::markdown(ElementId::named_usize("md", ix), text.to_owned())
                    .text_color(theme.foreground),
            );
        if live {
            row = row.child(
                div()
                    .w(px(7.))
                    .h(px(14.))
                    .mt(px(SP_1))
                    .ml(px(2.))
                    .bg(theme.foreground)
                    .with_animation(
                        ElementId::named_usize("caret", ix),
                        Animation::new(Duration::from_millis(1000)).repeat(),
                        |caret, delta| caret.opacity(if delta < 0.5 { 1.0 } else { 0.0 }),
                    ),
            );
        }
        row
    }

    /// Assistant reasoning: a live peek, then a collapsible body.
    fn render_thinking(
        &self,
        ix: usize,
        text: &str,
        live: bool,
        expanded: bool,
        duration: Option<Duration>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let ink = crate::theme::Palette::active(cx);
        let view = cx.entity();
        let label = match (live, duration) {
            (true, _) => "Thinking".to_owned(),
            (false, Some(span)) => format!("Thought for {}s", span.as_secs().max(1)),
            (false, None) => "Thought".to_owned(),
        };
        let verb_color = if live {
            theme.secondary_foreground
        } else {
            theme.foreground
        };
        let mut head = h_flex()
            .id("head")
            .min_h(px(ROW_H))
            .ml(px(-SP_3))
            .px(px(SP_3))
            .py(px(SP_1))
            .gap(px(SP_4))
            .items_center()
            .rounded(px(R_SM))
            .cursor_pointer()
            .text_size(px(FS_SM))
            .hover(move |style| style.bg(theme.list_hover))
            .on_click(move |_, _, cx| {
                view.update(cx, |this, cx| {
                    this.toggle(RowKey::Item(ix));
                    cx.notify();
                });
            })
            .child(if live {
                spinner(theme.primary, ix).into_any_element()
            } else {
                icon(IconName::Lightbulb, theme.muted_foreground).into_any_element()
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(verb_color)
                    .child(label),
            );
        if live && !expanded {
            head = head.child(
                div()
                    .min_w_0()
                    .truncate()
                    .italic()
                    .text_size(px(FS_SM))
                    .text_color(ink.faint)
                    .child(SharedString::from(last_sentence(text))),
            );
        }
        head = head.child(chevron(expanded, ink.faint));
        let mut row = div()
            .id(ElementId::named_usize("row-thinking", ix))
            .w_full()
            .flex()
            .flex_col()
            .child(head);
        if expanded {
            row = row.child(
                div()
                    .mt(px(SP_2))
                    .ml(px(DETAIL_INDENT))
                    .pl(px(SP_5))
                    .border_l_2()
                    .border_color(theme.border)
                    .text_size(px(FS_SM))
                    .line_height(px(THINK_LINE))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(text.to_owned())),
            );
        }
        row
    }

    /// One tool call: state icon, verb, target, honest chips, and the
    /// detail box when expanded.
    fn render_tool(
        &self,
        ix: usize,
        call: &ToolCallItem,
        nested: bool,
        first_nested: bool,
        took: Option<Duration>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let mono = cx.theme().mono_font_family.clone();
        let ink = crate::theme::Palette::active(cx);
        let view = cx.entity();
        let (verb, target) = tool_verb(call);
        let expanded = self.ui.expanded.contains(&RowKey::Item(ix)) || live_shell_tail(call);
        let running =
            call.status == ToolCallStatus::InProgress || call.status == ToolCallStatus::Pending;
        let failed = call.status == ToolCallStatus::Failed;
        let output = call.text();
        let diff = if call.title == "edit" || call.title == "write" {
            diff_lines(call)
        } else {
            None
        };
        let verb_color = if failed {
            theme.danger
        } else if running {
            theme.secondary_foreground
        } else {
            theme.foreground
        };
        let lead = if failed {
            icon(IconName::CircleX, theme.danger).into_any_element()
        } else if running {
            spinner(theme.primary, ix).into_any_element()
        } else {
            icon(tool_icon(&call.title), theme.muted_foreground).into_any_element()
        };
        let mut head = h_flex()
            .id("head")
            .when(nested, |head| head.w_full())
            .min_h(px(ROW_H))
            .when(!nested, |head| head.ml(px(-SP_3)))
            .pl(px(if nested { SP_5 } else { SP_3 }))
            .pr(px(SP_3))
            .py(px(SP_1))
            .gap(px(SP_4))
            .items_center()
            .text_size(px(FS_SM))
            .hover(move |style| style.bg(theme.list_hover))
            .child(lead)
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(verb_color)
                    .child(SharedString::from(verb)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(mono.clone())
                    .text_size(px(DETAIL_SIZE))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(target)),
            );
        let mut meta = h_flex().gap(px(SP_4)).items_center().flex_none().ml_auto();
        if call.status == ToolCallStatus::Pending {
            meta = meta.child(chip(
                "waiting".to_owned(),
                theme.muted_foreground,
                theme.list_hover,
                mono.clone(),
            ));
        }
        for mark in chips_for(call) {
            let (fg, bg) = match mark.tone {
                ChipTone::Neutral => (theme.muted_foreground, theme.list_hover),
                ChipTone::Good => (ink.diff_add, ink.diff_add_bg),
                ChipTone::Bad => (ink.diff_del, ink.diff_del_bg),
            };
            meta = meta.child(chip(mark.text, fg, bg, mono.clone()));
        }
        if failed {
            meta = meta.child(chip(
                "failed".to_owned(),
                theme.danger,
                ink.danger_soft,
                mono.clone(),
            ));
        }
        if let Some(took) = took {
            meta = meta.child(
                div()
                    .font_family(mono.clone())
                    .text_size(px(FS_2XS))
                    .text_color(ink.faint)
                    .child(crate::clock::span(took)),
            );
        }
        head = head.child(meta);
        if diff.is_some() || !output.is_empty() {
            let view_head = view.clone();
            head = head
                .child(chevron(expanded, ink.faint))
                .on_click(move |_, _, cx| {
                    view_head.update(cx, |this, cx| {
                        this.toggle(RowKey::Item(ix));
                        cx.notify();
                    });
                });
        }
        let head = if !nested {
            head.rounded(px(R_SM))
        } else {
            head.when(!first_nested, |row| {
                row.border_t_1().border_color(ink.subtle)
            })
        };
        let mut unit = div()
            .id(ElementId::named_usize("row-tool", ix))
            .w_full()
            .flex()
            .flex_col()
            .child(head);
        if expanded {
            if let Some(lines) = diff {
                unit = unit.child(render_detail(None, render_diff(&lines, cx), ix, cx));
            } else if !output.is_empty() {
                let detail_head = if call.title == "read" {
                    Some((
                        IconName::File,
                        input_str(call.input.as_ref(), "path").to_owned(),
                    ))
                } else if call.title == "shell" {
                    Some((
                        IconName::Terminal,
                        format!("$ {}", input_str(call.input.as_ref(), "command")),
                    ))
                } else if call.title == "edit" || call.title == "write" {
                    Some((
                        IconName::FileDiff,
                        input_str(call.input.as_ref(), "path").to_owned(),
                    ))
                } else {
                    None
                };
                unit = unit.child(render_detail(detail_head, render_pre(&output, cx), ix, cx));
            }
        }
        unit
    }

    /// A collapsed run of same-family tool calls.
    fn render_group(&self, row: &Row, cx: &Context<Self>) -> Stateful<Div> {
        let Row::Group {
            key,
            label,
            family,
            failed,
            running,
            expanded,
            ..
        } = row
        else {
            return blank_row(0);
        };
        let (key, family, failed, running, expanded) =
            (*key, *family, *failed, *running, *expanded);
        let theme = cx.theme().colors;
        let mono = cx.theme().mono_font_family.clone();
        let ink = crate::theme::Palette::active(cx);
        let view = cx.entity();
        let RowKey::Group(group_ix) = key else {
            return blank_row(0);
        };
        let group_icon = match family {
            Family::Shell => IconName::Terminal,
            Family::Explore => IconName::Search,
        };
        let mut head = h_flex()
            .id("head")
            .min_h(px(ROW_H))
            .ml(px(-SP_3))
            .px(px(SP_3))
            .py(px(SP_1))
            .gap(px(SP_4))
            .items_center()
            .rounded(px(R_SM))
            .cursor_pointer()
            .text_size(px(FS_SM))
            .hover(move |style| style.bg(theme.list_hover))
            .child(if running {
                spinner(theme.primary, group_ix).into_any_element()
            } else {
                icon(group_icon, theme.muted_foreground).into_any_element()
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(if running {
                        ink.ink_strong
                    } else {
                        theme.foreground
                    })
                    .child(SharedString::from(label.to_owned())),
            )
            .on_click(move |_, _, cx| {
                view.update(cx, |this, cx| {
                    this.toggle(key);
                    cx.notify();
                });
            });
        if failed > 0 {
            head = head.child(
                chip(
                    format!("{failed} failed"),
                    theme.danger,
                    ink.danger_soft,
                    mono.clone(),
                )
                .ml_auto(),
            );
        }
        head = head.child(chevron(expanded, ink.faint));
        div()
            .id(ElementId::named_usize("row-group", group_ix))
            .w_full()
            .flex()
            .flex_col()
            .child(head)
    }

    /// The end of a run: when it ended and how long it ran, when this
    /// client watched it, the stop outcome, and the run's actions on
    /// hover.
    fn render_turn_end(
        &self,
        session: &Session,
        ix: usize,
        outcome: Option<Outcome>,
        end: Option<RunEnd>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let mono = cx.theme().mono_font_family.clone();
        let ink = crate::theme::Palette::active(cx);
        let mut row = h_flex()
            .gap(px(SP_4))
            .items_center()
            .min_h(px(TURN_END_H))
            .text_size(px(FS_2XS))
            .text_color(ink.faint);
        if let Some(end) = end {
            row = row
                .child(crate::clock::time_of_day(end.at))
                .child("\u{b7}")
                .child(crate::clock::span(end.took));
        }
        if let Some(outcome) = outcome {
            row = row.child(match outcome {
                Outcome::Interrupted => chip(
                    "interrupted".to_owned(),
                    theme.muted_foreground,
                    theme.list_hover,
                    mono.clone(),
                ),
                Outcome::Failed => chip(
                    "failed".to_owned(),
                    theme.danger,
                    ink.danger_soft,
                    mono.clone(),
                ),
            });
        }
        let (prompt, reply) = run_texts(session, ix);
        let composer = self.composer.clone();
        let store = self.store.clone();
        let mut actions = h_flex()
            .gap(px(SP_1))
            .opacity(0.)
            .group_hover("turn-end", |style| style.opacity(1.));
        if !reply.is_empty() {
            actions = actions.child(
                Clipboard::new(ElementId::named_usize("copy-turn", ix))
                    .value(reply)
                    .tooltip("Copy response"),
            );
        }
        if let Some(prompt) = prompt {
            actions = actions.child(
                Button::new(ElementId::named_usize("retry-turn", ix))
                    .icon(IconName::RefreshCw)
                    .xsmall()
                    .ghost()
                    .tooltip("Retry")
                    .on_click(move |_, window, cx| {
                        let accepted = store.act(cx, |store| store.submit(&prompt).is_some());
                        if !accepted {
                            let text = prompt.clone();
                            composer.update(cx, |state, cx| {
                                state.set_value(&text, window, cx);
                                state.focus(window, cx);
                            });
                        }
                    }),
            );
        }
        div()
            .id(ElementId::named_usize("row-turn-end", ix))
            .group("turn-end")
            .w_full()
            .child(row.child(actions))
    }

    /// The placeholder shown while the session has nothing to show.
    fn placeholder(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let text = match self.store.read(cx).active_session() {
            None => "no session yet; the agent answers here once one opens",
            Some(session) if session.items.is_empty() => {
                "say hello to start the run; the transcript plays out here"
            }
            Some(_) => "",
        };
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.muted_foreground)
            .child(text)
    }
}

/// The prompt that started the run ending at `end`, and the reply text
/// the run produced, for the turn-end actions.
fn run_texts(session: &Session, end: usize) -> (Option<String>, String) {
    let start = session.items[..end]
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::User { steered: false, .. }));
    let prompt = start.and_then(|ix| match &session.items[ix] {
        TranscriptItem::User { content, .. } => Some(prompt_text(content)),
        _ => None,
    });
    let reply = session.items[start.map_or(0, |ix| ix + 1)..end]
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (prompt.filter(|text| !text.is_empty()), reply)
}

/// An empty row, shown only when the model names an item that is gone.
fn blank_row(ix: usize) -> Stateful<Div> {
    div().id(ElementId::named_usize("row-blank", ix)).w_full()
}

/// The design's notice row: the text between two flanking hairlines,
/// in the tone's color.
fn render_notice(
    ix: usize,
    tone: NoticeTone,
    text: &str,
    cx: &Context<TranscriptView>,
) -> Stateful<Div> {
    let theme = cx.theme().colors;
    let ink = crate::theme::Palette::active(cx);
    let (color, pill) = match tone {
        NoticeTone::Info => (ink.faint, false),
        NoticeTone::Warn => (theme.warning, false),
        NoticeTone::Error => (theme.danger, true),
        NoticeTone::Success => (theme.success, false),
    };
    let rule = || div().flex_1().h(px(1.)).bg(ink.subtle);
    div()
        .id(ElementId::named_usize("row-notice", ix))
        .w_full()
        .flex()
        .items_center()
        .gap(px(SP_4))
        .child(rule())
        .child(
            div()
                .when(pill, |body| {
                    body.px(px(10.))
                        .py(px(SP_2))
                        .rounded(px(R_FULL))
                        .bg(ink.danger_soft)
                })
                .text_size(px(FS_XS))
                .text_color(color)
                .child(SharedString::from(text.to_owned())),
        )
        .child(rule())
}

/// A compaction row with the tokens the summary saved, between the
/// same flanking hairlines the notices use.
fn render_compaction(
    ix: usize,
    kept: u64,
    before: u64,
    after: u64,
    cx: &Context<TranscriptView>,
) -> Stateful<Div> {
    let ink = crate::theme::Palette::active(cx);
    let rule = || div().flex_1().h(px(1.)).bg(ink.subtle);
    div()
        .id(ElementId::named_usize("row-compaction", ix))
        .w_full()
        .flex()
        .items_center()
        .gap(px(SP_4))
        .child(rule())
        .child(
            div()
                .text_size(px(FS_XS))
                .text_color(ink.faint)
                .child(SharedString::from(format!(
                    "context compacted: kept {kept} turns, {before} -> {after} tokens"
                ))),
        )
        .child(rule())
}

/// Where one todo stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TodoState {
    /// Not started.
    Pending,
    /// The one in progress.
    Running,
    /// Finished.
    Done,
    /// Given up on.
    Failed,
}

/// One todo of a todos row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Todo {
    text: String,
    state: TodoState,
}

/// The todos of an ACP plan update: `content` and `status` per entry.
fn todos_of_plan(entries: &[serde_json::Value]) -> Vec<Todo> {
    entries
        .iter()
        .map(|entry| Todo {
            text: entry
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            state: match entry.get("status").and_then(serde_json::Value::as_str) {
                Some("completed") => TodoState::Done,
                Some("in_progress") => TodoState::Running,
                _ => TodoState::Pending,
            },
        })
        .collect()
}

/// The todos a `todo_list` call wrote: `title` and `status` per entry.
fn todos_of_call(call: &ToolCallItem) -> Option<Vec<Todo>> {
    if call.title != "todo_list" {
        return None;
    }
    let todos = call.input.as_ref()?.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .map(|todo| Todo {
                text: todo
                    .get("title")
                    .or_else(|| todo.get("content"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                state: match todo.get("status").and_then(serde_json::Value::as_str) {
                    Some("done" | "completed") => TodoState::Done,
                    Some("in_progress") => TodoState::Running,
                    Some("failed") => TodoState::Failed,
                    _ => TodoState::Pending,
                },
            })
            .collect(),
    )
}

/// The todos row: Updated todos, the one in progress or all done, the
/// done count, and the list when expanded.
fn render_todos(
    ix: usize,
    todos: &[Todo],
    expanded: bool,
    cx: &Context<TranscriptView>,
) -> Stateful<Div> {
    let theme = cx.theme().colors;
    let mono = cx.theme().mono_font_family.clone();
    let ink = crate::theme::Palette::active(cx);
    let view = cx.entity();
    let done = todos
        .iter()
        .filter(|todo| todo.state == TodoState::Done)
        .count();
    let complete = done == todos.len() && !todos.is_empty();
    let current = todos
        .iter()
        .find(|todo| todo.state == TodoState::Running)
        .map(|todo| todo.text.clone())
        .unwrap_or_default();
    let head = h_flex()
        .id("head")
        .min_h(px(ROW_H))
        .ml(px(-SP_3))
        .px(px(SP_3))
        .py(px(SP_1))
        .gap(px(SP_4))
        .items_center()
        .rounded(px(R_SM))
        .cursor_pointer()
        .text_size(px(FS_SM))
        .hover(move |style| style.bg(theme.list_hover))
        .on_click(move |_, _, cx| {
            view.update(cx, |this, cx| {
                this.toggle(RowKey::Item(ix));
                cx.notify();
            });
        })
        .child(icon(IconName::ListTodo, theme.muted_foreground))
        .child(
            div()
                .whitespace_nowrap()
                .text_color(theme.foreground)
                .child("Updated todos"),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(mono.clone())
                .text_size(px(DETAIL_SIZE))
                .text_color(theme.muted_foreground)
                .child(SharedString::from(if complete {
                    "all done".to_owned()
                } else {
                    current
                })),
        )
        .child(
            chip(
                format!("{done}/{}", todos.len()),
                if complete {
                    theme.success
                } else {
                    theme.muted_foreground
                },
                if complete {
                    ink.ok_soft
                } else {
                    theme.list_hover
                },
                mono.clone(),
            )
            .ml_auto(),
        )
        .child(chevron(expanded, ink.faint));
    let mut row = div()
        .id(ElementId::named_usize("row-plan", ix))
        .w_full()
        .flex()
        .flex_col()
        .child(head);
    if !expanded {
        return row;
    }
    let mut list = v_flex().pt(px(SP_3)).px(px(SP_5)).pb(px(10.));
    for (todo_ix, todo) in todos.iter().enumerate() {
        let (lead, text_color) = match todo.state {
            TodoState::Done => (
                icon(IconName::CircleCheck, theme.success).into_any_element(),
                theme.muted_foreground,
            ),
            TodoState::Running => (
                spinner(theme.primary, ix * 1000 + todo_ix).into_any_element(),
                theme.secondary_foreground,
            ),
            TodoState::Failed => (
                icon(IconName::CircleX, theme.danger).into_any_element(),
                theme.danger,
            ),
            TodoState::Pending => (
                icon(IconName::Circle, ink.ghost).into_any_element(),
                theme.foreground,
            ),
        };
        let mut line = h_flex()
            .items_start()
            .gap(px(9.))
            .py(px(3.))
            .text_size(px(FS_SM))
            .text_color(text_color)
            .child(div().mt(px(3.)).child(lead))
            .child(SharedString::from(todo.text.clone()));
        if todo.state == TodoState::Done {
            line = line.line_through();
        }
        list = list.child(line);
    }
    row = row.child(
        div()
            .mt(px(SP_2))
            .ml(px(DETAIL_INDENT))
            .border_1()
            .border_color(theme.border)
            .rounded(px(R_LG))
            .bg(theme.muted)
            .overflow_hidden()
            .child(list),
    );
    row
}

/// The one-line record of an answered permission ask: what the ask
/// was about, the chosen label as offered, and the feedback quoted
/// when it rode along.
fn render_decision(
    ix: usize,
    subject: &str,
    label: &str,
    allowed: bool,
    feedback: Option<&str>,
    cx: &Context<TranscriptView>,
) -> AnyElement {
    let theme = cx.theme().colors;
    let ink = crate::theme::Palette::active(cx);
    let mono = cx.theme().mono_font_family.clone();
    let color = if allowed { theme.success } else { theme.danger };
    let mut row = h_flex()
        .w_full()
        .min_h(px(ROW_H))
        .px(px(SP_3))
        .py(px(SP_1))
        .gap(px(SP_4))
        .items_center()
        .text_size(px(FS_XS))
        .text_color(theme.muted_foreground)
        .child(
            Icon::new(if allowed {
                IconName::ShieldCheck
            } else {
                IconName::ShieldX
            })
            .with_size(px(ICON_XS))
            .text_color(color),
        )
        .child(
            div()
                .whitespace_nowrap()
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(mono.clone())
                .text_color(theme.foreground)
                .child(SharedString::from(subject.to_owned())),
        );
    if let Some(feedback) = feedback {
        row = row.child(
            div()
                .flex_none()
                .italic()
                .text_color(ink.faint)
                .child(SharedString::from(format!("\"{feedback}\""))),
        );
    }
    div()
        .id(ElementId::named_usize("row-decision", ix))
        .test_support()
        .w_full()
        .aria_label(SharedString::from(match feedback {
            Some(feedback) => format!("{label} {subject} \"{feedback}\""),
            None => format!("{label} {subject}"),
        }))
        .child(row)
        .into_any_element()
}

/// The design's detail box: a bordered surface with an optional head
/// naming the path or command, indented under the row that opened it.
/// It carries no bottom margin of its own: [`margins`] reports the
/// design's 10px as the row's closing margin and [`gap`] seats the next
/// row on it, so a margin here too would double it.
fn render_detail(
    head: Option<(IconName, String)>,
    body: Div,
    ix: usize,
    cx: &Context<TranscriptView>,
) -> impl IntoElement {
    let theme = cx.theme().colors;
    let mono = cx.theme().mono_font_family.clone();
    let mut box_ = div()
        .id(ElementId::named_usize("detail", ix))
        .test_support()
        .mt(px(SP_2))
        .ml(px(DETAIL_INDENT))
        .border_1()
        .border_color(theme.border)
        .rounded(px(R_LG))
        .bg(theme.muted)
        .overflow_hidden();
    if let Some((icon_name, path)) = head {
        box_ = box_.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(SP_4))
                .px(px(SP_5))
                .py(px(DETAIL_HEAD_PY))
                .border_b_1()
                .border_color(crate::theme::Palette::active(cx).subtle)
                .text_size(px(FS_XS))
                .text_color(theme.muted_foreground)
                .child(Icon::new(icon_name).with_size(px(ICON_XS)))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .text_color(theme.foreground)
                        .child(SharedString::from(path)),
                ),
        );
    }
    box_.child(body).into_any_element()
}

/// The mono body of a detail box: the design's deep well at its 12px
/// detail size.
fn render_pre(text: &str, cx: &Context<TranscriptView>) -> Div {
    div()
        .w_full()
        .px(px(SP_5))
        .py(px(DETAIL_PRE_PY))
        .bg(crate::theme::Palette::active(cx).deep)
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(DETAIL_SIZE))
        .line_height(px(DETAIL_LINE))
        .child(SharedString::from(text.to_owned()))
}

/// A unified diff with per-line added and removed coloring, computed
/// from the markers the delivered text carries.
/// The diff block as the transcript and the workbench both draw it.
pub(crate) fn render_diff(lines: &[DiffLine], cx: &App) -> Div {
    let theme = cx.theme().colors;
    let mono = cx.theme().mono_font_family.clone();
    let ink = crate::theme::Palette::active(cx);
    let mut view = v_flex().w_full().bg(ink.deep);
    for line in lines {
        let (color, bg) = match line {
            DiffLine::Add(_) => (ink.diff_add, ink.diff_add_bg),
            DiffLine::Del(_) => (ink.diff_del, ink.diff_del_bg),
            DiffLine::Head(_) => (ink.faint, theme.list_hover),
            DiffLine::Ctx(_) => (theme.foreground, ink.deep),
        };
        view = view.child(
            h_flex()
                .whitespace_nowrap()
                .font_family(mono.clone())
                .text_size(px(DETAIL_SIZE))
                .line_height(px(DETAIL_LINE))
                .bg(bg)
                .text_color(color)
                .child(
                    div()
                        .w(px(20.))
                        .flex_none()
                        .text_center()
                        .text_color(ink.ghost)
                        .child(line.marker()),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .pr(px(SP_5))
                        .child(SharedString::from(line.body().to_owned())),
                ),
        );
    }
    view
}

impl Render for TranscriptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.store.read(cx).active_id().map(str::to_owned);
        if active != self.session_key {
            let left = std::mem::take(&mut self.ui);
            if let Some(key) = self.session_key.take() {
                self.stashed.insert(key, left);
            }
            self.ui = active
                .as_ref()
                .and_then(|key| self.stashed.remove(key))
                .unwrap_or_default();
            self.session_key = active;
            self.render_counts.clear();
            self.find = None;
            self.list = following_list();
            self.signatures.clear();
        }
        let model = {
            let session = self.store.read(cx).active_session();
            match session {
                Some(session) => Rc::new(row_model(session, &self.ui)),
                None => Rc::default(),
            }
        };
        let signatures = match self.store.read(cx).active_session() {
            Some(session) => signatures(session, &model.rows),
            None => Vec::new(),
        };
        self.sync_list(signatures);
        self.model = model.clone();
        let count = model.rows.len();
        let banner = self.banner(cx);
        let follow = self.following();
        let colors = cx.theme().colors;

        let mut panel = v_flex().id("transcript").size_full().min_h_0();
        if let Some(banner) = banner {
            panel = panel.child(banner);
        }
        let view = cx.entity().downgrade();
        panel.child(if count > 0 {
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .overflow_hidden()
                .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                .child(
                    list(self.list.clone(), move |ix, _window, cx| {
                        view.update(cx, |this, cx| {
                            let model = this.model.clone();
                            let Some(row) = model.rows.get(ix) else {
                                return div().into_any_element();
                            };
                            *this.render_counts.entry(row.key()).or_insert(0) += 1;
                            let prev = ix.checked_sub(1).and_then(|p| model.rows.get(p));
                            let last = ix + 1 == model.rows.len();
                            let element = this.render_row(ix, row, prev, last, cx);
                            match this.row_tint(row.key(), colors) {
                                Some(tint) => div()
                                    .id(ElementId::named_usize("find-hit", ix))
                                    .test_support()
                                    .w_full()
                                    .bg(tint)
                                    .child(element)
                                    .into_any_element(),
                                None => element,
                            }
                        })
                        .unwrap_or_else(|_| div().into_any_element())
                    })
                    .size_full(),
                )
                .when(!follow, |area| {
                    area.child(
                        div().absolute().bottom_3().right_4().child(
                            Button::new("jump-latest")
                                .label("Jump to latest")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.list.set_follow_mode(FollowMode::Tail);
                                    cx.notify();
                                })),
                        ),
                    )
                })
                .into_any_element()
        } else {
            self.placeholder(cx).into_any_element()
        })
    }
}

/// A list that starts at the bottom and follows it.
fn following_list() -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW));
    list.set_follow_mode(FollowMode::Tail);
    list
}

/// One signature per row: what decides its laid-out height. A row whose
/// signature is unchanged keeps its measurement.
fn signatures(session: &Session, rows: &[Row]) -> Vec<u64> {
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
                RowKey::Item(item) => item_fingerprint(session, item, &mut hasher),
                RowKey::Group(_) => {
                    for item in members {
                        item_fingerprint(session, *item, &mut hasher);
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
        Some(TranscriptItem::Assistant { text } | TranscriptItem::Thinking { text }) => {
            text.len().hash(hasher);
        }
        Some(TranscriptItem::User { content, .. }) => content.len().hash(hasher),
        Some(TranscriptItem::ToolCall(call)) => {
            format!("{:?}", call.status).hash(hasher);
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

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, ElementId, TestAppContext, Window};

    use super::{
        ChipTone, Outcome, Row, RowKey, UiState, chips_for, diff_lines, diff_stat, last_sentence,
        row_model, tool_verb,
    };
    use crate::store::{Command, Store};
    use crate::transport::State;
    use crate::transport::replay;
    use kage_client::wire::{
        ContentBlock, DiffContent, MessageChunk, ToolCallContent, ToolCallStatus, ToolKind,
        TurnReason,
    };
    use kage_client::{Frame, Session, ToolCallItem, TranscriptItem};

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
            title: title.into(),
            kind,
            status,
            input,
            swarm: None,
            content,
            raw_output,
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
        };
        let model = row_model(&session, &ui);
        assert_eq!(model.kinds(), vec!["group", "tool", "tool", "tool"]);
        let nested: Vec<usize> = model.rows[1..]
            .iter()
            .map(|row| match row {
                Row::Tool {
                    ix, nested: true, ..
                } => *ix,
                other => panic!("expected a nested tool row, got {other:?}"),
            })
            .collect();
        assert_eq!(nested, vec![0, 1, 2]);
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
            },
            TranscriptItem::TurnEnd {
                reason: Some(TurnReason::NoToolCalls),
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
    fn compaction_rows_keep_the_delivered_token_counts() {
        let session = session_with(vec![TranscriptItem::Compaction {
            kept: 4,
            before: 90_000,
            after: 12_000,
        }]);
        let model = row_model(&session, &UiState::default());
        assert_eq!(model.kinds(), vec!["compaction"]);
        match session.items[0] {
            TranscriptItem::Compaction {
                kept,
                before,
                after,
            } => assert_eq!((kept, before, after), (4, 90_000, 12_000)),
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
    fn a_streaming_delta_moves_only_its_own_row_signature() {
        let mut session = session_with(vec![
            TranscriptItem::User {
                content: vec![ContentBlock::text("go")],
                steered: false,
            },
            TranscriptItem::Assistant { text: "a".into() },
        ]);
        let ui = UiState::default();
        let before = super::signatures(&session, &row_model(&session, &ui).rows);
        session.items[1] = TranscriptItem::Assistant {
            text: "a longer reply".into(),
        };
        let after = super::signatures(&session, &row_model(&session, &ui).rows);
        assert_eq!(before[0], after[0], "the prompt row keeps its measurement");
        assert_ne!(before[1], after[1], "the growing reply is measured again");
    }

    #[test]
    fn the_peek_is_the_last_sentence_one_line_long() {
        assert_eq!(
            last_sentence("First idea. Second thought."),
            "Second thought"
        );
        assert_eq!(last_sentence("no terminator"), "no terminator");
        let peek = last_sentence(&"word ".repeat(40));
        assert!(peek.chars().count() <= 121);
        assert!(peek.ends_with('\u{2026}'));
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

    /// An initialize answer with everything the gate accepts.
    fn init_answer() -> Frame {
        Frame::Success {
            id: 1,
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "steer": true,
                    "sessionCapabilities": {"close": {}},
                },
                "agentInfo": {"name": "kage", "version": "0.1.0"},
            }),
        }
    }

    /// A store with one open, empty session.
    fn booted_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer());
        let _ = store.take_outgoing();
        store.new_session();
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 3,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();
        store
    }

    /// A user chunk frame for `session`.
    fn user_chunk(session: &str, text: &str) -> Frame {
        Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({
                "sessionId": session,
                "update": {
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": text},
                },
            }),
        }
    }

    /// An assistant chunk frame for `session`.
    fn agent_chunk(session: &str, text: &str) -> Frame {
        Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({
                "sessionId": session,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                },
            }),
        }
    }

    /// A `session/request_permission` ask on "s1" with three options.
    fn ask_frame() -> Frame {
        Frame::Request {
            id: 101,
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "call-sh", "title": "shell", "kind": "execute",
                    "status": "pending", "rawInput": {"command": "cargo test"}},
                "options": [
                    {"optionId": "allow", "name": "Allow shell", "kind": "allow_once"},
                    {"optionId": "allow_session", "name": "Allow shell for this session",
                        "kind": "allow_always"},
                    {"optionId": "reject", "name": "Reject shell", "kind": "reject_once"},
                ],
            }),
        }
    }

    /// Opens a window on a transcript view over `store`.
    fn window_on(
        cx: &mut TestAppContext,
        store: gpui_kit::Entity<Store>,
    ) -> (
        gpui_kit::Entity<super::TranscriptView>,
        &mut gpui_kit::VisualTestContext,
    ) {
        cx.update(gpui_kit::init);
        cx.add_window_view(|window: &mut Window, cx| {
            let composer = cx.new(|cx| gpui_kit::component::input::TextareaState::new(window, cx));
            super::TranscriptView::new(store.clone(), composer, cx)
        })
    }

    #[gpui_kit::test]
    fn a_delta_rebuilds_only_the_visible_window_over_5000_items(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (view, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                for i in 0..5_000 {
                    store.absorb(user_chunk(
                        "s1",
                        &format!("message {i} with a line\nand another"),
                    ));
                }
            });
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let viewport_height = visual.update(|window, _| f32::from(window.viewport_size().height));
        let before = visual.update(|_, cx| view.read(cx).render_counts.clone());
        assert!(
            before.len() < 300,
            "the first draw renders a window, not 5000 rows: {}",
            before.len()
        );

        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.absorb(agent_chunk("s1", "the one streaming delta"));
            });
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let after = visual.update(|_, cx| view.read(cx).render_counts.clone());

        assert!(
            after.contains_key(&RowKey::Item(5_000)),
            "the delta row rendered"
        );
        assert_eq!(
            after.get(&RowKey::Item(2_000)),
            before.get(&RowKey::Item(2_000)),
            "a row far from the viewport never renders"
        );
        let grew = after
            .iter()
            .filter(|(key, count)| **count > before.get(*key).copied().unwrap_or(0))
            .count();
        // A two-line prompt bubble with its margin is well over 40px.
        let row_height = 40.0;
        let viewport_rows = (viewport_height / row_height) as usize;
        assert!(
            grew <= viewport_rows + 4,
            "a delta re-renders the visible window only: {grew} rows over a ~{viewport_rows} row viewport"
        );
        assert!(
            after.len() < before.len() + viewport_rows + 8,
            "no frame ever touched the whole list: {} distinct rows",
            after.len()
        );
    }

    #[gpui_kit::test]
    fn scrolling_up_stops_the_bottom_follow(cx: &mut TestAppContext) {
        use gpui_kit::InputEvent as _;
        use gpui_kit::{Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, point, px};

        let store = cx.new(|_| booted_store());
        let (view, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                for i in 0..400 {
                    store.absorb(user_chunk("s1", &format!("message {i}")));
                }
            });
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            visual.update(|_, cx| view.read(cx).following()),
            "a fresh list follows the bottom"
        );

        visual.update(|window, cx| {
            window.dispatch_event(
                gpui_kit::MouseMoveEvent {
                    position: point(px(80.), px(200.)),
                    pressed_button: Option::<MouseButton>::None,
                    modifiers: Modifiers::default(),
                }
                .to_platform_input(),
                cx,
            );
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.update(|window, cx| {
            window.dispatch_event(
                ScrollWheelEvent {
                    position: point(px(80.), px(200.)),
                    delta: ScrollDelta::Pixels(point(px(0.), px(240.))),
                    modifiers: Modifiers::default(),
                    touch_phase: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            !visual.update(|_, cx| view.read(cx).following()),
            "scrolling up stops the bottom follow"
        );
    }

    #[gpui_kit::test]
    fn a_decision_renders_as_a_one_line_row_quoting_the_feedback(cx: &mut TestAppContext) {
        let mut store = booted_store();
        store.absorb(ask_frame());
        let store = cx.new(|_| store);
        let (_view, visual) = window_on(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.reply_permission(
                    "s1",
                    101,
                    &kage_client::PermissionDecision::Feedback {
                        option_id: "reject".into(),
                        feedback: "use rustfmt first".into(),
                    },
                );
            });
        });
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert_eq!(
                window
                    .find(gpui_kit::ElementId::named_usize("row-decision", 0))
                    .label(),
                Some("Reject shell cargo test \"use rustfmt first\""),
                "the decision row names the chosen label, the subject and the feedback"
            );
        });
    }

    #[gpui_kit::test]
    fn scroll_to_plan_jumps_to_the_plan_card_and_stops_the_follow(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (view, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                for i in 0..400 {
                    store.absorb(user_chunk("s1", &format!("message {i}")));
                }
                store.absorb(plan_frame("s1"));
                for i in 400..800 {
                    store.absorb(user_chunk("s1", &format!("message {i}")));
                }
            });
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            visual.update(|_, cx| view.read(cx).following()),
            "a fresh list follows the bottom"
        );
        assert!(
            !visual.update(|_, cx| view.read(cx).render_counts.contains_key(&RowKey::Item(400))),
            "the plan card starts far outside the viewport"
        );

        visual.update(|_, cx| view.update(cx, |view, cx| view.scroll_to_plan(cx)));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            !visual.update(|_, cx| view.read(cx).following()),
            "jumping to the plan stops the bottom follow"
        );
        assert!(
            visual.update(|_, cx| view.read(cx).render_counts.contains_key(&RowKey::Item(400))),
            "the plan card is in view after the jump"
        );
    }

    /// The design seats an expanded row's next sibling on the collapsed
    /// margin, which is the larger of the two, not their sum. The detail
    /// box itself carries no bottom margin, so the gap between the box
    /// and the next row is exactly that collapsed value.
    #[gpui_kit::test]
    fn an_expanded_detail_closes_with_one_collapsed_margin(cx: &mut TestAppContext) {
        let mut store = booted_store();
        store.absorb(shell_call_frame("s1"));
        store.absorb(agent_chunk("s1", "the answer"));
        let store = cx.new(|_| store);
        let (view, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.ui.expanded.insert(RowKey::Item(0));
                cx.notify();
            });
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let (detail, cell) = visual.update(|window, _| {
            (
                window.find(ElementId::named_usize("detail", 0)).bounds(),
                window.find(ElementId::named_usize("row-cell", 0)).bounds(),
            )
        });
        // The detail box ends where its row cell ends. The design closes an
        // expanded row with a 10px margin, and [`margins`] reports that as
        // the row's closing margin so [`gap`] seats the next row on it; a
        // margin on the box as well would add a second one, which is what a
        // box bottom past its cell's bottom would mean.
        assert_eq!(
            detail.origin.y + detail.size.height,
            cell.origin.y + cell.size.height,
            "the detail box adds no margin of its own below the row"
        );
    }

    /// A completed read tool call on `session`, carrying output so the row
    /// has a detail box to show.
    fn shell_call_frame(session: &str) -> Frame {
        Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({
                "sessionId": session,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-read",
                    "title": "read",
                    "kind": "read",
                    "status": "completed",
                    "rawInput": {"path": "src/main.rs"},
                    "content": [{
                        "type": "content",
                        "content": {"type": "text", "text": "fn main() {}"},
                    }],
                },
            }),
        }
    }

    /// A plan update frame for `session`.
    fn plan_frame(session: &str) -> Frame {
        Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({
                "sessionId": session,
                "update": {
                    "sessionUpdate": "plan",
                    "entries": [
                        {"content": "read the parser", "priority": "high",
                            "status": "completed"},
                        {"content": "write the tests", "priority": "medium",
                            "status": "in_progress"},
                        {"content": "ship it", "priority": "low", "status": "pending"},
                    ],
                },
            }),
        }
    }
}
