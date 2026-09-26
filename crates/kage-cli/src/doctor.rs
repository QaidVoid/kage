//! `kage doctor`: diagnostic command.
//!
//! Lists the four directories kage resolves (config, data, state,
//! cache), then walks a fixed checklist (config, credentials,
//! providers, plugins, mcp) and prints one row per item with
//! status + body. Exit code is `0` when every check is OK or WARN; `1`
//! if any check FAILs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use kage_core::config::Config;
use kage_plugin::{HostLog, LogLevel, PluginRuntime};
use serde_json::json;

use crate::auth::{self, AuthStore, KNOWN_PROVIDERS};

/// Outcome bucket for a single check row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

/// One line of `kage doctor` output.
struct Check {
    name: &'static str,
    status: Status,
    body: String,
    hint: Option<String>,
}

/// Entry point invoked from the CLI dispatcher.
pub fn run() -> ExitCode {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "kage doctor");
    let _ = writeln!(stdout);
    write_directories(&mut stdout, &directories());
    let _ = writeln!(stdout);

    let checks = collect_checks();
    let any_fail = checks.iter().any(|c| matches!(c.status, Status::Fail));

    for check in &checks {
        let _ = writeln!(
            stdout,
            "  {:<10} {:<5} {}",
            check.name,
            check.status.label(),
            check.body
        );
        if let Some(hint) = &check.hint {
            let _ = writeln!(stdout, "             hint: {hint}");
        }
    }

    let _ = writeln!(stdout);
    if any_fail {
        let _ = writeln!(stdout, "doctor: one or more checks failed");
        ExitCode::from(1)
    } else {
        let _ = writeln!(stdout, "doctor: all checks ok");
        ExitCode::SUCCESS
    }
}

