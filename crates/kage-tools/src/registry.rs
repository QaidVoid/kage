//! Lookup of [`Tool`] implementations by name.

use std::collections::HashMap;
use std::sync::Arc;

use kage_core::ToolSpec;

use crate::Tool;

/// Registry of tools keyed by their stable name.
///
/// Cloning is cheap: the inner map uses `Arc<dyn Tool>` so all clones share
/// the same instances.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Replaces any prior entry with the same name.
    ///
    /// Returns `self` for chaining.
    #[must_use]
    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.register(tool);
        self
    }

    /// Register a tool. Replaces any prior entry with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Iterate registered tool names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Snapshot of all registered tools as [`ToolSpec`]s for the provider.
    #[must_use]
    pub fn list_for_provider(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_owned(),
                description: t.description().to_owned(),
                schema: t.schema(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kage_core::{CancelFlag, Risk, ToolOutput};

    use super::*;
    use crate::{Tool, ToolContext, ToolError};

    #[derive(Debug)]
    struct EchoTool {
        name: &'static str,
    }

    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "echo input back"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        }
        fn risk(&self) -> Risk {
            Risk::Read
        }
        fn execute(
            &self,
            input: serde_json::Value,
            _cx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            let text = input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            Ok(ToolOutput {
                is_error: false,
                text,
                structured: None,
            })
        }
    }

    fn echo(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(EchoTool { name })
    }

    #[test]
    fn empty_registry() {
        let r = ToolRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.get("anything").is_none());
        assert!(r.list_for_provider().is_empty());
    }

    #[test]
    fn registered_tool_resolves_by_name() {
        let r = ToolRegistry::new().with(echo("greet"));
        assert_eq!(r.len(), 1);
        let tool = r.get("greet").expect("present");
        assert_eq!(tool.name(), "greet");
    }

    #[test]
    fn register_replaces_existing_entry() {
        let mut r = ToolRegistry::new();
        r.register(echo("greet"));
        r.register(echo("greet"));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn list_for_provider_emits_all_tools() {
        let r = ToolRegistry::new()
            .with(echo("greet"))
            .with(echo("farewell"));
        let mut specs: Vec<String> = r.list_for_provider().into_iter().map(|s| s.name).collect();
        specs.sort();
        assert_eq!(specs, vec!["farewell", "greet"]);
    }

    #[test]
    fn names_iterates_registered_names() {
        let r = ToolRegistry::new()
            .with(echo("a"))
            .with(echo("b"))
            .with(echo("c"));
        let mut names: Vec<&str> = r.names().collect();
        names.sort_unstable();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn execute_runs_through_arc() {
        let r = ToolRegistry::new().with(echo("greet"));
        let workdir = PathBuf::from("/tmp");
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(&workdir, &cancel);
        let tool = r.get("greet").unwrap();
        let out = tool.execute(serde_json::json!({"text":"hi"}), &cx).unwrap();
        assert_eq!(out.text, "hi");
        assert!(!out.is_error);
    }
}
