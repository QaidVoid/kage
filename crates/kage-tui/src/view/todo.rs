//! The todo list as the conversation's last `todo_list` call left it.
//!
//! The TUI has no handle on the tool's own state, so this reads the
//! transcript instead: the most recent `todo_list` call's `todos`
//! argument is what the model last wrote, which is what a sticky row
//! above the input should show. A replayed session shows its list for
//! free, since the call is already in the buffer.

use serde_json::Value;

use std::borrow::Borrow;

use crate::buffer::Block;

/// Tool name this reads from.
const TODO_TOOL: &str = "todo_list";

/// Marker of a task the model marked `in_progress`, and of a task not
/// started yet. A view test matches painted rows on these.
pub(crate) const MARK_RUNNING: &str = "\u{25cf}";
/// See [`MARK_RUNNING`].
pub(crate) const MARK_PENDING: &str = "\u{25cb}";

/// How many task rows the box paints inside its border before it
/// leaves the rest to the count on the frame. The box competes with
/// the pinned agents and pending prompts, so it stays short.
pub(crate) const TODO_MAX_ROWS: usize = 5;

/// One task, as the sticky row shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TodoRow {
    /// The task's title, trimmed.
    pub(crate) title: String,
    /// `pending`, `in_progress` or `done`.
    pub(crate) status: String,
}

/// The todo list derived from a transcript, ready to pin under the
/// working row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TodoStrip {
    /// Every task, in the order the model sent them.
    pub(crate) items: Vec<TodoRow>,
    /// How many are done.
    pub(crate) done: usize,
    /// Whether a `todo_list` write was seen at all, so a cleared list
    /// is not confused with a session that never tracked one.
    pub(crate) tracked: bool,
}

impl TodoStrip {
    /// `done / total` for the summary row, `None` when nothing was ever
    /// tracked.
    pub(crate) fn progress(&self) -> Option<(usize, usize)> {
        self.tracked.then_some((self.done, self.items.len()))
    }
}

/// The last `todo_list` write in `blocks`, as a strip. A call with no
/// list argument was a read, so it leaves the previous write in place
/// rather than clearing the row. The argument reads leniently, matching
/// what the tool accepts: a bare array is the list, and a `todos` map
/// nested one level deep unwraps.
pub(crate) fn from_blocks<'a, I, B>(blocks: I) -> TodoStrip
where
    I: IntoIterator<Item = &'a B>,
    B: Borrow<Block> + 'a + ?Sized,
{
    let mut strip = TodoStrip::default();
    for block in blocks {
        let Block::ToolCall { name, input, .. } = block.borrow() else {
            continue;
        };
        if name != TODO_TOOL {
            continue;
        }
        let Some(list) = todo_list_arg(input) else {
            continue;
        };
        strip = parse(list);
    }
    strip
}

/// The list a `todo_list` call carried, or `None` for a read. An empty
/// map where a list was meant reads as "cleared", matching the tool.
fn todo_list_arg(input: &Value) -> Option<&[Value]> {
    const EMPTY: &[Value] = &[];
    if let Some(list) = input.as_array() {
        return Some(list);
    }
    let mut todos: &Value = input.get("todos").filter(|value| !value.is_null())?;
    if let Some(inner) = todos
        .as_object()
        .and_then(|nested| nested.get("todos"))
        .filter(|value| !value.is_null())
    {
        todos = inner;
    }
    match todos.as_array() {
        Some(list) => Some(list),
        None if todos.as_object().is_some_and(serde_json::Map::is_empty) => Some(EMPTY),
        None => None,
    }
}

/// A todo list as `todos` carried it, ignoring entries it cannot read.
fn parse(list: &[Value]) -> TodoStrip {
    let mut items = Vec::with_capacity(list.len());
    for entry in list {
        let (Some(title), Some(status)) = (
            entry.get("title").and_then(Value::as_str),
            entry.get("status").and_then(Value::as_str),
        ) else {
            continue;
        };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        items.push(TodoRow {
            title: title.to_owned(),
            status: status.to_owned(),
        });
    }
    let done = items.iter().filter(|i| i.status == "done").count();
    TodoStrip {
        items,
        done,
        tracked: true,
    }
}

/// What a painted row stands for. A variant rather than a word the
/// painter matches on, so a row cannot claim a status the painter would
/// then render as a different one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RowKind {
    /// The `done / total` counter. A heading, so it lifts onto the
    /// box's top border.
    Heading,
    /// A task the model marked `in_progress`.
    Running,
    /// A task not started yet.
    Pending,
    /// A finished task, struck through.
    Done,
}

/// A painted row: its text and what the row stands for.
pub(crate) type PaintedRow = (String, RowKind);

