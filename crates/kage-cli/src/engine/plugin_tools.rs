//! The plugin tools a session registered, so a reload can swap them.

use std::collections::HashMap;
use std::sync::Arc;

use kage_plugin::PluginRuntime;
use kage_tools::{Tool, ToolRegistry};

/// Names a session's plugin runtime registered, the tools those
/// registrations displaced, and the renames it applied.
#[derive(Default)]
pub(super) struct PluginTools {
    names: Vec<String>,
    displaced: HashMap<String, Arc<dyn Tool>>,
    renames: Vec<(String, String)>,
}

impl PluginTools {
    /// Remove the previous plugin tools and renames from `tools`,
    /// restore what they displaced, then register what `rt` registers
    /// now and re-apply its renames. Returns the names of overrides
    /// that matched no existing tool.
    pub(super) fn apply(&mut self, tools: &mut ToolRegistry, rt: &PluginRuntime) -> Vec<String> {
        for name in self.names.drain(..) {
            tools.unregister(&name);
        }
        for (_, tool) in self.displaced.drain() {
            tools.register(tool);
        }
        for (from, _) in self.renames.drain(..) {
            tools.remove_rename(&from);
        }
        for tool in rt.registered_tools() {
            self.add(tools, tool);
        }
        let mut unmatched = Vec::new();
        for tool in rt.registered_tool_overrides() {
            if tools.get(tool.name()).is_none() {
                unmatched.push(tool.name().to_owned());
            }
            self.add(tools, tool);
        }
        self.renames = rt.registered_renames();
        for (from, to) in &self.renames {
            tools.rename_in_place(from, to);
        }
        unmatched
    }

    fn add(&mut self, tools: &mut ToolRegistry, tool: Arc<dyn Tool>) {
        let name = tool.name().to_owned();
        if !self.names.contains(&name)
            && let Some(existing) = tools.get(&name)
        {
            self.displaced.insert(name.clone(), Arc::clone(existing));
        }
        tools.register(tool);
        self.names.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(lua: &str) -> PluginRuntime {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(lua).unwrap();
        rt
    }

    fn tool(name: &str) -> String {
        format!(
            "kage.register_tool({{ name = '{name}', description = 'd', \
             schema = {{ type = 'object' }}, execute = function() return 'x' end }})"
        )
    }

    #[test]
    fn reapplying_swaps_tools_and_restores_overridden_builtins() {
        let mut tools = kage_tools::builtin_registry();
        let builtin_read = Arc::as_ptr(tools.get("read").unwrap()).cast::<()>();
        let mut plugin_tools = PluginTools::default();

        let first = runtime(&format!(
            "{}\nkage.override_tool({{ name = 'read', description = 'd', \
             schema = {{ type = 'object' }}, execute = function() return 'x' end }})",
            tool("old_tool")
        ));
        assert!(plugin_tools.apply(&mut tools, &first).is_empty());
        assert!(tools.get("old_tool").is_some());
        assert_ne!(
            Arc::as_ptr(tools.get("read").unwrap()).cast::<()>(),
            builtin_read
        );

        let second = runtime(&tool("new_tool"));
        plugin_tools.apply(&mut tools, &second);
        assert!(tools.get("old_tool").is_none());
        assert!(tools.get("new_tool").is_some());
        assert_eq!(
            Arc::as_ptr(tools.get("read").unwrap()).cast::<()>(),
            builtin_read
        );
    }

    #[test]
    fn reapplying_swaps_renames_and_reverts_dropped_ones() {
        let mut tools = kage_tools::builtin_registry();
        let mut plugin_tools = PluginTools::default();

        let first = runtime(
            "kage.rename_tool({ from = 'shell', to = 'run_command' })\n\
             kage.rename_tool({ from = 'web_fetch', to = 'browse' })",
        );
        assert!(plugin_tools.apply(&mut tools, &first).is_empty());
        let names: Vec<String> = tools
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"run_command".to_owned()), "{names:?}");
        assert!(names.contains(&"browse".to_owned()), "{names:?}");
        assert!(!names.contains(&"shell".to_owned()), "{names:?}");

        // A reload without the shell rename reverts it, keeping the rest.
        let second = runtime("kage.rename_tool({ from = 'web_fetch', to = 'browse' })");
        assert!(plugin_tools.apply(&mut tools, &second).is_empty());
        let names: Vec<String> = tools
            .list_for_provider()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"shell".to_owned()), "{names:?}");
        assert!(names.contains(&"browse".to_owned()), "{names:?}");
        assert!(!names.contains(&"run_command".to_owned()), "{names:?}");
    }

    #[test]
    fn a_rename_of_an_unknown_tool_changes_nothing() {
        let mut tools = kage_tools::builtin_registry();
        let rt = runtime("kage.rename_tool({ from = 'ghost', to = 'renamed' })");
        assert!(PluginTools::default().apply(&mut tools, &rt).is_empty());
        assert!(tools.get("renamed").is_none());
    }

    #[test]
    fn overrides_without_a_target_are_reported() {
        let mut tools = ToolRegistry::new();
        let rt = runtime(
            "kage.override_tool({ name = 'ghost', description = 'd', \
             schema = { type = 'object' }, execute = function() return 'x' end })",
        );
        assert_eq!(PluginTools::default().apply(&mut tools, &rt), ["ghost"]);
    }
}
