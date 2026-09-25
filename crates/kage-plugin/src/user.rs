//! The trusted user config: `init.lua` and its `lua/` modules.
//!
//! `<user_dir>/init.lua` runs last on every load, after `_defaults.lua`
//! and the plugins, so it overrides both. It runs in the `@user`
//! environment, which is built like a plugin environment under a name no
//! plugin file can take, and which has every capability attached without
//! the request handshake. The sandbox removals still apply, so it has no
//! raw `io`, `os.execute` or `debug`.
//!
//! The environment also gets a `require` confined to `<user_dir>/lua/`.
//! Module names are dot-separated segments of `[A-Za-z0-9_-]`. `a.b`
//! resolves to `lua/a/b.lua`, then `lua/a/b/init.lua`, and a path that
//! canonicalizes outside `lua/` (through a symlink) is rejected. Modules
//! run in the user environment and are cached until the next reload.
//!
//! The environment is private like every plugin environment, so plugins
//! cannot reach its capabilities, its `require` or the module cache.

use std::path::{Path, PathBuf};

use kage_core::sync::lock;
use mlua::{Function, Lua, Table};

use crate::api::LogLevel;
use crate::capabilities;
use crate::error::PluginError;
use crate::runtime::EvalState;

/// Environment name of the trusted user config. The `@` prefix is
/// reserved, so no plugin file stem can collide with it.
pub(crate) const USER_ENV: &str = "@user";

/// Source of the `require` wrapper that caches modules and detects
/// cycles over the Rust searcher.
const REQUIRE: &str = include_str!("../lua/require.lua");

