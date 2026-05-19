//! The `exec` capability: `kage.exec`.
//!
//! Attached only onto the `kage` proxy of a plugin that was granted
//! `exec` (see [`crate::capabilities`]); an ungranted plugin never
//! sees it. It spawns a subprocess directly - no shell, so there is
//! no quoting or injection surface - with its working directory
//! pinned under the host workdir via [`kage_tools::resolve_under`]
//! (the same escape check `kage.fs` uses). The call blocks until the
//! process exits and returns its captured output, the way
//! `kage.http.get` blocks; a rewind plugin uses it to snapshot files
//! with `git` between turns.

use std::path::{Path, PathBuf};
use std::process::Command;

use kage_tools::resolve_under;
use mlua::{Lua, Table};

use crate::capabilities::{Capability, CapabilityRegistry};

/// Register the `exec` installer into `registry`.
///
/// The installer runs (via `request_capabilities`) against a granted
/// plugin's `kage` proxy and sets `exec` on it. `workdir` is the host
/// workdir; a spec `cwd` is resolved under it and may not escape.
pub(crate) fn register(registry: &CapabilityRegistry, workdir: PathBuf) {
    let mut reg = registry.lock().expect("capability registry mutex poisoned");
    reg.insert(
        Capability::Exec,
        Box::new(move |lua: &Lua, pkage: &Table| {
            let root = workdir.clone();
            pkage.set(
                "exec",
                lua.create_function(move |lua, spec: Table| {
                    let cmd: Option<String> = spec.get("cmd")?;
                    let cmd = cmd.filter(|c| !c.is_empty()).ok_or_else(|| {
                        mlua::Error::external("kage.exec: `cmd` must be a non-empty string")
                    })?;
                    let args: Vec<String> =
                        spec.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
                    let cwd: Option<String> = spec.get("cwd")?;
                    let dir = match cwd {
                        Some(rel) => resolve_under(&root, Path::new(&rel)).map_err(|e| {
                            mlua::Error::external(format!("kage.exec: cwd {rel}: {e}"))
                        })?,
                        None => root.clone(),
                    };
                    let output = Command::new(&cmd)
                        .args(&args)
                        .current_dir(&dir)
                        .output()
                        .map_err(|e| {
                            mlua::Error::external(format!("kage.exec: spawn {cmd}: {e}"))
                        })?;
                    let out = lua.create_table()?;
                    out.set("code", output.status.code().unwrap_or(-1))?;
                    out.set("stdout", lua.create_string(&output.stdout)?)?;
                    out.set("stderr", lua.create_string(&output.stderr)?)?;
                    Ok(out)
                })?,
            )?;
            Ok(())
        }),
    );
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    fn rt_with_exec() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["exec".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    #[test]
    fn exec_runs_a_process_and_reports_exit_code() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local ok = kage.exec({ cmd = 'true' }); \
                 local no = kage.exec({ cmd = 'false' }); \
                 return ok.code == 0 and no.code ~= 0",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn exec_captures_stdout() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'echo', args = { 'hello' } }); \
                 return r.code == 0 and r.stdout:find('hello') ~= nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn exec_cwd_may_not_escape_the_workdir() {
        let rt = rt_with_exec();
        let res = rt.eval_plugin(
            "p",
            "kage.request_capabilities({'exec'}); kage.exec({ cmd = 'true', cwd = '../etc' })",
        );
        assert!(res.is_err(), "escaping cwd must raise, got {res:?}");
    }

    #[test]
    fn exec_requires_a_command() {
        let rt = rt_with_exec();
        let res = rt.eval_plugin("p", "kage.request_capabilities({'exec'}); kage.exec({})");
        assert!(res.is_err(), "missing cmd must raise, got {res:?}");
    }

    #[test]
    fn ungranted_plugin_has_no_exec() {
        let rt = rt_with_exec();
        let v = rt.eval_plugin("other", "return kage.exec == nil").unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }
}
