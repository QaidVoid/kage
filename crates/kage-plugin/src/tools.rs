//! `kage.register_tool` and the `Tool` adapter that backs into Lua.
//!
//! Plugins call
//! ```lua
//! kage.register_tool({
//!     name = "echo",
//!     description = "echoes its input",
//!     schema = { type = "object" },
//!     risk = "read",
//!     execute = function(input) return { text = input.msg } end,
//! })
//! ```
//! and the runtime stores a [`LuaTool`] that the host can hand to a
//! `ToolRegistry`. Tool execution serializes through the runtime's Lua
//! mutex.

use std::sync::{Arc, Mutex};

use kage_core::{Risk, ToolOutput};
use kage_tools::{Tool, ToolContext, ToolError};
use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// Shared collection of tools registered by plugins. Cloned into the Lua
/// callback so registrations made during `dofile` accumulate here.
pub type RegisteredTools = Arc<Mutex<Vec<Arc<dyn Tool>>>>;

/// Construct an empty registered-tools collection.
#[must_use]
pub fn registered_tools() -> RegisteredTools {
    Arc::new(Mutex::new(Vec::new()))
}

/// A `Tool` whose `execute` runs inside the plugin runtime's Lua state.
pub struct LuaTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    risk: Risk,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
}

impl std::fmt::Debug for LuaTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaTool")
            .field("name", &self.name)
            .field("risk", &self.risk)
            .finish_non_exhaustive()
    }
}

impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk(&self) -> Risk {
        self.risk
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let lua = self
            .lua
            .lock()
            .map_err(|_| ToolError::Other("plugin lua mutex poisoned".to_owned()))?;
        let func: Function = lua
            .registry_value(&self.handler_key)
            .map_err(|e| ToolError::Other(format!("plugin tool '{}': {e}", self.name)))?;
        let lua_input =
            json_to_lua(&lua, &input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        match func.call::<Value>(lua_input) {
            Ok(returned) => Ok(value_to_output(returned)),
            Err(err) => {
                if let Ok(mut s) = self.sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin tool '{}' raised: {err}", self.name),
                    );
                }
                Ok(ToolOutput {
                    is_error: true,
                    text: err.to_string(),
                    structured: None,
                })
            }
        }
    }
}

fn value_to_output(value: Value) -> ToolOutput {
    match value {
        Value::Nil => ToolOutput {
            is_error: false,
            text: String::new(),
            structured: None,
        },
        Value::String(s) => ToolOutput {
            is_error: false,
            text: s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
            structured: None,
        },
        Value::Boolean(b) => ToolOutput {
            is_error: false,
            text: b.to_string(),
            structured: None,
        },
        Value::Integer(i) => ToolOutput {
            is_error: false,
            text: i.to_string(),
            structured: None,
        },
        Value::Number(n) => ToolOutput {
            is_error: false,
            text: n.to_string(),
            structured: None,
        },
        Value::Table(t) => table_to_output(&t),
        other => {
            let json = lua_to_json(other).unwrap_or(serde_json::Value::Null);
            ToolOutput {
                is_error: false,
                text: json.to_string(),
                structured: Some(json),
            }
        }
    }
}

fn table_to_output(table: &Table) -> ToolOutput {
    let is_error: bool = table.get("is_error").unwrap_or(false);
    let text: Option<String> = table.get("text").ok();
    let structured: Option<Value> = table.get("structured").ok();
    let structured_json = structured.and_then(|v| lua_to_json(v).ok());
    if let Some(text) = text {
        return ToolOutput {
            is_error,
            text,
            structured: structured_json,
        };
    }
    // No `text` field: serialize the table itself as JSON for the model.
    let as_json = lua_to_json(Value::Table(table.clone())).unwrap_or(serde_json::Value::Null);
    ToolOutput {
        is_error,
        text: as_json.to_string(),
        structured: Some(as_json),
    }
}

/// Install `kage.register_tool` on the running Lua state. The closure
/// pushes each registered [`LuaTool`] into `registered`, which the host
/// later drains via [`PluginRuntime::registered_tools`].
pub fn install_register_tool(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: RegisteredTools,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_tool",
        lua.create_function(move |lua, spec: Table| {
            let name: String = spec.get("name")?;
            let description: String = spec.get("description")?;
            let risk_str: Option<String> = spec.get("risk").ok();
            let risk = parse_risk(risk_str.as_deref());
            let schema_value: Value = spec.get("schema").unwrap_or(Value::Nil);
            let schema = lua_to_json(schema_value).unwrap_or(serde_json::Value::Null);
            let execute: Function = spec.get("execute")?;
            let key = lua.create_registry_value(execute)?;
            let tool = LuaTool {
                name,
                description,
                schema,
                risk,
                lua: shared_lua.clone(),
                sink: sink.clone(),
                handler_key: Arc::new(key),
            };
            registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin tools registry poisoned"))?
                .push(Arc::new(tool) as Arc<dyn Tool>);
            Ok(())
        })?,
    )?;
    Ok(())
}

fn parse_risk(raw: Option<&str>) -> Risk {
    match raw {
        Some("write") => Risk::Write,
        Some("network") => Risk::Network,
        _ => Risk::Read,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kage_core::CancelFlag;
    use serde_json::json;

    use super::*;
    use crate::PluginRuntime;

    #[test]
    fn register_tool_appends_to_registry() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'echo',
                description = 'echo back',
                schema = { type = 'object' },
                risk = 'read',
                execute = function(input) return 'hi:' .. (input.msg or '?') end,
            })
            ",
        )
        .unwrap();
        let tools = rt.registered_tools();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.risk(), Risk::Read);
    }

    #[test]
    fn lua_tool_execute_returns_text() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'add',
                description = 'sum of a + b',
                schema = { type = 'object' },
                risk = 'read',
                execute = function(input) return tostring(input.a + input.b) end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let out = tool.execute(json!({"a": 2, "b": 3}), &cx).unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text, "5");
    }

    #[test]
    fn lua_tool_returns_table_with_structured_output() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'meta',
                description = 'returns table with text and structured',
                schema = {},
                execute = function() return { text = 'ok', structured = { count = 7 } } end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let out = tool.execute(json!({}), &cx).unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.structured.as_ref().unwrap()["count"], 7);
    }

    #[test]
    fn lua_tool_lua_error_becomes_is_error_output() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 'oops',
                description = 'always fails',
                schema = {},
                execute = function() error('boom') end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let out = tool.execute(json!({}), &cx).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("boom"));
    }

    #[test]
    fn lua_tool_respects_cancel() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_tool({
                name = 't',
                description = 'noop',
                schema = {},
                execute = function() return 'never' end,
            })
            ",
        )
        .unwrap();
        let tool = rt.registered_tools().pop().unwrap();
        let cancel = CancelFlag::new();
        cancel.cancel();
        let workdir = PathBuf::from("/tmp");
        let cx = ToolContext::new(&workdir, &cancel);
        let err = tool.execute(json!({}), &cx).unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[test]
    fn risk_parses_known_strings() {
        assert_eq!(parse_risk(Some("read")), Risk::Read);
        assert_eq!(parse_risk(Some("write")), Risk::Write);
        assert_eq!(parse_risk(Some("network")), Risk::Network);
        assert_eq!(parse_risk(Some("anything-else")), Risk::Read);
        assert_eq!(parse_risk(None), Risk::Read);
    }
}
