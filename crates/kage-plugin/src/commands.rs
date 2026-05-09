//! `kage.register_command` and the slash-command registry.
//!
//! Plugins declare a slash-style command:
//! ```lua
//! kage.register_command({
//!     name = "summarize",
//!     description = "shrink the conversation so far",
//!     handler = function(args, ctx) return "..." end,
//! })
//! ```
//! The runtime stores each registration. Phase 8 (TUI) and the CLI will
//! invoke a command by name, passing the trailing argument string and a
//! context table; the handler returns a string for the host to display.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// One slash command registered by a plugin.
pub struct LuaCommand {
    name: String,
    description: String,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
}

impl std::fmt::Debug for LuaCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaCommand")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl LuaCommand {
    /// Slash-command name, without leading `/`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Short description shown by `/help` and command palettes.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Run the command. `args` is the raw text after the command name.
    /// `ctx` carries any host context the command should see; pass `null`
    /// for "no context" rather than constructing an empty object.
    pub fn invoke(
        &self,
        args: &str,
        ctx: &serde_json::Value,
    ) -> Result<CommandOutput, PluginError> {
        let lua = self.lua.lock().expect("plugin lua mutex poisoned");
        let handler: Function = lua.registry_value(&self.handler_key)?;
        let lua_ctx = json_to_lua(&lua, ctx)?;
        match handler.call::<mlua::Value>((args.to_owned(), lua_ctx)) {
            Ok(v) => Ok(CommandOutput::from_value(v)),
            Err(err) => {
                if let Ok(mut s) = self.sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin command '{}' raised: {err}", self.name),
                    );
                }
                Ok(CommandOutput {
                    text: err.to_string(),
                    is_error: true,
                    structured: None,
                })
            }
        }
    }
}

/// Result of [`LuaCommand::invoke`].
#[derive(Clone, Debug, PartialEq)]
pub struct CommandOutput {
    /// Plain-text output the host should display.
    pub text: String,
    /// Whether the handler reported failure.
    pub is_error: bool,
    /// Optional structured payload if the handler returned a table with
    /// a `structured` field.
    pub structured: Option<serde_json::Value>,
}

impl CommandOutput {
    fn from_value(value: mlua::Value) -> Self {
        match value {
            mlua::Value::Nil => Self {
                text: String::new(),
                is_error: false,
                structured: None,
            },
            mlua::Value::String(s) => Self {
                text: s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                is_error: false,
                structured: None,
            },
            mlua::Value::Table(t) => {
                let text: Option<String> = t.get("text").ok();
                let is_error: bool = t.get("is_error").unwrap_or(false);
                let structured: Option<mlua::Value> = t.get("structured").ok();
                let structured_json = structured.and_then(|v| lua_to_json(v).ok());
                Self {
                    text: text.unwrap_or_default(),
                    is_error,
                    structured: structured_json,
                }
            }
            other => Self {
                text: lua_to_json(other)
                    .map(|j| j.to_string())
                    .unwrap_or_default(),
                is_error: false,
                structured: None,
            },
        }
    }
}

/// Shared slash-command registry. Cloned into the Lua callback so
/// registrations made during plugin load accumulate in one place.
pub type RegisteredCommands = Arc<Mutex<Vec<Arc<LuaCommand>>>>;

/// Construct an empty command registry.
#[must_use]
pub fn registered_commands() -> RegisteredCommands {
    Arc::new(Mutex::new(Vec::new()))
}

/// Install `kage.register_command` on the running Lua state.
pub fn install_register_command(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: RegisteredCommands,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_command",
        lua.create_function(move |lua, spec: Table| {
            let name: String = spec.get("name")?;
            let description: String = spec.get("description")?;
            let handler: Function = spec.get("handler")?;
            let key = lua.create_registry_value(handler)?;
            let cmd = LuaCommand {
                name,
                description,
                lua: shared_lua.clone(),
                sink: sink.clone(),
                handler_key: Arc::new(key),
            };
            registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin commands registry poisoned"))?
                .push(Arc::new(cmd));
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::PluginRuntime;

    #[test]
    fn register_command_appends_to_registry() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'echo',
                description = 'shouts the args back',
                handler = function(args, ctx) return 'echo:' .. args end,
            })
            ",
        )
        .unwrap();
        let commands = rt.registered_commands();
        assert_eq!(commands.len(), 1);
        let cmd = &commands[0];
        assert_eq!(cmd.name(), "echo");
        assert_eq!(cmd.description(), "shouts the args back");
    }

    #[test]
    fn invoke_passes_args_and_context() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'cwd',
                description = 'returns ctx.cwd plus args',
                handler = function(args, ctx) return ctx.cwd .. ' :: ' .. args end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let out = cmd.invoke("hello", &json!({"cwd": "/home/x"})).unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text, "/home/x :: hello");
    }

    #[test]
    fn invoke_handles_lua_error() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'broken',
                description = '',
                handler = function() error('nope') end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let out = cmd.invoke("", &json!(null)).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("nope"));
    }

    #[test]
    fn handler_returning_table_carries_structured() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 't',
                description = '',
                handler = function() return { text = 'ok', structured = { n = 1 } } end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let out = cmd.invoke("", &json!(null)).unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.structured.as_ref().unwrap()["n"], 1);
    }
}
