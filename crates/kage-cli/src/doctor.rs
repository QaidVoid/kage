//! `kage doctor`: diagnostic command.
//!
//! Lists the four directories kage resolves (config, data, state,
//! cache), then walks a fixed checklist (config, credentials, auth
//! file permissions, state-dir writability, providers, plugins, mcp)
//! and prints one row per item with status + body. The closing
//! verdict names the checks that ran. Exit code is `0` when every
//! check is OK or WARN; `1` if any check FAILs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use kage_core::config::Config;
use kage_plugin::{HostLog, LogLevel, PluginRuntime};
use serde_json::json;

use crate::auth::{self, AuthStore};

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
    write_verdict(&mut stdout, &checks);
    if checks.iter().any(|c| matches!(c.status, Status::Fail)) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Print the closing verdict line. The all-clear names the checks that
/// ran, so `doctor` never claims more than what it covered.
fn write_verdict(out: &mut impl Write, checks: &[Check]) {
    if checks.iter().any(|c| matches!(c.status, Status::Fail)) {
        let _ = writeln!(out, "doctor: one or more checks failed");
        return;
    }
    let names = checks.iter().map(|c| c.name).collect::<Vec<_>>().join(", ");
    let _ = writeln!(out, "doctor: {names} all ok");
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
    let mut checks = vec![check_config(&workdir), check_auth()];
    #[cfg(unix)]
    checks.push(check_auth_mode());
    checks.push(check_state_dir());
    checks.push(check_providers());
    checks.push(check_plugins(&workdir));
    checks.push(check_mcp(&workdir));
    checks
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
/// no plugin runtime loaded and only validates static config. The
/// config is read without validation, so the probe still runs when
/// the permission or shell tables are the broken part.
fn check_mcp(workdir: &Path) -> Check {
    match Config::load_layered_raw(workdir) {
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

/// The `config` row: whether the layered config passes the checks
/// kage runs when it starts. Unlike the other checks this one keeps
/// the validating loader, because the validity it reports is the
/// point: a parse or validation error must FAIL here with the fix.
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
    // Raw on purpose: the credentials row must render even when the
    // permission or shell tables are what is broken, and through the
    // same provider-key list `auth list` and the `providers` row use,
    // so a custom env key counts everywhere or nowhere.
    let config = Config::load_default_raw().unwrap_or_default();
    Check {
        name: "auth",
        status: Status::Ok,
        body: auth_body(
            &store,
            &auth::provider_keys(&config),
            |env| std::env::var(env).is_ok_and(|v| !v.is_empty()),
            chrono::Utc::now(),
        ),
        hint: None,
    }
}

/// The `auth` row body: stored credential counts plus how many
/// providers find a key in the environment.
fn auth_body(
    store: &AuthStore,
    keys: &[auth::ProviderKey],
    env_set: impl Fn(&str) -> bool,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let stored = store.providers.len();
    let oauth = store.providers.values().filter(|c| c.is_oauth()).count();
    let env_count = keys
        .iter()
        .filter(|key| !key.env.is_empty() && env_set(&key.env))
        .count();
    let mut body = format!(
        "{stored} stored ({oauth} oauth, {} api-key), {env_count} via env",
        stored - oauth
    );
    let expiring: Vec<String> = store
        .oauth_expiring(auth::OAUTH_EXPIRY_WARNING, now)
        .map(|(provider, at)| {
            format!(
                "the {provider} login {} (rerun `kage auth login {provider}`)",
                auth::expiry_label(at, now)
            )
        })
        .collect();
    if !expiring.is_empty() {
        body.push_str("; ");
        body.push_str(&expiring.join(", "));
    }
    body
}

/// The `permissions` row (Unix): the auth file holds provider keys,
/// so a mode looser than `0600` is a warning with the fix.
#[cfg(unix)]
fn check_auth_mode() -> Check {
    match AuthStore::default_path() {
        Ok(path) => auth_mode_check(&path),
        Err(err) => Check {
            name: "permissions",
            status: Status::Warn,
            body: format!("auth file unresolved: {err}"),
            hint: None,
        },
    }
}

/// The auth-file mode as a [`Check`] against an explicit path.
#[cfg(unix)]
fn auth_mode_check(path: &Path) -> Check {
    use std::os::unix::fs::PermissionsExt as _;

    if !path.exists() {
        return Check {
            name: "permissions",
            status: Status::Ok,
            body: "no auth file yet".into(),
            hint: None,
        };
    }
    let mode = match std::fs::metadata(path) {
        Ok(meta) => meta.permissions().mode() & 0o777,
        Err(err) => {
            return Check {
                name: "permissions",
                status: Status::Warn,
                body: format!("stat {}: {err}", path.display()),
                hint: None,
            };
        }
    };
    if mode.trailing_zeros() >= 6 {
        Check {
            name: "permissions",
            status: Status::Ok,
            body: format!("{} is mode {mode:o}", path.display()),
            hint: None,
        }
    } else {
        Check {
            name: "permissions",
            status: Status::Warn,
            body: format!(
                "{} is readable by other users (mode {mode:o})",
                path.display()
            ),
            hint: Some(format!("run `chmod 600 {}`", path.display())),
        }
    }
}

/// The `state` row: whether the state root can hold the files kage
/// writes there, proven by creating and removing a probe file.
fn check_state_dir() -> Check {
    match crate::state_root() {
        Ok(dir) => state_dir_check(&dir),
        Err(err) => Check {
            name: "state",
            status: Status::Warn,
            body: format!("unresolved: {err}"),
            hint: None,
        },
    }
}

/// Writability of `dir` as a [`Check`], creating it when missing.
fn state_dir_check(dir: &Path) -> Check {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return Check {
            name: "state",
            status: Status::Fail,
            body: format!("mkdir {}: {err}", dir.display()),
            hint: Some("check the KAGE_STATE_HOME / XDG_STATE_HOME environment variables".into()),
        };
    }
    match probe_writable(dir) {
        Ok(()) => Check {
            name: "state",
            status: Status::Ok,
            body: format!("{} is writable", dir.display()),
            hint: None,
        },
        Err(err) => Check {
            name: "state",
            status: Status::Fail,
            body: err,
            hint: Some("check the directory's ownership and permissions".into()),
        },
    }
}

/// Create and remove a probe file in `dir`, reporting the failing
/// half. The name carries the process id so concurrent doctors do not
/// race on one file.
fn probe_writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(".kage-doctor-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(file) => drop(file),
        Err(err) => return Err(format!("create {}: {err}", probe.display())),
    }
    if let Err(err) = std::fs::remove_file(&probe) {
        return Err(format!("remove {}: {err}", probe.display()));
    }
    Ok(())
}

