//! The `session_write` capability: `kage.session.entries`,
//! `kage.session.switch`, and `kage.session.fork_to`.
//!
//! All three are attached only onto the `kage` proxy of a plugin that
//! was granted `session_write` (see [`crate::capabilities`]); an
//! ungranted plugin's `kage.session` falls through to the shared base
//! table and never sees them. The shadow keeps the base
//! `list`/`fork`/`append_entry`/`set_label` working via an `__index`
//! to the shared table.
//!
//! The split mirrors the Pi coding agent: base `fork` *branches and
//! stays* (Pi `/fork`); the gated `switch` *moves the live session to
//! an existing one* (Pi `/tree`); `fork_to` is their composition -
//! *branch and go there* - which is the rewind move. A rewind plugin
//! reads [`entries`](kage.session.entries) to choose a point, then
//! `fork_to`s it. The plugin layer does not own the session file, so
//! these only *request*: the host drains the queued intent between
//! turns, consulting the `session_before_switch` veto, and performs
//! the reseat. `fork_to` reuses the existing base-`fork` plumbing for
//! the branch half, then asks the host to land on the new fork.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, Table, Value};

use crate::api::json_to_lua;
use crate::capabilities::{Capability, CapabilityRegistry};

/// Host-maintained snapshot of the current session's entries, each a
/// JSON object the host defines (at least `{ id, kind, ts }`, plus
/// `role` for messages). `kage.session.entries()` returns a copy.
pub(crate) type SharedSessionEntries = Arc<Mutex<Vec<serde_json::Value>>>;

/// Construct an empty session-entries snapshot.
#[must_use]
pub(crate) fn shared_session_entries() -> SharedSessionEntries {
    Arc::new(Mutex::new(Vec::new()))
}

/// What a pending `session_write` reseat should land on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchTarget {
    /// Reseat onto an existing session id or path (from
    /// `kage.session.list()`), via `kage.session.switch(target)`.
    Session(String),
    /// Reseat onto the fork that the just-queued `fork_to` creates;
    /// the host resolves the new session after performing the fork.
    PendingFork,
}

/// Pending reseat request. Drained by the host between turns; `None`
/// means nothing requested.
pub(crate) type SharedSwitchRequest = Arc<Mutex<Option<SwitchTarget>>>;

/// Construct an empty switch-request slot.
#[must_use]
pub(crate) fn shared_switch_request() -> SharedSwitchRequest {
    Arc::new(Mutex::new(None))
}

/// Register the `session_write` installer into `registry`.
///
/// The installer runs (via `request_capabilities`) against a granted
/// plugin's `kage` proxy and shadows its `session` table with one
/// that adds `entries`, `switch`, and `fork_to` while delegating
/// everything else to the shared base `session` table.
pub(crate) fn register(
    registry: &CapabilityRegistry,
    entries: SharedSessionEntries,
    switch: SharedSwitchRequest,
) {
    let mut reg = registry.lock().expect("capability registry mutex poisoned");
    reg.insert(
        Capability::SessionWrite,
        Box::new(move |lua: &Lua, pkage: &Table| {
            let kage: Table = lua.globals().get("kage")?;
            let base_session: Table = kage.get("session")?;
            let psession = lua.create_table()?;
            let mt = lua.create_table()?;
            mt.set("__index", base_session.clone())?;
            psession.set_metatable(Some(mt))?;

            let snapshot = Arc::clone(&entries);
            psession.set(
                "entries",
                lua.create_function(move |lua, ()| {
                    let guard = snapshot
                        .lock()
                        .map_err(|_| mlua::Error::external("session entries mutex poisoned"))?;
                    let arr = lua.create_table()?;
                    for (idx, item) in guard.iter().enumerate() {
                        arr.set(idx + 1, json_to_lua(lua, item)?)?;
                    }
                    Ok(arr)
                })?,
            )?;

            let switch_slot = Arc::clone(&switch);
            psession.set(
                "switch",
                lua.create_function(move |_, target: Value| {
                    let Value::String(target) = target else {
                        return Err(mlua::Error::external(
                            "kage.session.switch: target must be a session id or path string",
                        ));
                    };
                    let target = target.to_str()?.to_owned();
                    if target.is_empty() {
                        return Err(mlua::Error::external(
                            "kage.session.switch: target must be non-empty",
                        ));
                    }
                    let mut slot = switch_slot
                        .lock()
                        .map_err(|_| mlua::Error::external("switch request mutex poisoned"))?;
                    *slot = Some(SwitchTarget::Session(target));
                    Ok(())
                })?,
            )?;

            let switch_slot = Arc::clone(&switch);
            let base_fork: Function = base_session.get("fork")?;
            psession.set(
                "fork_to",
                lua.create_function(move |_, at: Value| {
                    // Reuse the existing base-`fork` plumbing for the
                    // branch half so there is one fork code path...
                    base_fork.call::<()>(at)?;
                    // ...then ask the host to land on the new fork.
                    let mut slot = switch_slot
                        .lock()
                        .map_err(|_| mlua::Error::external("switch request mutex poisoned"))?;
                    *slot = Some(SwitchTarget::PendingFork);
                    Ok(())
                })?,
            )?;

            pkage.set("session", psession)?;
            Ok(())
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::SwitchTarget;
    use crate::PluginRuntime;

    fn rt_with_session_write() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["session_write".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    #[test]
    fn entries_reads_host_snapshot_for_granted_plugin() {
        let rt = rt_with_session_write();
        rt.set_session_entries(vec![
            serde_json::json!({ "id": "e1", "kind": "message", "role": "user" }),
            serde_json::json!({ "id": "e2", "kind": "message", "role": "assistant" }),
        ]);
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'session_write'}); \
                 local e = kage.session.entries(); \
                 return #e == 2 and e[2].id == 'e2' and e[1].role == 'user'",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn switch_queues_session_target() {
        let rt = rt_with_session_write();
        rt.eval_plugin(
            "p",
            "kage.request_capabilities({'session_write'}); kage.session.switch('abc123')",
        )
        .unwrap();
        assert_eq!(
            rt.take_switch_request(),
            Some(SwitchTarget::Session("abc123".to_owned()))
        );
        assert_eq!(rt.take_switch_request(), None);
    }

    #[test]
    fn fork_to_queues_fork_and_pending_switch() {
        let rt = rt_with_session_write();
        rt.eval_plugin(
            "p",
            "kage.request_capabilities({'session_write'}); kage.session.fork_to('e1abc')",
        )
        .unwrap();
        // The branch half reuses base `fork`'s queue...
        assert_eq!(rt.take_fork_request().as_deref(), Some("e1abc"));
        // ...and the host is asked to land on the new fork.
        assert_eq!(rt.take_switch_request(), Some(SwitchTarget::PendingFork));
    }

    #[test]
    fn base_session_still_works_through_the_shadow() {
        let rt = rt_with_session_write();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'session_write'}); \
                 return type(kage.session.list) == 'function' \
                 and type(kage.session.entries) == 'function' \
                 and type(kage.session.switch) == 'function' \
                 and type(kage.session.fork_to) == 'function'",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn ungranted_plugin_has_no_session_write_api() {
        let rt = rt_with_session_write();
        let v = rt
            .eval_plugin(
                "other",
                "return kage.session.entries == nil and kage.session.switch == nil \
                 and kage.session.fork_to == nil and type(kage.session.list) == 'function'",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }
}
