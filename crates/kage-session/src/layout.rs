//! Where session files live under a sessions directory.
//!
//! ```text
//! <root>/
//!   blobs/                                shared media, content-addressed
//!   <project>/                            one directory per project
//!     <id>.jsonl                          a session a person started
//!     <id>/agents/<agent-id>.jsonl        its agents, nested agents included
//! ```
//!
//! A project directory is named for the repository the session started
//! in, or for its working directory ([`Grouping`]): a readable slug of
//! the path's last two components and the first 10 hex digits of the
//! path's SHA-256, such as `kage-personal-1a2b3c4d5e`. Sessions recorded
//! before this layout sit directly in the root and keep working: every
//! scan and lookup covers them too.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The directory that holds a root session's agents, inside the
/// directory named for the session id.
pub const AGENTS_DIR: &str = "agents";

pub use kage_core::config::SessionGrouping as Grouping;

/// The project directory under `root` that a session started in `cwd`
/// is recorded in.
#[must_use]
pub fn project_dir(root: &Path, cwd: &Path, grouping: Grouping) -> PathBuf {
    let cwd = normalize(cwd);
    let key = match grouping {
        Grouping::Repo => repo_root(&cwd).unwrap_or(cwd),
        Grouping::Cwd => cwd,
    };
    root.join(project_name(&key))
}

/// The directory an agent of the session at `parent` is recorded in: the
/// `agents` directory of the root session, so an agent's own agents land
/// beside it and the tree stays one level deep on disk.
#[must_use]
pub fn agents_dir(parent: &Path) -> PathBuf {
    let dir = parent.parent().unwrap_or(Path::new("."));
    if dir.file_name().is_some_and(|name| name == AGENTS_DIR) {
        return dir.to_path_buf();
    }
    let id = parent.file_stem().unwrap_or_default();
    dir.join(id).join(AGENTS_DIR)
}

/// The file of the agent `id` of the session at `parent`: in
/// [`agents_dir`], or beside `parent` for an agent recorded before this
/// layout. A fork or clone owns copies of its agents
/// ([`crate::fork`]), but one made before that names the agents of the
/// session it copied, which stay with that session, so the lookup follows
/// the header's `parent_session` to the copied session, through every
/// fork before it.
/// The [`agents_dir`] path when none exists. To look up many agents of
/// one session, resolve its [`fork_chain`] once and use [`agent_in`].
#[must_use]
pub fn agent_file(parent: &Path, id: impl std::fmt::Display) -> PathBuf {
    agent_in(&fork_chain(parent), id)
}

/// The session at `session`, then the session it was forked or cloned
/// from, and so on to the original, read from their headers and stopping
/// at a cycle: every place the agents its history names can be recorded.
#[must_use]
pub fn fork_chain(session: &Path) -> Vec<PathBuf> {
    let mut chain = vec![session.to_path_buf()];
    while let Some(source) = chain.last().and_then(|last| forked_from(last)) {
        if chain.contains(&source) {
            break;
        }
        chain.push(source);
    }
    chain
}

/// The file of the agent `id` along `chain`, a session's [`fork_chain`],
/// as [`agent_file`] finds it.
#[must_use]
pub fn agent_in(chain: &[PathBuf], id: impl std::fmt::Display) -> PathBuf {
    let name = format!("{id}.jsonl");
    for session in chain {
        let nested = agents_dir(session).join(&name);
        if nested.exists() {
            return nested;
        }
        if let Some(sibling) = session.parent().map(|dir| dir.join(&name))
            && sibling.exists()
        {
            return sibling;
        }
    }
    let parent = chain.first().map_or(Path::new(""), PathBuf::as_path);
    agents_dir(parent).join(name)
}

/// The most bytes of a header line [`forked_from`] reads. A header holds
/// the system prompt, so it can run to tens of kilobytes, but a first
/// line that never ends is not one.
const HEADER_MAX: u64 = 4 * 1024 * 1024;

/// The session the root session at `session` was forked or cloned from,
/// read from its header: beside it in its project directory, where forks
/// are recorded, or, for a fork of an agent, in the `agents` directory of
/// a session there. `None` for a nested agent, whose `parent_session` is the
/// session that started it, and for a session that is no copy. An agent
/// recorded flat before the layout steps to the session that started it,
/// which callers tell apart by the agent's own marker.
fn forked_from(session: &Path) -> Option<PathBuf> {
    if session
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == AGENTS_DIR)
    {
        return None;
    }
    let mut line = String::new();
    let file = std::io::Read::take(std::fs::File::open(session).ok()?, HEADER_MAX);
    std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut line).ok()?;
    let crate::SessionEntry::Header(header) = serde_json::from_str(&line).ok()? else {
        return None;
    };
    let name = format!("{}.jsonl", header.parent_session?);
    let project = project_of(session);
    let source = project.join(&name);
    if source.exists() {
        return Some(source);
    }
    read_dir(&project)
        .ok()
        .flatten()?
        .into_iter()
        .filter(|(_, is_dir)| *is_dir)
        .map(|(dir, _)| dir.join(AGENTS_DIR).join(&name))
        .find(|agent| agent.exists())
}

