//! Discover and execute `*.lua` plugin files in a directory.
//!
//! [`load_all`] first evaluates the embedded `_defaults.lua` in its own
//! environment, then reads every `*.lua` file in the plugins directory
//! and evaluates it inside the given [`PluginRuntime`], in file-name
//! order, then applies the `[keybindings]` table from `config.toml`
//! (see [`crate::PluginRuntimeBuilder::keybindings`]), and finally
//! evaluates the trusted `init.lua` when the runtime has a user dir
//! (see [`crate::user`]). Each later layer overrides the earlier ones.
//! Each file is loaded independently: a broken plugin, a bad
//! `[keybindings]` entry or a broken `init.lua` logs an error through
//! the runtime's host log and the load proceeds. The function returns a
//! summary the host can surface to the user. File stems starting with
//! `@` are reserved for kage's own environments and are rejected.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kage_core::sync::lock;
use mlua::Lua;

use crate::api::LogLevel;
use crate::error::PluginError;
use crate::runtime::{EvalState, PluginRuntime};

/// Outcome of [`load_dir`]: paths that loaded cleanly and ones that did not.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Plugin files whose chunk evaluated without raising.
    pub loaded: Vec<PathBuf>,
    /// Plugin files that failed, paired with the error encountered.
    pub failed: Vec<(PathBuf, String)>,
    /// Plugin files skipped because a non-empty `[plugins] enabled`
    /// allowlist did not name them. Reported, never silently dropped.
    pub skipped: Vec<PathBuf>,
    /// Outcome of the trusted `init.lua`, or `None` when there was none
    /// to load.
    pub init: Option<Result<(), String>>,
    /// One message per `[keybindings]` entry that could not be applied.
    pub keymap_errors: Vec<String>,
}

impl LoadReport {
    /// True if every plugin file, every `[keybindings]` entry and
    /// `init.lua` loaded successfully.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.failed.is_empty()
            && self.keymap_errors.is_empty()
            && !matches!(self.init, Some(Err(_)))
    }
}

/// Evaluate every `*.lua` file in `dir` against `runtime`.
///
/// Behavior on each file:
/// * Read the file from disk (errors logged + recorded, file skipped).
/// * Evaluate as a Lua chunk (errors logged + recorded, file skipped).
///
/// The whole directory loads as one job on the runtime's Lua owner
/// thread. Files are processed sorted by file name, so a plugin named
/// `a.lua` always loads before `b.lua`.
pub fn load_dir(dir: &Path, runtime: &PluginRuntime) -> Result<LoadReport, PluginError> {
    load_all(Some(dir), runtime)
}

/// Run the full load against `runtime`: `_defaults.lua`, the plugins in
/// `plugins_dir` as [`load_dir`] does, the `[keybindings]` table, then
/// the trusted `init.lua` when the runtime has a user dir. `None` loads
/// no plugins.
pub fn load_all(
    plugins_dir: Option<&Path>,
    runtime: &PluginRuntime,
) -> Result<LoadReport, PluginError> {
    let eval = Arc::clone(&runtime.eval);
    let dir = plugins_dir.map(Path::to_path_buf);
    runtime
        .host
        .call(move |lua| load_on(lua, dir.as_deref(), &eval))?
}

/// Body of [`load_all`], run on the owner thread.
pub(crate) fn load_on(
    lua: &Lua,
    dir: Option<&Path>,
    eval: &EvalState,
) -> Result<LoadReport, PluginError> {
    if let Err(err) = eval.eval_defaults(lua) {
        lock(eval.sink()).log(LogLevel::Error, &format!("_defaults.lua: {err}"));
    }
    let mut report = match dir {
        Some(dir) => load_plugins(lua, dir, eval)?,
        None => LoadReport::default(),
    };
    report.keymap_errors = eval.keymaps.apply_toml(lua, &eval.keybindings)?;
    for err in &report.keymap_errors {
        lock(eval.sink()).log(LogLevel::Error, err);
    }
    report.init = crate::user::load(lua, eval);
    Ok(report)
}

