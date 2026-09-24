//! Callbacks that the Lua owner thread runs between jobs.
//!
//! Three functions queue work on the owner thread:
//!
//! * `kage.schedule(fn)` runs `fn` once, right after the current job and
//!   before the next queued one.
//! * `kage.defer(fn, ms)` runs `fn` once, `ms` milliseconds from now, and
//!   returns `stop`, which cancels it.
//! * `kage.timer(fn, ms)` runs `fn` every `ms` milliseconds, at least
//!   [`MIN_INTERVAL_MS`], until the returned `stop` is called.
//!
//! All three share one deadline heap, and a scheduled callback is simply
//! due at once. After each job, and whenever the earliest deadline
//! passes, the owner thread (see [`crate::host`]) runs every due callback
//! in deadline order, each under the watchdog budget. A callback that
//! raises is logged, and a raising timer is stopped, so it logs once.
//! Reload cancels everything.
//!
//! Callbacks live in a Lua registry table keyed by id, so the Lua GC
//! frees them once they stop. The Rust side keeps only ids and
//! deadlines, and `stop` captures only an id, so nothing here keeps the
//! Lua state or the host alive.
//!
//! Callbacks run outside any coroutine, so a blocking `kage.ui.*` dialog
//! cannot suspend there. The dialog call raises "attempt to yield from
//! outside a coroutine", and the callback is logged (and a timer
//! stopped) like any other error.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kage_core::sync::lock;
use mlua::{Function, Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog};
use crate::capabilities::CurrentPlugin;
use crate::error::PluginError;
use crate::watchdog;

/// Lua-registry key of the `id -> callback` table.
const CALLBACKS_KEY: &str = "kage._timers";

/// Shortest interval `kage.timer` accepts, in milliseconds.
const MIN_INTERVAL_MS: u64 = 50;

/// Deadlines and metadata of every pending callback. Ids are never
/// reused, even across a reload, so a stale `stop` is harmless.
struct Timers {
    next_id: i64,
    queue: BinaryHeap<Reverse<(Instant, i64)>>,
    live: HashMap<i64, Timer>,
    sink: SharedHostLog,
    budget: u64,
}

#[derive(Clone)]
struct Timer {
    kind: &'static str,
    every: Option<Duration>,
    origin: Arc<str>,
}

/// Install `kage.schedule`, `kage.defer` and `kage.timer`. Callbacks run
/// under `budget` VM instructions each, and their errors go to `sink`.
pub(crate) fn install(
    lua: &Lua,
    sink: SharedHostLog,
    current: CurrentPlugin,
    budget: u64,
) -> Result<(), PluginError> {
    lua.set_app_data(Timers {
        next_id: 0,
        queue: BinaryHeap::new(),
        live: HashMap::new(),
        sink,
        budget,
    });
    lua.set_named_registry_value(CALLBACKS_KEY, lua.create_table()?)?;
    let kage: Table = lua.globals().get("kage")?;

    let owner = Arc::clone(&current);
    kage.set(
        "schedule",
        lua.create_function(move |lua, callback: Function| {
            add(lua, "kage.schedule", callback, 0, None, &owner).map(drop)
        })?,
    )?;

    let owner = Arc::clone(&current);
    kage.set(
        "defer",
        lua.create_function(move |lua, (callback, ms): (Function, i64)| {
            let ms = u64::try_from(ms).unwrap_or(0);
            let id = add(lua, "kage.defer", callback, ms, None, &owner)?;
            stop_fn(lua, id)
        })?,
    )?;

    kage.set(
        "timer",
        lua.create_function(move |lua, (callback, ms): (Function, i64)| {
            let ms = u64::try_from(ms).unwrap_or(0).max(MIN_INTERVAL_MS);
            let every = Some(Duration::from_millis(ms));
            let id = add(lua, "kage.timer", callback, ms, every, &current)?;
            stop_fn(lua, id)
        })?,
    )?;
    Ok(())
}

fn add(
    lua: &Lua,
    kind: &'static str,
    callback: Function,
    ms: u64,
    every: Option<Duration>,
    current: &CurrentPlugin,
) -> mlua::Result<i64> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(ms))
        .ok_or_else(|| mlua::Error::external(format!("{kind}: ms is too large")))?;
    let origin = lock(current)
        .as_deref()
        .map(|owner| format!(" from '{owner}'"))
        .unwrap_or_default()
        .into();
    let id = {
        let mut timers = lua
            .app_data_mut::<Timers>()
            .ok_or_else(|| mlua::Error::external(format!("{kind}: scheduler is not installed")))?;
        timers.next_id += 1;
        let id = timers.next_id;
        timers.queue.push(Reverse((deadline, id)));
        timers.live.insert(
            id,
            Timer {
                kind,
                every,
                origin,
            },
        );
        id
    };
    callbacks(lua)?.raw_set(id, callback)?;
    Ok(id)
}

