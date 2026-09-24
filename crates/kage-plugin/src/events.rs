//! `kage.on(event, handler)` and host-driven event dispatch.
//!
//! Plugins call `kage.on("message_end", function(ev) ... end)` to subscribe
//! to a named event. The call returns an `off` function that removes the
//! subscription; calling it again does nothing. The host then calls
//! [`dispatch`] (or one of the typed helpers) at the appropriate
//! boundaries to fire every registered handler in registration order.
//! Each dispatch iterates a snapshot of the handler list, so a handler
//! may call `off` for itself or another subscription while it runs.
//!
//! Most host events fire at turn boundaries; the `message_*` events are the
//! exception and fire mid-stream so plugins can react to partial output. The
//! host skips JSON conversion when [`handler_count`] reports zero subscribers
//! for the streaming names, so a no-listener configuration pays no per-delta
//! cost. v0.1 ships these names:
//! * `before_agent_start` - fires once before the first provider call, with
//!   the system prompt and the first user message text in scope
//! * `agent_start` - the agent loop is about to call the provider for the first time
//! * `agent_end` - the loop has returned (success or error)
//! * `turn_start` - a new inner-loop iteration is about to call the provider
//! * `turn_end` - the provider stream for the current turn has closed
//! * `message_start` - the model began a new assistant message (mid-stream)
//! * `message_update` - the model emitted a text delta (mid-stream)
//! * `after_provider_response` - the provider stream closed; payload
//!   mirrors `message_end` (id + usage)
//! * `message_end` - the model finished one assistant turn
//! * `tool_call` - a tool invocation has begun
//! * `tool_update` - mid-execution progress payload from a running tool
//!   (`{id, content, structured?}`); fires only when subscribers exist
//! * `tool_result` - a tool invocation produced an output
//! * `resources_discover` - fires once at startup; handlers return a
//!   table `{ skills?, templates?, themes? }` of directory paths the
//!   host should add to its filesystem-discovered set. See
//!   [`dispatch_resources_discover`] and [`DiscoveryEntries`].
//! * `model_select` - the active model changed. Payload:
//!   `{ prev, next, source }` where `source` is one of `"set"`,
//!   `"cycle"`, or `"restore"`. Today only the `set` source fires
//!   (from `:model` / model picker); `cycle` and `restore` are
//!   reserved for upcoming features.
//! * `thinking_level_select` - the thinking level changed. Payload:
//!   `{ prev, next, source }` where `source` is `"cycle"` or
//!   `"settings"`.
//! * `user_bash` - an inline `!cmd` from the input pane completed.
//!   Payload: `{ cmd, exit_code }`; `exit_code` is `nil` when the
//!   command was killed by a signal.
//!
//! Session-op pre-hooks fire before the host runs a session action and
//! let a plugin veto or patch the target:
//! * `session_before_switch` - target is a session id or path
//! * `session_before_fork` - target is the entry id to fork at
//!
//! See [`dispatch_session_op`] and [`SessionOpDecision`].
//!
//! These events use special dispatch shapes:
//! * `transform_context` (transform): the host passes the current message
//!   history, each subscriber receives the chained payload, and may return
//!   a replacement list. The host replaces history with whatever the last
//!   handler returned. See [`dispatch_transform`].
//! * `before_provider_request` (transform): same chaining as
//!   `transform_context`, but the payload is the serialized
//!   `StreamRequest` about to go out to the provider. Plugins can inject
//!   a system header, strip tools, swap the model, etc.
//! * `compact_prepare` (transform): fired right before history
//!   compaction calls the summarizer model. The payload is
//!   `{ transcript, instruction, prompt, model, summarized, kept }`.
//!   A handler may return a table with any of `prompt` / `instruction`
//!   (rewrite what the summarizer receives) or `summary` (skip the
//!   model call entirely and use this text as the summary body).
//!   Returning `nil` passes through unchanged.
//! * `should_stop_after_turn` (predicate): the host passes a turn summary;
//!   any handler returning `true` short-circuits the run. See
//!   [`dispatch_predicate`].

use std::path::PathBuf;

use kage_core::sync::lock;

