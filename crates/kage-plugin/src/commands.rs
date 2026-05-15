//! `kage.register_command` and the slash-command registry.
//!
//! Plugins declare a slash-style command:
//! ```lua
//! kage.register_command({
//!     name = "summarize",
//!     description = "shrink the conversation so far",
//!     args = {
//!         { name = "target", kind = "text", optional = true, hint = "topic" },
//!     },
//!     handler = function(args, ctx, parsed)
//!         return "summary of: " .. (parsed.target or "everything")
//!     end,
//! })
//! ```
//!
//! The `args` field is optional; plugins that omit it register an
//! arg-less command. When present, each entry has a `kind` of
//! `"text"`, `"choice"`, `"path"`, `"session"`, or `"flag"`. The
//! schema is parsed into [`PluginArgSpec`] entries and surfaced both
//! to the host's completion engine and to the handler at invoke time
//! as a third `parsed_args` table argument.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// Owned argument schema for a plugin-registered slash command. Each
/// variant mirrors a [`kage_tui::ArgSpec`] kind so the host can
/// translate without losing information, but the strings are owned so
/// the schema can be built at plugin-load time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginArgSpec {
    /// Free text consuming the rest of the input line.
    Text {
        /// Parameter name passed to the handler in `parsed_args`.
        name: String,
        /// Whether the user may omit this argument.
        optional: bool,
        /// Placeholder shown in inline help (e.g. `"topic"`).
        hint: String,
    },
    /// One token from a fixed set of values.
    Choice {
        /// Parameter name.
        name: String,
        /// Accepted literal values.
        values: Vec<String>,
        /// Whether the user may omit this argument.
        optional: bool,
    },
    /// Single-token file path. Validation is the host's job; this
    /// variant just records the kind so completion can offer paths.
    Path {
        /// Parameter name.
        name: String,
        /// Whether the user may omit this argument.
        optional: bool,
    },
    /// Single-token session identifier; completion is host-driven.
    Session {
        /// Parameter name.
        name: String,
        /// Whether the user may omit this argument.
        optional: bool,
    },
    /// Boolean flag (`true`/`false`/`on`/`off`/`yes`/`no`/`1`/`0`).
    Flag {
        /// Parameter name.
        name: String,
    },
}

/// One slash command registered by a plugin.
pub struct LuaCommand {
    name: String,
    description: String,
    args: Vec<PluginArgSpec>,
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

    /// Argument schema declared by the plugin. Empty when the plugin
    /// omitted the `args` field (backwards-compatible argless form).
    #[must_use]
    pub fn args(&self) -> &[PluginArgSpec] {
        &self.args
    }

