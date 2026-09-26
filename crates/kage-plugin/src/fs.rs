//! `kage.fs.read` / `kage.fs.write` - filesystem helpers confined to the
//! plugin's workdir.
//!
//! The host supplies a workdir at runtime construction. Every path passed
//! by Lua is resolved through [`kage_tools::resolve_under`], which
//! normalizes `..` lexically, rejects absolute paths outside the workdir,
//! and canonicalizes the existing ancestor chain so that symlinks
//! resolving outside are rejected. On any rejection the helper raises a
//! Lua error so plugins fail loudly rather than silently touching the
//! wrong place.
//!
//! Writes additionally re-verify containment after creating parent
//! directories: `create_dir_all` follows symlinks, so a symlink planted
//! inside the workdir (by a shell command or the user - the plugin API
//! cannot create symlinks) must not redirect the write. A local process
//! swapping a symlink into place between the final check and the write
//! remains outside the threat model.
//!
//! Built-in tools use the looser [`kage_tools::resolve`] (no escape check)
//! because the model already has shell access via the `shell` tool; plugins keep the
//! tighter check because they are third-party code in a sandbox.

use std::path::{Path, PathBuf};

use kage_core::sync::lock;
use kage_tools::resolve_under;
use mlua::{Lua, Table};

use crate::capabilities::{Capability, CapabilityRegistry};
use crate::error::PluginError;

/// Install `kage.fs.read` on the running Lua state.
///
/// The helper anchors at `workdir`. Pass an absolute path here; relative
/// paths are interpreted against the process cwd at install time.
/// `kage.fs.write` is attached separately through the `fs_write`
/// capability (see [`register`]), so an ungranted plugin can look but
/// not touch.
pub fn install_fs(lua: &Lua, workdir: &Path) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let fs = lua.create_table()?;

    let read_root = workdir.to_path_buf();
    fs.set(
        "read",
        lua.create_function(move |_, path: String| {
            let resolved = resolve(&read_root, &path)?;
            std::fs::read_to_string(&resolved)
                .map_err(|err| mlua::Error::external(format!("read {path}: {err}")))
        })?,
    )?;

    kage.set("fs", fs)?;
    Ok(())
}

/// Register the `fs_write` installer that shadows `kage.fs` on a
/// granted plugin's `kage` proxy with one that adds `write`, reading
/// through to the shared base table.
pub(crate) fn register(registry: &CapabilityRegistry, workdir: PathBuf) {
    let mut reg = lock(registry);
    reg.entry(Capability::FsWrite)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
            let kage: Table = lua.globals().get("kage")?;
            let base_fs: Table = kage.get("fs")?;
            let pfs = lua.create_table()?;

            let write_root = workdir.clone();
            pfs.set(
                "write",
                lua.create_function(move |_, (path, content): (String, mlua::String)| {
                    let resolved = resolve(&write_root, &path)?;
                    write_confined(&write_root, &resolved, content.as_bytes().as_ref())
                        .map_err(|err| mlua::Error::external(format!("write {path}: {err}")))?;
                    Ok(())
                })?,
            )?;

            let mt = lua.create_table()?;
            mt.set("__index", base_fs)?;
            mt.set("__metatable", false)?;
            pfs.set_metatable(Some(mt))?;
            pkage.set("fs", pfs)?;
            Ok(())
        }));
}

fn resolve(root: &Path, candidate: &str) -> mlua::Result<PathBuf> {
    resolve_under(root, Path::new(candidate))
        .map_err(|err| mlua::Error::external(format!("path {candidate}: {err}")))
}

/// Write `content` to `resolved`, keeping the whole operation inside
/// `root`.
///
/// `create_dir_all` resolves symlinks in the parent chain, so the parent
/// is re-canonicalized afterwards and verified against `root`; the target
/// itself must not be a symlink.
fn write_confined(root: &Path, resolved: &Path, content: &[u8]) -> std::io::Result<()> {
    let root = root.canonicalize()?;
    let Some(parent) = resolved.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        ));
    };
    if parent
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "parent directory is a symlink",
        ));
    }
    std::fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("parent directory escapes workdir {}", root.display()),
        ));
    }
    let Some(name) = resolved.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no file name",
        ));
    };
    let target = parent.join(name);
    if target
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "target is a symlink",
        ));
    }
    std::fs::write(&target, content)
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use crate::PluginRuntime;

    /// A runtime whose plugin `t` holds `fs_write`, for exercising the
    /// granted write path.
    fn granted_fs_write(workdir: &std::path::Path) -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("t".to_owned(), vec!["fs_write".to_owned()]);
        PluginRuntime::builder()
            .workdir(workdir.to_path_buf())
            .capabilities(caps)
            .build()
            .unwrap()
    }

    fn granted_write(
        rt: &PluginRuntime,
        code: &str,
    ) -> Result<mlua::Value, crate::error::PluginError> {
        rt.eval_plugin(
            "t",
            &format!("kage.request_capabilities({{'fs_write'}}); {code}"),
        )
    }

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
        let rt = granted_fs_write(dir.path());
        granted_write(&rt, "kage.fs.write('out/log.txt', 'hi')").unwrap();
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

    #[test]
    fn dot_dot_over_missing_component_rejects_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let parent = dir.path().parent().unwrap();
        let rt = granted_fs_write(dir.path());
        let res = granted_write(&rt, "kage.fs.write('a/../../escape.txt', 'x')");
        assert!(res.is_err(), "traversal must error, got {res:?}");
        assert!(!parent.join("escape.txt").exists());
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn dot_dot_over_missing_component_staying_inside_writes() {
        let dir = tempdir().unwrap();
        let rt = granted_fs_write(dir.path());
        granted_write(&rt, "kage.fs.write('missing/../ok.txt', 'hi')").unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("ok.txt")).unwrap(), "hi");
    }

    #[test]
    fn write_is_absent_without_fs_write_capability() {
        let dir = tempdir().unwrap();
        let rt = PluginRuntime::builder()
            .workdir(dir.path().to_path_buf())
            .build()
            .unwrap();
        let out = rt
            .eval_plugin("u", "return kage.fs.write == nil and kage.fs.read ~= nil")
            .unwrap();
        assert_eq!(out, mlua::Value::Boolean(true));
    }

    #[cfg(unix)]
    #[test]
    fn write_through_dangling_symlink_parent_is_rejected() {
        let dir = tempdir().unwrap();
        let parent = dir.path().parent().unwrap();
        symlink("../escape-target", dir.path().join("dangling")).unwrap();
        let rt = granted_fs_write(dir.path());
        let res = granted_write(&rt, "kage.fs.write('dangling/x.txt', 'x')");
        assert!(res.is_err(), "symlinked parent must error, got {res:?}");
        assert!(
            !parent.join("escape-target").exists(),
            "no side effect outside"
        );
        assert!(!parent.join("escape-target").join("x.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_onto_dangling_symlink_name_is_rejected() {
        let dir = tempdir().unwrap();
        let parent = dir.path().parent().unwrap();
        symlink("../escape-file", dir.path().join("link")).unwrap();
        let rt = granted_fs_write(dir.path());
        let res = granted_write(&rt, "kage.fs.write('link', 'x')");
        assert!(res.is_err(), "symlink target must error, got {res:?}");
        assert!(!parent.join("escape-file").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_through_symlink_into_workdir_still_works() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        symlink("sub", dir.path().join("alias")).unwrap();
        let rt = granted_fs_write(dir.path());
        granted_write(&rt, "kage.fs.write('alias/x.txt', 'hi')").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("sub/x.txt")).unwrap(),
            "hi"
        );
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
