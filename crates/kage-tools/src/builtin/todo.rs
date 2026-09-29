//! `todo_list`: the model's own task list for the session.
//!
//! One tool, both directions. No arguments reads the list; `todos`
//! replaces it wholesale, so the model cannot leave an item stranded
//! in `in_progress` by forgetting to clear it.
//!
//! A write also returns the stored list as structured output, which
//! the ACP bridge turns into a `plan` update for the client. State is
//! in memory for the session; a resumed session starts empty, so a
//! replay sends no `plan` update.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolContext, ToolError, schema_for};

/// Upper bound on items in the list, and on a single title's length.
/// A list this size is already too big to track; longer titles burn
/// context without telling the model more.
const MAX_ITEMS: usize = 100;
const MAX_TITLE: usize = 200;

/// What one task is doing. Serialized lowercase, which is what the
/// model sends back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Being worked on now.
    InProgress,
    /// Finished.
    Done,
}

impl TodoStatus {
    /// The marker the rendered list shows.
    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Done => "[x]",
        }
    }
}

/// One task. `title` and `status` are required; `id`, `owner` and
/// `blockedBy` are kept when supplied, so a later adoption of the
/// plan 019 item shape keeps its fields.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    /// Short, actionable description of the task.
    pub title: String,
    /// Where the task stands.
    pub status: TodoStatus,
    /// Stable id of the task, when the model supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Who is on the task, when the model says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Ids of tasks that must finish before this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Vec<String>>,
}

impl TodoItem {
    /// The task as stored, with its title trimmed and bounded. An
    /// empty title is an error rather than a blank line in the list.
    fn checked(&self) -> Result<TodoItem, ToolError> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err(ToolError::InvalidInput(
                "every todo needs a non-empty title".to_owned(),
            ));
        }
        Ok(TodoItem {
            title: truncate(title),
            status: self.status,
            id: self.id.clone(),
            owner: self.owner.clone(),
            blocked_by: self.blocked_by.clone(),
        })
    }
}

/// Cut `text` to `MAX_TITLE` characters, never mid-character. A longer
/// title is cut rather than refused: it is a display line, and the
/// model's intent survives the tail.
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_TITLE {
        return text.to_owned();
    }
    text.chars().take(MAX_TITLE).collect()
}

/// The session's task list.
#[derive(Clone, Debug, Default)]
pub struct TodoList(Arc<Mutex<Vec<TodoItem>>>);

impl TodoList {
    /// An empty list. The engine builds one per session and registers
    /// the tool over it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> Vec<TodoItem> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the list, checking every item first so a rejected write
    /// leaves the previous list intact rather than half-applied.
    fn replace(&self, todos: &[TodoItem]) -> Result<Vec<TodoItem>, ToolError> {
        if todos.len() > MAX_ITEMS {
            return Err(ToolError::InvalidInput(format!(
                "todo list has {} items, at most {MAX_ITEMS} are kept",
                todos.len()
            )));
        }
        let checked: Vec<TodoItem> = todos
            .iter()
            .map(TodoItem::checked)
            .collect::<Result<_, _>>()?;
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&checked);
        Ok(checked)
    }
}

/// The list as text, one task per line, or a note that it is empty.
fn render(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".to_owned();
    }
    let mut out = String::from("Current todo list:");
    for todo in todos {
        let _ = write!(out, "\n  {} {}", todo.status.marker(), todo.title);
    }
    out
}

/// Input shape for the `todo_list` tool. Only the schema is built from
/// this; [`TodoListTool::execute`] reads the argument off the JSON.
#[derive(Debug, Deserialize, JsonSchema)]
struct TodoListInput {
    /// The full todo list, replacing whatever is there. Omit this
    /// argument to read the list without changing it. Send `[]` to
    /// clear it. Keep exactly one item `in_progress` while work is
    /// under way, and mark an item `done` only once it is finished.
    #[expect(dead_code, reason = "the field only feeds the derived schema")]
    todos: Option<Vec<TodoItem>>,
}

/// Read or replace the session's task list.
#[derive(Debug)]
pub struct TodoListTool {
    todos: TodoList,
}

impl TodoListTool {
    /// The tool over `todos`, the session's list.
    #[must_use]
    pub fn new(todos: TodoList) -> Self {
        Self { todos }
    }
}

