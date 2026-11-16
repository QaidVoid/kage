//! Moving sessions recorded before the project layout into it.
//!
//! Older builds wrote every session flat into the sessions directory, and
//! the agents of such a session started after the layout landed went to
//! `<root>/<id>/agents/`. [`migrate`] files each into the layout
//! ([`crate::layout`]): a session a person started into its project
//! directory, an agent into the `agents` directory of the session its
//! `parent_session` chain starts at. Every move is a rename under the
//! session's own lock, so a crash leaves a file either moved or not, and
//! a session another process holds stays put until a later run. The
//! `.lock` sidecars older builds kept beside each session go too, unless
//! an older build still holds one.

use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use crate::entry::{SessionEntry, SessionId};
use crate::layout::{AGENTS_DIR, Grouping};

/// The lock a migration holds, so two processes never migrate at once.
const LOCK_FILE: &str = ".layout.lock";

/// The files a migration could not move for good, one path per line,
/// which later runs leave alone: a destination already taken, or a file
/// with no session header that no writer holds. Remove a line to have
/// the next run try that file again.
pub const SKIP_FILE: &str = ".layout-skipped";

/// How many agents deep [`migrate`] follows a `parent_session` chain.
const CHAIN_DEPTH: usize = 64;

/// The most bytes of a header line read.
const HEADER_MAX: u64 = 4 * 1024 * 1024;

/// What a [`migrate`] run did.
#[derive(Debug, Default)]
pub struct Migration {
    /// Each file moved, from where to where.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Files another process holds, left for a later run.
    pub held: Vec<PathBuf>,
    /// Files that could not be moved this time, with why. Later runs try
    /// them again.
    pub failed: Vec<(PathBuf, String)>,
    /// Files that cannot be moved, with why, listed in [`SKIP_FILE`] for
    /// later runs to leave alone.
    pub refused: Vec<(PathBuf, String)>,
}

/// One session file left in a place older builds used.
struct Legacy {
    path: PathBuf,
    id: SessionId,
    cwd: PathBuf,
    parent: Option<SessionId>,
    agent: bool,
}

/// Files the sessions recorded before the project layout under `root`
/// into it, grouping by `grouping`. Another process migrating at the same
/// time makes this one do nothing.
///
/// # Errors
///
/// When the sessions directory cannot be read.
pub fn migrate(root: &Path, grouping: Grouping) -> std::io::Result<Migration> {
    let mut report = Migration::default();
    let skipped = skipped(root);
    let (legacy, sidecars) = legacy_files(root)?;
    let legacy: Vec<PathBuf> = legacy
        .into_iter()
        .filter(|path| !skipped.contains(path))
        .collect();
    if legacy.is_empty() && sidecars.is_empty() {
        return Ok(report);
    }
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(LOCK_FILE))
    else {
        return Ok(report);
    };
    if lock.try_lock().is_err() {
        return Ok(report);
    }

    let sessions: Vec<Legacy> = legacy
        .into_iter()
        .filter_map(|path| read_legacy(root, path, &mut report))
        .collect();
    let by_id: HashMap<SessionId, &Legacy> = sessions.iter().map(|s| (s.id, s)).collect();
    let mut placed: HashMap<SessionId, PathBuf> = HashMap::new();

    // Sessions a person started first, so their agents know where to go.
    for session in sessions.iter().filter(|s| !s.agent) {
        let dst = crate::layout::project_dir(root, &session.cwd, grouping)
            .join(format!("{}.jsonl", session.id));
        if move_one(&session.path, &dst, &mut report) {
            placed.insert(session.id, dst);
        }
    }
    for session in sessions.iter().filter(|s| s.agent) {
        let dst = match root_session(session, &by_id, &placed, root) {
            Home::Placed(home) => {
                crate::layout::agents_dir(&home).join(format!("{}.jsonl", session.id))
            }
            // Its session did not move this run, held or refused: the
            // agent waits beside it for a later run.
            Home::Waiting => continue,
            // An agent whose session is gone is filed as a session of
            // its own, in the project of its working directory.
            Home::Gone => crate::layout::project_dir(root, &session.cwd, grouping)
                .join(format!("{}.jsonl", session.id)),
        };
        move_one(&session.path, &dst, &mut report);
    }
    // A sidecar whose session is still here may be about to be locked by
    // an older build, which on Windows has no other lock: it stays.
    for sidecar in sidecars {
        if !sidecar.with_extension("").exists() {
            crate::writer::remove_stray_sidecar(&sidecar);
        }
    }
    remove_empty_agent_dirs(root);
    crate::list::relocate_index(root, &report.moved);
    remember_refused(root, &report.refused);
    Ok(report)
}