/// Evaluate `init.lua` from the runtime's user dir. Returns `None` when
/// no user dir is configured or it holds no `init.lua`. A failure is
/// logged through the host log and returned.
pub(crate) fn load(lua: &Lua, eval: &EvalState) -> Option<Result<(), String>> {
    let dir = eval.user_dir.as_deref()?;
    let path = dir.join("init.lua");
    let result = match std::fs::read_to_string(&path) {
        Ok(source) => eval_init(lua, eval, dir, &path, &source).map_err(|err| err.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => Err(format!("read failed: {err}")),
    };
    if let Err(err) = &result {
        lock(eval.sink()).log(LogLevel::Error, &format!("init.lua: {err}"));
    }
    Some(result)
}

fn eval_init(
    lua: &Lua,
    eval: &EvalState,
    dir: &Path,
    path: &Path,
    source: &str,
) -> Result<(), PluginError> {
    let env = user_env(lua, eval, dir.join("lua"))?;
    let chunk = format!("@{}", path.display());
    eval.eval_in(lua, USER_ENV, &chunk, env, source)?;
    Ok(())
}

/// Get or create the user environment and attach the trusted surface:
/// every capability and a `require` confined to `modules`.
fn user_env(lua: &Lua, eval: &EvalState, modules: PathBuf) -> mlua::Result<Table> {
    let env = eval.env(lua, USER_ENV)?;
    let kage: Table = env.raw_get("kage")?;
    capabilities::install_trusted(lua, &eval.capabilities, &kage)?;
    env.raw_set("require", require_fn(lua, &env, modules)?)?;
    Ok(env)
}

fn require_fn(lua: &Lua, env: &Table, root: PathBuf) -> mlua::Result<Function> {
    let find = lua.create_function(move |lua, (name, env): (String, Table)| {
        let path = resolve(&root, &name).map_err(mlua::Error::external)?;
        let source = std::fs::read_to_string(&path)
            .map_err(|err| mlua::Error::external(format!("require: {}: {err}", path.display())))?;
        lua.load(source)
            .set_name(format!("@{}", path.display()))
            .set_environment(env)
            .into_function()
    })?;
    lua.load(REQUIRE)
        .set_name("=kage/require.lua")
        .call((find, env.clone()))
}

/// Resolve module `name` to a file under `root`: `a.b` tries
/// `a/b.lua`, then `a/b/init.lua`. Invalid names and files whose
/// canonical path leaves `root` are rejected.
fn resolve(root: &Path, name: &str) -> Result<PathBuf, String> {
    let valid = !name.is_empty()
        && name.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    if !valid {
        return Err(format!("require: invalid module name '{name}'"));
    }
    let not_found = || {
        format!(
            "require: module '{name}' not found under {}",
            root.display()
        )
    };
    let root = root.canonicalize().map_err(|_| not_found())?;
    let base: PathBuf = name.split('.').collect();
    for candidate in [base.with_extension("lua"), base.join("init.lua")] {
        let Ok(path) = root.join(candidate).canonicalize() else {
            continue;
        };
        if !path.starts_with(&root) {
            return Err(format!("require: module '{name}' resolves outside lua/"));
        }
        return Ok(path);
    }
    Err(not_found())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::PluginRuntime;
    use crate::loader::load_all;
    use crate::testing::{RecordingSink, recording_sink};

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn user_runtime(user: &Path) -> (RecordingSink, PluginRuntime) {
        let (rec, sink) = recording_sink();
        let rt = PluginRuntime::builder()
            .sink(sink)
            .user_dir(Some(user.to_path_buf()))
            .build()
            .unwrap();
        (rec, rt)
    }

    fn init_error(user: &TempDir, init: &str) -> String {
        write(user.path(), "init.lua", init);
        let (_, rt) = user_runtime(user.path());
        match load_all(None, &rt).unwrap().init {
            Some(Err(err)) => err,
            other => panic!("expected an init.lua error, got {other:?}"),
        }
    }

    #[test]
    fn init_has_every_capability_and_plugins_do_not() {
        let user = tempdir().unwrap();
        write(
            user.path(),
            "init.lua",
            "secret = 1
             local caps = kage.request_capabilities({ 'exec', 'net' })
             kage.notify(table.concat({
               type(kage.exec), type(kage.env), type(kage.http),
               type(kage.session.fork_to), tostring(caps.exec and caps.net),
               tostring(io == nil and os.execute == nil and debug == nil),
             }, ' '))",
        );
        let (rec, rt) = user_runtime(user.path());
        let report = load_all(None, &rt).unwrap();
        assert_eq!(report.init, Some(Ok(())));
        assert_eq!(
            rec.snapshot().notifications,
            ["function function table function true true"]
        );
        let plugin = rt
            .eval_plugin(
                "p",
                "return kage.exec == nil and kage.env == nil and require == nil \
                 and secret == nil and not kage.request_capabilities({ 'exec' }).exec",
            )
            .unwrap();
        assert_eq!(plugin.as_boolean(), Some(true));
    }

    #[test]
    fn require_loads_files_and_init_modules_once() {
        let user = tempdir().unwrap();
        write(
            user.path(),
            "lua/a/b.lua",
            "count = (count or 0) + 1 return { v = 'file' }",
        );
        write(user.path(), "lua/c/d/init.lua", "return 'dir'");
        write(
            user.path(),
            "init.lua",
            "local ab = require('a.b')
             kage.notify(ab.v .. ' ' .. require('c.d') .. ' '
               .. tostring(require('a.b') == ab) .. ' ' .. count)",
        );
        let (rec, rt) = user_runtime(user.path());
        assert_eq!(load_all(None, &rt).unwrap().init, Some(Ok(())));
        assert_eq!(rec.snapshot().notifications, ["file dir true 1"]);
    }

    #[test]
    fn require_rejects_invalid_names() {
        let root = tempdir().unwrap();
        write(root.path(), "a.lua", "");
        for name in ["..", "../a", "/etc/passwd", "a..b", "", ".a", "a/b", "a b"] {
            let err = resolve(root.path(), name).unwrap_err();
            assert!(err.contains("invalid module name"), "{name}: {err}");
        }
        assert!(resolve(root.path(), "a").is_ok());
        let user = tempdir().unwrap();
        let err = init_error(&user, "require('../secret')");
        assert!(err.contains("invalid module name"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn require_rejects_a_symlink_out_of_lua() {
        let outside = tempdir().unwrap();
        write(outside.path(), "evil.lua", "return 'escaped'");
        let user = tempdir().unwrap();
        fs::create_dir_all(user.path().join("lua")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("evil.lua"),
            user.path().join("lua/evil.lua"),
        )
        .unwrap();
        let err = init_error(&user, "require('evil')");
        assert!(err.contains("resolves outside lua/"), "{err}");
    }

    #[test]
    fn require_cycles_raise() {
        let user = tempdir().unwrap();
        write(user.path(), "lua/x.lua", "require('y')");
        write(user.path(), "lua/y.lua", "require('x')");
        let err = init_error(&user, "require('x')");
        assert!(err.contains("loop requiring x"), "{err}");
    }

    #[test]
    fn missing_module_is_reported() {
        let user = tempdir().unwrap();
        let err = init_error(&user, "require('nope')");
        assert!(err.contains("module 'nope' not found"), "{err}");
    }

    #[test]
    fn broken_init_keeps_plugins_and_reports_the_error() {
        let plugins = tempdir().unwrap();
        write(
            plugins.path(),
            "a.lua",
            "kage.register_command({ name='a', description='', handler=function() end })",
        );
        let user = tempdir().unwrap();
        write(user.path(), "init.lua", "error('boom')");
        let (rec, rt) = user_runtime(user.path());
        let report = load_all(Some(plugins.path()), &rt).unwrap();
        assert_eq!(report.loaded.len(), 1);
        assert!(matches!(&report.init, Some(Err(err)) if err.contains("boom")));
        assert!(!report.all_ok());
        assert_eq!(rt.registered_commands().len(), 1);
        let logs = rec.snapshot().logs;
        assert!(
            logs.iter().any(|(level, msg)| *level == LogLevel::Error
                && msg.starts_with("init.lua: ")
                && msg.contains("boom")),
            "{logs:?}"
        );
    }

    #[test]
    fn missing_init_is_not_an_error() {
        let user = tempdir().unwrap();
        let (rec, rt) = user_runtime(user.path());
        assert_eq!(load_all(None, &rt).unwrap().init, None);
        assert!(rec.snapshot().logs.is_empty());
    }

    #[test]
    fn reload_reruns_every_layer_once() {
        let plugins = tempdir().unwrap();
        write(plugins.path(), "p.lua", "kage.notify('plugin')");
        let user = tempdir().unwrap();
        write(user.path(), "lua/m.lua", "kage.notify('module')");
        write(user.path(), "init.lua", "require('m') kage.notify('init')");
        let (rec, sink) = recording_sink();
        let rt = PluginRuntime::builder()
            .sink(sink)
            .defaults("kage.notify('defaults')")
            .user_dir(Some(user.path().to_path_buf()))
            .build()
            .unwrap();
        load_all(Some(plugins.path()), &rt).unwrap();
        rt.reload_all(Some(plugins.path())).unwrap();
        let once = ["defaults", "plugin", "module", "init"];
        assert_eq!(rec.snapshot().notifications, [once, once].concat());
    }

    #[test]
    fn reserved_plugin_stem_is_rejected() {
        let plugins = tempdir().unwrap();
        write(plugins.path(), "@user.lua", "kage.notify('impostor')");
        let user = tempdir().unwrap();
        write(user.path(), "init.lua", "");
        let (rec, rt) = user_runtime(user.path());
        let report = load_all(Some(plugins.path()), &rt).unwrap();
        assert_eq!(report.failed.len(), 1);
        assert!(report.loaded.is_empty());
        assert!(rec.snapshot().notifications.is_empty());
    }
}