/// The directories kage keeps its files in, as `(role, path)` rows:
/// config, data, state and the cache.
fn directories() -> [(&'static str, Result<PathBuf, String>); 4] {
    [
        ("config", crate::config_dir()),
        ("data", crate::data_root()),
        ("state", crate::state_root()),
        ("cache", crate::cache_root()),
    ]
}

/// Print `rows` under a `directories` heading.
fn write_directories(out: &mut impl Write, rows: &[(&'static str, Result<PathBuf, String>)]) {
    let _ = writeln!(out, "  directories");
    for (role, path) in rows {
        let shown = match path {
            Ok(path) => path.display().to_string(),
            Err(e) => format!("unresolved: {e}"),
        };
        let _ = writeln!(out, "    {role:<7}{shown}");
    }
}

/// Run every check in order. Each helper returns a [`Check`]; the
/// caller doesn't need to know which checks exist, only how to render
/// them.
fn collect_checks() -> Vec<Check> {
    let workdir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    vec![
        check_config(&workdir),
        check_auth(),
        check_providers(),
        check_plugins(&workdir),
        check_mcp(&workdir),
    ]
}

/// Per-server bound for the spawn + `initialize` + `tools/list`
/// probe. A server that never answers `initialize` is exactly what
/// this check should flag, so we cap the wait rather than block
/// `doctor` forever.
const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn each enabled `[mcp.servers.*]`, handshake, and list its
/// tools, reporting one aggregate row. A server that fails to spawn,
/// errors during discovery, or does not answer within
/// [`MCP_PROBE_TIMEOUT`] makes the check FAIL with the offender
/// named. Plugin-declared servers are not probed here: `doctor` has
/// no plugin runtime loaded and only validates static config.
fn check_mcp(workdir: &Path) -> Check {
    match Config::load_layered(workdir) {
        Ok(c) => check_mcp_servers(c.mcp.servers),
        Err(err) => Check {
            name: "mcp",
            status: Status::Warn,
            body: format!("config unreadable: {err} (skipped)"),
            hint: None,
        },
    }
}

/// Probe `servers` and fold the outcomes into the `mcp` row.
fn check_mcp_servers(
    servers: std::collections::BTreeMap<String, kage_core::config::McpServer>,
) -> Check {
    if servers.is_empty() {
        return Check {
            name: "mcp",
            status: Status::Ok,
            body: "no mcp servers configured (skipped)".into(),
            hint: None,
        };
    }

    let mut ok: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (name, spec) in servers {
        if spec.disabled {
            skipped.push(name);
            continue;
        }
        match probe_mcp_server(&name, &spec) {
            Ok(count) => ok.push(format!("{name} ({count} tools)")),
            Err(err) => failures.push(format!("{name}: {err}")),
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !ok.is_empty() {
        parts.push(format!("ok: {}", ok.join(", ")));
    }
    if !skipped.is_empty() {
        parts.push(format!("disabled: {}", skipped.join(", ")));
    }
    if !failures.is_empty() {
        parts.push(format!("failed: {}", failures.join("; ")));
    }
    let body = parts.join(" | ");
    if failures.is_empty() {
        Check {
            name: "mcp",
            status: Status::Ok,
            body,
            hint: None,
        }
    } else {
        Check {
            name: "mcp",
            status: Status::Fail,
            body,
            hint: Some(
                "check the server `command`/`args`; run it by hand to see its stderr".into(),
            ),
        }
    }
}

/// Spawn one server in a worker thread and wait at most
/// [`MCP_PROBE_TIMEOUT`] for the handshake + tool list. The worker
/// owns the [`kage_mcp::McpServerHandle`], so it kills the child when
/// it finishes; a pathologically hung server (never answers
/// `initialize`) leaks only this diagnostic thread, never affecting a
/// real `kage` run.
fn probe_mcp_server(name: &str, spec: &kage_core::config::McpServer) -> Result<usize, String> {
    let (tx, rx) = mpsc::channel();
    let server_name = name.to_owned();
    let spec = spec.clone();
    std::thread::spawn(move || {
        let result = kage_mcp::McpServerHandle::spawn(&server_name, &spec, &[], None)
            .map_err(|e| e.to_string())
            .and_then(|handle| {
                handle
                    .connection()
                    .list_tools()
                    .map(|tools| tools.len())
                    .map_err(|e| e.to_string())
            });
        let _ = tx.send(result);
    });
    match rx.recv_timeout(MCP_PROBE_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "no response within {}s",
            MCP_PROBE_TIMEOUT.as_secs()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("probe thread died".into()),
    }
}

fn check_config(workdir: &Path) -> Check {
    let user = Config::default_path();
    let user_exists = user.as_deref().is_some_and(Path::exists);
    let project = Config::project_path(workdir);
    let project_exists = project.exists();

    match Config::load_layered(workdir) {
        Ok(_) => {
            if let Some(summary) = kage_core::trust::untrusted_project(workdir) {
                return Check {
                    name: "config",
                    status: Status::Warn,
                    body: format!(
                        "ignoring untrusted {} settings in {}",
                        summary.keys.join(", "),
                        summary.path.display()
                    ),
                    hint: Some("run `kage trust` in this directory to allow them".into()),
                };
            }
            let mut parts = Vec::new();
            if let Some(path) = user.as_deref().filter(|_| user_exists) {
                parts.push(format!("user={}", path.display()));
            }
            if project_exists {
                parts.push(format!("project={}", project.display()));
            }
            let body = if parts.is_empty() {
                "using built-in defaults (no config files found)".to_owned()
            } else {
                parts.join(", ")
            };
            Check {
                name: "config",
                status: Status::Ok,
                body,
                hint: None,
            }
        }
        Err(err) => {
            let files: Vec<String> = user
                .iter()
                .filter(|_| user_exists)
                .chain(project_exists.then_some(&project))
                .map(|path| path.display().to_string())
                .collect();
            let hint = if files.is_empty() {
                "check the KAGE_* environment variables".to_owned()
            } else {
                format!("fix the error in {}", files.join(" or "))
            };
            Check {
                name: "config",
                status: Status::Fail,
                body: err.to_string(),
                hint: Some(hint),
            }
        }
    }
}

fn check_auth() -> Check {
    let store = match AuthStore::load() {
        Ok(s) => s,
        Err(err) => {
            return Check {
                name: "auth",
                status: Status::Fail,
                body: err,
                hint: Some("delete the malformed auth file or rerun `kage auth login`".into()),
            };
        }
    };
    let stored = store.providers.len();
    let oauth = store.providers.values().filter(|c| c.is_oauth()).count();
    let env_count = KNOWN_PROVIDERS
        .iter()
        .filter(|p| {
            let env = auth::env_var_for(p);
            !env.is_empty() && std::env::var(env).is_ok_and(|v| !v.is_empty())
        })
        .count();
    let body = format!(
        "{stored} stored ({oauth} oauth, {} api-key), {env_count} via env",
        stored - oauth,
    );
    Check {
        name: "auth",
        status: Status::Ok,
        body,
        hint: None,
    }
}

fn check_providers() -> Check {
    let config = Config::load_default().unwrap_or_default();
    let store = AuthStore::load().unwrap_or_else(|_| AuthStore::empty());
    providers_check(&config, &store)
}

/// The `providers` row: every known or custom provider with a
/// credential, or with an endpoint that needs none.
fn providers_check(config: &Config, store: &AuthStore) -> Check {
    let available: Vec<String> = auth::provider_keys(config)
        .into_iter()
        .filter(|p| p.source(store).is_some())
        .map(|p| p.id)
        .collect();
    if available.is_empty() {
        Check {
            name: "providers",
            status: Status::Fail,
            body: "no provider credentials available".into(),
            hint: Some(
                "run `kage auth login <provider>`, export an *_API_KEY env var, \
                 or add a [providers.custom.<id>] endpoint"
                    .into(),
            ),
        }
    } else {
        Check {
            name: "providers",
            status: Status::Ok,
            body: format!("{} ready: {}", available.len(), available.join(", ")),
            hint: None,
        }
    }
}

fn check_plugins(workdir: &Path) -> Check {
    let dir = match crate::plugins_dir() {
        Ok(p) => p,
        Err(err) => {
            return Check {
                name: "plugins",
                status: Status::Warn,
                body: format!("plugin dir unresolved: {err}"),
                hint: None,
            };
        }
    };
    if !dir.exists() {
        return Check {
            name: "plugins",
            status: Status::Ok,
            body: format!("no plugin dir at {} (skipped)", dir.display()),
            hint: None,
        };
    }
    // Use a no-op sink so plugin errors don't pollute stderr while we
    // diagnose - we surface them in our own line instead.
    let sink: kage_plugin::SharedHostLog = Arc::new(Mutex::new(Box::new(SilentSink)));
    let enabled = match Config::load_layered(workdir) {
        Ok(c) => c.plugins.enabled,
        Err(err) => {
            return Check {
                name: "plugins",
                status: Status::Warn,
                body: format!("config unreadable: {err} (skipped)"),
                hint: None,
            };
        }
    };
    let runtime = match PluginRuntime::builder()
        .sink(Arc::clone(&sink))
        .workdir(workdir.to_path_buf())
        .enabled(enabled)
        .config(json!({}))
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            return Check {
                name: "plugins",
                status: Status::Fail,
                body: format!("runtime: {err}"),
                hint: Some("check Lua dependencies; rerun with RUST_LOG=debug".into()),
            };
        }
    };
    match kage_plugin::load_dir(&dir, &runtime) {
        Ok(report) if report.failed.is_empty() => Check {
            name: "plugins",
            status: Status::Ok,
            body: if report.skipped.is_empty() {
                format!("{} loaded from {}", report.loaded.len(), dir.display())
            } else {
                format!(
                    "{} loaded, {} skipped by [plugins] enabled, from {}",
                    report.loaded.len(),
                    report.skipped.len(),
                    dir.display()
                )
            },
            hint: None,
        },
        Ok(report) => {
            let first = report
                .failed
                .first()
                .map(|(p, err)| format!("{}: {err}", p.display()))
                .unwrap_or_default();
            Check {
                name: "plugins",
                status: Status::Warn,
                body: format!(
                    "{} loaded, {} failed (first: {first})",
                    report.loaded.len(),
                    report.failed.len()
                ),
                hint: Some("inspect the failing file; broken plugins are skipped".into()),
            }
        }
        Err(err) => Check {
            name: "plugins",
            status: Status::Fail,
            body: format!("scan {}: {err}", dir.display()),
            hint: None,
        },
    }
}

/// Tests-only access to a single check helper. Keeps the
/// public-from-tests surface minimal.
#[cfg(test)]
fn run_check_config(workdir: &Path) -> Check {
    check_config(workdir)
}

/// Drop-on-floor [`HostLog`] used while `check_plugins` evaluates Lua
/// chunks. Doctor reports plugin failures through its own row, so the
/// usual stderr-backed sink would just duplicate noise.
#[derive(Debug)]
struct SilentSink;

impl HostLog for SilentSink {
    fn notify(&mut self, _message: &str) {}
    fn log(&mut self, _level: LogLevel, _message: &str) {}
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn directories_list_all_four_roles() {
        let roles: Vec<&str> = directories().iter().map(|(role, _)| *role).collect();
        assert_eq!(roles, ["config", "data", "state", "cache"]);
        let mut out = Vec::new();
        write_directories(
            &mut out,
            &[
                ("config", Ok(PathBuf::from("/c/kage"))),
                ("cache", Err("no home directory".to_owned())),
            ],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  directories\n    config /c/kage\n    cache  unresolved: no home directory\n"
        );
    }

    #[test]
    fn config_check_reports_ok_with_default_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let check = run_check_config(dir.path());
        // A workdir with no `.kage/config.toml` should fall through
        // to figment defaults; that's an OK row.
        assert_eq!(check.status, Status::Ok);
        assert!(check.body.contains("default") || check.body.contains("config.toml"));
    }

    #[test]
    fn config_check_reports_fail_on_invalid_project_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".kage")).unwrap();
        // Wrong type for a string field forces a parse error.
        fs::write(
            dir.path().join(".kage").join("config.toml"),
            "[ui]\ntheme = 42\n",
        )
        .unwrap();
        let check = run_check_config(dir.path());
        assert_eq!(check.status, Status::Fail);
        let hint = check.hint.unwrap();
        let project = dir.path().join(".kage").join("config.toml");
        assert!(hint.contains(&project.display().to_string()), "{hint}");
        assert!(!hint.contains("--force"), "{hint}");
    }

    #[test]
    fn mcp_check_ok_when_no_servers_configured() {
        let check = check_mcp_servers(servers(""));
        assert_eq!(check.status, Status::Ok);
        assert!(check.body.contains("no mcp servers"));
    }

    fn servers(toml: &str) -> std::collections::BTreeMap<String, kage_core::config::McpServer> {
        toml::from_str::<kage_core::config::McpConfig>(toml)
            .unwrap()
            .servers
    }

    #[test]
    fn mcp_check_lists_disabled_without_spawning() {
        let check = check_mcp_servers(servers(
            "[servers.off]\ncommand = \"no-such-binary-xyz\"\ndisabled = true\n",
        ));
        assert_eq!(check.status, Status::Ok, "{}", check.body);
        assert!(check.body.contains("disabled: off"), "{}", check.body);
    }

    #[test]
    fn mcp_check_fails_on_unspawnable_server() {
        let check = check_mcp_servers(servers(
            "[servers.broken]\ncommand = \"definitely-not-a-real-binary-xyz\"\n",
        ));
        assert_eq!(check.status, Status::Fail, "{}", check.body);
        assert!(check.body.contains("broken"), "{}", check.body);
        assert!(check.hint.is_some());
    }

    #[test]
    fn providers_check_counts_a_keyless_custom_provider() {
        let config: Config = toml::from_str(
            "[providers.custom.fake]\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key_env = \"\"\n\
             [[providers.custom.fake.models]]\nid = \"small\"\nname = \"Small\"\n",
        )
        .unwrap();
        let check = providers_check(&config, &AuthStore::empty());
        assert_eq!(check.status, Status::Ok, "{}", check.body);
        assert!(check.body.contains("fake"), "{}", check.body);
    }
}