/// The files earlier runs could not move for good.
fn skipped(root: &Path) -> std::collections::HashSet<PathBuf> {
    std::fs::read_to_string(root.join(SKIP_FILE))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Adds the files this run refused for good to [`SKIP_FILE`].
fn remember_refused(root: &Path, refused: &[(PathBuf, String)]) {
    if refused.is_empty() {
        return;
    }
    let mut lines = String::new();
    for (path, _) in refused {
        if let Some(path) = path.to_str() {
            lines.push_str(path);
            lines.push('\n');
        }
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(SKIP_FILE))
        .and_then(|mut file| std::io::Write::write_all(&mut file, lines.as_bytes()));
}

/// Where an agent's root session is, as far as a migration run knows.
enum Home {
    /// In the layout, at this path.
    Placed(PathBuf),
    /// Still in a legacy place: held, or refused this run.
    Waiting,
    /// No longer recorded.
    Gone,
}

/// The session files under `root` in places older builds used, flat in the
/// root and in the `agents` directory of a flat session, and the `.lock`
/// sidecars older builds kept there.
fn legacy_files(root: &Path) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut files = Vec::new();
    let mut sidecars = Vec::new();
    let mut sort = |path: PathBuf| {
        if is_jsonl(&path) {
            files.push(path);
        } else if is_sidecar(&path) {
            sidecars.push(path);
        }
    };
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((files, sidecars)),
        Err(err) => return Err(err),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_dir {
            sort(path);
            continue;
        }
        if crate::layout::is_project_dir(&path) {
            continue;
        }
        if let Ok(agents) = std::fs::read_dir(path.join(AGENTS_DIR)) {
            for agent in agents.flatten() {
                sort(agent.path());
            }
        }
    }
    Ok((files, sidecars))
}

/// Whether `path` is a lock sidecar older builds kept: `<id>.jsonl.lock`.
fn is_sidecar(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "lock")
        && path
            .file_stem()
            .is_some_and(|stem| is_jsonl(Path::new(stem)))
}

/// The header facts of the session at `path`, or `None` (recorded as a
/// failure) when its header cannot be read.
fn read_legacy(root: &Path, path: PathBuf, report: &mut Migration) -> Option<Legacy> {
    let header = std::fs::File::open(&path).ok().and_then(|file| {
        let mut line = String::new();
        std::io::BufReader::new(file.take(HEADER_MAX))
            .read_line(&mut line)
            .ok()?;
        match serde_json::from_str(&line).ok()? {
            SessionEntry::Header(header) => Some(header),
            _ => None,
        }
    });
    let Some(header) = header else {
        // A writer creating the file holds it before the header lands.
        if crate::writer::is_locked(&path) {
            report.held.push(path);
        } else {
            report
                .refused
                .push((path, "no readable session header".to_owned()));
        }
        return None;
    };
    // Only an agent or a fork names a parent, and only an agent carries
    // the marker, so a session without a parent needs no scan.
    let agent = header.parent_session.is_some() && crate::list::is_agent_session(&path);
    // A relative or empty working directory would be read against wherever
    // kage runs now, so such a session gets the project of the sessions
    // directory itself.
    let cwd = if header.cwd.is_absolute() {
        header.cwd
    } else {
        root.to_path_buf()
    };
    Some(Legacy {
        path,
        id: header.session,
        cwd,
        parent: header.parent_session,
        agent,
    })
}

/// Where the session `agent`'s chain of parents starts: the root session
/// this run moved, or a session already in the layout, an agent included,
/// whose [`crate::layout::agents_dir`] is its root's.
fn root_session(
    agent: &Legacy,
    by_id: &HashMap<SessionId, &Legacy>,
    placed: &HashMap<SessionId, PathBuf>,
    root: &Path,
) -> Home {
    let Some(mut parent) = agent.parent else {
        return Home::Gone;
    };
    for _ in 0..CHAIN_DEPTH {
        match by_id.get(&parent) {
            Some(session) if session.agent => match session.parent {
                Some(next) => parent = next,
                None => return Home::Gone,
            },
            Some(_) => {
                return placed
                    .get(&parent)
                    .map_or(Home::Waiting, |home| Home::Placed(home.clone()));
            }
            None => {
                return match crate::layout::find_session(root, &parent.to_string()) {
                    // A session in a project directory, the root's or an
                    // agent's, is filed; one left in a legacy place, such
                    // as a skipped file, is not yet.
                    Some(home)
                        if crate::layout::is_project_dir(&crate::layout::project_of(&home)) =>
                    {
                        Home::Placed(home)
                    }
                    Some(_) => Home::Waiting,
                    None => Home::Gone,
                };
            }
        }
    }
    Home::Gone
}

