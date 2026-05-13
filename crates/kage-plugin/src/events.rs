//! `kage.on(event, handler)` and host-driven event dispatch.
//!
//! Plugins call `kage.on("message_end", function(ev) ... end)` to subscribe
//! to a named event. The host then calls [`dispatch`] (or one of the typed
//! helpers added by future tasks) at the appropriate boundaries to fire
//! every registered handler in registration order.
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
//! * `tool_result` - a tool invocation produced an output
//! * `session_open` - the host opened a session writer
//! * `session_close` - the host closed a session writer
//! * `model_select` - the active model changed. Payload:
//!   `{ prev, next, source }` where `source` is one of `"set"`,
//!   `"cycle"`, or `"restore"`. Today only the `set` source fires
//!   (from `:model` / model picker); `cycle` and `restore` are
//!   reserved for upcoming features.
//! * `thinking_level_select` - reserved for the thinking-level UI
//!   (PP.C); same payload shape as `model_select`.
//! * `user_bash` - reserved for inline (`!cmd`) and background
//!   (`!!cmd`) bash from the input pane; not wired in v0.1.
//!
//! Session-op pre-hooks fire before the host runs a session action and
//! let a plugin veto or patch the target:
//! * `session_before_switch` - target is a session id or path
//! * `session_before_fork` - target is the entry id to fork at
//! * `session_before_tree` - target is the current session id (or empty)
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
//! * `should_stop_after_turn` (predicate): the host passes a turn summary;
//!   any handler returning `true` short-circuits the run. See
//!   [`dispatch_predicate`].

