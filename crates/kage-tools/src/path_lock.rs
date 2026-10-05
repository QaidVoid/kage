//! Per-path mutual exclusion shared by `write` and `edit`.
//!
//! Parallel tool dispatch runs one call per thread, so two calls
//! touching the same file can interleave: `write` can slip its
//! create-past the check of another, and `edit` can drop one side of a
//! concurrent read-modify-write. Locking the resolved path for the
//! whole sequence closes that window in-process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// One lock per resolved path, created on first use.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Run `f` while holding the lock for `path`, serializing concurrent
/// check-then-write and read-modify-write sequences on one file.
/// Paths must be resolved (canonical), so two spellings of the same
/// file map to the same lock.
pub(crate) fn with_path_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let lock = Arc::clone(
        kage_core::sync::lock(&LOCKS)
            .entry(path.to_path_buf())
            .or_default(),
    );
    let _guard = kage_core::sync::lock(&lock);
    f()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn serializes_callers_on_one_path() {
        let current = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let (current, max_seen) = (Arc::clone(&current), Arc::clone(&max_seen));
                scope.spawn(move || {
                    with_path_lock(Path::new("/tmp/kage-path-lock-test"), || {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                        current.fetch_sub(1, Ordering::SeqCst);
                    });
                });
            }
        });
        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "sections overlapped");
    }

    #[test]
    fn distinct_paths_do_not_block_each_other() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                with_path_lock(Path::new("/tmp/kage-path-lock-a"), || {
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_millis(100));
                });
            });
            let _ = rx.recv();
            let start = Instant::now();
            with_path_lock(Path::new("/tmp/kage-path-lock-b"), || {});
            assert!(
                start.elapsed() < Duration::from_millis(50),
                "lock B waited for lock A's holder"
            );
        });
    }
}