fn check_providers() -> Check {
    // Raw on purpose: the credentials row must render even when the
    // permission or shell tables are what is broken.
    let config = Config::load_default_raw().unwrap_or_default();
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
    let enabled = match Config::load_layered_raw(workdir) {
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

    use chrono::TimeZone;

    use super::*;

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

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
    fn mcp_check_still_probes_when_the_shell_policy_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".kage")).unwrap();
        // `[shell]` is not a trust-gated table, so this untrusted
        // project config loads raw but refuses the validating loader.
        fs::write(
            dir.path().join(".kage/config.toml"),
            "[shell]\nscrub_env = [\"[\"]\n",
        )
        .unwrap();
        let check = check_mcp(dir.path());
        assert!(!check.body.contains("config unreadable"), "{}", check.body);
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

    fn check(name: &'static str, status: Status) -> Check {
        Check {
            name,
            status,
            body: String::new(),
            hint: None,
        }
    }

    #[test]
    fn the_verdict_names_its_scope_and_changes_on_a_fail() {
        let ok = vec![check("config", Status::Ok), check("auth", Status::Ok)];
        let mut out = Vec::new();
        write_verdict(&mut out, &ok);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "doctor: config, auth all ok\n"
        );

        let failed = vec![check("config", Status::Ok), check("auth", Status::Fail)];
        let mut out = Vec::new();
        write_verdict(&mut out, &failed);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "doctor: one or more checks failed\n"
        );
    }

    #[test]
    fn the_state_check_passes_when_a_probe_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let check = state_dir_check(dir.path());
        assert_eq!(check.status, Status::Ok, "{}", check.body);
    }

    #[cfg(unix)]
    #[test]
    fn the_state_check_fails_on_an_unwritable_dir() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();
        let check = state_dir_check(&state);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(check.status, Status::Fail, "{}", check.body);
    }

    #[cfg(unix)]
    #[test]
    fn the_auth_mode_check_warns_looser_than_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let check = auth_mode_check(&path);
        assert_eq!(check.status, Status::Warn, "{}", check.body);
        assert!(check.hint.unwrap().contains("chmod 600"), "{}", check.body);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(auth_mode_check(&path).status, Status::Ok);
        assert_eq!(
            auth_mode_check(&dir.path().join("gone.json")).status,
            Status::Ok
        );
    }

    #[test]
    fn the_auth_row_counts_the_credentials_auth_list_shows() {
        let config: Config = toml::from_str(
            "[providers.custom.lab]\nbase_url = \"http://lab:1/v1\"\napi_key_env = \"MY_KEY\"\n\
             [[providers.custom.lab.models]]\nid = \"m\"\nname = \"M\"\n",
        )
        .unwrap();
        let keys = auth::provider_keys(&config);
        let store = AuthStore::empty();
        let body = auth_body(&store, &keys, |env| env == "MY_KEY", fixed_now());
        assert!(body.contains("1 via env"), "{body}");

        let ready: Vec<&str> = keys
            .iter()
            .filter(|key| {
                key.source_with(&store, |env| env == "MY_KEY", fixed_now())
                    .is_some()
            })
            .map(|key| key.id.as_str())
            .collect();
        assert_eq!(
            ready,
            ["lab"],
            "`auth list` shows ready exactly what the auth row counts"
        );
    }

    /// An OAuth login inside the expiry window is named in the auth
    /// row with its label and the fix, so `kage doctor` surfaces what
    /// the TUI notices surface at startup.
    #[test]
    fn the_auth_row_names_expiring_logins() {
        let mut store = AuthStore::empty();
        store.set_oauth(
            "zai",
            auth::OAuthCredential {
                access_token: "t".into(),
                expires_at: Some(fixed_now() + chrono::Duration::days(1)),
                ..auth::OAuthCredential::default()
            },
        );
        let body = auth_body(&store, &[], |_: &str| false, fixed_now());
        assert!(body.contains("expires in 1 day"), "{body}");
        assert!(body.contains("rerun `kage auth login zai`"), "{body}");

        let fresh = auth_body(
            &store,
            &[],
            |_: &str| false,
            fixed_now() - chrono::Duration::days(30),
        );
        assert!(!fresh.contains("expires"), "{fresh}");
    }
}
