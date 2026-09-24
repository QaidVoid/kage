//! Detect plugin and user config changes between turn boundaries.
//!
//! [`PluginWatcher`] uses the `notify` crate's recommended OS-level
//! watcher (`inotify` on Linux, `FSEvents` on macOS, `ReadDirectoryChangesW`
//! on Windows) so changes are observed instantly and atomic-rename writes
//! are picked up reliably. The host calls [`PluginWatcher::poll`] at safe
//! points (typically the start of a new turn). If anything changed since
//! the last poll, the caller drives a reload through
//! [`crate::PluginRuntime::reload_all`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::error::PluginError;

/// Filesystem watcher that flips a flag whenever a watched Lua file is
/// added, modified, or removed: a `*.lua` file directly in the plugins
/// directory, the user `init.lua`, or a `*.lua` file anywhere under the
/// user `lua/` directory.
pub struct PluginWatcher {
    scope: Scope,
    dirty: Arc<AtomicBool>,
    // The watcher's worker thread is owned by this field; dropping it
    // stops the thread.
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for PluginWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginWatcher")
            .field("scope", &self.scope)
            .field("dirty", &self.dirty.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Paths whose changes count.
#[derive(Clone, Debug, Default)]
struct Scope {
    plugins: Option<PathBuf>,
    init: Option<PathBuf>,
    modules: Option<PathBuf>,
}

impl Scope {
    fn matches(&self, path: &Path) -> bool {
        let lua = path.extension().is_some_and(|ext| ext == "lua");
        self.init.as_deref() == Some(path)
            || lua
                && self
                    .plugins
                    .as_deref()
                    .is_some_and(|dir| path.parent() == Some(dir))
            || lua
                && self
                    .modules
                    .as_deref()
                    .is_some_and(|dir| path.starts_with(dir))
    }
}

impl PluginWatcher {
    /// Begin watching the plugins directory `dir`. The directory is
    /// watched non-recursively; nested folders are ignored. Returns an
    /// error if the OS watcher could not start (permission denied, dir
    /// does not exist, etc.).
    pub fn new(dir: PathBuf) -> Result<Self, PluginError> {
        let dir = canonical(dir);
        let scope = Scope {
            plugins: Some(dir.clone()),
            ..Scope::default()
        };
        Self::start(scope, &[(dir, RecursiveMode::NonRecursive)])
    }

    /// Watch the plugins directory, when given, plus the trusted user
    /// config in `user_dir`: its `init.lua` and, recursively, its `lua/`
    /// directory. Directories that do not exist yet are skipped, so a
    /// `lua/` directory created later is watched from the next start.
    pub fn for_config(
        plugins_dir: Option<PathBuf>,
        user_dir: Option<PathBuf>,
    ) -> Result<Self, PluginError> {
        let plugins_dir = plugins_dir.map(canonical);
        let user_dir = user_dir.map(canonical);
        let modules = user_dir.as_ref().map(|dir| dir.join("lua"));
        let scope = Scope {
            plugins: plugins_dir.clone(),
            init: user_dir.as_ref().map(|dir| dir.join("init.lua")),
            modules: modules.clone(),
        };
        let roots: Vec<_> = [
            (plugins_dir, RecursiveMode::NonRecursive),
            (user_dir, RecursiveMode::NonRecursive),
            (modules, RecursiveMode::Recursive),
        ]
        .into_iter()
        .filter_map(|(dir, mode)| dir.filter(|d| d.is_dir()).map(|d| (d, mode)))
        .collect();
        Self::start(scope, &roots)
    }

    fn start(scope: Scope, roots: &[(PathBuf, RecursiveMode)]) -> Result<Self, PluginError> {
        let dirty = Arc::new(AtomicBool::new(false));
        let dirty_for_handler = Arc::clone(&dirty);
        let filter = scope.clone();
        let io_error = |path: &Path, err: notify::Error| PluginError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(err.to_string()),
        };
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let Ok(event) = res else { return };
                if is_meaningful_kind(event.kind) && event.paths.iter().any(|p| filter.matches(p)) {
                    dirty_for_handler.store(true, Ordering::Relaxed);
                }
            },
            Config::default(),
        )
        .map_err(|err| io_error(Path::new(""), err))?;
        for (dir, mode) in roots {
            watcher
                .watch(dir, *mode)
                .map_err(|err| io_error(dir, err))?;
        }
        Ok(Self {
            scope,
            dirty,
            _watcher: watcher,
        })
    }

    /// Return whether any watched change has been observed since the
    /// last poll, then reset the flag. Cheap to call: just an atomic swap.
    #[must_use]
    pub fn poll(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }
}

/// Canonicalize `dir` when it exists, so event paths from backends that
/// report resolved paths still match. A missing dir is kept as given.
fn canonical(dir: PathBuf) -> PathBuf {
    dir.canonicalize().unwrap_or(dir)
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

    #[test]
    fn detects_user_init_and_nested_modules() {
        let user = tempdir().unwrap();
        fs::create_dir_all(user.path().join("lua/a")).unwrap();
        let w = PluginWatcher::for_config(None, Some(user.path().to_path_buf())).unwrap();
        let _ = wait_for_change(&w, Duration::from_millis(50));
        fs::write(user.path().join("lua/a/b.lua"), "return 1").unwrap();
        assert!(wait_for_change(&w, Duration::from_secs(2)));
        fs::write(user.path().join("init.lua"), "-- hi").unwrap();
        assert!(wait_for_change(&w, Duration::from_secs(2)));
        sleep(Duration::from_millis(200));
        let _ = w.poll();
        fs::write(user.path().join("other.lua"), "-- not init").unwrap();
        fs::write(user.path().join("lua/a/notes.txt"), "x").unwrap();
        assert!(!wait_for_change(&w, Duration::from_millis(300)));
    }

    #[test]
    fn config_watch_skips_missing_dirs() {
        let user = tempdir().unwrap();
        let w = PluginWatcher::for_config(
            Some(user.path().join("plugins")),
            Some(user.path().to_path_buf()),
        )
        .unwrap();
        fs::write(user.path().join("init.lua"), "-- hi").unwrap();
        assert!(wait_for_change(&w, Duration::from_secs(2)));
    }
}