/// The file of the session `id` anywhere under `root`, matching the id
/// exactly.
#[must_use]
pub fn find_session(root: &Path, id: &str) -> Option<PathBuf> {
    session_files(root)
        .ok()?
        .into_iter()
        .find(|path| path.file_stem().is_some_and(|stem| *stem == *id))
}

/// The directory a session recorded in its own right would sit in beside
/// the one at `session`: its project directory, the project of the root
/// session for an agent, or the sessions directory itself for a session
/// recorded before this layout. Forks and clones go there.
#[must_use]
pub fn project_of(session: &Path) -> PathBuf {
    let dir = session.parent().unwrap_or(Path::new("."));
    if dir.file_name().is_some_and(|name| name == AGENTS_DIR) {
        return dir
            .ancestors()
            .nth(2)
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    }
    dir.to_path_buf()
}

/// The sessions directory a session file sits under, wherever the layout
/// put it: above the project directory of a session or of an agent's root
/// session, or the directory of a session recorded before the layout,
/// whose agents nest under it in the root itself.
#[must_use]
pub fn root_of(session: &Path) -> PathBuf {
    let dir = session.parent().unwrap_or(Path::new("."));
    let home = if dir.file_name().is_some_and(|name| name == AGENTS_DIR) {
        dir.parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."))
    } else {
        dir
    };
    if home.file_name().is_some_and(is_project_name) {
        return home.parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    home.to_path_buf()
}

/// Whether the session at `session` sits in the sessions directory
/// itself: recorded before this layout and not moved yet. Only there can
/// an agent it started sit beside it rather than in its [`agents_dir`].
#[must_use]
pub fn is_flat(session: &Path) -> bool {
    session.parent().is_some_and(|dir| root_of(session) == dir)
}

/// The name of the project directory for `key`, an absolute, normalized
/// path.
fn project_name(key: &Path) -> String {
    let parts: Vec<String> = key
        .components()
        .rev()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .take(2)
        .collect();
    let mut slug = String::new();
    for c in parts.join("-").chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(c);
    }
    slug.truncate(40);
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    let digest = Sha256::digest(key.as_os_str().as_encoded_bytes());
    format!("{slug}-{}", &hex::encode(digest)[..10])
}

/// Whether `dir` is a project directory, by its name.
pub(crate) fn is_project_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(is_project_name)
}