    /// Run the command. `args` is the raw text after the command name.
    /// `ctx` carries any host context the command should see; pass `null`
    /// for "no context" rather than constructing an empty object.
    ///
    /// The handler receives `(args, ctx, parsed_args)`. `parsed_args` is
    /// a table keyed by the names declared in the plugin's `args` schema;
    /// plugins that did not declare a schema see an empty table.
    /// Argument parse errors surface as a [`CommandOutput`] with
    /// `is_error = true` rather than failing the whole invoke, mirroring
    /// the existing Lua-error handling.
    pub fn invoke(
        &self,
        args: &str,
        ctx: &serde_json::Value,
    ) -> Result<CommandOutput, PluginError> {
        let lua = self.lua.lock().expect("plugin lua mutex poisoned");
        let handler: Function = lua.registry_value(&self.handler_key)?;
        let lua_ctx = json_to_lua(&lua, ctx)?;
        let parsed_args = match build_parsed_args(&lua, args, &self.args) {
            Ok(table) => table,
            Err(err) => {
                return Ok(CommandOutput {
                    text: format!("{}: {err}", self.name),
                    is_error: true,
                    structured: None,
                });
            }
        };
        match handler.call::<mlua::Value>((args.to_owned(), lua_ctx, parsed_args)) {
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

    /// Prepare a bridged invocation: parse args against the schema and
    /// fetch the handler, returning owned values that outlive the Lua
    /// lock (so the caller can hand them to
    /// [`crate::PluginRuntime::bridge_call`] without nesting locks).
    ///
    /// Unlike [`Self::invoke`], the handler runs inside a coroutine, so
    /// it may call blocking `kage.ui.*` APIs. Argument-parse failures
    /// come back as [`BridgePrep::ArgError`] (same text as `invoke`'s
    /// inline error), not a hard `Err`.
    pub fn prepare_bridge(
        &self,
        raw: &str,
        ctx: &serde_json::Value,
    ) -> Result<BridgePrep, PluginError> {
        let lua = self.lua.lock().expect("plugin lua mutex poisoned");
        let parsed = match build_parsed_args(&lua, raw, &self.args) {
            Ok(table) => table,
            Err(err) => {
                return Ok(BridgePrep::ArgError(CommandOutput {
                    text: format!("{}: {err}", self.name),
                    is_error: true,
                    structured: None,
                }));
            }
        };
        let parsed_json = lua_to_json(mlua::Value::Table(parsed))?;
        let handler: Function = lua.registry_value(&self.handler_key)?;
        Ok(BridgePrep::Ready(BridgeArgs {
            handler,
            args: vec![
                serde_json::Value::String(raw.to_owned()),
                ctx.clone(),
                parsed_json,
            ],
        }))
    }
}

/// Walk the plugin's arg schema and pull values out of the raw input
/// string, returning a Lua table keyed by arg name. Whitespace-only
/// tokens; quoted-string handling is the host's [`crate::cmdparse`]
/// concern, not duplicated here, so plugin commands that need complex
/// quoting should expose a single `text` (Rest) arg and parse
/// themselves.
fn build_parsed_args(lua: &Lua, raw: &str, schema: &[PluginArgSpec]) -> Result<Table, String> {
    let table = lua.create_table().map_err(|e| format!("lua table: {e}"))?;
    let mut remaining = raw.trim_start();
    for spec in schema {
        match spec {
            PluginArgSpec::Text { name, optional, .. } => {
                let value = remaining.trim_end();
                if value.is_empty() {
                    if !optional {
                        return Err(format!("missing required arg `{name}`"));
                    }
                } else {
                    table
                        .set(name.as_str(), value)
                        .map_err(|e| format!("set {name}: {e}"))?;
                }
                break;
            }
            PluginArgSpec::Choice {
                name,
                values,
                optional,
            } => {
                let (head, rest) = split_first_token(remaining);
                if head.is_empty() {
                    if !optional {
                        return Err(format!("missing required arg `{name}`"));
                    }
                    continue;
                }
                if !values.iter().any(|v| v == head) {
                    let allowed = values.join("|");
                    return Err(format!(
                        "arg `{name}`: expected one of {allowed}, got `{head}`"
                    ));
                }
                table
                    .set(name.as_str(), head)
                    .map_err(|e| format!("set {name}: {e}"))?;
                remaining = rest;
            }
            PluginArgSpec::Path { name, optional } | PluginArgSpec::Session { name, optional } => {
                let (head, rest) = split_first_token(remaining);
                if head.is_empty() {
                    if !optional {
                        return Err(format!("missing required arg `{name}`"));
                    }
                    continue;
                }
                table
                    .set(name.as_str(), head)
                    .map_err(|e| format!("set {name}: {e}"))?;
                remaining = rest;
            }
            PluginArgSpec::Flag { name } => {
                let (head, rest) = split_first_token(remaining);
                if head.is_empty() {
                    continue;
                }
                let b = match head {
                    "true" | "yes" | "on" | "1" => true,
                    "false" | "no" | "off" | "0" => false,
                    other => {
                        return Err(format!("arg `{name}`: expected boolean, got `{other}`"));
                    }
                };
                table
                    .set(name.as_str(), b)
                    .map_err(|e| format!("set {name}: {e}"))?;
                remaining = rest;
            }
        }
    }
    Ok(table)
}

fn split_first_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    }
}

fn parse_arg_schema(value: mlua::Value) -> Result<Vec<PluginArgSpec>, mlua::Error> {
    match value {
        mlua::Value::Nil => Ok(Vec::new()),
        mlua::Value::Table(args) => {
            let mut out = Vec::new();
            for entry in args.sequence_values::<Table>() {
                let entry = entry?;
                let name: String = entry.get("name")?;
                let kind: String = entry.get("kind")?;
                let optional: bool = entry.get("optional").unwrap_or(false);
                let spec = match kind.as_str() {
                    "text" => PluginArgSpec::Text {
                        name,
                        optional,
                        hint: entry
                            .get::<String>("hint")
                            .unwrap_or_else(|_| "value".into()),
                    },
                    "choice" => {
                        let choices: Vec<String> = entry.get("choices")?;
                        if choices.is_empty() {
                            return Err(mlua::Error::external(
                                "register_command: `choice` kind requires non-empty `choices`",
                            ));
                        }
                        PluginArgSpec::Choice {
                            name,
                            values: choices,
                            optional,
                        }
                    }
                    "path" => PluginArgSpec::Path { name, optional },
                    "session" => PluginArgSpec::Session { name, optional },
                    "flag" => PluginArgSpec::Flag { name },
                    other => {
                        return Err(mlua::Error::external(format!(
                            "register_command: unknown arg kind `{other}` (try text, choice, path, session, flag)"
                        )));
                    }
                };
                out.push(spec);
            }
            Ok(out)
        }
        other => Err(mlua::Error::external(format!(
            "register_command: `args` must be a table, got {other:?}"
        ))),
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

    /// Build a [`CommandOutput`] from a handler's return value already
    /// converted to JSON. Used by the bridged invocation path (a
    /// coroutine's final value comes back as `serde_json::Value`, not
    /// `mlua::Value`). Mirrors [`Self::from_value`]: a string is the
    /// text, an object reads `text` / `is_error` / `structured`, null
    /// is empty, anything else is stringified.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self {
                text: String::new(),
                is_error: false,
                structured: None,
            },
            serde_json::Value::String(s) => Self {
                text: s.clone(),
                is_error: false,
                structured: None,
            },
            serde_json::Value::Object(map) => Self {
                text: map
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                is_error: map
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                structured: map.get("structured").filter(|v| !v.is_null()).cloned(),
            },
            other => Self {
                text: other.to_string(),
                is_error: false,
                structured: None,
            },
        }
    }
}

