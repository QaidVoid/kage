//! Host-driven event dispatch over the autocmd registry.
//!
//! Plugins subscribe with `kage.on("message_end", function(payload) ... end)`,
//! a stdlib alias over `kage.api.autocmd_create` (see [`crate::autocmd`]).
//! The call returns an `off` function that removes the subscription;
//! calling it again does nothing. The host then calls [`dispatch`] (or one
//! of the typed helpers) at the appropriate boundaries to fire every
//! matching autocmd in creation order. Each dispatch iterates a snapshot,
//! so a handler may call `off` for itself or another subscription while
//! it runs.
//!
//! Most host events fire at turn boundaries; the `message_*` events are the
//! exception and fire mid-stream so plugins can react to partial output.
//! [`crate::PluginRuntime`] keeps subscriber counts outside Lua and skips
//! the owner thread entirely for an event nobody subscribes to, so a
//! no-listener configuration pays no per-event cost. These names fire:
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
//! * `permission_mode_select` - the permission mode changed. Payload:
//!   `{ prev, next, source }`.
//! * `option_set` - an option changed. Payload: `{ name, old, new,
//!   source }` where `source` is `"lua"` or `"runtime"`. Matched
//!   against the option name.
//! * `color_scheme` - the theme's base groups changed, on a theme
//!   switch and once after every load. Matched against the theme name.
//! * `user` - fired only by `kage.api.autocmd_exec("user", { pattern,
//!   data })`, matched against `pattern`.
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

use mlua::{Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::autocmd;
use crate::error::PluginError;

/// Every event name `kage.on` and `kage.api.autocmd_create` recognise, with its dispatch kind and
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
        "permission_mode_select",
        "notification",
        "permission mode changed",
    ),
    ("option_set", "notification", "an option changed"),
    (
        "color_scheme",
        "notification",
        "the theme's base groups changed",
    ),
    (
        "user",
        "notification",
        "fired by autocmd_exec with a pattern",
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
    let matched = autocmd::match_key(event_name, payload);
    let targets = autocmd::targets(lua, event_name, matched)?;
    if targets.is_empty() {
        return Ok(());
    }
    let data = json_to_lua(lua, payload)?;
    autocmd::notify(lua, sink, event_name, matched, &targets, &data);
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
    let matched = autocmd::match_key(event_name, &payload).map(str::to_owned);
    let targets = autocmd::targets(lua, event_name, matched.as_deref())?;
    let mut current = payload;
    for target in targets {
        let data = json_to_lua(lua, &current)?;
        match target.call::<Value>(lua, event_name, matched.as_deref(), data) {
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
            Err(err) => target.log_error(sink, event_name, err),
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
    for target in autocmd::targets(lua, "resources_discover", None)? {
        match target.call::<Value>(lua, "resources_discover", None, Value::Nil) {
            Ok(Value::Table(table)) => {
                collect_paths(&table, "skills", &mut entries.skills);
                collect_paths(&table, "templates", &mut entries.templates);
                collect_paths(&table, "themes", &mut entries.themes);
            }
            Ok(_) => {}
            Err(err) => target.log_error(sink, "resources_discover", err),
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
    let targets = autocmd::targets(lua, event_name, None)?;
    if targets.is_empty() {
        return Ok(SessionOpDecision::Proceed);
    }
    let data = Value::String(lua.create_string(target)?);
    for target in targets {
        match target.call::<Value>(lua, event_name, None, data.clone()) {
            Ok(Value::Table(t)) => {
                if let Ok(reason) = t.get::<String>("cancel") {
                    return Ok(SessionOpDecision::Cancel { reason });
                }
                if let Ok(patch) = t.get::<String>("patch") {
                    return Ok(SessionOpDecision::Patch(patch));
                }
            }
            Ok(_) => {}
            Err(err) => target.log_error(sink, event_name, err),
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
    let matched = autocmd::match_key(event_name, payload);
    let targets = autocmd::targets(lua, event_name, matched)?;
    if targets.is_empty() {
        return Ok(false);
    }
    let data = json_to_lua(lua, payload)?;
    for target in targets {
        match target.call::<Value>(lua, event_name, matched, data.clone()) {
            Ok(Value::Boolean(true)) => return Ok(true),
            Ok(_) => {}
            Err(err) => target.log_error(sink, event_name, err),
        }
    }
    Ok(false)
}

/// Number of handlers currently subscribed to `event_name`. Reads the
/// autocmd counts kept outside Lua, so it never runs any Lua code.
#[must_use]
pub fn handler_count(lua: &Lua, event_name: &str) -> usize {
    autocmd::count(lua, event_name)
}

#[cfg(test)]
mod tests;
