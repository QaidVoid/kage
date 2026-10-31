//! Project trust: the TUI prompt, the non-interactive warning and
//! `kage trust`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kage_core::trust::{self, StandingTrust, TrustSummary};

/// The line `kage trust` prints for a project a `[trust] paths` entry
/// already fully trusts: which entry matched. Nothing is written.
fn paths_trust_message(workdir: &Path, standing: &StandingTrust) -> String {
    format!(
        "kage: {} is already trusted by [trust] paths ({})",
        workdir.display(),
        standing.entries.join(", ")
    )
}

/// The line `kage trust` prints when `[trust] parts` entries cover
/// some parts of the project: which parts and which entries matched.
/// The parts still waiting for trust follow in the normal flow.
fn parts_trust_message(standing: &StandingTrust) -> String {
    format!(
        "kage: [trust] parts already cover {} here ({})",
        standing.part_list(),
        standing.entries.join(", ")
    )
}

/// The line `kage trust --revoke` prints when the trust comes from
/// the user config, which no command can revoke.
fn revoke_from_config_message(workdir: &Path, standing: &StandingTrust) -> String {
    if standing.full {
        format!(
            "kage: {} is trusted by [trust] paths ({}); edit your user config to change it",
            workdir.display(),
            standing.entries.join(", ")
        )
    } else {
        format!(
            "kage: [trust] parts cover {} here ({}); edit your user config to change it",
            standing.part_list(),
            standing.entries.join(", ")
        )
    }
}

/// Before the TUI starts, ask whether to trust `workdir`'s project
/// when it has untrusted risky settings or agents. Yes records trust,
/// and anything else runs with those settings and agents ignored.
/// Without a terminal on stdin this only prints the warning.
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
    let _ = write!(err, "Trust this project? [y/N] ");
    let _ = err.flush();
    drop(err);
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        eprintln!("kage: continuing without them. Run `kage trust` later to allow them.");
        return;
    }
    if let Err(e) = trust::trust_project(workdir, &summary) {
        eprintln!("kage: trust: {e}");
    }
}

/// Print one stderr line when `workdir`'s project has untrusted risky
/// settings or agents that this run ignores.
pub(crate) fn warn_if_untrusted(workdir: &Path) {
    if let Some(summary) = trust::untrusted_project(workdir) {
        eprintln!("{}", warning(workdir, &summary));
    }
}

fn warning(workdir: &Path, summary: &TrustSummary) -> String {
    let settings: Vec<&str> = summary
        .keys
        .iter()
        .filter(|key| !matches!(key.as_str(), "agents" | "skills"))
        .map(String::as_str)
        .collect();
    let mut parts = Vec::new();
    if !settings.is_empty() {
        parts.push(format!(
            ".kage/config.toml settings ({})",
            settings.join(", ")
        ));
    }
    if !summary.agents.is_empty() {
        parts.push(format!("project agents ({})", summary.agents.join(", ")));
    }
    if !summary.skills.is_empty() {
        parts.push(format!("project skills ({})", summary.skills.join(", ")));
    }
    format!(
        "kage: ignoring untrusted {} in {}. \
         Run `kage trust` in that directory to allow them.",
        parts.join(" and "),
        workdir.display()
    )
}

/// `kage trust [--revoke]` for the current directory.
pub(crate) fn run(revoke: bool) -> ExitCode {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let standing = trust::standing_trust(&workdir);
    if revoke {
        return match trust::revoke_project(&workdir) {
            Ok(removed) => {
                if standing.full || !standing.parts.is_empty() {
                    eprintln!("{}", revoke_from_config_message(&workdir, &standing));
                    if removed {
                        eprintln!("kage: removed the trust recorded for {}", workdir.display());
                    }
                    return ExitCode::SUCCESS;
                }
                if removed {
                    eprintln!("kage: revoked trust for {}", workdir.display());
                } else {
                    eprintln!("kage: {} was not trusted", workdir.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("kage: trust: {e}");
                ExitCode::from(1)
            }
        };
    }
    if standing.full {
        eprintln!("{}", paths_trust_message(&workdir, &standing));
        return ExitCode::SUCCESS;
    }
    if !standing.parts.is_empty() {
        eprintln!("{}", parts_trust_message(&standing));
    }
    let Some(presented) = trust::untrusted_project(&workdir) else {
        eprintln!(
            "kage: {} has no settings waiting for trust and no agents or skills to trust",
            workdir.join(".kage").display()
        );
        return ExitCode::SUCCESS;
    };
    match trust::trust_project(&workdir, &presented) {
        Ok(Some(summary)) => {
            eprintln!("kage: trusted {}:", summary.path.display());
            for item in &summary.items {
                eprintln!("  - {item}");
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("kage: nothing left to trust in {}", workdir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: trust: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(keys: &[&str], agents: &[&str], skills: &[&str]) -> TrustSummary {
        TrustSummary {
            path: PathBuf::from("/p/.kage"),
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            items: Vec::new(),
            agents: agents.iter().map(|a| (*a).to_owned()).collect(),
            skills: skills.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn standing(
        full: bool,
        parts: &[kage_core::config::TrustPart],
        entries: &[&str],
    ) -> StandingTrust {
        StandingTrust {
            full,
            parts: parts.iter().copied().collect(),
            entries: entries.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    #[test]
    fn trust_in_a_path_trusted_project_says_so_without_writing() {
        let dir = Path::new("/dev/project");
        let standing = standing(true, &[], &["~/dev", "~/other"]);
        assert_eq!(
            paths_trust_message(dir, &standing),
            "kage: /dev/project is already trusted by [trust] paths (~/dev, ~/other)"
        );
    }

    #[test]
    fn trust_lists_the_parts_the_config_covers() {
        let standing = standing(
            false,
            &[
                kage_core::config::TrustPart::Instructions,
                kage_core::config::TrustPart::Skills,
            ],
            &["~/work/client"],
        );
        assert_eq!(
            parts_trust_message(&standing),
            "kage: [trust] parts already cover instructions, skills here (~/work/client)"
        );
    }

    #[test]
    fn revoke_in_a_config_trusted_project_points_at_the_config() {
        let dir = Path::new("/dev/project");
        let full = standing(true, &[], &["~/dev"]);
        assert_eq!(
            revoke_from_config_message(dir, &full),
            "kage: /dev/project is trusted by [trust] paths (~/dev); \
             edit your user config to change it"
        );
        let parts = standing(false, &[kage_core::config::TrustPart::Instructions], &["/"]);
        assert_eq!(
            revoke_from_config_message(dir, &parts),
            "kage: [trust] parts cover instructions here (/); \
             edit your user config to change it"
        );
    }

    #[test]
    fn warning_names_settings_and_agents() {
        let dir = Path::new("/p");
        assert_eq!(
            warning(
                dir,
                &summary(&["mcp", "agents"], &["reviewer", "auditor"], &[])
            ),
            "kage: ignoring untrusted .kage/config.toml settings (mcp) and project agents \
             (reviewer, auditor) in /p. Run `kage trust` in that directory to allow them."
        );
        assert_eq!(
            warning(dir, &summary(&["agents"], &["reviewer"], &[])),
            "kage: ignoring untrusted project agents (reviewer) in /p. \
             Run `kage trust` in that directory to allow them."
        );
        assert_eq!(
            warning(dir, &summary(&["skills"], &[], &["helper"])),
            "kage: ignoring untrusted project skills (helper) in /p. \
             Run `kage trust` in that directory to allow them."
        );
    }
}
