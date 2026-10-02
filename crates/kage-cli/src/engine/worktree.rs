//! Checkouts of their own for agents whose definition asks for
//! `isolation: worktree`: a jj workspace, or a git worktree on a branch
//! of its own, outside the repository. One is made at spawn,
//! checkpointed at the end of each run and removed when the agent is
//! dropped. Nothing is merged: the agent's report says where its work
//! is and how to take it.

use std::path::{Path, PathBuf};
use std::process::Command;

use kage_core::SessionId;

/// One agent's checkout. Dropping it removes the checkout; work the
/// agent left stays in the repository as a jj change or a git branch.
#[derive(Debug)]
pub(super) struct Worktree {
    /// Where the checkout is.
    path: PathBuf,
    /// The agent's working directory: the parent's place in the
    /// repository, inside the checkout.
    workdir: PathBuf,
    /// The repository's main checkout, where the drop runs.
    repo: PathBuf,
    /// The git commit message: the agent and its task.
    label: String,
    vcs: Vcs,
}

#[derive(Debug)]
enum Vcs {
    Jj { workspace: String },
    Git { branch: String, base: String },
}

impl Worktree {
    /// Make a checkout for the agent `id` under `dir`, of the repository
    /// that holds `parent_workdir`: a jj workspace when jj manages it,
    /// else a git worktree on a new branch from `HEAD`. `label` names
    /// the agent's work in a git commit.
    pub(super) fn create(
        parent_workdir: &Path,
        dir: &Path,
        id: SessionId,
        label: String,
    ) -> Result<Self, String> {
        let short = short(id);
        let path = dir.join(id.to_string());
        let target = path.to_string_lossy().into_owned();
        let made = |what: &str, err: String| format!("cannot make the worktree {what}: {err}");
        if let Ok(root) = run(parent_workdir, "jj", &["root"]) {
            std::fs::create_dir_all(dir).map_err(|err| made("directory", err.to_string()))?;
            let workspace = format!("kage-{short}");
            let args = ["workspace", "add", "--name", &workspace, "-r", "@", &target];
            run(parent_workdir, "jj", &args).map_err(|err| made("workspace", err))?;
            let repo = PathBuf::from(root);
            return Ok(Self {
                workdir: inside(&path, &repo, parent_workdir),
                path,
                repo,
                label,
                vcs: Vcs::Jj { workspace },
            });
        }
        if let Ok(root) = run(parent_workdir, "git", &["rev-parse", "--show-toplevel"]) {
            std::fs::create_dir_all(dir).map_err(|err| made("directory", err.to_string()))?;
            let repo = PathBuf::from(root);
            let base =
                run(&repo, "git", &["rev-parse", "HEAD"]).map_err(|err| made("base", err))?;
            let branch = format!("kage/agent-{short}");
            let args = ["worktree", "add", "-b", &branch, &target, "HEAD"];
            run(&repo, "git", &args).map_err(|err| made("checkout", err))?;
            return Ok(Self {
                workdir: inside(&path, &repo, parent_workdir),
                path,
                repo,
                label,
                vcs: Vcs::Git { branch, base },
            });
        }
        Err("worktree isolation needs a git or jj repository".to_owned())
    }

    /// Where the agent works.
    pub(super) fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Record the agent's work so far and describe it for the report:
    /// where it is and the command that takes it.
    pub(super) fn checkpoint(&self) -> String {
        let at = self.path.display();
        match &self.vcs {
            Vcs::Jj { workspace } => {
                let template = ["log", "-r", "@", "--no-graph", "-T", "change_id.short()"];
                let read = run(&self.path, "jj", &template).and_then(|change| {
                    run(&self.path, "jj", &["diff", "--stat", "-r", "@"]).map(|stat| (change, stat))
                });
                match read {
                    Ok((change, stat)) => match summary(&stat) {
                        Some(stat) => format!(
                            "Worktree: jj change {change} in workspace {workspace} ({stat}). To \
                             take it: jj squash --from {change}. Nothing was merged."
                        ),
                        None => format!("Worktree: workspace {workspace} has no changes."),
                    },
                    Err(err) => format!("Worktree: the checkout at {at} could not be read: {err}"),
                }
            }
            Vcs::Git { branch, base } => {
                let committed = run(&self.path, "git", &["status", "--porcelain"])
                    .and_then(|status| {
                        if status.is_empty() {
                            return Ok(());
                        }
                        run(&self.path, "git", &["add", "-A"])?;
                        run(&self.path, "git", &["commit", "-q", "-m", &self.label]).map(|_| ())
                    })
                    .and_then(|()| {
                        run(
                            &self.path,
                            "git",
                            &["diff", "--stat", &format!("{base}..HEAD")],
                        )
                    });
                match committed {
                    Ok(stat) => match summary(&stat) {
                        Some(stat) => format!(
                            "Worktree: branch {branch} ({stat}). To take it: git merge {branch}. \
                             Nothing was merged."
                        ),
                        None => format!("Worktree: branch {branch} has no changes."),
                    },
                    Err(err) => {
                        format!("Worktree: the work at {at} could not be committed: {err}")
                    }
                }
            }
        }
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let target = self.path.to_string_lossy().into_owned();
        match &self.vcs {
            Vcs::Jj { workspace } => {
                if run(&self.repo, "jj", &["workspace", "forget", workspace]).is_ok() {
                    let _ = std::fs::remove_dir_all(&self.path);
                }
            }
            Vcs::Git { branch, base } => {
                if run(&self.repo, "git", &["worktree", "remove", &target]).is_err() {
                    return;
                }
                let ahead = run(
                    &self.repo,
                    "git",
                    &["rev-list", "--count", &format!("{base}..{branch}")],
                );
                if ahead.as_deref() == Ok("0") {
                    let _ = run(&self.repo, "git", &["branch", "-D", branch]);
                }
            }
        }
    }
}

