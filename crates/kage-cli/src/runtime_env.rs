//! Runtime-environment helpers for the CLI.
//!
//! Today this just composes the agent's system prompt with a small
//! `<environment>` block describing the current cwd, OS, shell, and
//! model. The block lives at the top of the system prompt so the
//! model has concrete grounding for filesystem and shell suggestions
//! and does not, for example, invent paths like `/home/user`.

use std::path::Path;

use kage_core::Skill;
use kage_loop::{EnvContext, compose_system_prompt, with_skills};

/// Look up the context-window size (input tokens) the catalog
/// reports for `qualified_model` (`provider:model`). Returns `None`
/// when either side is missing or the catalog has no entry; callers
/// fall back to the [`kage_loop::AgentContext`] default.
#[must_use]
pub fn context_window_for(qualified_model: &str) -> Option<u64> {
    let (provider, model) = qualified_model.split_once(':')?;
    kage_provider::catalog::model(provider, model)?.context
}

/// Look up the per-turn max output tokens the catalog reports for
/// `qualified_model`. Returned as `u32` because the wire protocols
/// all use 32-bit ints for this field. Saturates at `u32::MAX`. The
/// loop forwards this on every [`kage_provider::StreamRequest`] so
/// the provider's conservative 4K-ish default never silently truncates
/// large tool-call argument JSON.
#[must_use]
pub fn max_output_tokens_for(qualified_model: &str) -> Option<u32> {
    let (provider, model) = qualified_model.split_once(':')?;
    let raw = kage_provider::catalog::model(provider, model)?.output?;
    Some(u32::try_from(raw).unwrap_or(u32::MAX))
}

/// Build the full system prompt for an agent run: `role` (the user's
/// `--system` text or a default), an `<environment>` block, and a
/// `<skills>` block listing every discovered skill (when `skills` is
/// non-empty).
///
/// `model` is the qualified `provider:model` id; `workdir` is the
/// agent's effective working directory (the host's cwd).
#[must_use]
pub fn build_system_prompt(role: &str, workdir: &Path, model: &str, skills: &[Skill]) -> String {
    let shell_owned = std::env::var("SHELL").ok();
    let date_owned = chrono::Utc::now().date_naive().to_string();
    let env = EnvContext {
        cwd: workdir,
        os: std::env::consts::OS,
        shell: shell_owned.as_deref(),
        date: &date_owned,
        model,
    };
    let base = compose_system_prompt(role, &env);
    with_skills(base, skills)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_includes_role_and_env_block() {
        let out = build_system_prompt("you are kage.", Path::new("/tmp/work"), "x:y", &[]);
        assert!(out.starts_with("you are kage."));
        assert!(out.contains("<environment>"));
        assert!(out.contains("cwd: /tmp/work"));
        assert!(out.contains("model: x:y"));
        assert!(!out.contains("<skills>"));
    }

    #[test]
    fn build_appends_skills_when_present() {
        let skill = Skill {
            name: "code-review".into(),
            description: "review code".into(),
            body: "Be terse.".into(),
            disable_model_invocation: false,
            path: std::path::PathBuf::from("/x"),
        };
        let out = build_system_prompt(
            "role",
            Path::new("/tmp"),
            "x:y",
            std::slice::from_ref(&skill),
        );
        assert!(out.contains("<skills>"));
        assert!(out.contains("code-review"));
    }
}
