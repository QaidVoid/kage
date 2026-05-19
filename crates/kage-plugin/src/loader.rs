//! Discover and execute `*.lua` plugin files in a directory.
//!
//! [`load_dir`] reads every `*.lua` file in `dir` and evaluates it inside
//! the given [`PluginRuntime`]. Each file is loaded independently: a
//! broken plugin logs an error through the runtime's host log and is
//! skipped while the next file proceeds. The function returns a summary
//! the host can surface to the user.

use std::path::{Path, PathBuf};

use crate::api::LogLevel;
use crate::error::PluginError;
use crate::runtime::PluginRuntime;

/// Outcome of [`load_dir`]: paths that loaded cleanly and ones that did not.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Plugin files whose chunk evaluated without raising.
    pub loaded: Vec<PathBuf>,
    /// Plugin files that failed, paired with the error encountered.
    pub failed: Vec<(PathBuf, String)>,
}

impl LoadReport {
    /// True if every plugin file loaded successfully.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Evaluate every `*.lua` file in `dir` against `runtime`.
///
/// Behavior on each file:
/// * Read the file from disk (errors logged + recorded, file skipped).
/// * Evaluate as a Lua chunk (errors logged + recorded, file skipped).
///
/// Files are processed in directory-iteration order; order between
/// different filesystems is not stable.
pub fn load_dir(dir: &Path, runtime: &PluginRuntime) -> Result<LoadReport, PluginError> {
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

    let sink = runtime.sink();
    let mut report = LoadReport::default();
    for entry in read_dir {
        let entry = entry.map_err(|err| PluginError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("lua") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(source) => {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("plugin");
                match runtime.eval_plugin(name, &source) {
                    Ok(_) => report.loaded.push(path),
                    Err(err) => {
                        let msg = format!("plugin '{}': {err}", path.display());
                        if let Ok(mut s) = sink.lock() {
                            s.log(LogLevel::Error, &msg);
                        }
                        report.failed.push((path, err.to_string()));
                    }
                }
            }
            Err(err) => {
                let msg = format!("plugin '{}': read failed: {err}", path.display());
                if let Ok(mut s) = sink.lock() {
                    s.log(LogLevel::Error, &msg);
                }
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
