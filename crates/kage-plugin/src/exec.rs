//! The `exec` capability: `kage.exec`.
//!
//! Attached only onto the `kage` proxy of a plugin that was granted
//! `exec` (see [`crate::capabilities`]); an ungranted plugin never
//! sees it. It spawns a subprocess directly - no shell, so there is
//! no quoting or injection surface - with its working directory
//! pinned under the host workdir via [`kage_tools::resolve_under`]
//! (the same escape check `kage.fs` uses). The call blocks until the
//! process exits or `timeout_secs` elapses (default 30) and returns
//! its captured output plus a `timed_out` flag, the way
//! `kage.http.get` blocks; a rewind plugin uses it to snapshot files
//! with `git` between turns.
//!
//! The `exec` grant is coarse and all-or-nothing: there is no command
//! allowlist and no per-command scoping. A granted plugin may run any
//! executable on the process `PATH` (or any absolute path) with any
//! arguments; the only checks are that `cmd` is non-empty and that
//! `cwd` cannot escape the host workdir. Grant it only to plugins you
//! trust to run arbitrary programs as the kage process.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::sync::lock;
use kage_tools::resolve_under;
use mlua::{Lua, Table};

use crate::capabilities::{Capability, CapabilityRegistry};

/// How long a spawned process may run when the spec sets no
/// `timeout_secs`.
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the waiter polls the child while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Register the `exec` installer into `registry`.
///
/// The installer runs (via `request_capabilities`) against a granted
/// plugin's `kage` proxy and sets `exec` on it. `workdir` is the host
/// workdir; a spec `cwd` is resolved under it and may not escape.
pub(crate) fn register(registry: &CapabilityRegistry, workdir: PathBuf) {
    let mut reg = lock(registry);
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
                    let timeout_secs: Option<u64> = spec.get("timeout_secs")?;
                    if timeout_secs == Some(0) {
                        return Err(mlua::Error::external(
                            "kage.exec: `timeout_secs` must be at least 1",
                        ));
                    }
                    let timeout = timeout_secs.map_or(DEFAULT_EXEC_TIMEOUT, |s| {
                        Duration::from_secs(s).max(Duration::from_secs(1))
                    });

                    let mut child = Command::new(&cmd)
                        .args(&args)
                        .current_dir(&dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(|e| {
                            mlua::Error::external(format!("kage.exec: spawn {cmd}: {e}"))
                        })?;
                    // Drain each pipe on its own thread so a child that
                    // outgrows the OS pipe buffer never stalls on a
                    // full pipe while we are waiting for it to exit.
                    let stdout_pipe = child.stdout.take();
                    let stderr_pipe = child.stderr.take();
                    let stdout_thread = stdout_pipe.map(|mut pipe| {
                        thread::spawn(move || {
                            let mut buf = Vec::new();
                            let _ = pipe.read_to_end(&mut buf);
                            buf
                        })
                    });
                    let stderr_thread = stderr_pipe.map(|mut pipe| {
                        thread::spawn(move || {
                            let mut buf = Vec::new();
                            let _ = pipe.read_to_end(&mut buf);
                            buf
                        })
                    });

                    let deadline = Instant::now() + timeout;
                    let mut timed_out = false;
                    let status = loop {
                        match child.try_wait() {
                            Ok(Some(status)) => break status,
                            Ok(None) => {
                                if Instant::now() >= deadline {
                                    let _ = child.kill();
                                    let status = child.wait().map_err(|e| {
                                        mlua::Error::external(format!("kage.exec: wait {cmd}: {e}"))
                                    })?;
                                    timed_out = true;
                                    break status;
                                }
                                thread::sleep(POLL_INTERVAL);
                            }
                            Err(e) => {
                                return Err(mlua::Error::external(format!(
                                    "kage.exec: wait {cmd}: {e}"
                                )));
                            }
                        }
                    };

                    let stdout = stdout_thread
                        .map(|h| h.join().unwrap_or_default())
                        .unwrap_or_default();
                    let stderr = stderr_thread
                        .map(|h| h.join().unwrap_or_default())
                        .unwrap_or_default();
                    let out = lua.create_table()?;
                    out.set("code", status.code().unwrap_or(-1))?;
                    out.set("timed_out", timed_out)?;
                    out.set("stdout", lua.create_string(&stdout)?)?;
                    out.set("stderr", lua.create_string(&stderr)?)?;
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
    fn exec_kills_a_runaway_process_at_the_deadline() {
        let rt = rt_with_exec();
        let start = std::time::Instant::now();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 local r = kage.exec({ cmd = 'sleep', args = { '30' }, timeout_secs = 1 }); \
                 return r.timed_out and r.code ~= 0 and r.stdout == ''",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true), "timed_out must be set");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "deadline must kill the child promptly, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn exec_reports_no_timeout_for_a_fast_exit() {
        let rt = rt_with_exec();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'exec'}); \
                 return kage.exec({ cmd = 'true' }).timed_out == false",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn ungranted_plugin_has_no_exec() {
        let rt = rt_with_exec();
        let v = rt.eval_plugin("other", "return kage.exec == nil").unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }
}