/// The rows to pin: the progress title, then the tasks, capped at
/// [`TODO_MAX_ROWS`]. The title leads so its painter can lift it onto
/// the box's top border. Tasks paint running first, then pending, then
/// done, so the actionable rows stay on screen when the list is cut; a
/// stable sort keeps each group in the order the model sent. A plan
/// with nothing outstanding paints nothing: the box pins only while
/// work is in flight, and the transcript carries the completion.
/// `width` is the box's inner width; a task title yields the lead, its
/// marker and a space, so it truncates four cells inside it.
pub(crate) fn rows(strip: &TodoStrip, width: usize) -> Vec<PaintedRow> {
    let Some((done, total)) = strip.progress() else {
        return Vec::new();
    };
    if done >= total {
        return Vec::new();
    }
    let label = if total == 1 { "todo" } else { "todos" };
    let mut out = vec![(format!("{done}/{total} {label}"), RowKind::Heading)];
    let mut body: Vec<(RowKind, &str)> = strip
        .items
        .iter()
        .map(|i| (row_kind(&i.status), &*i.title))
        .collect();
    body.sort_by_key(|(kind, _)| match kind {
        RowKind::Running => 0,
        RowKind::Pending => 1,
        RowKind::Done => 2,
        RowKind::Heading => 3,
    });
    for (shown, (kind, title)) in body.into_iter().enumerate() {
        if shown >= TODO_MAX_ROWS {
            break;
        }
        out.push((
            super::truncate_to_width(title, width.saturating_sub(4), "..."),
            kind,
        ));
    }
    out
}

/// The marker class of a status the model sent. Unknown statuses read
/// as pending.
fn row_kind(status: &str) -> RowKind {
    match status {
        "in_progress" => RowKind::Running,
        "done" => RowKind::Done,
        _ => RowKind::Pending,
    }
}

