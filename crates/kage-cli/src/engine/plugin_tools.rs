//! The plugin tools a session registered, so a reload can swap them.

use std::collections::HashMap;
use std::sync::Arc;

use kage_plugin::PluginRuntime;
use kage_tools::{Tool, ToolRegistry};

/// Names a session's plugin runtime registered and the tools those
/// registrations displaced.
#[derive(Default)]
pub(super) struct PluginTools {
    names: Vec<String>,
    displaced: HashMap<String, Arc<dyn Tool>>,
}

impl PluginTools {
    /// Remove the previous plugin tools from `tools`, restore what they
    /// displaced, then register what `rt` registers now. Returns the
    /// names of overrides that matched no existing tool.
    pub(super) fn apply(&mut self, tools: &mut ToolRegistry, rt: &PluginRuntime) -> Vec<String> {
        for name in self.names.drain(..) {
            tools.unregister(&name);
        }
        for (_, tool) in self.displaced.drain() {
            tools.register(tool);
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
    fn overrides_without_a_target_are_reported() {
        let mut tools = ToolRegistry::new();
        let rt = runtime(
            "kage.override_tool({ name = 'ghost', description = 'd', \
             schema = { type = 'object' }, execute = function() return 'x' end })",
        );
        assert_eq!(PluginTools::default().apply(&mut tools, &rt), ["ghost"]);
    }
}
