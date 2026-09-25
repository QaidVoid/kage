//! Instruction watchdog: aborts plugin Lua code that overruns its VM
//! instruction budget, so a runaway loop cannot hang the host.
//!
//! An `every_nth_instruction` debug hook is installed on the main Lua
//! state, on every bridged coroutine, and on every coroutine a plugin
//! creates through `coroutine.create`/`coroutine.wrap` (see
//! `guard_coroutines` in the runtime module: hooks are per-thread and
//! are not inherited). While a budget is armed in the Lua registry, the
//! hook spends credits per tick; running out aborts the chunk and
//! surfaces as [`PluginError::Lua`]. Budgets are armed per host entry
//! through [`run`], which restores the previous arming afterwards.
//!
//! The budget counts VM instructions, not wall time: the hook only
//! fires while Lua executes, so host-side blocking (an HTTP request,
//! a long SSE stream via `kage.http.post_stream`) never consumes
//! budget and cannot falsely abort a legitimate plugin. The flip
//! side, accepted here: a plugin that hot-loops on *blocking* host
//! calls stays bounded only by each call's own timeout.

use mlua::{HookTriggers, Lua, Thread, VmState};

use crate::error::PluginError;

/// Registry slot holding the remaining hook ticks; [`DISARMED`] means
/// no budget is in force.
const CREDITS_KEY: &str = "kage._watchdog_credits";

/// Sentinel stored while disarmed.
const DISARMED: i64 = i64::MAX;

/// Fire the check every this many VM instructions.
const HOOK_INTERVAL: u32 = 1_000_000;

/// Default budget: VM instructions one host entry may execute. At the
/// Lua interpreter's typical tens of millions of instructions per
/// second this is seconds of continuous burn, and orders of magnitude
/// beyond any legitimate single handler call.
pub const BUDGET: u64 = 1_000_000_000;

/// Budget for render callbacks (slot components, widgets, block renderers).
/// Renders are small and frequent, so a looping one is cut off well
/// under a second instead of burning the full [`BUDGET`].
pub const RENDER_BUDGET: u64 = 10_000_000;

/// Install the watchdog hook on the main Lua state, disarmed. Per-entry
/// budgets are armed through [`run`].
pub fn install(lua: &Lua) -> Result<(), PluginError> {
    lua.set_named_registry_value(CREDITS_KEY, DISARMED)?;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        |lua, _| check(lua).map(|()| VmState::Continue),
    )?;
    Ok(())
}

/// Install the watchdog hook on a Lua thread (a bridged coroutine or
/// one a plugin created). Lua hooks are per-thread, so the main-state
/// hook does not cover resumed threads; the registry credits are
/// shared by all hooks.
pub fn install_on_thread(thread: &Thread) -> Result<(), PluginError> {
    thread.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        |lua, _| check(lua).map(|()| VmState::Continue),
    )?;
    Ok(())
}

/// Spend one tick of credit; abort the running chunk once spent out.
fn check(lua: &Lua) -> Result<(), mlua::Error> {
    let left: i64 = lua.named_registry_value(CREDITS_KEY)?;
    if left == DISARMED {
        return Ok(());
    }
    if left <= 0 {
        return Err(mlua::Error::runtime(
            "plugin script exceeded its CPU budget (runaway loop?)",
        ));
    }
    lua.set_named_registry_value(CREDITS_KEY, left - 1)?;
    Ok(())
}

/// Arm `budget` VM instructions for the duration of `f`, then restore
/// the previous arming. An overrun inside `f` surfaces as
/// [`PluginError::Lua`].
pub fn run<T, E>(lua: &Lua, budget: u64, f: impl FnOnce() -> Result<T, E>) -> Result<T, PluginError>
where
    E: Into<PluginError>,
{
    let previous: i64 = lua.named_registry_value(CREDITS_KEY)?;
    let ticks = i64::try_from(budget / u64::from(HOOK_INTERVAL)).unwrap_or(i64::MAX);
    lua.set_named_registry_value(CREDITS_KEY, ticks.max(1))?;
    let result = f();
    lua.set_named_registry_value(CREDITS_KEY, previous)?;
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn aborts_runaway_main_state_chunk() {
        let lua = Lua::new();
        install(&lua).unwrap();
        let start = Instant::now();
        let err = run(&lua, 20_000_000, || {
            lua.load("while true do end")
                .exec()
                .map_err(PluginError::from)
        })
        .unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let restored: i64 = lua.named_registry_value(CREDITS_KEY).unwrap();
        assert_eq!(restored, DISARMED);
    }

    #[test]
    fn aborts_runaway_bridged_coroutine() {
        let lua = Lua::new();
        install(&lua).unwrap();
        let func: mlua::Function = lua
            .load("return function() while true do end end")
            .eval()
            .unwrap();
        let thread = lua.create_thread(func).unwrap();
        install_on_thread(&thread).unwrap();
        let err = run(&lua, 20_000_000, || {
            thread
                .resume::<mlua::MultiValue>(mlua::MultiValue::new())
                .map_err(PluginError::from)
        })
        .unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
    }

    #[test]
    fn disarmed_hook_lets_normal_code_run() {
        let lua = Lua::new();
        install(&lua).unwrap();
        let value: i64 = run(&lua, BUDGET, || {
            lua.load("return 21 * 2")
                .eval::<i64>()
                .map_err(PluginError::from)
        })
        .unwrap();
        assert_eq!(value, 42);
    }
}