use mlua::{Function, Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;

/// Lua-registry key under which subscribed handlers are stored.
const HANDLERS_KEY: &str = "kage._handlers";

/// Install `kage.on` on the running Lua state. Idempotent: calling twice
/// rebinds the same handler table without losing previous subscriptions.
pub fn install_subscriptions(lua: &Lua) -> Result<(), PluginError> {
    if !has_handlers_table(lua)? {
        let table = lua.create_table()?;
        lua.set_named_registry_value(HANDLERS_KEY, table)?;
    }

    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "on",
        lua.create_function(|lua, (event, handler): (String, Function)| {
            let handlers: Table = lua.named_registry_value(HANDLERS_KEY)?;
            let list_v: Value = handlers.get(event.clone())?;
            let list = if let Value::Table(t) = list_v {
                t
            } else {
                let t = lua.create_table()?;
                handlers.set(event.clone(), t.clone())?;
                t
            };
            list.push(handler)?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn has_handlers_table(lua: &Lua) -> Result<bool, PluginError> {
    let v: Value = lua.named_registry_value(HANDLERS_KEY)?;
    Ok(matches!(v, Value::Table(_)))
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
    let Ok(handlers) = lua.named_registry_value::<Table>(HANDLERS_KEY) else {
        return Ok(());
    };
    let list: Value = handlers.get(event_name)?;
    let Value::Table(list) = list else {
        return Ok(());
    };
    let lua_payload = json_to_lua(lua, payload)?;
    for pair in list.clone().sequence_values::<Function>() {
        let func = pair?;
        if let Err(err) = func.call::<()>(lua_payload.clone()) {
            if let Ok(mut s) = sink.lock() {
                s.log(
                    LogLevel::Error,
                    &format!("plugin handler for '{event_name}' raised: {err}"),
                );
            }
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
    let Ok(handlers) = lua.named_registry_value::<Table>(HANDLERS_KEY) else {
        return Ok(payload);
    };
    let list: Value = handlers.get(event_name)?;
    let Value::Table(list) = list else {
        return Ok(payload);
    };
    let mut current = payload;
    for pair in list.clone().sequence_values::<Function>() {
        let func = pair?;
        let lua_payload = json_to_lua(lua, &current)?;
        match func.call::<Value>(lua_payload) {
            Ok(Value::Nil) => {}
            Ok(value) => match lua_to_json(value) {
                Ok(next) => current = next,
                Err(err) => {
                    if let Ok(mut s) = sink.lock() {
                        s.log(
                            LogLevel::Error,
                            &format!(
                                "plugin handler for '{event_name}' \
                                 returned a non-serializable value: {err}",
                            ),
                        );
                    }
                }
            },
            Err(err) => {
                if let Ok(mut s) = sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin handler for '{event_name}' raised: {err}"),
                    );
                }
            }
        }
    }
    Ok(current)
}

/// Outcome of a session-op pre-hook (`session_before_switch`,
/// `session_before_fork`, `session_before_tree`).
///
/// Mirrors the shape of `kage_loop::HookResult<String>` but stays in
/// `kage-plugin` to avoid pulling the loop crate into plugin code. The
/// host translates between the two when it needs to.
///
/// Lua handlers return one of:
/// * nothing / `nil` / non-table value → [`SessionOpDecision::Proceed`]
/// * `{ cancel = "reason" }` → [`SessionOpDecision::Cancel`]
/// * `{ patch = "new-target" }` → [`SessionOpDecision::Patch`]
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
    let Ok(handlers) = lua.named_registry_value::<Table>(HANDLERS_KEY) else {
        return Ok(SessionOpDecision::Proceed);
    };
    let list: Value = handlers.get(event_name)?;
    let Value::Table(list) = list else {
        return Ok(SessionOpDecision::Proceed);
    };
    let lua_payload = lua.create_string(target)?;
    for pair in list.clone().sequence_values::<Function>() {
        let func = pair?;
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
                if let Ok(mut s) = sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin handler for '{event_name}' raised: {err}"),
                    );
                }
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
    let Ok(handlers) = lua.named_registry_value::<Table>(HANDLERS_KEY) else {
        return Ok(false);
    };
    let list: Value = handlers.get(event_name)?;
    let Value::Table(list) = list else {
        return Ok(false);
    };
    let lua_payload = json_to_lua(lua, payload)?;
    for pair in list.clone().sequence_values::<Function>() {
        let func = pair?;
        match func.call::<Value>(lua_payload.clone()) {
            Ok(Value::Boolean(true)) => return Ok(true),
            Ok(_) => {}
            Err(err) => {
                if let Ok(mut s) = sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin handler for '{event_name}' raised: {err}"),
                    );
                }
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
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::api::{self, HostLog, SharedHostLog, default_host_log};

    fn fresh_lua_with_kage() -> Lua {
        let lua = Lua::new();
        api::install(&lua, default_host_log(), json!({})).unwrap();
        install_subscriptions(&lua).unwrap();
        lua
    }

    #[test]
    fn handlers_run_in_registration_order() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            log = {}
            kage.on('agent_end', function(ev) log[#log + 1] = 'a:' .. ev.foo end)
            kage.on('agent_end', function(ev) log[#log + 1] = 'b:' .. ev.foo end)
            ",
        )
        .exec()
        .unwrap();
        dispatch(
            &lua,
            "agent_end",
            &json!({"foo": "bar"}),
            &default_host_log(),
        )
        .unwrap();
        let log: Vec<String> = lua.globals().get::<Vec<String>>("log").unwrap();
        assert_eq!(log, vec!["a:bar".to_owned(), "b:bar".to_owned()]);
    }

    #[test]
    fn unsubscribed_event_is_silent() {
        let lua = fresh_lua_with_kage();
        dispatch(&lua, "nobody_listens", &json!({}), &default_host_log()).unwrap();
    }

    #[test]
    fn handler_count_reflects_subscriptions() {
        let lua = fresh_lua_with_kage();
        assert_eq!(handler_count(&lua, "tool_call"), 0);
        lua.load("kage.on('tool_call', function() end)")
            .exec()
            .unwrap();
        lua.load("kage.on('tool_call', function() end)")
            .exec()
            .unwrap();
        assert_eq!(handler_count(&lua, "tool_call"), 2);
    }

    #[test]
    fn handler_error_is_logged_but_does_not_stop_other_handlers() {
        #[derive(Default)]
        struct Recording {
            errors: Vec<String>,
        }
        impl HostLog for Recording {
            fn notify(&mut self, _: &str) {}
            fn log(&mut self, level: LogLevel, msg: &str) {
                if level == LogLevel::Error {
                    self.errors.push(msg.to_owned());
                }
            }
        }
        let rec = Arc::new(Mutex::new(Recording::default()));
        let sink: SharedHostLog = {
            #[derive(Clone)]
            struct Forwarder(Arc<Mutex<Recording>>);
            impl HostLog for Forwarder {
                fn notify(&mut self, _: &str) {}
                fn log(&mut self, level: LogLevel, msg: &str) {
                    self.0.lock().unwrap().log(level, msg);
                }
            }
            Arc::new(Mutex::new(
                Box::new(Forwarder(rec.clone())) as Box<dyn HostLog + Send>
            ))
        };

        let lua = Lua::new();
        api::install(&lua, sink.clone(), json!({})).unwrap();
        install_subscriptions(&lua).unwrap();

        lua.load(
            r"
            ran = 0
            kage.on('agent_end', function() error('boom') end)
            kage.on('agent_end', function() ran = ran + 1 end)
            ",
        )
        .exec()
        .unwrap();
        dispatch(&lua, "agent_end", &json!({}), &sink).unwrap();

        let ran: i64 = lua.globals().get("ran").unwrap();
        assert_eq!(ran, 1, "second handler ran despite first throwing");
        let errs = rec.lock().unwrap();
        assert_eq!(errs.errors.len(), 1);
        assert!(errs.errors[0].contains("agent_end"));
        assert!(errs.errors[0].contains("boom"));
    }

    #[test]
    fn turn_lifecycle_events_dispatch_by_name() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            seen = {}
            kage.on('before_agent_start', function(ev)
                seen[#seen + 1] = 'before:' .. ev.system_prompt .. '|' .. ev.first_user_message
            end)
            kage.on('turn_start', function(ev)
                seen[#seen + 1] = 'turn_start:' .. ev.index
            end)
            kage.on('turn_end', function(ev)
                seen[#seen + 1] = 'turn_end:' .. ev.index .. ':' .. tostring(ev.had_tool_calls)
            end)
            ",
        )
        .exec()
        .unwrap();

        dispatch(
            &lua,
            "before_agent_start",
            &json!({"system_prompt": "be helpful", "first_user_message": "hi"}),
            &default_host_log(),
        )
        .unwrap();
        dispatch(
            &lua,
            "turn_start",
            &json!({"index": 0}),
            &default_host_log(),
        )
        .unwrap();
        dispatch(
            &lua,
            "turn_end",
            &json!({"index": 0, "had_tool_calls": false}),
            &default_host_log(),
        )
        .unwrap();

        let seen: Vec<String> = lua.globals().get("seen").unwrap();
        assert_eq!(
            seen,
            vec![
                "before:be helpful|hi".to_owned(),
                "turn_start:0".to_owned(),
                "turn_end:0:false".to_owned(),
            ]
        );
    }

    #[test]
    fn streaming_events_dispatch_by_name() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            chunks = {}
            kage.on('message_start', function(ev) chunks[#chunks + 1] = 'start:' .. ev.id end)
            kage.on('message_update', function(ev)
                chunks[#chunks + 1] = 'delta:' .. ev.delta
            end)
            ",
        )
        .exec()
        .unwrap();

        dispatch(
            &lua,
            "message_start",
            &json!({"id": "m1"}),
            &default_host_log(),
        )
        .unwrap();
        dispatch(
            &lua,
            "message_update",
            &json!({"id": "m1", "delta": "Hel"}),
            &default_host_log(),
        )
        .unwrap();
        dispatch(
            &lua,
            "message_update",
            &json!({"id": "m1", "delta": "lo"}),
            &default_host_log(),
        )
        .unwrap();

        let chunks: Vec<String> = lua.globals().get("chunks").unwrap();
        assert_eq!(
            chunks,
            vec![
                "start:m1".to_owned(),
                "delta:Hel".to_owned(),
                "delta:lo".to_owned(),
            ]
        );
    }

    #[test]
    fn dispatch_transform_chains_handler_returns() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            kage.on('transform_context', function(payload)
                payload.tag = 'first'
                return payload
            end)
            kage.on('transform_context', function(payload)
                payload.tag = payload.tag .. ',second'
                return payload
            end)
            ",
        )
        .exec()
        .unwrap();
        let out = dispatch_transform(
            &lua,
            "transform_context",
            json!({"tag": "init"}),
            &default_host_log(),
        )
        .unwrap();
        assert_eq!(out["tag"], "first,second");
    }

    #[test]
    fn dispatch_transform_passthrough_when_handler_returns_nil() {
        let lua = fresh_lua_with_kage();
        lua.load(r"kage.on('transform_context', function(payload) return nil end)")
            .exec()
            .unwrap();
        let out = dispatch_transform(
            &lua,
            "transform_context",
            json!({"keep": true}),
            &default_host_log(),
        )
        .unwrap();
        assert_eq!(out["keep"], true);
    }

    #[test]
    fn dispatch_predicate_returns_true_when_any_handler_votes_stop() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            kage.on('should_stop_after_turn', function() return false end)
            kage.on('should_stop_after_turn', function() return true end)
            kage.on('should_stop_after_turn', function() error('never reached') end)
            ",
        )
        .exec()
        .unwrap();
        let stop = dispatch_predicate(
            &lua,
            "should_stop_after_turn",
            &json!({"index": 0}),
            &default_host_log(),
        )
        .unwrap();
        assert!(stop);
    }

    #[test]
    fn dispatch_predicate_returns_false_when_no_handlers() {
        let lua = fresh_lua_with_kage();
        let stop = dispatch_predicate(
            &lua,
            "should_stop_after_turn",
            &json!({}),
            &default_host_log(),
        )
        .unwrap();
        assert!(!stop);
    }

    #[test]
    fn dispatch_session_op_default_proceeds() {
        let lua = fresh_lua_with_kage();
        let decision = dispatch_session_op(
            &lua,
            "session_before_switch",
            "session-id-1",
            &default_host_log(),
        )
        .unwrap();
        assert_eq!(decision, SessionOpDecision::Proceed);
    }

    #[test]
    fn dispatch_session_op_picks_up_cancel() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            kage.on('session_before_switch', function(target)
                if target == 'locked' then return { cancel = 'busy' } end
            end)
            ",
        )
        .exec()
        .unwrap();
        let d = dispatch_session_op(&lua, "session_before_switch", "locked", &default_host_log())
            .unwrap();
        assert_eq!(
            d,
            SessionOpDecision::Cancel {
                reason: "busy".into()
            }
        );
        let d = dispatch_session_op(&lua, "session_before_switch", "other", &default_host_log())
            .unwrap();
        assert_eq!(d, SessionOpDecision::Proceed);
    }

    #[test]
    fn dispatch_session_op_picks_up_patch() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            kage.on('session_before_fork', function(at)
                return { patch = at .. '-renamed' }
            end)
            ",
        )
        .exec()
        .unwrap();
        let d =
            dispatch_session_op(&lua, "session_before_fork", "abc", &default_host_log()).unwrap();
        assert_eq!(d, SessionOpDecision::Patch("abc-renamed".into()));
    }

    #[test]
    fn dispatch_session_op_first_decision_wins() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            kage.on('session_before_fork', function(_) return nil end)
            kage.on('session_before_fork', function(_) return { cancel = 'first' } end)
            kage.on('session_before_fork', function(_) return { cancel = 'never' } end)
            ",
        )
        .exec()
        .unwrap();
        let d = dispatch_session_op(&lua, "session_before_fork", "x", &default_host_log()).unwrap();
        assert_eq!(
            d,
            SessionOpDecision::Cancel {
                reason: "first".into()
            }
        );
    }

    #[test]
    fn payload_arrives_as_lua_table() {
        let lua = fresh_lua_with_kage();
        lua.load(
            r"
            captured = nil
            kage.on('tool_call', function(ev) captured = ev end)
            ",
        )
        .exec()
        .unwrap();
        dispatch(
            &lua,
            "tool_call",
            &json!({"name": "echo", "args": {"k": 1}}),
            &default_host_log(),
        )
        .unwrap();
        let name: String = lua.load("return captured.name").eval().unwrap();
        let k: i64 = lua.load("return captured.args.k").eval().unwrap();
        assert_eq!(name, "echo");
        assert_eq!(k, 1);
    }
}
