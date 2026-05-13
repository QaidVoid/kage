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
//! * `message_end` - the model finished one assistant turn
//! * `tool_call` - a tool invocation has begun
//! * `tool_result` - a tool invocation produced an output
//! * `session_open` - the host opened a session writer
//! * `session_close` - the host closed a session writer

use mlua::{Function, Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua};
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
