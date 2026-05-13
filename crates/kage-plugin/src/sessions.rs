//! `kage.session.fork` / `kage.session.list`: plugin-facing session
//! ops.
//!
//! `list()` returns a snapshot the host refreshes periodically (each
//! redraw is more than enough). Each entry is a JSON object the host
//! defines: at minimum `{"id": "<short id>", "value": "<absolute
//! path>"}` so plugins can drive their own pickers or summary
//! dashboards.
//!
//! `fork(at?)` writes a pending request that the host drains between
//! turns. `at` is an entry-id prefix or `nil` for "fork at the most
//! recent entry". The function returns `nil` in v0.1: synchronous
//! return of the new session id needs a callback pattern, which is
//! deferred until PE.B's coroutine bridge.

use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};

use crate::api::json_to_lua;
use crate::error::PluginError;

/// Shared list of session entries the host keeps current.
pub type SharedSessionList = Arc<Mutex<Vec<serde_json::Value>>>;

/// Construct an empty session list.
#[must_use]
pub fn shared_session_list() -> SharedSessionList {
    Arc::new(Mutex::new(Vec::new()))
}

/// Pending fork request slot. `Some(at)` means "the plugin asked to
/// fork at entry `at`"; an empty string means "fork at the latest
/// entry".
pub type SharedForkRequest = Arc<Mutex<Option<String>>>;

/// Construct an empty fork-request slot.
#[must_use]
pub fn shared_fork_request() -> SharedForkRequest {
    Arc::new(Mutex::new(None))
}

/// Install `kage.session.list()` and `kage.session.fork(at?)` on the
/// running Lua state.
pub fn install_sessions(
    lua: &Lua,
    list: SharedSessionList,
    fork: SharedForkRequest,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let session = lua.create_table()?;

    let list_for_lua = list;
    session.set(
        "list",
        lua.create_function(move |lua, ()| {
            let guard = list_for_lua
                .lock()
                .map_err(|_| mlua::Error::external("plugin session list mutex poisoned"))?;
            let table = lua.create_table()?;
            for (idx, item) in guard.iter().enumerate() {
                table.set(idx + 1, json_to_lua(lua, item)?)?;
            }
            Ok(table)
        })?,
    )?;

    let fork_for_lua = fork;
    session.set(
        "fork",
        lua.create_function(move |_lua, at: Value| {
            let at_str = match at {
                Value::Nil => String::new(),
                Value::String(s) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                other => {
                    return Err(mlua::Error::external(format!(
                        "kage.session.fork: expected string or nil entry-id, got {other:?}"
                    )));
                }
            };
            let mut slot = fork_for_lua
                .lock()
                .map_err(|_| mlua::Error::external("plugin fork mutex poisoned"))?;
            *slot = Some(at_str);
            Ok(mlua::Value::Nil)
        })?,
    )?;

    kage.set("session", session)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn session_list_returns_empty_table_by_default() {
        let rt = PluginRuntime::new().unwrap();
        let len = rt
            .eval("return #kage.session.list()")
            .unwrap()
            .as_integer()
            .unwrap();
        assert_eq!(len, 0);
    }

    #[test]
    fn session_list_returns_host_supplied_entries() {
        let rt = PluginRuntime::new().unwrap();
        rt.set_session_list(vec![
            serde_json::json!({ "id": "abc", "value": "/tmp/a.jsonl" }),
            serde_json::json!({ "id": "def", "value": "/tmp/b.jsonl" }),
        ]);
        let v = rt.eval("return kage.session.list()[1].id").unwrap();
        let id = match v {
            mlua::Value::String(s) => s.to_str().unwrap().to_owned(),
            other => panic!("expected string, got {other:?}"),
        };
        assert_eq!(id, "abc");
    }

    #[test]
    fn session_fork_with_no_arg_records_empty_request() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.fork()").unwrap();
        assert_eq!(rt.take_fork_request(), Some(String::new()));
    }

    #[test]
    fn session_fork_with_entry_id_records_it() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.fork('e123abc')").unwrap();
        assert_eq!(rt.take_fork_request(), Some("e123abc".to_owned()));
    }

    #[test]
    fn take_fork_request_clears_the_slot() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.fork()").unwrap();
        let _ = rt.take_fork_request();
        assert!(rt.take_fork_request().is_none());
    }

    #[test]
    fn session_fork_with_non_string_arg_raises() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.eval("kage.session.fork(42)").unwrap_err();
        assert!(err.to_string().contains("string or nil"));
    }
}
