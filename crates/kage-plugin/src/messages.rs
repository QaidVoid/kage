//! `kage.send_message`: programmatic message injection from Lua.
//!
//! A plugin handler calls `kage.send_message(text, opts?)` to enqueue
//! a synthetic message the host should drain between turns. The
//! runtime owns the queue; the host implements the actual delivery
//! semantics (which today is "treat each entry as a steering message
//! the loop will consume on its next pass").
//!
//! # Scope in 0.1
//!
//! Only `deliver_as = "user"` is wired end-to-end. Passing
//! `"assistant"` or `"system"` raises a Lua error rather than
//! silently doing the wrong thing; the entries ride a different code
//! path (history insertion / system note injection) that lands
//! together with the rest of PE.D's hooks.
//!
//! `trigger_turn` is captured but does not change in-loop behavior in
//! 0.1: when the loop is already running, the next steering poll
//! consumes the queue regardless. The flag exists so a future
//! "wake-up" path (TUI worker auto-submit while idle) can switch on
//! it without breaking the API.

use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};

use crate::error::PluginError;

/// Role under which a queued plugin message should be delivered. Today
/// only [`PendingRole::User`] is wired through to the loop; the other
/// variants exist so the type can grow without breaking callers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingRole {
    /// Inject as a synthetic user message the loop picks up via the
    /// steering buffer.
    User,
    /// Append as a synthetic assistant message. Reserved for a later
    /// task; rejected at the Lua boundary today.
    Assistant,
    /// Append as a system note. Reserved for a later task; rejected at
    /// the Lua boundary today.
    System,
}

/// One queued message. The host drains the queue between turns and
/// decides how to deliver each entry.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingMessage {
    /// Raw text the plugin asked the host to deliver.
    pub text: String,
    /// `true` when the plugin wants the host to immediately wake the
    /// loop (TUI: re-submit). When the loop is already running, this
    /// is informational.
    pub trigger_turn: bool,
    /// Target conversation role.
    pub deliver_as: PendingRole,
}

/// Shared queue of plugin-supplied messages, drained by the host.
pub type SharedPendingMessages = Arc<Mutex<Vec<PendingMessage>>>;

/// Construct an empty queue.
#[must_use]
pub fn shared_pending_messages() -> SharedPendingMessages {
    Arc::new(Mutex::new(Vec::new()))
}

/// Install `kage.send_message` on the running Lua state.
pub fn install_send_message(lua: &Lua, queue: SharedPendingMessages) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let queue_for_lua = queue;
    kage.set(
        "send_message",
        lua.create_function(move |_, (text, opts): (Value, Option<Table>)| {
            let text = string_arg(&text)
                .ok_or_else(|| mlua::Error::external("kage.send_message: text must be a string"))?;
            let (trigger_turn, deliver_as) = parse_opts(opts.as_ref())?;
            let mut queue_guard = queue_for_lua
                .lock()
                .map_err(|_| mlua::Error::external("plugin send_message mutex poisoned"))?;
            queue_guard.push(PendingMessage {
                text,
                trigger_turn,
                deliver_as,
            });
            Ok(mlua::Value::Nil)
        })?,
    )?;
    Ok(())
}

/// Pull a string out of an `mlua::Value`, returning `None` for any
/// other type. Used at the API boundary where strict typing matters.
fn string_arg(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => s.to_str().ok().map(|s| s.to_owned()),
        _ => None,
    }
}

fn parse_opts(opts: Option<&Table>) -> mlua::Result<(bool, PendingRole)> {
    let Some(table) = opts else {
        return Ok((true, PendingRole::User));
    };
    let trigger_turn = match table.get::<Value>("trigger_turn")? {
        Value::Nil => true,
        Value::Boolean(b) => b,
        other => {
            return Err(mlua::Error::external(format!(
                "kage.send_message: opts.trigger_turn must be a boolean, got {other:?}"
            )));
        }
    };
    let deliver_as = match table.get::<Value>("deliver_as")? {
        Value::Nil => PendingRole::User,
        Value::String(s) => match s.to_str()?.as_ref() {
            "user" => PendingRole::User,
            "assistant" => {
                return Err(mlua::Error::external(
                    "kage.send_message: deliver_as=\"assistant\" is not implemented yet \
                     (PE.D follow-up); use deliver_as=\"user\" for now",
                ));
            }
            "system" => {
                return Err(mlua::Error::external(
                    "kage.send_message: deliver_as=\"system\" is not implemented yet \
                     (PE.D follow-up); use deliver_as=\"user\" for now",
                ));
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "kage.send_message: deliver_as must be one of \"user\", \"assistant\", \
                     \"system\", got \"{other}\""
                )));
            }
        },
        other => {
            return Err(mlua::Error::external(format!(
                "kage.send_message: opts.deliver_as must be a string, got {other:?}"
            )));
        }
    };
    Ok((trigger_turn, deliver_as))
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    use super::*;

    #[test]
    fn send_message_with_text_only_queues_a_user_message_with_defaults() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.send_message('look at this')").unwrap();
        let drained = rt.take_pending_messages();
        assert_eq!(
            drained,
            vec![PendingMessage {
                text: "look at this".into(),
                trigger_turn: true,
                deliver_as: PendingRole::User,
            }]
        );
    }

    #[test]
    fn send_message_respects_trigger_turn_false() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.send_message('keep idle', { trigger_turn = false })")
            .unwrap();
        let drained = rt.take_pending_messages();
        assert_eq!(drained.len(), 1);
        assert!(!drained[0].trigger_turn);
    }

    #[test]
    fn take_pending_messages_drains_the_queue() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.send_message('one'); kage.send_message('two')")
            .unwrap();
        let drained = rt.take_pending_messages();
        assert_eq!(drained.len(), 2);
        // Subsequent drain returns nothing.
        assert!(rt.take_pending_messages().is_empty());
    }

    #[test]
    fn deliver_as_assistant_rejected_with_helpful_error() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval("kage.send_message('hi', { deliver_as = 'assistant' })")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deliver_as"));
        assert!(msg.contains("assistant"));
    }

    #[test]
    fn deliver_as_system_rejected_with_helpful_error() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval("kage.send_message('hi', { deliver_as = 'system' })")
            .unwrap_err();
        assert!(err.to_string().contains("system"));
    }

    #[test]
    fn unknown_deliver_as_rejected() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval("kage.send_message('hi', { deliver_as = 'robot' })")
            .unwrap_err();
        assert!(err.to_string().contains("robot"));
    }

    #[test]
    fn non_string_text_rejected() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.eval("kage.send_message(42)").unwrap_err();
        assert!(err.to_string().contains("string"));
    }

    #[test]
    fn non_boolean_trigger_turn_rejected() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval("kage.send_message('hi', { trigger_turn = 'sometimes' })")
            .unwrap_err();
        assert!(err.to_string().contains("trigger_turn"));
    }
}