use mlua::{Function, Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;

/// Every event name `kage.on` recognises, with its dispatch kind and
/// a one-line summary. The single source of truth for runtime
/// introspection (`:events`) so the catalog cannot drift from what
/// the host actually fires. Kinds: `notification` (return ignored),
/// `transform` (chained, may replace the payload), `predicate` (any
/// `true` short-circuits), `veto` (first decision wins).
pub const KNOWN_EVENTS: &[(&str, &str, &str)] = &[
    (
        "before_agent_start",
        "notification",
        "before the first provider call",
    ),
    (
        "agent_start",
        "notification",
        "loop about to call the provider",
    ),
    ("agent_end", "notification", "loop returned (ok or error)"),
    ("turn_start", "notification", "a new inner turn is starting"),
    (
        "turn_end",
        "notification",
        "the turn's provider stream closed",
    ),
    (
        "message_start",
        "notification",
        "model began an assistant message",
    ),
    (
        "message_update",
        "notification",
        "model emitted a text delta",
    ),
    (
        "message_end",
        "notification",
        "model finished an assistant turn",
    ),
    (
        "after_provider_response",
        "notification",
        "provider stream closed (id + usage)",
    ),
    ("tool_call", "notification", "a tool invocation began"),
    ("tool_update", "notification", "mid-execution tool progress"),
    ("tool_result", "notification", "a tool produced output"),
    ("model_select", "notification", "active model changed"),
    (
        "thinking_level_select",
        "notification",
        "thinking level changed",
    ),
    (
        "user_bash",
        "notification",
        "inline `!cmd` from input completed",
    ),
    (
        "resources_discover",
        "notification",
        "return {skills?,templates?,themes?} dirs",
    ),
    (
        "transform_context",
        "transform",
        "rewrite message history before send",
    ),
    (
        "before_provider_request",
        "transform",
        "patch the outgoing StreamRequest",
    ),
    (
        "compact_prepare",
        "transform",
        "customize or skip compaction summary",
    ),
    (
        "should_stop_after_turn",
        "predicate",
        "true abandons the run after a turn",
    ),
    (
        "session_before_switch",
        "veto",
        "veto/patch a session switch",
    ),
    ("session_before_fork", "veto", "veto/patch a fork point"),
];

/// Lua-registry key under which subscribed handlers are stored.
const HANDLERS_KEY: &str = "kage._handlers";

/// Lua-registry key of the factory that builds the `off` closure
/// returned by `kage.on`.
const MAKE_OFF_KEY: &str = "kage._make_off";

/// Builds `off` in Lua so a handler that captures its own `off` forms
/// a cycle the Lua collector can reclaim.
const MAKE_OFF: &str = r"
local remove, rawequal = table.remove, rawequal
return function(list, handler)
    local done = false
    return function()
        if done then return end
        done = true
        for i = 1, #list do
            if rawequal(list[i], handler) then
                remove(list, i)
                return
            end
        end
    end
end
";

/// Install `kage.on` on the running Lua state. Idempotent: calling twice
/// rebinds the same handler table without losing previous subscriptions.
pub fn install_subscriptions(lua: &Lua) -> Result<(), PluginError> {
    if !has_handlers_table(lua)? {
        let table = lua.create_table()?;
        lua.set_named_registry_value(HANDLERS_KEY, table)?;
    }
    let make_off: Function = lua.load(MAKE_OFF).set_name("=kage.on").eval()?;
    lua.set_named_registry_value(MAKE_OFF_KEY, make_off)?;

    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "on",
        lua.create_function(|lua, (event, handler): (String, Function)| {
            let handlers: Table = lua.named_registry_value(HANDLERS_KEY)?;
            let list = if let Value::Table(t) = handlers.get::<Value>(event.as_str())? {
                t
            } else {
                let t = lua.create_table()?;
                handlers.set(event, t.clone())?;
                t
            };
            list.push(handler.clone())?;
            let make_off: Function = lua.named_registry_value(MAKE_OFF_KEY)?;
            make_off.call::<Function>((list, handler))
        })?,
    )?;
    Ok(())
}

fn has_handlers_table(lua: &Lua) -> Result<bool, PluginError> {
    let v: Value = lua.named_registry_value(HANDLERS_KEY)?;
    Ok(matches!(v, Value::Table(_)))
}

/// Snapshot of the handlers subscribed to `event_name`, in registration
/// order. Dispatch iterates this copy so `off` during a call is safe.
fn subscribers(lua: &Lua, event_name: &str) -> Result<Vec<Function>, PluginError> {
    let Ok(handlers) = lua.named_registry_value::<Table>(HANDLERS_KEY) else {
        return Ok(Vec::new());
    };
    let Value::Table(list) = handlers.get::<Value>(event_name)? else {
        return Ok(Vec::new());
    };
    Ok(list
        .sequence_values::<Function>()
        .collect::<mlua::Result<_>>()?)
}

