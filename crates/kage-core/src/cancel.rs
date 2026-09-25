//! Cooperative cancellation flag shared across providers, the loop, and tools.
//!
//! A [`CancelFlag`] is a clonable handle around an [`AtomicBool`]. The host
//! sets it once with [`CancelFlag::cancel`]; work checks
//! [`CancelFlag::is_cancelled`] at safe points (between SSE events, between
//! tool calls, after each iteration) and bails out cleanly when set. A
//! thread that blocks instead takes a [`CancelWatch`] and selects on its
//! receiver next to its own reply channel, so it wakes the moment the flag
//! flips.
//!
//! Flags form a tree: [`CancelFlag::child`] makes a flag that also reads as
//! cancelled whenever its parent is, so cancelling a session stops every
//! session below it while cancelling a child never reaches its parent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};

use crate::sync::lock;

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
    watchers: Mutex<Vec<Sender<()>>>,
}

/// A wake-up for one [`CancelFlag`], made by [`CancelFlag::watch`].
///
/// Its [`receiver`](Self::receiver) gets a message when the flag or any
/// of its ancestors is cancelled, or right away if one already was.
/// Dropping the watch unregisters it.
#[derive(Debug)]
pub struct CancelWatch {
    tx: Sender<()>,
    rx: Receiver<()>,
    nodes: Vec<Arc<Node>>,
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
            watchers: Mutex::default(),
        }))
    }

    /// Mark the flag as cancelled and wake every [`CancelWatch`] on it or
    /// on a descendant. Idempotent. Takes a mutex, so never call it from a
    /// signal handler.
    pub fn cancel(&self) {
        // Storing before locking means a concurrent `watch` either sees
        // the flag or is already in the list taken here.
        self.0.flag.store(true, Ordering::Release);
        let watchers = std::mem::take(&mut *lock(&self.0.watchers));
        for tx in watchers {
            let _ = tx.try_send(());
        }
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

    /// Register a [`CancelWatch`] that wakes when this flag or any of its
    /// ancestors is cancelled. It is ready at once if one already is.
    #[must_use]
    pub fn watch(&self) -> CancelWatch {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut nodes = Vec::new();
        let mut node = Some(&self.0);
        while let Some(n) = node {
            let mut watchers = lock(&n.watchers);
            if n.flag.load(Ordering::Acquire) {
                drop(watchers);
                let _ = tx.try_send(());
                break;
            }
            watchers.push(tx.clone());
            drop(watchers);
            nodes.push(Arc::clone(n));
            node = n.parent.as_ref();
        }
        CancelWatch { tx, rx, nodes }
    }
}

impl CancelWatch {
    /// The channel to select on. It delivers one message per wake-up, so a
    /// waiter that selects more than once checks
    /// [`CancelFlag::is_cancelled`] before each select.
    #[must_use]
    pub fn receiver(&self) -> &Receiver<()> {
        &self.rx
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        for node in &self.nodes {
            lock(&node.watchers).retain(|tx| !tx.same_channel(&self.tx));
        }
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

    fn is_ready(watch: &CancelWatch) -> bool {
        watch.receiver().try_recv().is_ok()
    }

    fn watcher_count(flag: &CancelFlag) -> usize {
        lock(&flag.0.watchers).len()
    }

    #[test]
    fn watch_receives_after_cancel() {
        let flag = CancelFlag::new();
        let watch = flag.watch();
        assert!(!is_ready(&watch));
        flag.cancel();
        assert!(is_ready(&watch));
    }

    #[test]
    fn cancelling_a_grandparent_wakes_a_grandchild_watch() {
        let grandparent = CancelFlag::new();
        let grandchild = grandparent.child().child();
        let watch = grandchild.watch();
        grandparent.cancel();
        assert!(is_ready(&watch));
    }

    #[test]
    fn cancelling_a_child_does_not_wake_the_parent_watch() {
        let parent = CancelFlag::new();
        let child = parent.child();
        let watch = parent.watch();
        child.cancel();
        assert!(!is_ready(&watch));
    }

    #[test]
    fn watch_under_a_cancelled_ancestor_is_ready_at_once() {
        let parent = CancelFlag::new();
        let child = parent.child();
        parent.cancel();
        assert!(is_ready(&child.watch()));
    }

    #[test]
    fn dropping_watches_unregisters_them() {
        let parent = CancelFlag::new();
        let child = parent.child();
        let grandchild = child.child();
        let watches = [parent.watch(), child.watch(), grandchild.watch()];
        assert_eq!(watcher_count(&parent), 3);
        assert_eq!(watcher_count(&child), 2);
        assert_eq!(watcher_count(&grandchild), 1);
        drop(watches);
        for flag in [&parent, &child, &grandchild] {
            assert_eq!(watcher_count(flag), 0);
        }
    }

    #[test]
    fn watch_after_cancel_and_reset_is_not_ready() {
        let flag = CancelFlag::new();
        flag.cancel();
        flag.reset();
        assert!(!is_ready(&flag.watch()));
    }

    #[test]
    fn blocked_receiver_wakes_on_cancel_from_another_thread() {
        let flag = CancelFlag::new();
        let watch = flag.watch();
        let canceller = flag.clone();
        let handle = std::thread::spawn(move || canceller.cancel());
        let woke = watch
            .receiver()
            .recv_timeout(std::time::Duration::from_secs(5));
        handle.join().unwrap();
        assert!(woke.is_ok());
    }
}
