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

/// The entry the provider itself declares for `qualified_model`
/// (`provider:model`), for custom and plugin providers, else the catalog
/// entry, as a [`kage_provider::ProviderModel`].
fn model_entry(
    registry: &ProviderRegistry,
    qualified_model: &str,
) -> Option<kage_provider::ProviderModel> {
    let (provider_id, model_id) = qualified_model.split_once(':')?;
    let declared = registry
        .get(provider_id)
        .and_then(|p| p.models().into_iter().find(|m| m.id == model_id));
    if declared.is_some() {
        return declared;
    }
    let m = kage_provider::catalog::model(provider_id, model_id)?;
    Some(kage_provider::ProviderModel {
        id: m.id.to_owned(),
        name: m.name.to_owned(),
        context: m.prompt_window(),
        max_output: m.output.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        reasoning: m.reasoning,
        input: m.input,
        interleaved: m.interleaved,
        cost: m.cost,
    })
}

/// Look up how many tokens a prompt to `qualified_model`
/// (`provider:model`) may fill: the provider's own `models()` entry when
/// it declares one, so custom and plugin providers can surface a window,
/// else the catalog's input limit or context window.
#[must_use]
pub fn context_window_for(registry: &ProviderRegistry, qualified_model: &str) -> Option<u64> {
    model_entry(registry, qualified_model)?.context
}

/// Thinking settings `qualified_model` accepts, from the provider's own
/// model list or the catalog. [`Reasoning::Unknown`] when neither
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
/// silently truncates large tool-call argument JSON. The provider's own
/// model list wins over the catalog.
#[must_use]
pub fn max_output_tokens_for(registry: &ProviderRegistry, qualified_model: &str) -> Option<u32> {
    model_entry(registry, qualified_model)?.max_output
}

/// Build the full system prompt for an agent run: `role` (the user's
/// `--system` text or a default), an `<environment>` block, the
/// [`AGENTS.md` instruction files](#instruction-files), and a
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
    let with_instructions = format!("{base}{}", instructions_block(&instruction_files(workdir)));
    with_skills(with_instructions, skills)
}

/// Instruction files that apply to `workdir`, user level first.
///
/// `<config dir>/AGENTS.md` always applies. `<workdir>/.kage/AGENTS.md`
/// applies only once the project is trusted: a cloned repo's
/// instructions are third-party input, so they wait for `kage trust`
/// exactly like the project's agent files. Both files are optional.
fn instruction_files(workdir: &Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let mut files = Vec::new();
    if let Ok(dir) = crate::config_dir() {
        files.push((dir.join("AGENTS.md"), "user_instructions"));
    }
    if kage_core::trust::project_agents_trusted(workdir) {
        files.push((
            workdir.join(".kage").join("AGENTS.md"),
            "project_instructions",
        ));
    }
    files
}

/// The instructions block for `files`: one labeled element per file
/// that exists and holds non-blank text, user level before project so
/// both apply and the project has the last word on conflicts.
/// Missing, unreadable, and blank files contribute nothing.
fn instructions_block(files: &[(std::path::PathBuf, &'static str)]) -> String {
    let mut out = String::new();
    for (path, tag) in files {
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        out.push_str("\n\n<");
        out.push_str(tag);
        out.push_str(" path=\"");
        out.push_str(&path.display().to_string());
        out.push_str("\">\n");
        out.push_str(body);
        out.push_str("\n</");
        out.push_str(tag);
        out.push('>');
    }
    out
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
    fn instructions_block_combines_user_then_project() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.md");
        let project = dir.path().join("project.md");
        let missing = dir.path().join("missing.md");
        let blank = dir.path().join("blank.md");
        std::fs::write(&user, "Be terse.\n").unwrap();
        std::fs::write(&project, "Never use em-dashes.").unwrap();
        std::fs::write(&blank, "  \n").unwrap();
        let out = instructions_block(&[
            (user.clone(), "user_instructions"),
            (missing, "user_instructions"),
            (blank, "user_instructions"),
            (project.clone(), "project_instructions"),
        ]);
        let user_pos = out.find("Be terse.").expect("user text");
        let project_pos = out.find("Never use em-dashes.").expect("project text");
        assert!(user_pos < project_pos, "{out}");
        assert!(out.contains(&format!("<user_instructions path=\"{}\">", user.display())));
        assert!(out.contains(&format!(
            "<project_instructions path=\"{}\">",
            project.display()
        )));
        assert!(!out.contains("missing.md"));
        assert!(!out.contains("blank.md"));
    }

    #[test]
    fn instructions_block_is_empty_when_nothing_reads() {
        let dir = tempfile::tempdir().unwrap();
        let out = instructions_block(&[(dir.path().join("nope.md"), "user_instructions")]);
        assert!(out.is_empty());
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
