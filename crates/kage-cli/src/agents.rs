//! Agent definition discovery: the built-ins, the user config
//! directory and, once trusted, the project.

use std::path::{Path, PathBuf};

use kage_core::agents::{self, AgentDefs, AgentSource};

/// Load the agent definitions for `workdir`: the built-ins, then
/// `<config dir>/agents`, then `<workdir>/.kage/agents` when the
/// project is trusted. A later definition replaces an earlier one of
/// the same name. Returns the definitions and one message per file
/// that failed to load.
#[expect(dead_code, reason = "the agent tool is not wired yet")]
pub(crate) fn load(workdir: &Path) -> (AgentDefs, Vec<String>) {
    let mut dirs = Vec::new();
    if let Ok(dir) = crate::config_dir() {
        dirs.push((dir.join("agents"), AgentSource::User));
    }
    if kage_core::trust::project_agents_trusted(workdir) {
        dirs.push((agents::project_dir(workdir), AgentSource::Project));
    }
    merge(&dirs)
}

fn merge(dirs: &[(PathBuf, AgentSource)]) -> (AgentDefs, Vec<String>) {
    let mut defs = AgentDefs::builtin();
    let mut errors = Vec::new();
    for (dir, source) in dirs {
        for result in agents::load_agents_dir(dir, *source) {
            match result {
                Ok(def) => defs.insert(def),
                Err(err) => errors.push(err.to_string()),
            }
        }
    }
    (defs, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_dirs_win_and_broken_files_are_reported() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("explore.md"),
            "---\ndescription: user explore\n---\n",
        )
        .unwrap();
        std::fs::write(
            user.path().join("reviewer.md"),
            "---\ndescription: user reviewer\n---\n",
        )
        .unwrap();
        std::fs::write(
            project.path().join("reviewer.md"),
            "---\ndescription: project reviewer\n---\n",
        )
        .unwrap();
        std::fs::write(project.path().join("broken.md"), "---\ntools: read\n---\n").unwrap();
        let (defs, errors) = merge(&[
            (user.path().to_path_buf(), AgentSource::User),
            (project.path().to_path_buf(), AgentSource::Project),
        ]);
        assert_eq!(defs.get("explore").unwrap().description, "user explore");
        let reviewer = defs.get("reviewer").unwrap();
        assert_eq!(reviewer.description, "project reviewer");
        assert_eq!(reviewer.source, AgentSource::Project);
        assert_eq!(defs.get("general").unwrap().source, AgentSource::Builtin);
        assert!(defs.get("broken").is_none());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("broken.md"), "{errors:?}");
    }
}
