//! Render output retained between frames.
//!
//! Status-bar widgets never call Lua on the caller's thread. They return the
//! output of an earlier owner-thread job and queue a recompute when
//! that output is stale. A recompute whose output differs sets the
//! runtime's redraw flag (see [`crate::PluginRuntime::redraw_flag`]).
//!
//! A surface with no output yet waits up to [`COLD_WAIT`] for its first
//! result, and only while the owner thread is idle. A fresh surface
//! therefore paints on its first frame, while a busy owner never stalls
//! the render loop: the first frame then falls back to empty output.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kage_core::sync::lock;
use mlua::Lua;

use crate::host::LuaHost;

/// Longest a render waits for a first result on an idle owner thread.
pub(crate) const COLD_WAIT: Duration = Duration::from_millis(50);

/// Age after which widget output is recomputed even when
/// the width is unchanged.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Output of one width-keyed render surface.
pub(crate) struct Retained<V> {
    slot: Arc<Mutex<Slot<V>>>,
}

struct Slot<V> {
    value: V,
    /// Width and time of the last queued recompute.
    requested: Option<(u16, Instant)>,
    pending: bool,
}

impl<V: Clone + Default + PartialEq + Send + 'static> Retained<V> {
    pub(crate) fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(Slot {
                value: V::default(),
                requested: None,
                pending: false,
            })),
        }
    }

    /// Return the retained output, queueing a recompute on `host` when
    /// the width changed or the output is older than
    /// [`REFRESH_INTERVAL`]. `compute` runs on the owner thread; `None`
    /// (a failed render, already logged) keeps the previous output.
    pub(crate) fn get<F>(&self, host: &LuaHost, width: u16, compute: F) -> V
    where
        F: FnOnce(&Lua, u16) -> Option<V> + Send + 'static,
    {
        let mut slot = lock(&self.slot);
        let fresh = slot
            .requested
            .is_some_and(|(w, at)| w == width && at.elapsed() < REFRESH_INTERVAL);
        if fresh || slot.pending {
            return slot.value.clone();
        }
        let cold = slot.requested.is_none() && host.is_idle();
        slot.requested = Some((width, Instant::now()));
        slot.pending = true;
        drop(slot);

        let target = Arc::clone(&self.slot);
        let redraw = host.redraw_flag();
        let queued = host.queue(move |lua| {
            let output = compute(lua, width);
            let mut slot = lock(&target);
            slot.pending = false;
            if let Some(value) = output
                && value != slot.value
            {
                slot.value = value;
                redraw.store(true, Ordering::SeqCst);
            }
        });
        match queued {
            Ok(done) if cold => {
                let _ = done.recv_timeout(COLD_WAIT);
            }
            Ok(_) => {}
            Err(_) => lock(&self.slot).pending = false,
        }
        lock(&self.slot).value.clone()
    }
}
