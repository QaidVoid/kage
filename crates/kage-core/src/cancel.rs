//! Cooperative cancellation flag shared across providers, the loop, and tools.
//!
//! A [`CancelFlag`] is a clonable handle around an [`AtomicBool`]. The host
//! sets it once with [`CancelFlag::cancel`]; long-running operations poll
//! [`CancelFlag::is_cancelled`] at safe points (between SSE events, between
//! tool calls, after each iteration) and bail out cleanly when set.
//!
//! Flags form a tree: [`CancelFlag::child`] makes a flag that also reads as
//! cancelled whenever its parent is, so cancelling a session stops every
//! session below it while cancelling a child never reaches its parent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Clonable cancellation flag.
///
/// All clones share the same atomic; cancelling one cancels them all.
/// The flag latches once set, but the host can clear it via
/// [`CancelFlag::reset`] when reusing the same handle across turns.
#[derive(Clone, Debug, Default)]
pub struct CancelFlag(Arc<Node>);

#[derive(Debug, Default)]
struct Node {
    flag: AtomicBool,
    parent: Option<Arc<Node>>,
}

impl CancelFlag {
    /// Create a fresh, not-cancelled flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A flag that also reads as cancelled whenever `self` or any of its
    /// ancestors is. Cancelling or resetting the child touches only the
    /// child.
    #[must_use]
    pub fn child(&self) -> Self {
        Self(Arc::new(Node {
            flag: AtomicBool::new(false),
            parent: Some(Arc::clone(&self.0)),
        }))
    }

    /// Mark the flag as cancelled. Idempotent.
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::Release);
    }

    /// Clear a previously-cancelled flag so the same handle can be
    /// reused for the next operation. Ancestors are left alone, so a
    /// child of a cancelled flag still reads as cancelled. Hosts that
    /// want each operation to own a fresh handle should prefer
    /// [`Self::new`] instead.
    pub fn reset(&self) {
        self.0.flag.store(false, Ordering::Release);
    }

    /// Whether this flag or any of its ancestors has been set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        let mut node = Some(&self.0);
        while let Some(n) = node {
            if n.flag.load(Ordering::Acquire) {
                return true;
            }
            node = n.parent.as_ref();
        }
        false
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

    #[test]
    fn cancelling_a_parent_reaches_every_descendant() {
        let parent = CancelFlag::new();
        let child = parent.child();
        let grandchild = child.child();
        parent.cancel();
        assert!(child.is_cancelled());
        assert!(grandchild.is_cancelled());
    }

    #[test]
    fn cancelling_a_child_leaves_the_parent_clear() {
        let parent = CancelFlag::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[test]
    fn resetting_a_child_keeps_a_cancelled_parent() {
        let parent = CancelFlag::new();
        let child = parent.child();
        parent.cancel();
        child.reset();
        assert!(child.is_cancelled());
    }

    #[test]
    fn resetting_the_parent_clears_the_cascade() {
        let parent = CancelFlag::new();
        let child = parent.child();
        let grandchild = child.child();
        parent.cancel();
        parent.reset();
        assert!(!child.is_cancelled());
        assert!(!grandchild.is_cancelled());
    }

    #[test]
    fn child_clones_share_the_child_flag() {
        let parent = CancelFlag::new();
        let child = parent.child();
        let clone = child.clone();
        child.cancel();
        assert!(clone.is_cancelled());
        assert!(!parent.is_cancelled());
    }
}
