//! Project config trust: the TUI prompt, the non-interactive warning
//! and `kage trust`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kage_core::trust::{self, TrustSummary};

/// Before the TUI starts, ask whether to trust `workdir`'s project
/// config when it has untrusted risky settings. Yes records trust, and
/// anything else runs with those settings ignored. Without a terminal
/// on stdin this only prints the warning.
pub(crate) fn confirm_project_trust(workdir: &Path) {
    let Some(summary) = trust::untrusted_project(workdir) else {
        return;
    };
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", warning(workdir, &summary));
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(
        err,
        "kage: {} wants to enable settings you have not trusted:",
        summary.path.display()
    );
    for item in &summary.items {
        let _ = writeln!(err, "  - {item}");
    }
    let _ = write!(err, "Trust this project config? [y/N] ");
    let _ = err.flush();
    drop(err);
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        eprintln!("kage: continuing without them. Run `kage trust` later to allow them.");
        return;
    }
    if let Err(e) = trust::trust_project(workdir) {
        eprintln!("kage: trust: {e}");
    }
}

/// Print one stderr line when `workdir`'s project config has untrusted
/// risky settings that this run ignores.
pub(crate) fn warn_if_untrusted(workdir: &Path) {
    if let Some(summary) = trust::untrusted_project(workdir) {
        eprintln!("{}", warning(workdir, &summary));
    }
}

fn warning(workdir: &Path, summary: &TrustSummary) -> String {
    format!(
        "kage: ignoring untrusted .kage/config.toml settings ({}) in {}. \
         Run `kage trust` in that directory to allow them.",
        summary.keys.join(", "),
        workdir.display()
    )
}

/// `kage trust [--revoke]` for the current directory.
pub(crate) fn run(revoke: bool) -> ExitCode {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if revoke {
        return match trust::revoke_project(&workdir) {
            Ok(true) => {
                eprintln!("kage: revoked trust for {}", workdir.display());
                ExitCode::SUCCESS
            }
            Ok(false) => {
                eprintln!("kage: {} was not trusted", workdir.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("kage: trust: {e}");
                ExitCode::from(1)
            }
        };
    }
    match trust::trust_project(&workdir) {
        Ok(Some(summary)) => {
            eprintln!("kage: trusted {}:", summary.path.display());
            for item in &summary.items {
                eprintln!("  - {item}");
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!(
                "kage: {} has no mcp, permissions or plugins.capabilities settings to trust",
                kage_core::config::Config::project_path(&workdir).display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: trust: {e}");
            ExitCode::from(1)
        }
    }
}