/// Handler plus positional JSON arguments ready to run through the
/// coroutine bridge ([`crate::PluginRuntime::bridge_call`]). The args
/// are `[raw_arg_string, host_ctx, parsed_args]`, matching the
/// `(args, ctx, parsed_args)` shape [`LuaCommand::invoke`] passes.
pub struct BridgeArgs {
    /// The plugin's Lua handler, fetched from the registry.
    pub handler: Function,
    /// Positional arguments for the handler, in order.
    pub args: Vec<serde_json::Value>,
}

impl std::fmt::Debug for BridgeArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeArgs")
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

/// Outcome of [`LuaCommand::prepare_bridge`]: either everything needed
/// to run the handler, or a ready-to-render argument error (a missing
/// required arg is the plugin user's mistake, not a host failure).
#[derive(Debug)]
pub enum BridgePrep {
    /// Arguments parsed; run [`BridgeArgs`] through the bridge.
    Ready(BridgeArgs),
    /// Argument parsing failed; surface this output instead of running.
    ArgError(CommandOutput),
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
            let args_value: mlua::Value = spec.get("args").unwrap_or(mlua::Value::Nil);
            let args = parse_arg_schema(args_value)?;
            let key = lua.create_registry_value(handler)?;
            let cmd = LuaCommand {
                name,
                description,
                args,
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
    fn omitting_args_field_registers_argless_command() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'noargs',
                description = '',
                handler = function() return 'ok' end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        assert!(cmd.args().is_empty());
        let out = cmd.invoke("", &json!(null)).unwrap();
        assert_eq!(out.text, "ok");
    }

    #[test]
    fn args_schema_surfaces_via_getter() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'cmd',
                description = '',
                args = {
                    { name = 'target', kind = 'text', optional = true, hint = 'thing' },
                    { name = 'mode', kind = 'choice', choices = {'fast', 'slow'}, optional = false },
                },
                handler = function() return 'ok' end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let args = cmd.args();
        assert_eq!(args.len(), 2);
        match &args[0] {
            crate::commands::PluginArgSpec::Text {
                name,
                optional,
                hint,
            } => {
                assert_eq!(name, "target");
                assert!(*optional);
                assert_eq!(hint, "thing");
            }
            other => panic!("expected Text, got {other:?}"),
        }
        match &args[1] {
            crate::commands::PluginArgSpec::Choice {
                name,
                values,
                optional,
            } => {
                assert_eq!(name, "mode");
                assert_eq!(values, &vec!["fast".to_owned(), "slow".to_owned()]);
                assert!(!*optional);
            }
            other => panic!("expected Choice, got {other:?}"),
        }
    }

    #[test]
    fn parsed_args_table_reaches_handler() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'go',
                description = '',
                args = {
                    { name = 'mode', kind = 'choice', choices = {'up', 'down'}, optional = false },
                    { name = 'count', kind = 'text', optional = true, hint = 'n' },
                },
                handler = function(_args, _ctx, parsed)
                    return parsed.mode .. ':' .. (parsed.count or 'none')
                end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let out = cmd.invoke("up three times", &json!(null)).unwrap();
        assert_eq!(out.text, "up:three times");
    }

    #[test]
    fn invalid_choice_yields_command_output_error() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'g',
                description = '',
                args = {
                    { name = 'dir', kind = 'choice', choices = {'n', 's'}, optional = false },
                },
                handler = function() return 'ok' end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let out = cmd.invoke("east", &json!(null)).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("east"));
    }

    #[test]
    fn flag_arg_parses_yes_to_true() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_command({
                name = 'v',
                description = '',
                args = { { name = 'on', kind = 'flag' } },
                handler = function(_args, _ctx, parsed)
                    return tostring(parsed.on)
                end,
            })
            ",
        )
        .unwrap();
        let cmd = rt.registered_commands().pop().unwrap();
        let out = cmd.invoke("yes", &json!(null)).unwrap();
        assert_eq!(out.text, "true");
    }

    #[test]
    fn unknown_arg_kind_rejects_registration() {
        let rt = PluginRuntime::new().unwrap();
        let res = rt.eval(
            r"
            kage.register_command({
                name = 'bad',
                description = '',
                args = { { name = 'x', kind = 'mysterious' } },
                handler = function() return 'ok' end,
            })
            ",
        );
        assert!(res.is_err(), "expected registration error for unknown kind");
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