/// The trimmed output of `program args` run in `dir`, or what it
/// printed on failure.
fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("{program}: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The last line of a diff stat, or `None` when nothing changed.
fn summary(stat: &str) -> Option<String> {
    let last = stat.lines().last()?.trim();
    (!last.is_empty() && !last.starts_with("0 files changed")).then(|| last.to_owned())
}

/// `parent_workdir`'s place in `repo`, inside the checkout at `path`.
fn inside(path: &Path, repo: &Path, parent_workdir: &Path) -> PathBuf {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canonical(parent_workdir)
        .strip_prefix(canonical(repo))
        .map_or_else(|_| path.to_path_buf(), |rel| path.join(rel))
}

/// The random tail of `id`, short enough for a branch name.
fn short(id: SessionId) -> String {
    let text = id.to_string().to_ascii_lowercase();
    text[text.len() - 8..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(program: &str) -> bool {
        Command::new(program).arg("--version").output().is_ok()
    }

    fn sh(dir: &Path, program: &str, args: &[&str]) -> String {
        run(dir, program, args).unwrap_or_else(|err| panic!("{err}"))
    }

    /// A git repository with one commit and a `sub` directory.
    fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        sh(dir, "git", &["init", "-q", "-b", "main"]);
        sh(dir, "git", &["config", "user.name", "kage test"]);
        sh(dir, "git", &["config", "user.email", "test@invalid"]);
        sh(dir, "git", &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "a\n").unwrap();
        sh(dir, "git", &["add", "-A"]);
        sh(dir, "git", &["commit", "-q", "-m", "init"]);
        repo
    }

    #[test]
    fn a_git_worktree_keeps_a_changed_branch_and_drops_an_unchanged_one() {
        if !has("git") {
            return;
        }
        let repo = git_repo();
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let tree = Worktree::create(
            &repo.path().join("sub"),
            dir.path(),
            id,
            "general: add b".into(),
        )
        .unwrap();
        assert!(tree.workdir().ends_with("sub"), "{:?}", tree.workdir());
        std::fs::write(tree.workdir().join("b.txt"), "b\n").unwrap();
        let report = tree.checkpoint();
        let branch = format!("kage/agent-{}", short(id));
        assert!(
            report.contains(&format!("branch {branch} (1 file changed")),
            "{report}"
        );
        assert!(report.contains(&format!("git merge {branch}")), "{report}");
        let path = tree.path.clone();
        drop(tree);
        assert!(!path.exists());
        assert_eq!(
            sh(repo.path(), "git", &["branch", "--list", &branch]),
            branch
        );

        let id = SessionId::new();
        let tree = Worktree::create(repo.path(), dir.path(), id, "general: look".into()).unwrap();
        assert!(tree.checkpoint().contains("has no changes"));
        drop(tree);
        let branch = format!("kage/agent-{}", short(id));
        assert_eq!(sh(repo.path(), "git", &["branch", "--list", &branch]), "");
    }

    #[test]
    fn a_jj_workspace_leaves_its_change_after_the_drop() {
        if !has("jj") {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        sh(repo.path(), "jj", &["git", "init", "--quiet"]);
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tree =
            Worktree::create(repo.path(), dir.path(), SessionId::new(), "general".into()).unwrap();
        assert!(tree.checkpoint().contains("has no changes"));
        std::fs::write(tree.workdir().join("b.txt"), "b\n").unwrap();
        let report = tree.checkpoint();
        assert!(report.contains("(1 file changed"), "{report}");
        let change = report
            .split("jj change ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .to_owned();
        let path = tree.path.clone();
        drop(tree);
        assert!(!path.exists());
        let files = sh(repo.path(), "jj", &["file", "list", "-r", &change]);
        assert!(files.lines().any(|f| f == "b.txt"), "{files}");
    }

    #[test]
    fn outside_a_repository_there_is_no_worktree() {
        let plain = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = Worktree::create(plain.path(), dir.path(), SessionId::new(), String::new())
            .unwrap_err();
        assert_eq!(err, "worktree isolation needs a git or jj repository");
    }
}