/// Run `callback` every `ms` milliseconds, at least [`MIN_INTERVAL_MS`],
/// like `kage.timer`, for a host-side consumer. `kind` labels errors.
/// Returns the id [`cancel`] takes.
pub(crate) fn every(
    lua: &Lua,
    kind: &'static str,
    ms: u64,
    callback: Function,
    current: &CurrentPlugin,
) -> mlua::Result<i64> {
    let ms = ms.max(MIN_INTERVAL_MS);
    add(
        lua,
        kind,
        callback,
        ms,
        Some(Duration::from_millis(ms)),
        current,
    )
}

fn stop_fn(lua: &Lua, id: i64) -> mlua::Result<Function> {
    lua.create_function(move |lua, ()| cancel(lua, id))
}

/// Cancel the pending callback `id`. A stale id is ignored.
pub(crate) fn cancel(lua: &Lua, id: i64) -> mlua::Result<()> {
    if let Some(mut timers) = lua.app_data_mut::<Timers>() {
        timers.live.remove(&id);
    }
    callbacks(lua)?.raw_set(id, Value::Nil)
}

fn callbacks(lua: &Lua) -> mlua::Result<Table> {
    lua.named_registry_value(CALLBACKS_KEY)
}

/// Time until the earliest pending callback is due, zero when one is
/// already due, or `None` when nothing is pending.
pub(crate) fn next_wait(lua: &Lua) -> Option<Duration> {
    let mut timers = lua.app_data_mut::<Timers>()?;
    while let Some(&Reverse((deadline, id))) = timers.queue.peek() {
        if timers.live.contains_key(&id) {
            return Some(deadline.saturating_duration_since(Instant::now()));
        }
        timers.queue.pop();
    }
    None
}

/// Run every callback that is due now, in deadline order. Callbacks
/// queued meanwhile wait for the next pass, so a callback that keeps
/// rescheduling itself cannot starve queued jobs.
pub(crate) fn run_due(lua: &Lua) {
    let Some((sink, budget)) = lua
        .app_data_ref::<Timers>()
        .map(|timers| (Arc::clone(&timers.sink), timers.budget))
    else {
        return;
    };
    let now = Instant::now();
    while let Some((id, deadline, timer)) = pop_due(lua, now) {
        if let Err(err) = call(lua, id, &timer, budget) {
            let _ = cancel(lua, id);
            let stopped = if timer.every.is_some() {
                " and was stopped"
            } else {
                ""
            };
            lock(&sink).log(
                LogLevel::Error,
                &format!(
                    "{} callback{} raised{stopped}: {err}",
                    timer.kind, timer.origin
                ),
            );
        } else if let Some(every) = timer.every {
            reschedule(lua, id, deadline, every, now);
        }
    }
}

/// Pop the earliest live callback due at `now`. A one-shot callback
/// leaves the live set here, before it runs.
fn pop_due(lua: &Lua, now: Instant) -> Option<(i64, Instant, Timer)> {
    let mut timers = lua.app_data_mut::<Timers>()?;
    loop {
        let Reverse((deadline, id)) = *timers.queue.peek()?;
        if deadline > now {
            return None;
        }
        timers.queue.pop();
        let timer = match timers.live.get(&id) {
            None => continue,
            Some(timer) if timer.every.is_some() => timer.clone(),
            Some(_) => timers.live.remove(&id)?,
        };
        return Some((id, deadline, timer));
    }
}

fn call(lua: &Lua, id: i64, timer: &Timer, budget: u64) -> Result<(), PluginError> {
    let callbacks = callbacks(lua)?;
    let callback: Option<Function> = callbacks.raw_get(id)?;
    if timer.every.is_none() {
        callbacks.raw_set(id, Value::Nil)?;
    }
    match callback {
        Some(callback) => watchdog::run(lua, budget, || callback.call::<()>(())),
        None => Ok(()),
    }
}

/// Queue the next run of a repeating timer that is still live. A timer
/// that fell behind skips the missed runs instead of bursting.
fn reschedule(lua: &Lua, id: i64, deadline: Instant, every: Duration, now: Instant) {
    let Some(mut timers) = lua.app_data_mut::<Timers>() else {
        return;
    };
    if timers.live.contains_key(&id) {
        let next = deadline + every;
        let next = if next > now { next } else { now + every };
        timers.queue.push(Reverse((next, id)));
    }
}

/// Cancel every pending callback. Used by reload.
pub(crate) fn clear(lua: &Lua) -> mlua::Result<()> {
    if let Some(mut timers) = lua.app_data_mut::<Timers>() {
        timers.queue.clear();
        timers.live.clear();
    }
    lua.set_named_registry_value(CALLBACKS_KEY, lua.create_table()?)
}

#[cfg(test)]
mod tests;
