//! `kage.fs.read` / `kage.fs.write` - filesystem helpers confined to the
//! plugin's workdir.
//!
//! The host supplies a workdir at runtime construction. Every path passed
//! by Lua is resolved through [`kage_tools::resolve_under`], which rejects
//! anything that would escape the workdir (absolute paths, `..`, symlinks
//! pointing outside). On any rejection the helper raises a Lua error so
//! plugins fail loudly rather than silently writing to the wrong place.
//!
//! Built-in tools use the looser [`kage_tools::resolve`] (no escape check)
//! because the model already has shell access via `bash`; plugins keep the
//! tighter check because they are third-party code in a sandbox.

use std::path::{Path, PathBuf};

use kage_tools::resolve_under;
use mlua::{Lua, Table};

use crate::error::PluginError;

/// Install `kage.fs.read` and `kage.fs.write` on the running Lua state.
///
/// Both helpers anchor at `workdir`. Pass an absolute path here; relative
/// paths are interpreted against the process cwd at install time.
pub fn install_fs(lua: &Lua, workdir: PathBuf) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let fs = lua.create_table()?;

    let read_root = workdir.clone();
    fs.set(
        "read",
        lua.create_function(move |_, path: String| {
            let resolved = resolve(&read_root, &path)?;
            std::fs::read_to_string(&resolved)
                .map_err(|err| mlua::Error::external(format!("read {path}: {err}")))
        })?,
    )?;

    let write_root = workdir;
    fs.set(
        "write",
        lua.create_function(move |_, (path, content): (String, mlua::String)| {
            let resolved = resolve(&write_root, &path)?;
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| mlua::Error::external(format!("mkdirp {path}: {err}")))?;
            }
            std::fs::write(&resolved, content.as_bytes().as_ref())
                .map_err(|err| mlua::Error::external(format!("write {path}: {err}")))?;
            Ok(())
        })?,
    )?;

    kage.set("fs", fs)?;
    Ok(())
}

fn resolve(root: &Path, candidate: &str) -> mlua::Result<PathBuf> {
    resolve_under(root, Path::new(candidate))
        .map_err(|err| mlua::Error::external(format!("path {candidate}: {err}")))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::PluginRuntime;

    #[test]
    fn read_inside_workdir_succeeds() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), "world").unwrap();
        let rt = PluginRuntime::builder()
            .workdir(dir.path().to_path_buf())
            .build()
            .unwrap();
        let v: String = rt.eval("return kage.fs.read('hello.txt')").unwrap_lua();
        assert_eq!(v, "world");
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let rt = PluginRuntime::builder()
            .workdir(dir.path().to_path_buf())
            .build()
            .unwrap();
        rt.eval("kage.fs.write('out/log.txt', 'hi')").unwrap();
        let on_disk = fs::read_to_string(dir.path().join("out/log.txt")).unwrap();
        assert_eq!(on_disk, "hi");
    }

    #[test]
    fn escaping_path_raises() {
        let dir = tempdir().unwrap();
        let rt = PluginRuntime::builder()
            .workdir(dir.path().to_path_buf())
            .build()
            .unwrap();
        let res = rt.eval("return kage.fs.read('../etc/passwd')");
        assert!(res.is_err(), "escaping path must error, got {res:?}");
    }

    #[test]
    fn absolute_path_is_rejected() {
        let dir = tempdir().unwrap();
        let rt = PluginRuntime::builder()
            .workdir(dir.path().to_path_buf())
            .build()
            .unwrap();
        let res = rt.eval("return kage.fs.read('/etc/passwd')");
        assert!(res.is_err());
    }

    /// Test helper: extract a Lua String from the runtime's value-returning
    /// `eval`, panicking on type mismatch with a useful message.
    trait UnwrapLua {
        fn unwrap_lua(self) -> String;
    }
    impl UnwrapLua for Result<mlua::Value, crate::PluginError> {
        fn unwrap_lua(self) -> String {
            match self.unwrap() {
                mlua::Value::String(s) => s.to_str().unwrap().to_owned(),
                other => panic!("expected string, got {other:?}"),
            }
        }
    }
}
