//! One plugin runtime per project, shared by every session the host runs
//! in that project.
//!
//! A project's runtime loads the plugins directory and the user's
//! `init.lua`, as the TUI's does, so their tools, hooks and engine options
//! apply to every client's sessions. It starts with the project's first
//! session and stays [`GRACE`] after the last one lets go, so reopening a
//! session or restarting a client does not reload every plugin. A session
//! that opens after a plugin, `init.lua`, the config or the project's trust
//! changed gets a freshly loaded runtime; sessions already open keep theirs.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kage_core::config::Config;
use kage_core::environment::Environment;
use kage_core::options::OptionStore;
use kage_core::sync::lock;
use kage_plugin::{PluginRuntime, SharedOptions};

/// How long a runtime no session holds is kept.
const GRACE: Duration = Duration::from_secs(60);
/// How long a project's fingerprint is trusted before it is read again.
const RECHECK: Duration = Duration::from_secs(1);

/// A project's plugin runtime and the option store its `init.lua` set.
#[derive(Clone)]
pub(super) struct Project {
    pub(super) runtime: Arc<PluginRuntime>,
    pub(super) options: SharedOptions,
}

struct Entry {
    project: Project,
    /// What the runtime was loaded from; see [`fingerprint`].
    fingerprint: u64,
    /// When the fingerprint was last found unchanged.
    checked: Instant,
    /// When the registry became the runtime's only holder.
    unused_since: Option<Instant>,
}

/// Builds the runtime of the project at a workdir, given what
/// `kage.config()` reports for its model and system prompt and the
/// environment work no session asked for runs with.
type Loader = fn(&Path, &str, &str, &Environment) -> Option<Project>;

/// The runtimes of the projects the host serves, by canonical workdir.
pub(super) struct Projects {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// Held while a runtime loads, so two sessions opening one project at
    /// once load its plugins once.
    loading: Mutex<()>,
    load: Loader,
    fingerprint: fn(&Path) -> u64,
}

impl Default for Projects {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            loading: Mutex::default(),
            load,
            fingerprint,
        }
    }
}

impl Projects {
    /// The runtime of the project at `workdir`, loaded when the project has
    /// none or what it loads from changed, with `model` and the prompt
    /// `system_prompt` builds as what `kage.config()` reports, and `env`
    /// for work no session asked for. What it loads from is read at most
    /// every [`RECHECK`]. `None` when the runtime cannot be built; sessions
    /// then run without plugins.
    pub(super) fn get(
        &self,
        workdir: &Path,
        model: &str,
        system_prompt: &dyn Fn() -> String,
        env: &Environment,
    ) -> Option<Project> {
        self.prune();
        let key = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        if let Some(entry) = lock(&self.entries)
            .get_mut(&key)
            .filter(|entry| entry.checked.elapsed() < RECHECK)
        {
            entry.unused_since = None;
            return Some(entry.project.clone());
        }
        let fingerprint = (self.fingerprint)(&key);
        let current = |entries: &mut HashMap<PathBuf, Entry>| {
            let entry = entries
                .get_mut(&key)
                .filter(|entry| entry.fingerprint == fingerprint)?;
            entry.unused_since = None;
            entry.checked = Instant::now();
            Some(entry.project.clone())
        };
        if let Some(project) = current(&mut lock(&self.entries)) {
            return Some(project);
        }
        let _loading = lock(&self.loading);
        if let Some(project) = current(&mut lock(&self.entries)) {
            return Some(project);
        }
        let project = (self.load)(&key, model, &system_prompt(), env)?;
        lock(&self.entries).insert(
            key,
            Entry {
                project: project.clone(),
                fingerprint,
                checked: Instant::now(),
                unused_since: None,
            },
        );
        Some(project)
    }

    /// Drop the runtimes no session has held for [`GRACE`].
    pub(super) fn prune(&self) {
        self.prune_at(Instant::now());
    }

    fn prune_at(&self, now: Instant) {
        lock(&self.entries).retain(|_, entry| {
            if Arc::strong_count(&entry.project.runtime) > 1 {
                entry.unused_since = None;
                return true;
            }
            let since = *entry.unused_since.get_or_insert(now);
            now.duration_since(since) < GRACE
        });
    }
}

/// Build and load the runtime of the project at `workdir`. Its config
/// decides which plugins load and what they may do, as for the TUI.
fn load(workdir: &Path, model: &str, system_prompt: &str, env: &Environment) -> Option<Project> {
    let config = Config::load_layered(workdir).unwrap_or_else(|e| {
        eprintln!("kage: plugins: {e}; loading with the defaults");
        Config::default()
    });
    let (store, errors) = OptionStore::from_config(&config);
    for err in errors {
        eprintln!("kage: config: {err}");
    }
    let options: SharedOptions = Arc::new(Mutex::new(store));
    let plugins_dir = crate::plugins_dir().ok();
    let user_dir = crate::config_dir().ok();
    match crate::plugins::setup_user_runtime(
        plugins_dir.as_deref(),
        user_dir.as_deref(),
        config.plugins,
        config.keybindings,
        Arc::clone(&options),
        workdir,
        model,
        system_prompt,
        kage_plugin::default_host_log(),
        env,
    ) {
        Ok(runtime) => Some(Project { runtime, options }),
        Err(e) => {
            eprintln!("kage: {e}");
            None
        }
    }
}