impl Tool for TodoListTool {
    fn name(&self) -> &'static str {
        "todo_list"
    }

    fn description(&self) -> &'static str {
        "Read or replace the todo list for this session. Call it with no \
         arguments to read the list; call it with `todos` to replace the \
         whole list. Each item is `{ \"title\": \"...\", \"status\": \
         \"pending\" | \"in_progress\" | \"done\" }`, optionally with \
         `id`, `owner` and `blockedBy` (ids of items that must finish \
         first), which are kept with the item. The rendered list \
         marks items `[ ]` pending, `[~]` in progress, `[x]` done. Send \
         an empty list to clear it. Worth using for a task that takes \
         several tool calls; not worth it for a single step."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<TodoListInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        // Read off the JSON rather than through `TodoListInput`, so a
        // malformed list errors instead of falling back to a read, and
        // an explicit null counts as absent. Models reach for the
        // shapes other CLIs' tools taught them, so a bare array and a
        // `todos` map nested one level deep also read as the list.
        let sent = if input.is_array() {
            Some(input)
        } else {
            match input.get("todos").filter(|value| !value.is_null()) {
                None => None,
                Some(list @ serde_json::Value::Array(_)) => Some(list.clone()),
                Some(serde_json::Value::Object(nested)) => {
                    match nested.get("todos").filter(|value| !value.is_null()) {
                        Some(list) => Some(list.clone()),
                        // An empty map carries no list at all, which
                        // can only mean "clear it".
                        None if nested.is_empty() => Some(serde_json::Value::Array(Vec::new())),
                        None => Some(serde_json::Value::Object(nested.clone())),
                    }
                }
                Some(other) => Some(other.clone()),
            }
        };
        let Some(sent) = sent else {
            return Ok(ToolOutput {
                is_error: false,
                text: render(&self.todos.read()),
                structured: None,
                terminate: false,
            });
        };
        let todos: Vec<TodoItem> = serde_json::from_value(sent)?;
        let stored = self.todos.replace(&todos)?;
        Ok(ToolOutput {
            is_error: false,
            text: if stored.is_empty() {
                "Todo list cleared.".to_owned()
            } else {
                format!("Todo list updated.\n{}", render(&stored))
            },
            structured: Some(serde_json::to_value(&stored)?),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use kage_core::CancelFlag;

    use super::*;

    fn run(todos: TodoList, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        TodoListTool::new(todos)
            .execute(input, &ToolContext::new(std::path::Path::new("."), &cancel))
    }

    fn item(title: &str, status: &str) -> serde_json::Value {
        serde_json::json!({ "title": title, "status": status })
    }

    #[test]
    fn a_new_list_reads_as_empty() {
        let out = run(TodoList::new(), serde_json::json!({})).unwrap();
        assert_eq!(out.text, "Todo list is empty.");
        assert!(!out.is_error);
    }

    #[test]
    fn no_argument_reads_and_a_list_replaces() {
        let todos = TodoList::new();
        let out = run(
            todos.clone(),
            serde_json::json!({"todos": [
                item("Read the code", "in_progress"), item("Write it", "pending")
            ]}),
        )
        .unwrap();
        assert!(out.text.starts_with("Todo list updated."), "{}", out.text);

        let read = run(todos, serde_json::json!({})).unwrap();
        assert_eq!(
            read.text,
            "Current todo list:\n  [~] Read the code\n  [ ] Write it"
        );
    }

    #[test]
    fn a_write_replaces_the_whole_list() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("a", "pending"), item("b", "pending")]}),
        )
        .unwrap();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("b", "done")]}),
        )
        .unwrap();
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Current todo list:\n  [x] b"
        );
    }

    #[test]
    fn a_doubly_nested_list_writes() {
        let todos = TodoList::new();
        let out = run(
            todos.clone(),
            serde_json::json!({"todos": {"todos": [item("a", "pending")]}}),
        )
        .unwrap();
        assert!(out.text.starts_with("Todo list updated."), "{}", out.text);
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Current todo list:\n  [ ] a"
        );
    }

    #[test]
    fn a_nested_empty_list_clears() {
        let todos = TodoList::new();
        run(todos.clone(), serde_json::json!([item("a", "pending")])).unwrap();
        let out = run(todos.clone(), serde_json::json!({"todos": {"todos": []}})).unwrap();
        assert_eq!(out.text, "Todo list cleared.");
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Todo list is empty."
        );
    }

    #[test]
    fn a_bare_array_writes() {
        let todos = TodoList::new();
        let out = run(todos.clone(), serde_json::json!([item("a", "done")])).unwrap();
        assert!(out.text.starts_with("Todo list updated."), "{}", out.text);
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Current todo list:\n  [x] a"
        );
    }

    #[test]
    fn an_empty_map_clears() {
        let todos = TodoList::new();
        run(todos.clone(), serde_json::json!([item("a", "pending")])).unwrap();
        let out = run(todos.clone(), serde_json::json!({"todos": {}})).unwrap();
        assert_eq!(out.text, "Todo list cleared.");
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Todo list is empty."
        );
    }

    #[test]
    fn a_map_without_the_list_errors() {
        let todos = TodoList::new();
        run(todos.clone(), serde_json::json!([item("a", "pending")])).unwrap();
        let err = run(todos.clone(), serde_json::json!({"todos": {"items": []}})).unwrap_err();
        assert!(err.to_string().contains("invalid type: map"), "{err}");
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Current todo list:\n  [ ] a"
        );
    }

    #[test]
    fn an_empty_list_clears_it() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("a", "pending")]}),
        )
        .unwrap();
        let out = run(todos.clone(), serde_json::json!({"todos": []})).unwrap();
        assert_eq!(out.text, "Todo list cleared.");
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Todo list is empty."
        );
    }

    #[test]
    fn an_explicit_null_reads_rather_than_clears() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("keep", "done")]}),
        )
        .unwrap();
        let out = run(todos.clone(), serde_json::json!({"todos": null})).unwrap();
        assert!(out.text.contains("[x] keep"), "{}", out.text);
    }

    #[test]
    fn statuses_round_trip_through_every_marker() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [
                item("p", "pending"), item("i", "in_progress"), item("d", "done")
            ]}),
        )
        .unwrap();
        let read = run(todos, serde_json::json!({})).unwrap();
        assert!(read.text.contains("[ ] p"), "{}", read.text);
        assert!(read.text.contains("[~] i"), "{}", read.text);
        assert!(read.text.contains("[x] d"), "{}", read.text);
    }

    #[test]
    fn a_rejected_write_leaves_the_previous_list_intact() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("keep", "done")]}),
        )
        .unwrap();
        let err = run(
            todos.clone(),
            serde_json::json!({"todos": [item("new", "pending"), item("  ", "pending")]}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Current todo list:\n  [x] keep"
        );
    }

    #[test]
    fn an_empty_title_is_refused() {
        let todos = TodoList::new();
        for title in ["", "   "] {
            let err = run(
                todos.clone(),
                serde_json::json!({"todos": [item(title, "pending")]}),
            )
            .unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        }
    }

    #[test]
    fn a_malformed_list_is_refused_rather_than_read() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("keep", "done")]}),
        )
        .unwrap();
        for bad in [
            serde_json::json!({"todos": [{"title": "a", "status": "blocked"}]}),
            serde_json::json!({"todos": [{"status": "done"}]}),
            serde_json::json!({"todos": "everything"}),
            serde_json::json!({"todos": [{"title": "a"}]}),
        ] {
            let err = run(todos.clone(), bad.clone()).unwrap_err();
            assert!(matches!(err, ToolError::Json(_)), "{bad} gave {err:?}");
        }
        assert_eq!(
            run(todos, serde_json::json!({})).unwrap().text,
            "Current todo list:\n  [x] keep"
        );
    }

    #[test]
    fn an_over_long_title_is_truncated_to_the_limit() {
        let todos = TodoList::new();
        let long = "x".repeat(MAX_TITLE + 50);
        run(
            todos.clone(),
            serde_json::json!({"todos": [item(&long, "pending")]}),
        )
        .unwrap();
        let read = run(todos, serde_json::json!({})).unwrap();
        assert!(read.text.contains(&"x".repeat(MAX_TITLE)), "{}", read.text);
        assert!(!read.text.contains(&"x".repeat(MAX_TITLE + 1)));
    }

    #[test]
    fn a_title_of_exactly_the_limit_is_kept() {
        let todos = TodoList::new();
        let exact = "y".repeat(MAX_TITLE);
        run(
            todos.clone(),
            serde_json::json!({"todos": [item(&exact, "pending")]}),
        )
        .unwrap();
        assert!(
            run(todos, serde_json::json!({}))
                .unwrap()
                .text
                .contains(&exact),
            "title was not kept whole"
        );
    }

    #[test]
    fn a_too_long_list_is_refused() {
        let todos: Vec<_> = (0..=MAX_ITEMS)
            .map(|i| item(&format!("t{i}"), "pending"))
            .collect();
        let err = run(TodoList::new(), serde_json::json!({ "todos": todos })).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn titles_are_trimmed() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("  padded  ", "pending")]}),
        )
        .unwrap();
        assert!(
            run(todos, serde_json::json!({}))
                .unwrap()
                .text
                .contains("[ ] padded")
        );
    }

    /// `schemars` puts the item behind a `$ref`, so the statuses live
    /// under `$defs`.
    #[test]
    fn the_schema_names_the_statuses_and_omitting_reads() {
        let s = TodoListTool::new(TodoList::new()).schema();
        assert!(s["properties"]["todos"]["items"]["$ref"].is_string(), "{s}");
        let required = s["required"].as_array().map_or(0, Vec::len);
        assert_eq!(required, 0, "todos must be optional: {s}");
        let description = s["properties"]["todos"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            description.contains("Omit this"),
            "the schema must say omitting reads: {description}"
        );
        let status = &s["$defs"]["TodoStatus"];
        let rendered = serde_json::to_string(status).unwrap();
        for name in ["pending", "in_progress", "done"] {
            assert!(rendered.contains(name), "{name} missing from {rendered}");
        }
        let item_required = s["$defs"]["TodoItem"]["required"]
            .as_array()
            .expect("required is an array");
        assert!(item_required.iter().any(|v| v == "title"));
        assert!(item_required.iter().any(|v| v == "status"));
    }

    #[test]
    fn the_tool_is_read_only_risk() {
        assert_eq!(TodoListTool::new(TodoList::new()).risk(), Risk::Read);
    }

    #[test]
    fn two_sessions_do_not_share_a_list() {
        let mine = TodoList::new();
        let theirs = TodoList::new();
        run(mine, serde_json::json!({"todos": [item("a", "pending")]})).unwrap();
        assert_eq!(
            run(theirs, serde_json::json!({})).unwrap().text,
            "Todo list is empty."
        );
    }

    #[test]
    fn a_write_returns_the_list_as_structured_output() {
        let todos = TodoList::new();
        let out = run(
            todos,
            serde_json::json!({"todos": [
                {"title": "Read", "status": "in_progress", "id": "1"},
                {"title": "Write", "status": "pending", "owner": "agent", "blockedBy": ["1"]},
                item("Ship", "done")
            ]}),
        )
        .unwrap();
        assert_eq!(
            out.structured,
            Some(serde_json::json!([
                {"title": "Read", "status": "in_progress", "id": "1"},
                {"title": "Write", "status": "pending", "owner": "agent", "blockedBy": ["1"]},
                {"title": "Ship", "status": "done"}
            ]))
        );
    }

    #[test]
    fn a_read_returns_no_structured_output() {
        let todos = TodoList::new();
        run(
            todos.clone(),
            serde_json::json!({"todos": [item("a", "pending")]}),
        )
        .unwrap();
        let out = run(todos, serde_json::json!({})).unwrap();
        assert_eq!(out.structured, None);
    }

    #[test]
    fn the_new_item_fields_are_optional_in_the_schema() {
        let s = TodoListTool::new(TodoList::new()).schema();
        let item = &s["$defs"]["TodoItem"];
        let required = item["required"].as_array().unwrap();
        assert_eq!(
            required.len(),
            2,
            "only title and status are required: {item}"
        );
        for field in ["id", "owner", "blockedBy"] {
            assert!(
                item["properties"].get(field).is_some(),
                "{field} missing from the schema"
            );
        }
    }
}