/// Moves `path` to `dst`, reporting how it went. Whether it moved.
fn move_one(path: &Path, dst: &Path, report: &mut Migration) -> bool {
    match crate::writer::move_locked(path, dst) {
        Ok(()) => {
            report.moved.push((path.to_path_buf(), dst.to_path_buf()));
            true
        }
        Err(crate::SessionError::Locked { .. }) => {
            report.held.push(path.to_path_buf());
            false
        }
        Err(crate::SessionError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            report.refused.push((
                path.to_path_buf(),
                "a session file is already there".to_owned(),
            ));
            false
        }
        Err(err) => {
            report.failed.push((path.to_path_buf(), err.to_string()));
            false
        }
    }
}

/// Removes the `agents` directories, and their session directories, that
/// the moves left empty in the root.
fn remove_empty_agent_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) && !crate::layout::is_project_dir(&dir)
        {
            let agents = dir.join(AGENTS_DIR);
            if std::fs::remove_dir(&agents).is_ok() {
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("jsonl"))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::entry::{Custom, EntryId, FORMAT_VERSION, Header};
    use crate::writer::SessionWriter;

    /// Writes a flat session `id` at `path`, started in `cwd`, with
    /// `parent` as its parent session and an agent marker when `agent`.
    fn session(path: &Path, cwd: &Path, parent: Option<SessionId>, agent: bool) -> SessionId {
        let id = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: cwd.to_path_buf(),
            model: "m".into(),
            backend: None,
            system_prompt: String::new(),
            parent_session: parent,
            parent_entry: None,
        };
        let path = path.join(format!("{id}.jsonl"));
        let mut writer = SessionWriter::create(&path, header).unwrap();
        if agent {
            writer
                .append(&SessionEntry::Custom(Custom {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kind: crate::list::AGENT_ENTRY_KIND.into(),
                    data: serde_json::json!({ "agent": "explore", "description": "look" }),
                }))
                .unwrap();
        }
        id
    }

    #[test]
    fn older_sessions_move_into_the_layout_and_agents_follow_their_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let elsewhere = dir.path().join("elsewhere");

        let main = session(&root, &repo.join("src"), None, false);
        let child = session(&root, &repo, Some(main), true);
        let grandchild = session(&root, &repo, Some(child), true);
        let nested_dir = root.join(main.to_string()).join(AGENTS_DIR);
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested = session(&nested_dir, &repo, Some(main), true);
        let of_child_dir = root.join(child.to_string()).join(AGENTS_DIR);
        std::fs::create_dir_all(&of_child_dir).unwrap();
        let of_child = session(&of_child_dir, &repo, Some(child), true);
        let fork = session(&root, &repo, Some(main), false);
        let orphan = session(&root, &elsewhere, Some(SessionId::new()), true);
        crate::list(&root).unwrap();

        let report = migrate(&root, Grouping::Repo).unwrap();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.held.is_empty());
        assert_eq!(report.moved.len(), 7);

        let project = crate::layout::project_dir(&root, &repo, Grouping::Repo);
        let home = project.join(format!("{main}.jsonl"));
        assert!(
            home.exists(),
            "the root session is in its repository's project"
        );
        assert!(project.join(format!("{fork}.jsonl")).exists(), "a fork too");
        for agent in [child, grandchild, nested, of_child] {
            assert!(
                crate::layout::agents_dir(&home)
                    .join(format!("{agent}.jsonl"))
                    .exists(),
                "every agent nests under the root session"
            );
        }
        assert!(
            crate::layout::project_dir(&root, &elsewhere, Grouping::Repo)
                .join(format!("{orphan}.jsonl"))
                .exists(),
            "an agent whose session is gone is filed by its own directory"
        );
        let left: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.') && !name.contains('-'))
            .collect();
        assert!(left.is_empty(), "nothing is left flat: {left:?}");

        let rows = crate::catalog::Catalog::open_existing(&root)
            .unwrap()
            .rows()
            .unwrap();
        let mut keys: Vec<_> = rows.keys().cloned().collect();
        let mut moved_to: Vec<_> = report.moved.iter().map(|(_, to)| to.clone()).collect();
        keys.sort();
        moved_to.sort();
        assert_eq!(keys, moved_to, "every row follows its file, agents too");
        assert!(
            rows.iter()
                .all(|(path, (_, _, summary))| summary.path == *path),
            "each summary names its new path"
        );
        assert_eq!(crate::list(&root).unwrap().len(), 7);
        assert!(migrate(&root, Grouping::Repo).unwrap().moved.is_empty());
    }

    #[test]
    fn deleting_a_session_takes_its_agents_along() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("kage-personal-0123456789");
        std::fs::create_dir_all(&project).unwrap();
        let main = session(&project, dir.path(), None, false);
        let home = project.join(format!("{main}.jsonl"));
        let agents = crate::layout::agents_dir(&home);
        std::fs::create_dir_all(&agents).unwrap();
        let agent = session(&agents, dir.path(), Some(main), true);
        let sibling = session(&agents, dir.path(), Some(main), true);
        crate::writer::delete_with_agents(&agents.join(format!("{agent}.jsonl"))).unwrap();
        assert!(
            agents.exists(),
            "deleting an agent keeps its siblings' directory"
        );
        crate::writer::delete_with_agents(&agents.join(format!("{sibling}.jsonl"))).unwrap();
        assert!(
            !project.join(main.to_string()).exists(),
            "deleting the last agent clears its directory"
        );
        std::fs::create_dir_all(&agents).unwrap();
        session(&agents, dir.path(), Some(main), true);
        crate::writer::delete_with_agents(&home).unwrap();
        assert!(!home.exists());
        assert!(
            !project.join(main.to_string()).exists(),
            "the agents went too"
        );
    }

    #[test]
    fn agents_wait_for_a_held_root_and_partial_runs_resume() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let main = session(&root, dir.path(), None, false);
        let child = session(&root, dir.path(), Some(main), true);
        let main_path = root.join(format!("{main}.jsonl"));
        let writer = SessionWriter::open(&main_path).unwrap();
        let report = migrate(&root, Grouping::Cwd).unwrap();
        assert!(report.moved.is_empty(), "the agent waits for its root");
        assert!(root.join(format!("{child}.jsonl")).exists());
        drop(writer);

        let report = migrate(&root, Grouping::Cwd).unwrap();
        assert_eq!(report.moved.len(), 2);
        let home = crate::layout::project_dir(&root, dir.path(), Grouping::Cwd)
            .join(format!("{main}.jsonl"));
        let agents = crate::layout::agents_dir(&home);
        assert!(agents.join(format!("{child}.jsonl")).exists());

        // A grandchild a crashed run left behind joins its root's agents.
        let grandchild = session(&root, dir.path(), Some(child), true);
        migrate(&root, Grouping::Cwd).unwrap();
        assert!(agents.join(format!("{grandchild}.jsonl")).exists());
    }

    #[test]
    fn odd_files_are_reported_once_and_relative_cwds_get_one_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stray = root.join("stray.jsonl");
        std::fs::write(&stray, b"not a header\n").unwrap();
        let relative = session(&root, Path::new("somewhere"), None, false);
        let report = migrate(&root, Grouping::Cwd).unwrap();
        assert_eq!(report.refused.len(), 1);
        assert!(
            crate::layout::project_dir(&root, &root, Grouping::Cwd)
                .join(format!("{relative}.jsonl"))
                .exists()
        );
        let again = migrate(&root, Grouping::Cwd).unwrap();
        assert!(
            again.refused.is_empty(),
            "a file refused for good is left alone"
        );
        assert!(stray.exists());
    }

    #[test]
    fn stray_sidecars_of_older_builds_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let gone = root.join("01GONE.jsonl.lock");
        std::fs::write(&gone, b"").unwrap();
        migrate(&root, Grouping::Cwd).unwrap();
        assert!(!gone.exists());
    }

    #[test]
    fn a_session_in_use_stays_until_a_later_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let id = session(&root, dir.path(), None, false);
        let path = root.join(format!("{id}.jsonl"));
        let writer = SessionWriter::open(&path).unwrap();
        let report = migrate(&root, Grouping::Cwd).unwrap();
        assert_eq!(report.held, std::slice::from_ref(&path));
        assert!(path.exists());
        drop(writer);
        let report = migrate(&root, Grouping::Cwd).unwrap();
        assert_eq!(report.moved.len(), 1);
        assert!(!path.exists());
    }
}
