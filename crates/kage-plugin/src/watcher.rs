//! Detect plugin-file changes between turn boundaries.
//!
//! [`PluginWatcher`] uses the `notify` crate's recommended OS-level
//! watcher (`inotify` on Linux, `FSEvents` on macOS, `ReadDirectoryChangesW`
//! on Windows) so changes are observed instantly and atomic-rename writes
//! are picked up reliably. The host calls [`PluginWatcher::poll`] at safe
//! points (typically the start of a new turn). If anything changed since
//! the last poll, the caller drives a reload through
//! [`crate::PluginRuntime::reload_dir`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::error::PluginError;

/// Filesystem watcher that flips a flag whenever a `*.lua` file in its
/// directory is added, modified, or removed.
pub struct PluginWatcher {
    dir: PathBuf,
    dirty: Arc<AtomicBool>,
    // The watcher's worker thread is owned by this field; dropping it
    // stops the thread.
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for PluginWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginWatcher")
            .field("dir", &self.dir)
            .field("dirty", &self.dirty.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl PluginWatcher {
    /// Begin watching `dir`. The directory is watched non-recursively;
    /// nested folders are ignored. Returns an error if the OS watcher
    /// could not start (permission denied, dir does not exist, etc.).
    pub fn new(dir: PathBuf) -> Result<Self, PluginError> {
        let dirty = Arc::new(AtomicBool::new(false));
        let dirty_for_handler = Arc::clone(&dirty);
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let Ok(event) = res else { return };
                if event_touches_lua(&event) && is_meaningful_kind(event.kind) {
                    dirty_for_handler.store(true, Ordering::Relaxed);
                }
            },
            Config::default(),
        )
        .map_err(|err| PluginError::Io {
            path: dir.clone(),
            source: std::io::Error::other(err.to_string()),
        })?;
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|err| PluginError::Io {
                path: dir.clone(),
                source: std::io::Error::other(err.to_string()),
            })?;
        Ok(Self {
            dir,
            dirty,
            _watcher: watcher,
        })
    }

    /// Path of the directory being watched.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Return whether any `*.lua` change has been observed since the last
    /// poll, then reset the flag. Cheap to call: just an atomic swap.
    #[must_use]
    pub fn poll(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }
}

fn event_touches_lua(event: &Event) -> bool {
    event
        .paths
        .iter()
        .any(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
}

fn is_meaningful_kind(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;

    /// Wait up to `timeout` for the watcher to flip dirty. Returns the
    /// final value the watcher reported. The OS-level events propagate
    /// asynchronously; a short window is needed even on Linux inotify.
    fn wait_for_change(w: &PluginWatcher, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if w.poll() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn detects_new_lua_file() {
        let dir = tempdir().unwrap();
        let w = PluginWatcher::new(dir.path().to_path_buf()).unwrap();
        fs::write(dir.path().join("a.lua"), "-- hi").unwrap();
        assert!(wait_for_change(&w, Duration::from_secs(2)));
    }

    #[test]
    fn detects_modification_via_overwrite() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.lua"), "v1").unwrap();
        let w = PluginWatcher::new(dir.path().to_path_buf()).unwrap();
        // Drain any spurious initial-event the watcher might emit while
        // settling the watch.
        let _ = wait_for_change(&w, Duration::from_millis(50));
        fs::write(dir.path().join("a.lua"), "v2").unwrap();
        assert!(wait_for_change(&w, Duration::from_secs(2)));
    }

    #[test]
    fn detects_deletion() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.lua"), "x").unwrap();
        let w = PluginWatcher::new(dir.path().to_path_buf()).unwrap();
        let _ = wait_for_change(&w, Duration::from_millis(50));
        fs::remove_file(dir.path().join("a.lua")).unwrap();
        assert!(wait_for_change(&w, Duration::from_secs(2)));
    }

    #[test]
    fn ignores_non_lua_files() {
        let dir = tempdir().unwrap();
        let w = PluginWatcher::new(dir.path().to_path_buf()).unwrap();
        fs::write(dir.path().join("notes.txt"), "y").unwrap();
        // Wait the same window we'd allow for a real event; if we don't
        // see one, the filter is doing its job.
        assert!(!wait_for_change(&w, Duration::from_millis(300)));
    }

    #[test]
    fn errors_on_missing_directory() {
        let res = PluginWatcher::new(PathBuf::from("/nonexistent/here"));
        assert!(res.is_err());
    }
}
