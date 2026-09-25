//! `kage.on_terminal_input(handler) -> off`: raw key interception.
//!
//! Registers a handler the host calls for every key event *before*
//! any modal layer or built-in binding sees it. Returning a truthy
//! value consumes the event (the host stops dispatching it). The call
//! returns an `off` function; invoking it unregisters that handler
//! (idempotent).
//!
//! This is a deliberately sharp tool. Prefer
//! [`kage.keymap.set`](crate::keymap) for "run X on
//! chord Y": it is declarative, shows up in help, and cannot wedge
//! the UI. Reach for `on_terminal_input` only when you must observe
//! or swallow arbitrary keys (a modal vi layer, a key logger for a
//! tutorial). A handler that always returns truthy makes the editor
//! unusable, so the host still honors its hard `ctrl+q` quit hatch
//! ahead of these hooks.
//!
//! Handlers run on the Lua owner thread while the host waits up to
//! [`INPUT_DEADLINE`] for the verdict. If none arrives in time (the
//! owner is busy with a long Lua tool, or the hook itself is slow), the
//! key passes through unexamined and a one-time warning goes to the
//! host log. A handler error or
//! non-boolean return logs to the host sink and is treated as "not
//! consumed".
//!
//! The handler receives a key descriptor table:
//! ```lua
//! { code = "char"|"enter"|"esc"|"tab"|"backtab"|"backspace"|"up"|"down"
//!        |"left"|"right"|"home"|"end"|"pageup"|"pagedown"
//!        |"delete"|"insert"|"f1".."f12"|"other",
//!   char = "a",        -- only when code == "char"
//!   ctrl = false, alt = false, shift = false }
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kage_core::sync::lock;

use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua};
use crate::error::PluginError;
use crate::host::{self, LuaHost, WeakHost};
use crate::watchdog;

/// Longest the host waits for a hook's verdict before passing the key
/// through.
pub const INPUT_DEADLINE: Duration = Duration::from_millis(20);

/// Shared list of active terminal-input hooks, in registration order.
/// The host snapshots it per keystroke so an `off` (or a hook
/// registered at runtime) takes effect immediately.
pub type RegisteredTerminalHooks = Arc<Mutex<Vec<Arc<LuaTerminalHook>>>>;

/// Construct an empty hook list.
#[must_use]
pub fn registered_terminal_hooks() -> RegisteredTerminalHooks {
    Arc::new(Mutex::new(Vec::new()))
}

/// A raw terminal-input handler defined in Lua.
pub struct LuaTerminalHook {
    id: u64,
    host: LuaHost,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
    warned: AtomicBool,
}

impl std::fmt::Debug for LuaTerminalHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaTerminalHook")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl LuaTerminalHook {
    /// Stable identifier used by the returned `off` closure to remove
    /// exactly this hook.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Offer a key descriptor to the handler. Returns `true` only when
    /// the handler explicitly returned a truthy value, meaning the
    /// host should consume the event. A Lua error or a non-boolean
    /// return logs and yields `false` so a broken hook cannot silently
    /// eat every keystroke. No verdict within `deadline` (the host
    /// passes [`INPUT_DEADLINE`]) also yields `false`; the first such
    /// timeout logs a warning.
    #[must_use]
    pub fn handle(&self, event: &serde_json::Value, deadline: Duration) -> bool {
        let id = self.id;
        let sink = Arc::clone(&self.sink);
        let handler = Arc::clone(&self.handler_key);
        let event = event.clone();
        let verdict = self.host.call_within(deadline, move |lua| {
            run_hook(lua, id, &sink, &handler, &event)
        });
        verdict.unwrap_or_else(|| {
            if !self.warned.swap(true, Ordering::SeqCst) {
                let mut s = lock(&self.sink);
                s.log(
                    LogLevel::Warn,
                    &format!(
                        "plugin on_terminal_input #{id}: no verdict within {} ms \
                         (Lua thread busy or hook too slow); passing keys through",
                        deadline.as_millis()
                    ),
                );
            }
            false
        })
    }
}

