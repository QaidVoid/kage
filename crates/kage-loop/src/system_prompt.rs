//! Compose a system prompt that pairs a role description with a small
//! `<environment>` block so the model knows where it is, when it is,
//! and what model it is talking through.
//!
//! Without this header the model has nothing to ground filesystem or
//! shell commands on, and reliably hallucinates paths like
//! `/home/user`. Pi's coding agent has a similar block; we keep it
//! minimal: ASCII only, no markdown headings, no narrative.

use std::path::Path;

/// Inputs to [`compose`]. The CLI fills these once at startup; tests
/// pass a fixed seed for deterministic snapshots.
#[derive(Clone, Copy, Debug)]
pub struct EnvContext<'a> {
    /// Working directory the agent was started in.
    pub cwd: &'a Path,
    /// Lowercase OS family, e.g. `linux`, `macos`, `windows`. Pass
    /// [`std::env::consts::OS`].
    pub os: &'a str,
    /// User's interactive shell, when known (`$SHELL`). The model
    /// uses this to pick syntax for shell-specific suggestions.
    pub shell: Option<&'a str>,
    /// Today's date in ISO-8601, like `2026-05-09`. Format the
    /// caller controls; we don't reformat.
    pub date: &'a str,
    /// Provider-qualified model id (`anthropic:claude-sonnet-4-6`).
    pub model: &'a str,
}

/// Default role text the CLI sends when the user does not override
/// it via `--system`. Deliberately short: the env block carries the
/// situational context, so the role just sets posture.
pub const DEFAULT_ROLE: &str = "You are kage, a coding agent. Use the provided tools when they help and ask only when blocked.";

/// Build the full system prompt: `role`, a blank line, then a
/// machine-parseable `<environment>` block.
#[must_use]
pub fn compose(role: &str, env: &EnvContext<'_>) -> String {
    let mut out = String::with_capacity(role.len() + 256);
    out.push_str(role.trim_end());
    out.push_str("\n\n<environment>\n");
    out.push_str("cwd: ");
    out.push_str(&env.cwd.display().to_string());
    out.push('\n');
    out.push_str("os: ");
    out.push_str(env.os);
    out.push('\n');
    if let Some(shell) = env.shell {
        out.push_str("shell: ");
        out.push_str(shell);
        out.push('\n');
    }
    out.push_str("date: ");
    out.push_str(env.date);
    out.push('\n');
    out.push_str("model: ");
    out.push_str(env.model);
    out.push_str("\n</environment>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_includes_role_and_env_block() {
        let cwd = Path::new("/tmp/work");
        let env = EnvContext {
            cwd,
            os: "linux",
            shell: Some("/bin/bash"),
            date: "2026-05-09",
            model: "anthropic:claude-sonnet-4-6",
        };
        let out = compose(DEFAULT_ROLE, &env);
        assert!(out.starts_with(DEFAULT_ROLE));
        assert!(out.contains("<environment>"));
        assert!(out.contains("cwd: /tmp/work"));
        assert!(out.contains("os: linux"));
        assert!(out.contains("shell: /bin/bash"));
        assert!(out.contains("date: 2026-05-09"));
        assert!(out.contains("model: anthropic:claude-sonnet-4-6"));
        assert!(out.ends_with("</environment>\n"));
    }

    #[test]
    fn compose_omits_shell_when_unknown() {
        let env = EnvContext {
            cwd: Path::new("."),
            os: "macos",
            shell: None,
            date: "2026-05-09",
            model: "openai:gpt-4o",
        };
        let out = compose(DEFAULT_ROLE, &env);
        assert!(!out.contains("shell:"));
    }
}
