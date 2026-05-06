//! Built-in tools shipped with kage.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod web_fetch;
pub mod write;

use std::sync::Arc;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

use crate::ToolRegistry;

/// Construct a [`ToolRegistry`] with all built-in tools registered.
///
/// Includes: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, `web_fetch`.
#[must_use]
pub fn builtin_registry() -> ToolRegistry {
    ToolRegistry::new()
        .with(Arc::new(ReadTool))
        .with(Arc::new(WriteTool))
        .with(Arc::new(EditTool))
        .with(Arc::new(BashTool))
        .with(Arc::new(GrepTool))
        .with(Arc::new(FindTool))
        .with(Arc::new(LsTool))
        .with(Arc::new(WebFetchTool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_all_eight_tools() {
        let r = builtin_registry();
        assert_eq!(r.len(), 8);
        let mut names: Vec<&str> = r.names().collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "bash",
                "edit",
                "find",
                "grep",
                "ls",
                "read",
                "web_fetch",
                "write"
            ],
        );
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
}
