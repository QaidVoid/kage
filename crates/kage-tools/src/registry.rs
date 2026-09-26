//! Lookup of [`Tool`] implementations by name.

use std::collections::BTreeMap;
use std::sync::Arc;

use kage_core::ToolSpec;

use crate::Tool;

/// Registry of tools keyed by their stable name.
///
/// Cloning is cheap: the inner map uses `Arc<dyn Tool>` so all clones share
/// the same instances.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    /// Sorted by name, so `list_for_provider` emits a stable tool
    /// order: a stable prefix is what lets the provider's prompt cache
    /// survive a restart or reload.
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// Alternate names that resolve to a registered tool, used when a
    /// model reaches for a familiar name (`bash`) that the registry
    /// hosts under another (`shell`). Aliases never appear in
    /// listings.
    aliases: BTreeMap<String, String>,
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

    /// Remove a tool by name, returning it if it was registered.
    ///
    /// Used by the MCP manager to drop a server's stale adapters on a
    /// hot tool-list refresh so a tool the server no longer offers
    /// does not linger.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Register `from` as an alternate name for the tool registered
    /// under `to`. Lookup through [`Self::get`] follows aliases;
    /// listings and [`Self::names`] never show them.
    ///
    /// Returns `self` for chaining.
    #[must_use]
    pub fn alias(mut self, from: &str, to: &str) -> Self {
        self.aliases.insert(from.to_owned(), to.to_owned());
        self
    }

    /// Look up a tool by name, following alias chains. A missing
    /// target, or an alias cycle, resolves to `None`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        let mut current = name;
        // Alias chains are bounded in practice; the cap only exists to
        // turn a hand-written cycle into `None` instead of a hang.
        for _ in 0..8 {
            match self.tools.get(current) {
                Some(tool) => return Some(tool),
                None => {
                    current = self.aliases.get(current)?;
                }
            }
        }
        None
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
                terminate: false,
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
    fn alias_resolves_to_the_target_tool() {
        let r = ToolRegistry::new()
            .with(echo("shell"))
            .alias("bash", "shell");
        assert_eq!(r.get("bash").unwrap().name(), "shell");
        assert!(r.get("nope").is_none());
        // Aliases are invisible to listings.
        assert_eq!(r.names().collect::<Vec<_>>(), vec!["shell"]);
    }

    #[test]
    fn alias_chains_resolve_and_cycles_stop() {
        let r = ToolRegistry::new()
            .with(echo("c"))
            .alias("a", "b")
            .alias("b", "c");
        assert_eq!(r.get("a").unwrap().name(), "c");
        let r = ToolRegistry::new()
            .with(echo("x"))
            .alias("p", "q")
            .alias("q", "p");
        assert!(r.get("p").is_none());
    }

    #[test]
    fn alias_survives_a_renamed_target() {
        // A plugin override under the real name keeps the alias working.
        let mut r = ToolRegistry::new()
            .with(echo("shell"))
            .alias("bash", "shell");
        r.register(echo("shell2"));
        r.register(echo("shell"));
        assert_eq!(r.get("bash").unwrap().name(), "shell");
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