/// A hash of what a project's runtime loads from: every file under the
/// plugins directory and the user's `lua/` modules, `init.lua`, the user
/// and project config files and the trust store, by path, size and
/// modification time.
fn fingerprint(workdir: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut add = |path: &Path| {
        let meta = std::fs::metadata(path).ok();
        path.hash(&mut hasher);
        meta.as_ref().map(std::fs::Metadata::len).hash(&mut hasher);
        meta.and_then(|meta| meta.modified().ok()).hash(&mut hasher);
    };
    let mut files = Vec::new();
    if let Ok(dir) = crate::plugins_dir() {
        walk(&dir, &mut files);
    }
    if let Ok(dir) = crate::config_dir() {
        files.push(dir.join("init.lua"));
        walk(&dir.join("lua"), &mut files);
    }
    files.extend(Config::default_path());
    files.push(Config::project_path(workdir));
    files.extend(Config::state_dir().map(|dir| dir.join("trust.json")));
    for file in &files {
        add(file);
    }
    hasher.finish()
}

/// Whether anything would load into a project runtime: an entry under
/// the plugins directory, the user's `init.lua`, or the user's `lua/`
/// modules. When none does, nothing a session consumes would register
/// in one, so sessions skip building it.
pub(super) fn has_plugin_content() -> bool {
    has_plugin_content_in(
        crate::plugins_dir().ok().as_deref(),
        crate::config_dir().ok().as_deref(),
    )
}

/// [`has_plugin_content`] over given directories, so tests need no
/// environment isolation.
fn has_plugin_content_in(plugins_dir: Option<&Path>, user_dir: Option<&Path>) -> bool {
    let occupied =
        |dir: &Path| std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some());
    plugins_dir.is_some_and(occupied)
        || user_dir.is_some_and(|dir| dir.join("init.lua").is_file() || occupied(&dir.join("lua")))
}

/// Collect every file under `dir`, at most a few thousand, in a stable
/// order.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    const LIMIT: usize = 4096;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if files.len() >= LIMIT {
            return;
        }
        if path.is_dir() {
            walk(&path, files);
        } else {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each kind of plugin content decides whether a project runtime
    /// loads at all: nothing anywhere loads none, a plugin file, the
    /// user's `init.lua` or a `lua/` module each load one.
    #[test]
    fn a_runtime_loads_only_for_plugin_content() {
        let empty = tempfile::tempdir().unwrap();
        assert!(!has_plugin_content_in(None, None));
        assert!(!has_plugin_content_in(Some(empty.path()), None));
        assert!(!has_plugin_content_in(None, Some(empty.path())));

        let plugins = tempfile::tempdir().unwrap();
        std::fs::write(plugins.path().join("greet.lua"), "").unwrap();
        assert!(has_plugin_content_in(Some(plugins.path()), None));

        let user = tempfile::tempdir().unwrap();
        std::fs::write(user.path().join("init.lua"), "").unwrap();
        assert!(has_plugin_content_in(None, Some(user.path())));

        std::fs::create_dir(user.path().join("lua")).unwrap();
        std::fs::write(user.path().join("lua").join("util.lua"), "").unwrap();
        assert!(has_plugin_content_in(None, Some(user.path())));
    }

    /// Sessions in one project share its runtime, another project gets its
    /// own, and a runtime nothing holds goes after the grace period.
    #[test]
    fn a_project_runtime_is_shared_and_dropped_once_unused() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let projects = Projects {
            entries: Mutex::default(),
            loading: Mutex::default(),
            fingerprint: |_| 0,
            load: |_, _, _, _| {
                Some(Project {
                    runtime: Arc::new(PluginRuntime::new().unwrap()),
                    options: SharedOptions::default(),
                })
            },
        };
        let first = projects
            .get(one.path(), "mock/m", &String::new, &Environment::inherit())
            .unwrap();
        let second = projects
            .get(one.path(), "mock/m", &String::new, &Environment::inherit())
            .unwrap();
        assert!(Arc::ptr_eq(&first.runtime, &second.runtime));
        let elsewhere = projects
            .get(
                other.path(),
                "mock/m",
                &String::new,
                &Environment::inherit(),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first.runtime, &elsewhere.runtime));
        let count = || lock(&projects.entries).len();
        assert_eq!(count(), 2);

        projects.prune_at(Instant::now() + GRACE + Duration::from_secs(1));
        assert_eq!(count(), 2, "runtimes a session holds stay");
        drop((first, second, elsewhere));
        projects.prune();
        assert_eq!(count(), 2, "unused runtimes stay for the grace period");
        projects.prune_at(Instant::now() + GRACE + Duration::from_secs(1));
        assert_eq!(count(), 0);
    }

    /// A change to what a project's runtime loads from gives the next
    /// session a fresh runtime, while sessions already open keep theirs.
    #[test]
    fn a_changed_project_loads_again_for_the_next_session() {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = tempfile::tempdir().unwrap();
        let projects = Projects {
            entries: Mutex::default(),
            loading: Mutex::default(),
            fingerprint: |_| GENERATION.load(std::sync::atomic::Ordering::SeqCst),
            load: |_, _, _, _| {
                Some(Project {
                    runtime: Arc::new(PluginRuntime::new().unwrap()),
                    options: SharedOptions::default(),
                })
            },
        };
        let get = || {
            projects
                .get(dir.path(), "mock/m", &String::new, &Environment::inherit())
                .unwrap()
        };
        let open = get();
        assert!(Arc::ptr_eq(&open.runtime, &get().runtime));
        GENERATION.store(1, std::sync::atomic::Ordering::SeqCst);
        // The recheck window trusts the fingerprint for a moment; expire it.
        let key = dir.path().canonicalize().unwrap();
        lock(&projects.entries).get_mut(&key).unwrap().checked =
            Instant::now().checked_sub(RECHECK).unwrap();
        let fresh = get();
        assert!(!Arc::ptr_eq(&open.runtime, &fresh.runtime));
        assert!(Arc::ptr_eq(&fresh.runtime, &get().runtime));
    }
}
