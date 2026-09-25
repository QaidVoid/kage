//! Fixtures shared by this crate's tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::sync::lock;

use crate::host::LuaHost;

/// How long a test waits for something that should happen promptly.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

/// Poll `done` until it holds, failing after [`TIMEOUT`].
pub(crate) fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Blocks every caller of [`Gate::hold`] until the test opens it.
#[derive(Default)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    expired: AtomicBool,
}

impl Gate {
    /// Wait until the gate opens. Gives up after [`TIMEOUT`], so a call
    /// that wrongly waits on the holder fails [`Gate::assert_held`]
    /// instead of hanging the test.
    pub(crate) fn hold(&self) {
        let open = lock(&self.open);
        let (open, wait) = self
            .opened
            .wait_timeout_while(open, TIMEOUT, |open| !*open)
            .unwrap();
        drop(open);
        if wait.timed_out() {
            self.expired.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn open(&self) {
        *lock(&self.open) = true;
        self.opened.notify_all();
    }

    /// Panic if a holder gave up waiting, which means the call under
    /// test blocked on it.
    pub(crate) fn assert_held(&self) {
        assert!(
            !self.expired.load(Ordering::SeqCst),
            "the call waited on the held gate"
        );
    }
}

/// Keep the Lua owner thread busy until the returned gate opens.
pub(crate) fn occupy(host: &LuaHost) -> Arc<Gate> {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    host.submit(move |_| held.hold()).unwrap();
    gate
}
