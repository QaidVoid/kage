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

use crate::api::{json_to_lua, lua_to_json};
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

/// A plugin-requested write to the session file that the host should
/// perform between turns. The host translates each variant into a
/// `SessionEntry` and appends it through its session writer.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingSessionOp {
    /// `kage.session.append_entry(kind, data)`: a `SessionEntry::Custom`
    /// with a plugin-defined kind and JSON payload.
    AppendCustom {
        /// Namespaced kind tag (e.g. `"my-plugin:bookmark"`).
        kind: String,
        /// Free-form JSON payload. Empty object when the caller
        /// passed `nil`.
        data: serde_json::Value,
    },
    /// `kage.session.set_label(anchor, label?)`: a `SessionEntry::Label`
    /// attached to the given anchor entry id. `text` is the empty
    /// string when the caller passed `nil` (meaning "clear the
    /// label"), since the on-disk format is append-only.
    SetLabel {
        /// Entry id of the entry being labeled.
        anchor: String,
        /// Label text; empty string when the plugin asked to clear.
        text: String,
    },
}

/// Shared queue of plugin-requested session writes. The host drains
/// this between turns and applies each entry to its session writer.
pub type SharedSessionOps = Arc<Mutex<Vec<PendingSessionOp>>>;

/// Construct an empty session-op queue.
#[must_use]
pub fn shared_session_ops() -> SharedSessionOps {
    Arc::new(Mutex::new(Vec::new()))
}

/// Install `kage.session.list()`, `kage.session.fork(at?)`,
/// `kage.session.append_entry(kind, data?)`, and
/// `kage.session.set_label(anchor, label?)` on the running Lua state.
pub fn install_sessions(
    lua: &Lua,
    list: SharedSessionList,
    fork: SharedForkRequest,
    ops: SharedSessionOps,
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

    let append_ops = Arc::clone(&ops);
    session.set(
        "append_entry",
        lua.create_function(move |_lua, (kind, data): (Value, Value)| {
            let kind = string_arg(&kind).ok_or_else(|| {
                mlua::Error::external("kage.session.append_entry: kind must be a string")
            })?;
            if kind.is_empty() {
                return Err(mlua::Error::external(
                    "kage.session.append_entry: kind must be a non-empty namespaced string",
                ));
            }
            let data_json = match data {
                Value::Nil => serde_json::Value::Object(serde_json::Map::new()),
                other => lua_to_json(other)?,
            };
            let mut q = append_ops
                .lock()
                .map_err(|_| mlua::Error::external("plugin session ops mutex poisoned"))?;
            q.push(PendingSessionOp::AppendCustom {
                kind,
                data: data_json,
            });
            Ok(mlua::Value::Nil)
        })?,
    )?;

    let label_ops = ops;
    session.set(
        "set_label",
        lua.create_function(move |_lua, (anchor, label): (Value, Value)| {
            let anchor = string_arg(&anchor).ok_or_else(|| {
                mlua::Error::external(
                    "kage.session.set_label: anchor must be the target entry id as a string",
                )
            })?;
            if anchor.is_empty() {
                return Err(mlua::Error::external(
                    "kage.session.set_label: anchor must be a non-empty entry id",
                ));
            }
            let text = match label {
                Value::Nil => String::new(),
                Value::String(s) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                other => {
                    return Err(mlua::Error::external(format!(
                        "kage.session.set_label: label must be a string or nil, got {other:?}"
                    )));
                }
            };
            let mut q = label_ops
                .lock()
                .map_err(|_| mlua::Error::external("plugin session ops mutex poisoned"))?;
            q.push(PendingSessionOp::SetLabel { anchor, text });
            Ok(mlua::Value::Nil)
        })?,
    )?;

    kage.set("session", session)?;
    Ok(())
}

fn string_arg(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => s.to_str().ok().map(|s| s.to_owned()),
        _ => None,
    }
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

    #[test]
    fn append_entry_with_kind_and_table_data_queues_a_custom_op() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.append_entry('plugin:tps', { note = 'first' })")
            .unwrap();
        let drained = rt.take_pending_session_ops();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            crate::sessions::PendingSessionOp::AppendCustom { kind, data } => {
                assert_eq!(kind, "plugin:tps");
                assert_eq!(data["note"], "first");
            }
            crate::sessions::PendingSessionOp::SetLabel { .. } => {
                panic!("expected AppendCustom, got SetLabel")
            }
        }
    }

    #[test]
    fn append_entry_with_nil_data_defaults_to_empty_object() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.append_entry('plugin:bookmark')")
            .unwrap();
        let drained = rt.take_pending_session_ops();
        match &drained[0] {
            crate::sessions::PendingSessionOp::AppendCustom { data, .. } => {
                assert_eq!(data, &serde_json::json!({}));
            }
            crate::sessions::PendingSessionOp::SetLabel { .. } => {
                panic!("expected AppendCustom, got SetLabel")
            }
        }
    }

    #[test]
    fn append_entry_rejects_empty_kind() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.eval("kage.session.append_entry('')").unwrap_err();
        assert!(err.to_string().contains("kind"));
    }

    #[test]
    fn append_entry_rejects_non_string_kind() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.eval("kage.session.append_entry(42)").unwrap_err();
        assert!(err.to_string().contains("string"));
    }

    #[test]
    fn set_label_with_anchor_and_text_queues_a_label_op() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.set_label('e01ABCD', 'milestone')")
            .unwrap();
        let drained = rt.take_pending_session_ops();
        match &drained[0] {
            crate::sessions::PendingSessionOp::SetLabel { anchor, text } => {
                assert_eq!(anchor, "e01ABCD");
                assert_eq!(text, "milestone");
            }
            crate::sessions::PendingSessionOp::AppendCustom { .. } => {
                panic!("expected SetLabel, got AppendCustom")
            }
        }
    }

    #[test]
    fn set_label_with_nil_text_clears() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.session.set_label('e01ABCD')").unwrap();
        let drained = rt.take_pending_session_ops();
        match &drained[0] {
            crate::sessions::PendingSessionOp::SetLabel { text, .. } => assert!(text.is_empty()),
            crate::sessions::PendingSessionOp::AppendCustom { .. } => {
                panic!("expected SetLabel, got AppendCustom")
            }
        }
    }

    #[test]
    fn set_label_rejects_empty_anchor() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.eval("kage.session.set_label('', 'oops')").unwrap_err();
        assert!(err.to_string().contains("anchor"));
    }

    #[test]
    fn take_pending_session_ops_drains_the_queue() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            "kage.session.append_entry('plugin:a'); \
             kage.session.set_label('e1', 'l1')",
        )
        .unwrap();
        let drained = rt.take_pending_session_ops();
        assert_eq!(drained.len(), 2);
        assert!(rt.take_pending_session_ops().is_empty());
    }
}
