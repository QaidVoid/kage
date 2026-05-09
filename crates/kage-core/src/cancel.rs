//! Cooperative cancellation flag shared across providers, the loop, and tools.
//!
//! A [`CancelFlag`] is a clonable handle around an [`AtomicBool`]. The host
//! sets it once with [`CancelFlag::cancel`]; long-running operations poll
//! [`CancelFlag::is_cancelled`] at safe points (between SSE events, between
//! tool calls, after each iteration) and bail out cleanly when set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Clonable cancellation flag.
///
/// All clones share the same atomic; cancelling one cancels them all.
/// The flag latches once set, but the host can clear it via
/// [`CancelFlag::reset`] when reusing the same handle across turns.
#[derive(Clone, Debug, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// Create a fresh, not-cancelled flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the flag as cancelled. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Clear a previously-cancelled flag so the same handle can be
    /// reused for the next operation. Hosts that want each operation
    /// to own a fresh handle should prefer [`Self::new`] instead.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// Whether the flag has been set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_flag_is_not_cancelled() {
        assert!(!CancelFlag::new().is_cancelled());
    }

    #[test]
    fn cancel_propagates_to_clones() {
        let a = CancelFlag::new();
        let b = a.clone();
        a.cancel();
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let f = CancelFlag::new();
        f.cancel();
        f.cancel();
        assert!(f.is_cancelled());
    }

    #[test]
    fn flag_is_send_and_sync() {
        fn check_send_sync<T: Send + Sync>(_: &T) {}
        check_send_sync(&CancelFlag::new());
    }

    #[test]
    fn reset_clears_a_cancelled_flag() {
        let f = CancelFlag::new();
        f.cancel();
        assert!(f.is_cancelled());
        f.reset();
        assert!(!f.is_cancelled());
    }
}
