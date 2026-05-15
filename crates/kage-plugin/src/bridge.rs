//! Coroutine bridge for blocking plugin APIs.
//!
//! Some plugin APIs are conceptually blocking: `kage.ui.select` (added
//! in PE.B.2+) opens an overlay, waits for the user, and returns the
//! choice. Lua cannot block the host thread, so the call instead
//! *suspends* the running plugin coroutine: the Lua side calls
//! `kage._suspend(kind, payload)`, which `coroutine.yield`s a tagged
//! request table. The host runs the corresponding action and resumes
//! the coroutine with the result, which becomes the return value of
//! the original blocking call.
//!
//! This module is the engine, not the individual APIs. It can:
//! * run an arbitrary plugin [`mlua::Function`] inside a fresh
//!   coroutine ([`PluginRuntime::bridge_call`]);
//! * recognise a `kage._suspend` yield, park the coroutine, and hand
//!   the host a [`SuspendRequest`];
//! * resume the parked coroutine with a result, with `nil` (user
//!   cancelled the dialog), or abandon it entirely.
//!
//! The bridge is single-slot: at most one suspended coroutine exists
//! at a time. A second [`PluginRuntime::bridge_call`] while one is
//! parked fails with [`PluginError::BridgeBusy`] rather than nesting
//! host actions, which the overlay stack cannot represent anyway.
//!
//! [`PluginRuntime::bridge_call`]: crate::PluginRuntime::bridge_call

use std::sync::{Arc, Mutex};

use mlua::{Lua, MultiValue, Thread, ThreadStatus, Value};

use crate::api::{json_to_lua, lua_to_json};
use crate::error::PluginError;

/// Field a yielded table must carry (set to `true`) to be read as a
/// host-action request rather than a plain `coroutine.yield`.
const SUSPEND_MARKER: &str = "__kage_suspend";

/// Lua source for the internal `kage._suspend` primitive. Underscore
/// prefixed: it is the substrate the PE.B `kage.ui.*` wrappers build
/// on, not a documented plugin entry point.
const SUSPEND_LUA: &str = "kage._suspend = function(kind, payload)\n  \
     return coroutine.yield({ __kage_suspend = true, kind = kind, payload = payload })\n\
     end\n";

/// A request a parked plugin coroutine made of the host.
#[derive(Clone, Debug, PartialEq)]
pub struct SuspendRequest {
    /// Namespace of the host action, e.g. `"ui.select"`. PE.B wrappers
    /// pick the kind; the host routes on it.
    pub kind: String,
    /// Action arguments, shaped per `kind`. `null` when the Lua side
    /// passed no payload.
    pub payload: serde_json::Value,
}

/// Outcome of stepping a bridged coroutine.
#[derive(Clone, Debug, PartialEq)]
pub enum BridgeStep {
    /// The coroutine returned. Carries its first return value as JSON
    /// (`null` when it returned nothing).
    Done(serde_json::Value),
    /// The coroutine parked on a host action. Resume it with
    /// [`crate::PluginRuntime::bridge_resume`] /
    /// [`crate::PluginRuntime::bridge_cancel`].
    Suspended(SuspendRequest),
}

/// Single-slot park for the at-most-one suspended plugin coroutine.
/// `None` means nothing is in flight.
pub type SharedBridge = Arc<Mutex<Option<Thread>>>;

/// Construct an empty bridge slot.
#[must_use]
pub fn shared_bridge() -> SharedBridge {
    Arc::new(Mutex::new(None))
}

/// Install the internal `kage._suspend(kind, payload)` primitive on the
/// running Lua state. Idempotent; the host calls it once at build.
pub fn install_suspend(lua: &Lua) -> Result<(), PluginError> {
    lua.load(SUSPEND_LUA).set_name("kage._suspend").exec()?;
    Ok(())
}

/// Convert host-supplied JSON arguments into a Lua argument tuple.
pub(crate) fn args_to_multi(
    lua: &Lua,
    args: &[serde_json::Value],
) -> Result<MultiValue, PluginError> {
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        values.push(json_to_lua(lua, arg)?);
    }
    Ok(MultiValue::from_vec(values))
}