/// Whether `name` has the shape [`project_name`] gives: a slug, a dash
/// and 10 hex digits.
fn is_project_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.rsplit_once('-').is_some_and(|(slug, hash)| {
        !slug.is_empty() && hash.len() == 10 && hash.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// `path` made absolute against the process's directory, with `.` and
/// `..` resolved by the path alone. Symlinks are not followed, so a
/// directory reached through one is filed as its own project. On Windows
/// the drive letter is uppercased.
fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(prefix) => {
                out.push(prefix.as_os_str().to_string_lossy().to_uppercase());
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The repository `cwd` sits in: the nearest ancestor holding `.jj` or
/// `.git`. A jj workspace and a git worktree point at their main
/// repository, whose root is used, so worktrees group with the project.
/// A git submodule is its own project.
fn repo_root(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let jj = dir.join(".jj");
        if jj.is_dir() {
            return Some(jj_main_root(&jj).unwrap_or_else(|| dir.to_path_buf()));
        }
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(dir.to_path_buf());
        }
        if git.is_file() {
            return Some(git_main_root(&git).unwrap_or_else(|| dir.to_path_buf()));
        }
    }
    None
}

/// The root of the main repository of a jj workspace: its `.jj/repo` is
/// a file naming the main repository's `.jj/repo`, relative to `.jj`.
fn jj_main_root(jj: &Path) -> Option<PathBuf> {
    let pointer = std::fs::read_to_string(jj.join("repo")).ok()?;
    let repo = normalize(&jj.join(pointer.trim()));
    Some(repo.parent()?.parent()?.to_path_buf())
}

/// The root of the main repository of a git worktree: its `.git` file
/// names the worktree's git directory, whose `commondir` names the main
/// repository's `.git`, or a bare repository, which is its own root. A
/// submodule's git directory has no `commondir`.
fn git_main_root(git: &Path) -> Option<PathBuf> {
    let pointer = std::fs::read_to_string(git).ok()?;
    let gitdir = pointer.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = normalize(&git.parent()?.join(gitdir));
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = normalize(&gitdir.join(common.trim()));
    if common.file_name().is_some_and(|name| name == ".git") {
        return Some(common.parent()?.to_path_buf());
    }
    Some(common)
}

/// The session files under `root` in every place the layout puts them:
/// the root itself (sessions recorded before the layout), each project
/// directory, and each session's `agents` directory. A missing root has
/// none, and a project or agents directory that cannot be read is
/// skipped rather than failing the whole scan.
pub(crate) fn session_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let Some(entries) = read_dir(root)? else {
        return Ok(files);
    };
    let agents_of = |dir: &Path, files: &mut Vec<PathBuf>| {
        files.extend(
            read_dir(&dir.join(AGENTS_DIR))
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .filter(|(file, is_dir)| !is_dir && is_jsonl(file))
                .map(|(file, _)| file),
        );
    };
    // A linked directory that leads back into the root is scanned under
    // its own name already; following it too would list its sessions
    // twice.
    let real_root = std::fs::canonicalize(root).ok();
    for (path, is_dir) in entries {
        if !is_dir {
            if is_jsonl(&path) {
                files.push(path);
            }
            continue;
        }
        if path.is_symlink()
            && std::fs::canonicalize(&path).ok().is_some_and(|real| {
                real.parent() == real_root.as_deref()
                    && real.file_name().is_some_and(is_project_name)
            })
        {
            continue;
        }
        if !path.file_name().is_some_and(is_project_name) {
            // The agents of a session recorded before the layout.
            agents_of(&path, &mut files);
            continue;
        }
        for (inner, inner_is_dir) in read_dir(&path).ok().flatten().unwrap_or_default() {
            if inner_is_dir {
                agents_of(&inner, &mut files);
            } else if is_jsonl(&inner) {
                files.push(inner);
            }
        }
    }
    Ok(files)
}

/// The entries of `dir`, each with whether it is a directory, from the
/// entry's own type so no further stat is needed. `None` when `dir` does
/// not exist.
fn read_dir(dir: &Path) -> std::io::Result<Option<Vec<(PathBuf, bool)>>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| {
                entry.map(|entry| {
                    let path = entry.path();
                    // The entry's own type needs no stat, but a symlink
                    // is followed so a linked directory still counts.
                    let is_dir = match entry.file_type() {
                        Ok(kind) if kind.is_symlink() => path.is_dir(),
                        Ok(kind) => kind.is_dir(),
                        Err(_) => false,
                    };
                    (path, is_dir)
                })
            })
            .collect::<Result<_, _>>()
            .map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Whether `path` names a session file: a `*.jsonl`, matching the