/// Fire every handler subscribed to `event_name`, passing `payload`
/// converted to a Lua table.
///
/// A handler that raises an error logs through `sink` at
/// [`LogLevel::Error`] and is skipped; subsequent handlers still run. This
/// keeps one buggy plugin from silencing every other plugin watching the
/// same event.
pub fn dispatch(
    lua: &Lua,
    event_name: &str,
    payload: &serde_json::Value,
    sink: &SharedHostLog,
) -> Result<(), PluginError> {
    let funcs = subscribers(lua, event_name)?;
    if funcs.is_empty() {
        return Ok(());
    }
    let lua_payload = json_to_lua(lua, payload)?;
    for func in funcs {
        if let Err(err) = func.call::<()>(lua_payload.clone()) {
            let mut s = lock(sink);
            s.log(
                LogLevel::Error,
                &format!("plugin handler for '{event_name}' raised: {err}"),
            );
        }
    }
    Ok(())
}

/// Fire every handler for `event_name` and chain their return values:
/// each handler receives the payload produced by the previous one (or the
/// initial payload for the first handler) and may return a replacement.
/// A handler returning `nil` or no value is treated as "no change". The
/// final payload is returned to the caller.
///
/// Used by transform-style hooks (e.g. `transform_context`) that let
/// plugins mutate a host-supplied value before the host acts on it.
///
/// A handler that raises an error is logged and skipped, just like
/// [`dispatch`]: the previous payload survives.
pub fn dispatch_transform(
    lua: &Lua,
    event_name: &str,
    payload: serde_json::Value,
    sink: &SharedHostLog,
) -> Result<serde_json::Value, PluginError> {
    let mut current = payload;
    for func in subscribers(lua, event_name)? {
        let lua_payload = json_to_lua(lua, &current)?;
        match func.call::<Value>(lua_payload) {
            Ok(Value::Nil) => {}
            Ok(value) => match lua_to_json(value) {
                Ok(next) => current = next,
                Err(err) => {
                    let mut s = lock(sink);
                    s.log(
                        LogLevel::Error,
                        &format!(
                            "plugin handler for '{event_name}' \
                             returned a non-serializable value: {err}",
                        ),
                    );
                }
            },
            Err(err) => {
                let mut s = lock(sink);
                s.log(
                    LogLevel::Error,
                    &format!("plugin handler for '{event_name}' raised: {err}"),
                );
            }
        }
    }
    Ok(current)
}

/// Paths collected from `resources_discover` plugin handlers.
///
/// Each Lua handler returns a table with optional `skills`, `templates`,
/// and `themes` keys, each carrying a list of directory paths. The host
/// concatenates all returned paths across all handlers; the returned
/// fields here are the union.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryEntries {
    /// Directories the host should walk when loading SKILL.md files,
    /// in addition to its built-in user/project dirs.
    pub skills: Vec<PathBuf>,
    /// Directories the host should walk when loading prompt templates.
    pub templates: Vec<PathBuf>,
    /// Directories the host should walk when loading theme files.
    pub themes: Vec<PathBuf>,
}

/// Fire every `resources_discover` handler once and collect the returned
/// paths into a [`DiscoveryEntries`] aggregate.
///
/// Each handler is called with no arguments and is expected to return a
/// table containing optional `skills`, `templates`, `themes` keys whose
/// values are lists of directory paths. Anything else is treated as the
/// handler reporting "nothing extra to discover."
///
/// Handler errors are logged through `sink` and skipped.
pub fn dispatch_resources_discover(
    lua: &Lua,
    sink: &SharedHostLog,
) -> Result<DiscoveryEntries, PluginError> {
    let mut entries = DiscoveryEntries::default();
    for func in subscribers(lua, "resources_discover")? {
        match func.call::<Value>(()) {
            Ok(Value::Table(table)) => {
                collect_paths(&table, "skills", &mut entries.skills);
                collect_paths(&table, "templates", &mut entries.templates);
                collect_paths(&table, "themes", &mut entries.themes);
            }
            Ok(_) => {}
            Err(err) => {
                let mut s = lock(sink);
                s.log(
                    LogLevel::Error,
                    &format!("plugin handler for 'resources_discover' raised: {err}"),
                );
            }
        }
    }
    Ok(entries)
}

