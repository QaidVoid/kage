//! Built-in tools shipped with kage.

pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod shell;
pub mod todo;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::sync::Arc;

pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use todo::{TodoList, TodoListTool};
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write::WriteTool;

use kage_core::config::{ShellConfig, WebSearchConfig};

use crate::ToolRegistry;

/// Deserialize an optional path argument, reading an empty string or the
/// literal `"null"` (which some models send instead of JSON null) as
/// absent.
pub(crate) fn optional_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.filter(|p| !p.is_empty() && p != "null"))
}

/// Construct a [`ToolRegistry`] with all built-in tools registered.
///
/// Includes: `read`, `write`, `edit`, `shell`, `grep`, `find`, `ls`,
/// `web_fetch`, `web_search` (on DuckDuckGo), `todo_list`. A caller that carries the todo list
/// across a session registers its own with [`ToolRegistry::with_todo_list`].
#[must_use]
pub fn builtin_registry() -> ToolRegistry {
    ToolRegistry::new()
        .with(Arc::new(ReadTool))
        .with(Arc::new(WriteTool))
        .with(Arc::new(EditTool))
        .with(Arc::new(ShellTool::default()))
        .with(Arc::new(GrepTool))
        .with(Arc::new(FindTool))
        .with(Arc::new(LsTool))
        .with(Arc::new(WebFetchTool))
        .with(Arc::new(WebSearchTool::default()))
        .with_todo_list(TodoList::new())
}

impl ToolRegistry {
    /// Register `todo_list` over `todos`, the session's task list.
    #[must_use]
    pub fn with_todo_list(mut self, todos: TodoList) -> Self {
        self.register(Arc::new(TodoListTool::new(todos)));
        self
    }

    /// Replace the registered `web_search` tool with one asking the
    /// engine `cfg` names. The default config leaves the registry
    /// unchanged.
    #[must_use]
    pub fn with_web_search(mut self, cfg: &WebSearchConfig) -> Self {
        if !cfg.is_default() {
            self.register(Arc::new(WebSearchTool::new(cfg.clone())));
        }
        self
    }

    /// Replace the registered `shell` tool with one running commands via
    /// `cfg.shell` (default `bash`, see [`ShellTool::with_shell`]) and
    /// stripping environment variables matched by `cfg.scrub_env` (see
    /// [`ShellTool::with_env_scrub`]). A fully default `cfg` leaves the
    /// registry unchanged.
    #[must_use]
    pub fn with_shell_config(mut self, cfg: &ShellConfig) -> Self {
        if cfg.program.is_some() || !cfg.scrub_env.is_empty() {
            let tool = ShellTool::default()
                .with_env_scrub(&cfg.scrub_env)
                .with_shell(cfg.program.as_deref());
            self.register(Arc::new(tool));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_all_ten_tools() {
        let r = builtin_registry();
        assert_eq!(r.len(), 10);
        let mut names: Vec<&str> = r.names().collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "edit",
                "find",
                "grep",
                "ls",
                "read",
                "shell",
                "todo_list",
                "web_fetch",
                "web_search",
                "write"
            ],
        );
    }

    /// A write has to be visible to the next read, since one tool does
    /// both. Two separate lists would read as the model losing its plan.
    #[test]
    fn a_todo_write_is_visible_to_the_next_read() {
        let r = builtin_registry();
        let cancel = kage_core::CancelFlag::new();
        let cx = crate::ToolContext::new(std::path::Path::new("."), &cancel);
        r.get("todo_list")
            .expect("todo_list")
            .execute(
                serde_json::json!({"todos": [{"title": "shared", "status": "done"}]}),
                &cx,
            )
            .unwrap();
        let out = r
            .get("todo_list")
            .expect("todo_list")
            .execute(serde_json::json!({}), &cx)
            .unwrap();
        assert!(out.text.contains("[x] shared"), "{}", out.text);
    }

    #[test]
    fn each_tool_has_a_schema_and_description() {
        let r = builtin_registry();
        for spec in r.list_for_provider() {
            assert!(
                !spec.description.is_empty(),
                "{} has no description",
                spec.name
            );
            assert_eq!(
                spec.schema["type"], "object",
                "{} has non-object schema",
                spec.name
            );
        }
    }

    /// `path` takes `filePath` as a deserialization alias only. The
    /// schema the model reads must keep a single canonical key, or it
    /// learns to send both and every call is rejected as a duplicate.
    #[test]
    fn filepath_alias_is_absent_from_the_advertised_schema() {
        let r = builtin_registry();
        for spec in r.list_for_provider() {
            let props = &spec.schema["properties"];
            assert!(
                !props
                    .as_object()
                    .is_some_and(|p| p.contains_key("filePath")),
                "{} advertises the filePath alias: {props}",
                spec.name
            );
        }
    }

    /// Every tool that reads a path must still name it `path` in the
    /// schema, since that is the key the alias falls back to.
    #[test]
    fn path_taking_tools_advertise_the_canonical_path_key() {
        let r = builtin_registry();
        for name in ["read", "write", "edit", "ls", "find", "grep"] {
            let spec = r
                .list_for_provider()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"));
            assert!(
                spec.schema["properties"]["path"].is_object(),
                "{name} has no path property: {}",
                spec.schema
            );
        }
    }
}