/// Rows the whole pinned box claims: the task rows plus the two
/// border rows. The heading rides the top border, so it costs no row
/// of its own. A strip with nothing to paint claims none, so a
/// finished or cleared plan collapses the box.
pub(crate) fn box_height(strip: &TodoStrip) -> u16 {
    let rows = rows(strip, usize::MAX).len();
    let interior = rows.saturating_sub(1);
    u16::try_from(if rows == 0 { 0 } else { interior + 2 }).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr as _;

    use std::sync::Arc;
    use std::time::Instant;

    use serde_json::json;

    use super::*;
    use crate::view::tool_view::ToolPhase;

    fn call(name: &str, input: Value) -> Block {
        Block::ToolCall {
            call_id: "c".to_owned(),
            name: name.to_owned(),
            input_summary: String::new(),
            input_pretty: String::new(),
            input: Arc::new(input),
            folded: false,
            phase: ToolPhase::Done,
            progress: String::new(),
            started_at: Instant::now(),
            diff: None,
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "a test constructor takes the payload the tool sends"
    )]
    fn list(todos: Value) -> Block {
        call(TODO_TOOL, json!({ "todos": todos }))
    }

    fn todo(title: &str, status: &str) -> Value {
        json!({ "title": title, "status": status })
    }

    #[test]
    fn a_transcript_without_todos_is_untracked() {
        let strip = from_blocks(&[call("read", json!({ "path": "a" }))]);
        assert!(!strip.tracked);
        assert_eq!(strip.progress(), None);
        assert!(rows(&strip, 80).is_empty());
        assert_eq!(box_height(&strip), 0);
    }

    #[test]
    fn a_read_call_does_not_clear_the_list() {
        let strip = from_blocks(&[
            list(json!([todo("a", "pending")])),
            call(TODO_TOOL, json!({})),
        ]);
        assert!(strip.tracked);
        assert_eq!(strip.items.len(), 1);
    }

    #[test]
    fn the_last_write_wins() {
        let strip = from_blocks(&[
            list(json!([todo("old", "pending")])),
            list(json!([todo("new", "done")])),
        ]);
        assert_eq!(strip.items.len(), 1);
        assert_eq!(strip.items[0].title, "new");
        assert_eq!(strip.done, 1);
    }

    #[test]
    fn an_explicit_empty_list_stays_tracked_with_nothing_to_do() {
        let strip = from_blocks(&[list(json!([todo("a", "pending")])), list(json!([]))]);
        assert!(strip.tracked);
        assert!(strip.items.is_empty());
        assert_eq!(strip.progress(), Some((0, 0)));
    }

    #[test]
    fn a_finished_list_unpins_the_box() {
        let strip = from_blocks(&[list(json!([todo("a", "done"), todo("b", "done")]))]);
        assert!(strip.tracked);
        assert_eq!(strip.progress(), Some((2, 2)));
        assert!(rows(&strip, 80).is_empty());
        assert_eq!(box_height(&strip), 0);
    }

    #[test]
    fn unreadable_entries_are_skipped() {
        let strip = from_blocks(&[list(json!([
            "not an object",
            todo("", "pending"),
            todo("  padded  ", "pending"),
            json!({ "status": "done" }),
            todo("ok", "done"),
        ]))]);
        assert_eq!(strip.items.len(), 2);
        assert_eq!(strip.items[0].title, "padded");
        assert_eq!(strip.done, 1);
    }

    #[test]
    fn a_non_array_todos_leaves_the_list_alone() {
        let strip = from_blocks(&[
            list(json!([todo("keep", "pending")])),
            list(json!("everything")),
        ]);
        assert_eq!(strip.items.len(), 1);
        assert_eq!(strip.items[0].title, "keep");
    }

    #[test]
    fn rows_lead_with_progress_then_the_running_task() {
        let strip = from_blocks(&[list(json!([
            todo("reading the code", "in_progress"),
            todo("writing it", "pending"),
            todo("checking", "done"),
        ]))]);
        let rows = rows(&strip, 80);
        assert_eq!(rows[0], ("1/3 todos".to_owned(), RowKind::Heading));
        assert_eq!(rows[1], ("reading the code".to_owned(), RowKind::Running));
        assert_eq!(rows[2], ("writing it".to_owned(), RowKind::Pending));
        assert_eq!(rows[3], ("checking".to_owned(), RowKind::Done));
    }

    #[test]
    fn a_single_item_is_not_pluralized() {
        let strip = from_blocks(&[list(json!([todo("only", "pending")]))]);
        assert_eq!(
            rows(&strip, 80)[0],
            ("0/1 todo".to_owned(), RowKind::Heading)
        );
    }

    #[test]
    fn rows_stop_at_the_cap() {
        let todos: Vec<_> = (0..10).map(|i| todo(&format!("t{i}"), "pending")).collect();
        let strip = from_blocks(&[list(json!(todos))]);
        let rows = rows(&strip, 80);
        assert_eq!(rows.len(), TODO_MAX_ROWS + 1);
        assert_eq!(rows[1].0, "t0");
        assert_eq!(rows[5].0, "t4");
    }

    #[test]
    fn a_long_title_is_cut_to_the_width() {
        let strip = from_blocks(&[list(json!([todo(&"x".repeat(200), "pending")]))]);
        let rows = rows(&strip, 20);
        assert!(rows[1].0.ends_with("..."), "{}", rows[1].0);
        assert!(rows[1].0.width() <= 20, "{}", rows[1].0.width());
    }

    #[test]
    fn a_finished_list_shows_no_rows() {
        let strip = from_blocks(&[list(json!([todo("a", "done"), todo("b", "done"),]))]);
        assert!(rows(&strip, 80).is_empty());
    }

    #[test]
    fn a_nested_write_updates_the_strip() {
        let strip = from_blocks(&[call(
            TODO_TOOL,
            json!({ "todos": { "todos": [todo("nested", "pending")] } }),
        )]);
        assert_eq!(strip.items.len(), 1);
        assert_eq!(strip.items[0].title, "nested");
    }

    #[test]
    fn a_bare_array_write_updates_the_strip() {
        let strip = from_blocks(&[call(TODO_TOOL, json!([todo("bare", "pending")]))]);
        assert_eq!(strip.items.len(), 1);
        assert_eq!(strip.items[0].title, "bare");
    }

    #[test]
    fn an_empty_map_write_clears_the_strip() {
        let strip = from_blocks(&[
            list(json!([todo("a", "pending")])),
            call(TODO_TOOL, json!({ "todos": {} })),
        ]);
        assert!(strip.tracked);
        assert!(strip.items.is_empty());
        assert_eq!(box_height(&strip), 0);
    }

    #[test]
    fn height_matches_the_rows_claimed() {
        let strip = from_blocks(&[list(json!([
            todo("a", "in_progress"),
            todo("b", "pending"),
            todo("c", "pending"),
            todo("d", "pending"),
        ]))]);
        assert_eq!(rows(&strip, 80).len(), 5);
        assert_eq!(box_height(&strip), 6);
        // A finished list claims nothing.
        let done = from_blocks(&[list(json!([todo("a", "done"), todo("b", "done")]))]);
        assert_eq!(rows(&done, 80).len(), 0);
        assert_eq!(box_height(&done), 0);
        assert_eq!(box_height(&TodoStrip::default()), 0);
    }
}