/// extension without case.
fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_is_named_for_its_last_two_components_and_its_hash() {
        let name = project_name(Path::new("/home/q/dev/Personal/Kage Tool"));
        let (slug, hash) = name.rsplit_once('-').unwrap();
        assert_eq!(slug, "kage-tool-personal");
        assert_eq!(hash.len(), 10);
        assert_eq!(
            name,
            project_name(Path::new("/home/q/dev/Personal/Kage Tool"))
        );
        assert_ne!(
            name,
            project_name(Path::new("/srv/q/dev/Personal/Kage Tool")),
            "two checkouts of one name stay apart"
        );
        assert!(project_name(Path::new("/")).starts_with("root-"));
        assert!(is_project_name(std::ffi::OsStr::new(&name)));
        assert!(!is_project_name(std::ffi::OsStr::new("sessions")));
    }

    #[test]
    fn normalizing_resolves_dots_by_the_path_alone() {
        let base = std::env::temp_dir();
        assert_eq!(
            normalize(&base.join("a").join(".").join("b").join("..").join("c")),
            normalize(&base).join("a").join("c")
        );
    }

    #[test]
    fn agents_nest_under_their_root_session_one_level_deep() {
        let root = Path::new("/s/kage-personal-0123456789/01ROOT.jsonl");
        let agents = agents_dir(root);
        assert_eq!(
            agents,
            PathBuf::from("/s/kage-personal-0123456789/01ROOT/agents")
        );
        assert_eq!(
            agents_dir(&agents.join("01CHILD.jsonl")),
            agents,
            "an agent's agents sit beside it"
        );
    }

    #[test]
    fn agents_and_forks_find_their_place() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("kage-personal-0123456789");
        let root = project.join("01ROOT.jsonl");
        let child = agent_file(&root, "01CHILD");
        assert_eq!(child, project.join("01ROOT/agents/01CHILD.jsonl"));
        assert_eq!(
            project_of(&child),
            project,
            "a fork of an agent is a project session"
        );
        assert_eq!(project_of(&root), project);

        let header = |session: crate::SessionId, parent: Option<crate::SessionId>| {
            let header = crate::Header {
                version: crate::FORMAT_VERSION,
                session,
                id: crate::EntryId::new(),
                ts: chrono::Utc::now(),
                cwd: PathBuf::from("/w"),
                model: "m".into(),
                backend: None,
                system_prompt: String::new(),
                parent_session: parent,
                parent_entry: None,
            };
            format!(
                "{}\n",
                serde_json::to_string(&crate::SessionEntry::Header(header)).unwrap()
            )
        };
        std::fs::create_dir_all(&project).unwrap();
        let source_id = crate::SessionId::new();
        let fork_id = crate::SessionId::new();
        let source = project.join(format!("{source_id}.jsonl"));
        let fork = project.join(format!("{fork_id}.jsonl"));
        let refork = project.join("01REFORK.jsonl");
        std::fs::write(&source, header(source_id, None)).unwrap();
        std::fs::write(&fork, header(fork_id, Some(source_id))).unwrap();
        std::fs::write(&refork, header(crate::SessionId::new(), Some(fork_id))).unwrap();
        let kid = agents_dir(&source).join("01KID.jsonl");
        std::fs::create_dir_all(kid.parent().unwrap()).unwrap();
        std::fs::write(&kid, b"").unwrap();
        assert_eq!(
            agent_file(&fork, "01KID"),
            kid,
            "a fork finds the agents its history names under the source"
        );
        assert_eq!(agent_file(&refork, "01KID"), kid, "a fork of a fork too");

        let mut last = fork.clone();
        let mut last_id = fork_id;
        for _ in 0..12 {
            let id = crate::SessionId::new();
            last = project.join(format!("{id}.jsonl"));
            std::fs::write(&last, header(id, Some(last_id))).unwrap();
            last_id = id;
        }
        assert_eq!(agent_file(&last, "01KID"), kid, "however long the chain");

        let agent_id = crate::SessionId::new();
        let agent = agents_dir(&source).join(format!("{agent_id}.jsonl"));
        std::fs::write(&agent, header(agent_id, Some(source_id))).unwrap();
        let grandkid = agents_dir(&source).join("01GRANDKID.jsonl");
        std::fs::write(&grandkid, b"").unwrap();
        let agent_fork = project.join("01AGENTFORK.jsonl");
        std::fs::write(&agent_fork, header(crate::SessionId::new(), Some(agent_id))).unwrap();
        assert_eq!(
            agent_file(&agent_fork, "01GRANDKID"),
            grandkid,
            "a fork of an agent finds the agents it started"
        );
        assert_eq!(find_session(dir.path(), "01KID"), Some(kid.clone()));

        let old_parent = dir.path().join("01OLD.jsonl");
        std::fs::write(dir.path().join("01OLDCHILD.jsonl"), b"").unwrap();
        assert_eq!(
            agent_file(&old_parent, "01OLDCHILD"),
            dir.path().join("01OLDCHILD.jsonl"),
            "an agent recorded before the layout is found beside its parent"
        );
        assert_eq!(
            agent_file(&old_parent, "01NEWCHILD"),
            dir.path().join("01OLD/agents/01NEWCHILD.jsonl"),
            "a new agent nests"
        );
    }

    #[test]
    fn the_root_is_found_from_every_depth() {
        let root = Path::new("/s");
        let project = root.join("kage-personal-0123456789");
        assert_eq!(root_of(&root.join("01OLD.jsonl")), root);
        assert_eq!(
            root_of(&root.join("01OLD/agents/01NEW.jsonl")),
            root,
            "a new agent of a session recorded before the layout"
        );
        assert_eq!(root_of(&project.join("01ROOT.jsonl")), root);
        assert_eq!(root_of(&project.join("01ROOT/agents/01CHILD.jsonl")), root);
        assert!(is_flat(&root.join("01OLD.jsonl")));
        assert!(!is_flat(&root.join("01OLD/agents/01NEW.jsonl")));
        assert!(!is_flat(&project.join("01ROOT.jsonl")));
        assert!(!is_flat(&project.join("01ROOT/agents/01CHILD.jsonl")));
        assert_eq!(
            crate::BlobStore::beside(&project.join("01ROOT/agents/01CHILD.jsonl"))
                .path(&"a".repeat(64)),
            crate::BlobStore::beside(&root.join("01OLD.jsonl")).path(&"a".repeat(64)),
            "every session shares the root's media"
        );
    }

    #[test]
    fn a_repository_groups_its_subdirectories_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        assert_eq!(repo_root(&repo.join("src/deep")), Some(repo.clone()));

        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        assert_eq!(repo_root(&worktree), Some(repo.clone()));

        let bare = dir.path().join("foo.git");
        std::fs::create_dir_all(bare.join("worktrees/feature")).unwrap();
        std::fs::write(bare.join("worktrees/feature/commondir"), "../..\n").unwrap();
        let bare_tree = dir.path().join("feature");
        std::fs::create_dir_all(&bare_tree).unwrap();
        std::fs::write(
            bare_tree.join(".git"),
            format!("gitdir: {}\n", bare.join("worktrees/feature").display()),
        )
        .unwrap();
        assert_eq!(
            repo_root(&bare_tree),
            Some(bare.clone()),
            "a worktree of a bare repository groups under the bare repository"
        );

        let submodule = repo.join("vendor/sub");
        std::fs::create_dir_all(repo.join(".git/modules/sub")).unwrap();
        std::fs::create_dir_all(&submodule).unwrap();
        std::fs::write(submodule.join(".git"), "gitdir: ../../.git/modules/sub\n").unwrap();
        assert_eq!(
            repo_root(&submodule),
            Some(submodule.clone()),
            "a submodule is its own project"
        );

        let main = dir.path().join("jjmain");
        std::fs::create_dir_all(main.join(".jj/repo")).unwrap();
        let workspace = dir.path().join("jjws");
        std::fs::create_dir_all(workspace.join(".jj")).unwrap();
        std::fs::write(workspace.join(".jj/repo"), "../../jjmain/.jj/repo").unwrap();
        assert_eq!(repo_root(&workspace), Some(main.clone()));
        assert_eq!(repo_root(&main.join("x")), Some(main));

        assert_eq!(
            project_dir(Path::new("/s"), &repo.join("src"), Grouping::Repo),
            project_dir(Path::new("/s"), &worktree, Grouping::Repo)
        );
        assert_ne!(
            project_dir(Path::new("/s"), &repo.join("src"), Grouping::Cwd),
            project_dir(Path::new("/s"), &repo, Grouping::Cwd)
        );
    }

    #[test]
    fn a_scan_covers_old_project_and_agent_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let project = root.join("kage-personal-0123456789");
        std::fs::create_dir_all(project.join("01ROOT").join(AGENTS_DIR)).unwrap();
        std::fs::create_dir_all(root.join("blobs/sha256")).unwrap();
        std::fs::create_dir_all(root.join("01OLD").join(AGENTS_DIR)).unwrap();
        for file in [
            root.join("01OLD").join(AGENTS_DIR).join("01OLDKID.jsonl"),
            root.join("01OLD.jsonl"),
            project.join("01ROOT.jsonl"),
            project
                .join("01ROOT")
                .join(AGENTS_DIR)
                .join("01CHILD.jsonl"),
        ] {
            std::fs::write(file, b"").unwrap();
        }
        std::fs::write(root.join(".index.json"), b"{}").unwrap();
        let mut found: Vec<String> = session_files(root)
            .unwrap()
            .iter()
            .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, ["01CHILD", "01OLD", "01OLDKID", "01ROOT"]);
        assert!(session_files(&root.join("missing")).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_project_is_scanned_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let project = root.join("kage-personal-0123456789");
        let elsewhere = dir.path().join("kept-9876543210");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(project.join("01ROOT.jsonl"), b"").unwrap();
        std::fs::write(elsewhere.join("01AWAY.jsonl"), b"").unwrap();
        std::os::unix::fs::symlink(&project, root.join("kage-alias-0123456789")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("kage-away-9876543210")).unwrap();
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("01PLAIN.jsonl"), b"").unwrap();
        std::os::unix::fs::symlink(&plain, root.join("kage-plain-0123456789")).unwrap();
        let mut found: Vec<String> = session_files(&root)
            .unwrap()
            .iter()
            .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(
            found,
            ["01AWAY", "01PLAIN", "01ROOT"],
            "a link out of the root, or to a directory not scanned as a project, still counts"
        );
    }
}
