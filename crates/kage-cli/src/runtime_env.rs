//! Runtime-environment helpers for the CLI.
//!
//! Today this just composes the agent's system prompt with a small
//! `<environment>` block describing the current cwd, OS, shell, and
//! model. The block lives at the top of the system prompt so the
//! model has concrete grounding for filesystem and shell suggestions
//! and does not, for example, invent paths like `/home/user`.

use std::path::Path;

use kage_core::{Inputs, Reasoning, Skill};
use kage_loop::{EnvContext, compose_system_prompt, with_skills};
use kage_provider::ProviderRegistry;

/// The catalog entry for `qualified_model` (`provider:model`), else
/// the entry the provider itself advertises (custom and plugin
/// providers), as a [`kage_provider::ProviderModel`].
fn model_entry(
    registry: &ProviderRegistry,
    qualified_model: &str,
) -> Option<kage_provider::ProviderModel> {
    let (provider_id, model_id) = qualified_model.split_once(':')?;
    if let Some(m) = kage_provider::catalog::model(provider_id, model_id) {
        return Some(kage_provider::ProviderModel {
            id: m.id.to_owned(),
            name: m.name.to_owned(),
            context: m.prompt_window(),
            max_output: m.output.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            reasoning: m.reasoning,
            input: m.input,
        });
    }
    registry
        .get(provider_id)?
        .models()
        .into_iter()
        .find(|m| m.id == model_id)
}

/// Look up how many tokens a prompt to `qualified_model`
/// (`provider:model`) may fill: the catalog's input limit when it has
/// one, else its context window, else the provider's own `models()`
/// entry, so plugin-registered providers can still surface a window.
#[must_use]
pub fn context_window_for(registry: &ProviderRegistry, qualified_model: &str) -> Option<u64> {
    model_entry(registry, qualified_model)?.context
}

/// Thinking settings `qualified_model` accepts, from the catalog or the
/// provider's own model list. [`Reasoning::Unknown`] when neither
/// knows the model.
#[must_use]
pub fn reasoning_for(registry: &ProviderRegistry, qualified_model: &str) -> Reasoning {
    model_entry(registry, qualified_model).map_or(Reasoning::Unknown, |m| m.reasoning)
}

/// Inputs `qualified_model` accepts. Empty when unknown.
#[must_use]
pub fn input_for(registry: &ProviderRegistry, qualified_model: &str) -> Inputs {
    model_entry(registry, qualified_model).map_or_else(Inputs::default, |m| m.input)
}

/// Look up the per-turn max output tokens for `qualified_model`.
/// Saturates at `u32::MAX`. The loop forwards this on every stream
/// request so the provider's conservative 4K-ish default never
/// silently truncates large tool-call argument JSON. Falls back to the
/// registry's own model list for plugin-registered providers.
#[must_use]
pub fn max_output_tokens_for(registry: &ProviderRegistry, qualified_model: &str) -> Option<u32> {
    model_entry(registry, qualified_model)?.max_output
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