fn load_plugins(lua: &Lua, dir: &Path, eval: &EvalState) -> Result<LoadReport, PluginError> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(LoadReport::default()),
        Err(err) => {
            return Err(PluginError::Io {
                path: dir.to_path_buf(),
                source: err,
            });
        }
    };

    let mut paths = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| PluginError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("lua") {
            paths.push(path);
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let sink = eval.sink();
    let mut report = LoadReport::default();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        if name.starts_with('@') {
            let err = "names starting with '@' are reserved";
            lock(sink).log(
                LogLevel::Error,
                &format!("plugin '{}': {err}", path.display()),
            );
            report.failed.push((path, err.to_owned()));
            continue;
        }
        if !eval.is_enabled(name) {
            let mut s = lock(sink);
            s.log(
                LogLevel::Info,
                &format!("plugin '{name}' not in [plugins] enabled allowlist; skipped"),
            );
            report.skipped.push(path);
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(source) => match eval.eval_plugin(lua, name, &source) {
                Ok(_) => report.loaded.push(path),
                Err(err) => {
                    let msg = format!("plugin '{}': {err}", path.display());
                    let mut s = lock(sink);
                    s.log(LogLevel::Error, &msg);
                    report.failed.push((path, err.to_string()));
                }
            },
            Err(err) => {
                let msg = format!("plugin '{}': read failed: {err}", path.display());
                let mut s = lock(sink);
                s.log(LogLevel::Error, &msg);
                report.failed.push((path, err.to_string()));
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn loads_every_lua_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.lua"),
            "kage.register_command({ name='a', description='', handler=function() end })",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.lua"),
            "kage.register_command({ name='b', description='', handler=function() end })",
        )
        .unwrap();
        fs::write(dir.path().join("notes.txt"), "skipped").unwrap();

        let rt = PluginRuntime::new().unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        assert_eq!(report.loaded.len(), 2);
        assert!(report.failed.is_empty());
        assert_eq!(rt.registered_commands().len(), 2);
    }

    #[test]
    fn loads_files_sorted_by_name() {
        let dir = tempdir().unwrap();
        for name in ["z", "a", "m"] {
            fs::write(dir.path().join(format!("{name}.lua")), "").unwrap();
        }
        let rt = PluginRuntime::new().unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        let names: Vec<_> = report
            .loaded
            .iter()
            .map(|p| p.file_stem().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["a", "m", "z"]);
    }

    #[test]
    fn defaults_run_before_plugins() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.lua"),
            "kage.on('agent_start', function() kage.notify('a') end)",
        )
        .unwrap();
        let (rec, sink) = crate::testing::recording_sink();
        let rt = PluginRuntime::builder()
            .sink(sink)
            .defaults("kage.on('agent_start', function() kage.notify('defaults') end)")
            .build()
            .unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        assert_eq!(report.loaded.len(), 1);
        rt.dispatch_event("agent_start", &serde_json::json!({}))
            .unwrap();
        assert_eq!(rec.snapshot().notifications, ["defaults", "a"]);

        rt.reload_dir(dir.path()).unwrap();
        assert_eq!(rt.handler_count("agent_start"), 2);
    }

    #[test]
    fn embedded_defaults_load_cleanly() {
        let (rec, rt) = crate::testing::runtime_with_recording(PathBuf::from("."));
        load_dir(Path::new("/nonexistent/here"), &rt).unwrap();
        assert!(rec.snapshot().logs.is_empty());
    }

    #[test]
    fn enabled_allowlist_skips_unlisted_plugins() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("trusted.lua"),
            "kage.register_command({ name='t', description='', handler=function() end })",
        )
        .unwrap();
        fs::write(
            dir.path().join("other.lua"),
            "kage.register_command({ name='o', description='', handler=function() end })",
        )
        .unwrap();

        let rt = PluginRuntime::builder()
            .enabled(vec!["trusted".to_owned()])
            .build()
            .unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        assert_eq!(report.loaded.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].ends_with("other.lua"));
        let commands = rt.registered_commands();
        assert_eq!(commands.len(), 1);
        assert!(rt.registered_commands().iter().any(|c| c.name() == "t"));
        assert!(!rt.registered_commands().iter().any(|c| c.name() == "o"));
    }

    #[test]
    fn empty_allowlist_loads_everything() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.lua"),
            "kage.register_command({ name='a', description='', handler=function() end })",
        )
        .unwrap();
        let rt = PluginRuntime::builder().enabled(vec![]).build().unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        assert_eq!(report.loaded.len(), 1);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn one_broken_plugin_does_not_abort_the_rest() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.lua"), "this is not valid lua = =").unwrap();
        fs::write(
            dir.path().join("b.lua"),
            "kage.register_command({ name='b', description='', handler=function() end })",
        )
        .unwrap();

        let rt = PluginRuntime::new().unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        assert_eq!(report.loaded.len(), 1);
        assert_eq!(report.failed.len(), 1);
        assert!(!report.all_ok());
        assert_eq!(rt.registered_commands().len(), 1);
    }

    #[test]
    fn missing_dir_is_treated_as_empty() {
        let rt = PluginRuntime::new().unwrap();
        let report = load_dir(Path::new("/nonexistent/here"), &rt).unwrap();
        assert!(report.loaded.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn ignores_non_lua_extensions() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "not a plugin").unwrap();
        fs::write(dir.path().join("README.md"), "# nope").unwrap();
        let rt = PluginRuntime::new().unwrap();
        let report = load_dir(dir.path(), &rt).unwrap();
        assert!(report.loaded.is_empty());
        assert!(report.failed.is_empty());
    }
}