fn run_hook(
    lua: &Lua,
    id: u64,
    sink: &SharedHostLog,
    handler_key: &RegistryKey,
    event: &serde_json::Value,
) -> bool {
    let fail = |e: &dyn std::fmt::Display| {
        let mut s = lock(sink);
        s.log(
            LogLevel::Error,
            &format!("plugin on_terminal_input #{id}: {e}"),
        );
        false
    };
    let func: Function = match lua.registry_value(handler_key) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let payload = match json_to_lua(lua, event) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    match watchdog::run(lua, watchdog::BUDGET, || func.call::<Value>(payload)) {
        Ok(Value::Boolean(b)) => b,
        Ok(_) => false,
        Err(e) => fail(&e),
    }
}

/// Install `kage.on_terminal_input` on the running Lua state. Each
/// call registers a handler and returns an `off` function that removes
/// exactly that handler when invoked (calling it twice is harmless).
pub(crate) fn install_on_terminal_input(
    lua: &Lua,
    host: WeakHost,
    sink: SharedHostLog,
    registered: &RegisteredTerminalHooks,
) -> Result<(), PluginError> {
    let registered = Arc::downgrade(registered);
    let kage: Table = lua.globals().get("kage")?;
    let next_id = Arc::new(AtomicU64::new(0));
    kage.set(
        "on_terminal_input",
        lua.create_function(move |lua, handler: Function| {
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let handler_key = lua.create_registry_value(handler)?;
            let hook = Arc::new(LuaTerminalHook {
                id,
                host: host.upgrade()?,
                sink: sink.clone(),
                handler_key: Arc::new(handler_key),
                warned: AtomicBool::new(false),
            });
            host::upgrade(&registered)?
                .lock()
                .map_err(|_| mlua::Error::external("plugin terminal-hook registry poisoned"))?
                .push(hook);
            let off_registry = registered.clone();
            let off = lua.create_function(move |_, ()| {
                if let Some(list) = off_registry.upgrade() {
                    lock(&list).retain(|h| h.id != id);
                }
                Ok(())
            })?;
            Ok(off)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;
    use crate::test_support::TIMEOUT;

    #[test]
    fn on_terminal_input_registers_a_hook() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.on_terminal_input(function(_ev) return false end)")
            .unwrap();
        assert_eq!(rt.registered_terminal_hooks().len(), 1);
    }

    #[test]
    fn handler_truthy_return_consumes() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.on_terminal_input(function(ev)
                return ev.code == 'char' and ev.char == 'x'
            end)
            ",
        )
        .unwrap();
        let hook = &rt.registered_terminal_hooks()[0];
        assert!(hook.handle(&serde_json::json!({ "code": "char", "char": "x" }), TIMEOUT));
        assert!(!hook.handle(&serde_json::json!({ "code": "char", "char": "y" }), TIMEOUT));
    }

    #[test]
    fn non_boolean_return_does_not_consume() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.on_terminal_input(function(_ev) return 'nope' end)")
            .unwrap();
        let hook = &rt.registered_terminal_hooks()[0];
        assert!(!hook.handle(&serde_json::json!({ "code": "enter" }), TIMEOUT));
    }

    #[test]
    fn handler_error_does_not_consume() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.on_terminal_input(function(_ev) error('boom') end)")
            .unwrap();
        let hook = &rt.registered_terminal_hooks()[0];
        assert!(!hook.handle(&serde_json::json!({ "code": "esc" }), TIMEOUT));
    }

    #[test]
    fn off_unregisters_the_hook() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            local off = kage.on_terminal_input(function() return true end)
            _G.__off = off
            ",
        )
        .unwrap();
        assert_eq!(rt.registered_terminal_hooks().len(), 1);
        rt.eval("_G.__off()").unwrap();
        assert_eq!(rt.registered_terminal_hooks().len(), 0);
        // Idempotent.
        rt.eval("_G.__off()").unwrap();
        assert_eq!(rt.registered_terminal_hooks().len(), 0);
    }

    #[test]
    fn off_removes_only_its_own_hook() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            _G.off1 = kage.on_terminal_input(function() return false end)
            kage.on_terminal_input(function() return false end)
            ",
        )
        .unwrap();
        assert_eq!(rt.registered_terminal_hooks().len(), 2);
        let first_id = rt.registered_terminal_hooks()[0].id();
        rt.eval("_G.off1()").unwrap();
        let remaining = rt.registered_terminal_hooks();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].id(), first_id);
    }
}
