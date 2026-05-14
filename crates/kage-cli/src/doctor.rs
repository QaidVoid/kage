//! `kage doctor`: diagnostic command.
//!
//! Walks a fixed checklist (config, credentials, providers, plugins,
//! sandbox) and prints one row per item with status + body. Exit code
//! is `0` when every check is OK or WARN; `1` if any check FAILs.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

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
        check_sandbox(&workdir),
    ]
}

fn check_config(workdir: &Path) -> Check {
    let user = Config::default_path();
    let user_exists = user.as_deref().is_some_and(Path::exists);
    let project = Config::project_path(workdir);
    let project_exists = project.exists();

    match Config::load_layered(workdir) {
        Ok(_) => {
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
        Err(err) => Check {
            name: "config",
            status: Status::Fail,
            body: err.to_string(),
            hint: Some("edit ~/.kage/config.toml or rerun `kage init --force`".into()),
        },
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
    let store = AuthStore::load().unwrap_or_else(|_| AuthStore::empty());
    let available: Vec<&str> = KNOWN_PROVIDERS
        .iter()
        .copied()
        .filter(|p| {
            let env = auth::env_var_for(p);
            let from_env = !env.is_empty() && std::env::var(env).is_ok_and(|v| !v.is_empty());
            from_env || store.access_token(p).is_some()
        })
        .collect();
    if available.is_empty() {
        Check {
            name: "providers",
            status: Status::Fail,
            body: "no provider credentials available".into(),
            hint: Some("run `kage auth login <provider>` or export an *_API_KEY env var".into()),
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
    let runtime = match PluginRuntime::builder()
        .sink(Arc::clone(&sink))
        .workdir(workdir.to_path_buf())
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
            body: format!("{} loaded from {}", report.loaded.len(), dir.display()),
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

fn check_sandbox(workdir: &Path) -> Check {
    use kage_core::config::SandboxBackend;
    let Ok(cfg) = Config::load_layered(workdir) else {
        return Check {
            name: "sandbox",
            status: Status::Warn,
            body: "config could not be parsed; falling back to defaults".into(),
            hint: None,
        };
    };
    let backend = match cfg.sandbox.backend {
        SandboxBackend::Local => "local",
        SandboxBackend::Bubblewrap => "bubblewrap",
        SandboxBackend::SandboxExec => "sandbox-exec",
    };
    let body = if matches!(cfg.sandbox.backend, SandboxBackend::Local) {
        format!("backend={backend} (0.1 default: no isolation)")
    } else {
        format!("backend={backend}")
    };
    let status =
        if matches!(cfg.sandbox.backend, SandboxBackend::Local) && !cfg.sandbox.suppress_warning {
            Status::Warn
        } else {
            Status::Ok
        };
    let hint = (status == Status::Warn).then_some(
        "set `sandbox.suppress_warning = true` in config.toml to silence this once acknowledged"
            .to_owned(),
    );
    Check {
        name: "sandbox",
        status,
        body,
        hint,
    }
}

/// Tests-only access to a single check helper. Keeps the
/// public-from-tests surface minimal.
#[cfg(test)]
fn run_check_config(workdir: &Path) -> Check {
    check_config(workdir)
}

#[cfg(test)]
fn run_check_sandbox(workdir: &Path) -> Check {
    check_sandbox(workdir)
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
        assert!(check.hint.is_some());
    }

    #[test]
    fn sandbox_check_warns_when_local_and_warning_not_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        let check = run_check_sandbox(dir.path());
        assert_eq!(check.status, Status::Warn);
        assert!(check.body.contains("local"));
    }

    #[test]
    fn sandbox_check_is_ok_when_warning_is_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".kage")).unwrap();
        fs::write(
            dir.path().join(".kage").join("config.toml"),
            "[sandbox]\nbackend = \"local\"\nsuppress_warning = true\n",
        )
        .unwrap();
        let check = run_check_sandbox(dir.path());
        assert_eq!(check.status, Status::Ok);
    }
}