/// Resume `thread` once with `resume_args` and classify where it
/// landed, updating `slot` so it holds the thread iff it parked again.
///
/// Any error (resume failure, malformed suspend request, unexpected
/// thread state) clears `slot`: a failed coroutine never stays parked,
/// so the next [`crate::PluginRuntime::bridge_call`] is not blocked by
/// a corpse.
pub(crate) fn step(
    thread: Thread,
    resume_args: MultiValue,
    slot: &mut Option<Thread>,
) -> Result<BridgeStep, PluginError> {
    let yielded: MultiValue = match thread.resume(resume_args) {
        Ok(values) => values,
        Err(err) => {
            *slot = None;
            return Err(err.into());
        }
    };
    match thread.status() {
        ThreadStatus::Resumable => {
            let first = yielded.into_vec().into_iter().next().unwrap_or(Value::Nil);
            match parse_suspend(first) {
                Ok(request) => {
                    *slot = Some(thread);
                    Ok(BridgeStep::Suspended(request))
                }
                Err(err) => {
                    *slot = None;
                    Err(err)
                }
            }
        }
        ThreadStatus::Finished => {
            *slot = None;
            let first = yielded.into_vec().into_iter().next().unwrap_or(Value::Nil);
            Ok(BridgeStep::Done(lua_to_json(first)?))
        }
        ThreadStatus::Running => {
            *slot = None;
            Err(PluginError::BridgeProtocol(
                "coroutine reported running after resume".to_owned(),
            ))
        }
        ThreadStatus::Error => {
            *slot = None;
            Err(PluginError::BridgeProtocol(
                "coroutine entered an error state".to_owned(),
            ))
        }
    }
}

/// Read a yielded value as a [`SuspendRequest`]. Anything other than a
/// marked table is a protocol error: a plugin must only suspend through
/// `kage._suspend`, never raw `coroutine.yield`.
fn parse_suspend(value: Value) -> Result<SuspendRequest, PluginError> {
    let Value::Table(table) = value else {
        return Err(PluginError::BridgeProtocol(
            "plugin yielded outside a kage blocking call".to_owned(),
        ));
    };
    let marked: bool = table.get(SUSPEND_MARKER).unwrap_or(false);
    if !marked {
        return Err(PluginError::BridgeProtocol(
            "plugin yielded a table without the kage suspend marker".to_owned(),
        ));
    }
    let kind: String = table.get("kind").map_err(|err| {
        PluginError::BridgeProtocol(format!("suspend request has no string `kind`: {err}"))
    })?;
    let payload_value: Value = table.get("payload").unwrap_or(Value::Nil);
    let payload = lua_to_json(payload_value)?;
    Ok(SuspendRequest { kind, payload })
}