fn collect_paths(table: &Table, key: &str, out: &mut Vec<PathBuf>) {
    let Ok(list) = table.get::<Value>(key) else {
        return;
    };
    let Value::Table(list) = list else {
        return;
    };
    for item in list.clone().sequence_values::<String>().flatten() {
        out.push(PathBuf::from(item));
    }
}

/// Outcome of a session-op pre-hook (`session_before_switch` or
/// `session_before_fork`).
///
/// Mirrors the shape of `kage_loop::HookResult<String>` but stays in
/// `kage-plugin` to avoid pulling the loop crate into plugin code. The
/// host translates between the two when it needs to.
///
/// Lua handlers return one of:
/// * nothing / `nil` / non-table value -> [`SessionOpDecision::Proceed`]
/// * `{ cancel = "reason" }` -> [`SessionOpDecision::Cancel`]
/// * `{ patch = "new-target" }` -> [`SessionOpDecision::Patch`]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionOpDecision {
    /// Run the action against the original target.
    Proceed,
    /// Abandon the action. `reason` is human-facing.
    Cancel {
        /// Reason text the host can show in a toast or error block.
        reason: String,
    },
    /// Run the action against the patched target instead.
    Patch(String),
}

/// Fire each handler registered for a session-op event in registration
/// order. The first handler whose return value resolves to
/// [`SessionOpDecision::Cancel`] or [`SessionOpDecision::Patch`] short-
/// circuits the chain and is returned to the host.
///
/// Errors raised by a handler are logged through `sink` and treated as
/// [`SessionOpDecision::Proceed`]; later handlers still run.
pub fn dispatch_session_op(
    lua: &Lua,
    event_name: &str,
    target: &str,
    sink: &SharedHostLog,
) -> Result<SessionOpDecision, PluginError> {
    let funcs = subscribers(lua, event_name)?;
    if funcs.is_empty() {
        return Ok(SessionOpDecision::Proceed);
    }
    let lua_payload = lua.create_string(target)?;
    for func in funcs {
        match func.call::<Value>(Value::String(lua_payload.clone())) {
            Ok(Value::Table(t)) => {
                if let Ok(reason) = t.get::<String>("cancel") {
                    return Ok(SessionOpDecision::Cancel { reason });
                }
                if let Ok(patch) = t.get::<String>("patch") {
                    return Ok(SessionOpDecision::Patch(patch));
                }
            }
            Ok(_) => {}
            Err(err) => {
                let mut s = lock(sink);
                s.log(
                    LogLevel::Error,
                    &format!("plugin handler for '{event_name}' raised: {err}"),
                );
            }
        }
    }
    Ok(SessionOpDecision::Proceed)
}

/// Fire every handler for `event_name` and short-circuit on the first one
/// that returns truthy. Returns `true` when any handler vetoed.
///
/// Used by predicate-style hooks (e.g. `should_stop_after_turn`) where
/// any plugin can demand the action stop.
///
/// A handler that raises an error is logged and treated as `false`.
pub fn dispatch_predicate(
    lua: &Lua,
    event_name: &str,
    payload: &serde_json::Value,
    sink: &SharedHostLog,
) -> Result<bool, PluginError> {
    let funcs = subscribers(lua, event_name)?;
    if funcs.is_empty() {
        return Ok(false);
    }
    let lua_payload = json_to_lua(lua, payload)?;
    for func in funcs {
        match func.call::<Value>(lua_payload.clone()) {
            Ok(Value::Boolean(true)) => return Ok(true),
            Ok(_) => {}
            Err(err) => {
                let mut s = lock(sink);
                s.log(
                    LogLevel::Error,
                    &format!("plugin handler for '{event_name}' raised: {err}"),
                );
            }
        }
    }
    Ok(false)
}

/// Number of handlers currently subscribed to `event_name`. Used by the
/// host to skip JSON conversion for events nobody listens to.
#[must_use]
pub fn handler_count(lua: &Lua, event_name: &str) -> usize {
    let Ok(handlers) = lua.named_registry_value::<Table>(HANDLERS_KEY) else {
        return 0;
    };
    let Ok(Value::Table(list)) = handlers.get::<Value>(event_name) else {
        return 0;
    };
    list.raw_len()
}

#[cfg(test)]
mod tests;
