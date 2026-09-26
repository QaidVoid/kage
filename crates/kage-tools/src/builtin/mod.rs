//! Built-in tools shipped with kage.

pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod shell;
pub mod web_fetch;
pub mod write;

use std::sync::Arc;

pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

use kage_core::config::ShellConfig;

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
/// Includes: `read`, `write`, `edit`, `shell`, `grep`, `find`, `ls`, `web_fetch`.
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
}

impl ToolRegistry {
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
    fn registers_all_eight_tools() {
        let r = builtin_registry();
        assert_eq!(r.len(), 8);
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