#[cfg(test)]
fn function_from(rt: &crate::PluginRuntime, src: &str) -> mlua::Function {
    let lua = rt.lock_lua();
    lua.load(src).eval::<mlua::Function>().unwrap()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::PluginRuntime;

    #[test]
    fn synchronous_function_runs_to_done_without_parking() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(&rt, "return function(x) return x + 1 end");
        let step = rt.bridge_call(&func, &[json!(41)]).unwrap();
        assert_eq!(step, BridgeStep::Done(json!(42)));
        assert!(!rt.bridge_is_suspended());
    }

    #[test]
    fn suspend_parks_with_kind_and_payload() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(
            &rt,
            "return function() return kage._suspend('ui.select', { items = {'a','b'} }) end",
        );
        let step = rt.bridge_call(&func, &[]).unwrap();
        assert_eq!(
            step,
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.select".to_owned(),
                payload: json!({ "items": ["a", "b"] }),
            })
        );
        assert!(rt.bridge_is_suspended());
    }

    #[test]
    fn resume_delivers_result_into_lua() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(
            &rt,
            "return function()\n  \
               local picked = kage._suspend('ui.select', {})\n  \
               return 'chose:' .. picked\n\
             end",
        );
        assert!(matches!(
            rt.bridge_call(&func, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        let step = rt.bridge_resume(&json!("banana")).unwrap();
        assert_eq!(step, BridgeStep::Done(json!("chose:banana")));
        assert!(!rt.bridge_is_suspended());
    }

    #[test]
    fn cancel_resumes_with_nil() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(
            &rt,
            "return function()\n  \
               local picked = kage._suspend('ui.select', {})\n  \
               if picked == nil then return 'cancelled' end\n  \
               return 'picked'\n\
             end",
        );
        assert!(matches!(
            rt.bridge_call(&func, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        let step = rt.bridge_cancel().unwrap();
        assert_eq!(step, BridgeStep::Done(json!("cancelled")));
    }

    #[test]
    fn second_call_while_parked_is_busy() {
        let rt = PluginRuntime::new().unwrap();
        let blocker = function_from(&rt, "return function() return kage._suspend('x', {}) end");
        let other = function_from(&rt, "return function() return 1 end");
        assert!(matches!(
            rt.bridge_call(&blocker, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        let err = rt.bridge_call(&other, &[]).unwrap_err();
        assert!(matches!(err, PluginError::BridgeBusy));
        assert!(rt.bridge_is_suspended());
    }

    #[test]
    fn resume_without_a_parked_coroutine_is_idle() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.bridge_resume(&json!(1)).unwrap_err();
        assert!(matches!(err, PluginError::BridgeIdle));
        let err = rt.bridge_cancel().unwrap_err();
        assert!(matches!(err, PluginError::BridgeIdle));
    }

    #[test]
    fn error_in_bridged_function_propagates_and_clears_slot() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(&rt, "return function() error('boom') end");
        let err = rt.bridge_call(&func, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(!rt.bridge_is_suspended());
        let ok = function_from(&rt, "return function() return 7 end");
        assert_eq!(
            rt.bridge_call(&ok, &[]).unwrap(),
            BridgeStep::Done(json!(7))
        );
    }

    #[test]
    fn error_after_resume_propagates_and_clears_slot() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(
            &rt,
            "return function()\n  \
               kage._suspend('x', {})\n  \
               error('post-resume boom')\n\
             end",
        );
        assert!(matches!(
            rt.bridge_call(&func, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        let err = rt.bridge_resume(&json!("v")).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(!rt.bridge_is_suspended());
    }

    #[test]
    fn abort_while_suspended_drops_the_coroutine() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(&rt, "return function() return kage._suspend('x', {}) end");
        assert!(matches!(
            rt.bridge_call(&func, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        assert!(rt.bridge_abort());
        assert!(!rt.bridge_is_suspended());
        assert!(!rt.bridge_abort());
        let ok = function_from(&rt, "return function() return 9 end");
        assert_eq!(
            rt.bridge_call(&ok, &[]).unwrap(),
            BridgeStep::Done(json!(9))
        );
    }

    #[test]
    fn raw_yield_without_marker_is_a_protocol_error() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(&rt, "return function() return coroutine.yield(42) end");
        let err = rt.bridge_call(&func, &[]).unwrap_err();
        assert!(matches!(err, PluginError::BridgeProtocol(_)));
        assert!(!rt.bridge_is_suspended());
    }

    #[test]
    fn multiple_suspensions_round_trip_in_order() {
        let rt = PluginRuntime::new().unwrap();
        let func = function_from(
            &rt,
            "return function()\n  \
               local a = kage._suspend('step', { n = 1 })\n  \
               local b = kage._suspend('step', { n = 2 })\n  \
               return a .. '+' .. b\n\
             end",
        );
        let s1 = rt.bridge_call(&func, &[]).unwrap();
        assert_eq!(
            s1,
            BridgeStep::Suspended(SuspendRequest {
                kind: "step".to_owned(),
                payload: json!({ "n": 1 }),
            })
        );
        let s2 = rt.bridge_resume(&json!("one")).unwrap();
        assert_eq!(
            s2,
            BridgeStep::Suspended(SuspendRequest {
                kind: "step".to_owned(),
                payload: json!({ "n": 2 }),
            })
        );
        let s3 = rt.bridge_resume(&json!("two")).unwrap();
        assert_eq!(s3, BridgeStep::Done(json!("one+two")));
    }
}
