//! Listing recorded sessions in a directory.
//!
//! [`list`] scans a directory for `*.jsonl` files, matching the
//! extension without case, reads each one's header
//! plus the most recent user prompt, and returns the resulting summaries
//! sorted by creation time (newest first).
//!
//! A summary needs only the header and the latest entry, user message
//! and title; the agent marker is read through the same whole-file
//! scan every reader shares. So each file is scanned for line
//! boundaries and entry tags without decoding, and only those few lines
//! are decoded, found by walking back from the end.
//!
//! Even so, reading a head and a tail of every file costs seconds once a
//! directory holds thousands of sessions. The summaries are therefore
//! kept in a catalog beside the sessions ([`crate::catalog`]), keyed by
//! each file's length and mtime, so a listing re-reads only the files
//! that changed since any process last listed.

use std::collections::HashMap;
use std::fs::{File, metadata};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::entry::{EntryId, Header, SessionEntry, SessionId};
use crate::error::SessionError;

/// Kind of the [`SessionEntry::Custom`] entry that marks a session an
/// `agent` call started. Written as the first entry after the header.
pub const AGENT_ENTRY_KIND: &str = "kage:agent";

/// Whether the session file at `path` records an agent an `agent` or
/// `swarm` call started: it carries that call's [`AGENT_ENTRY_KIND`]
/// entry, as [`agent_marker`] reads it. The catalog answers when its
/// row for the file is current; otherwise only the file's header and
/// custom entry lines are decoded. An unreadable file is not one.
#[must_use]
pub fn is_agent_session(path: &Path) -> bool {
    let cataloged = || {
        let meta = metadata(path).ok()?;
        crate::catalog::Catalog::open_existing(path.parent()?)?.is_agent(
            path,
            meta.len(),
            meta.modified().ok(),
        )
    };
    cataloged().unwrap_or_else(|| agent_session_name(path).is_some())
}

/// The one session-identity predicate. `Some` names the agent
/// definition when a session file records an `agent` or `swarm` call
/// start, as [`agent_marker`] reads it. `None` when the file records a
/// user session or cannot be read. The summary, [`is_agent_session`]
/// and `find_last` all read identity through this one scan.
pub(crate) fn agent_session_name(path: &Path) -> Option<String> {
    agent_marker(path).map(|data| {
        data.get("agent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })
}

/// The data of the [`AGENT_ENTRY_KIND`] entry the call that started the
/// session at `path` wrote: the one naming the session the header names
/// as parent. It sits right after the header, or after the history a
/// forked child copied. A fork copies its source's markers, which name
/// another session, so a fork of an agent records none and is a user
/// session. `None` too when the file cannot be read.
#[must_use]
pub fn agent_marker(path: &Path) -> Option<serde_json::Value> {
    let file = File::open(path).ok()?;
    let mut lines = BufReader::new(file).split(b'\n').map_while(Result::ok);
    let parent = match serde_json::from_slice(&lines.next()?) {
        Ok(SessionEntry::Header(header)) => header.parent_session?.to_string(),
        _ => return None,
    };
    // A foreign or hand-edited file can reorder keys or add spacing,
    // so the scan prefilters on the quoted kind alone and decodes only
    // the lines that carry it.
    let needle = format!("\"{AGENT_ENTRY_KIND}\"").into_bytes();
    lines
        .filter(|line| line.windows(needle.len()).any(|at| at == needle.as_slice()))
        .find_map(|line| match serde_json::from_slice::<SessionEntry>(&line) {
            Ok(SessionEntry::Custom(custom))
                if is_agent_marker(&custom)
                    && custom
                        .data
                        .get("parent")
                        .and_then(serde_json::Value::as_str)
                        == Some(parent.as_str()) =>
            {
                Some(custom.data)
            }
            _ => None,
        })
}

/// Whether one custom entry marks the session an `agent` or `swarm`
/// call started: the entry-level form the file scan and replay share.
#[must_use]
pub(crate) fn is_agent_marker(custom: &crate::Custom) -> bool {
    custom.kind == AGENT_ENTRY_KIND
}

/// Kind of the [`SessionEntry::Custom`] entry that records a swarm
/// mode toggle. Its payload is `{"on": bool}`; the latest entry wins.
pub const SWARM_MODE_ENTRY_KIND: &str = "kage:swarm_mode";

/// Kind of the [`SessionEntry::Custom`] entry that records a plan mode
/// toggle. Its payload is `{"on": bool}`; the latest entry wins.
pub const PLAN_MODE_ENTRY_KIND: &str = "kage:plan_mode";

/// One row in `kage list`. Reflects the persisted state of a session file
/// at the moment of listing; subsequent appends will not be visible until
/// [`list`] is called again.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    /// Session id from the header.
    pub id: SessionId,
    /// Absolute path to the session file.
    pub path: PathBuf,
    /// Header creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the most recent entry on the active branch, the
    /// ancestry of the file's head. Entries an undo or rewind
    /// abandoned do not count.
    pub updated_at: DateTime<Utc>,
    /// Working directory recorded in the header.
    pub cwd: PathBuf,
    /// Provider-qualified model from the header.
    pub model: String,
    /// Parent session this one was forked from or spawned by, if any.
    /// Lets callers reconstruct the session forest from a flat directory
    /// listing.
    pub parent_session: Option<SessionId>,
    /// Text of the most recent user message, if any.
    pub last_user_prompt: Option<String>,
    /// Generated session title, from a `Title` entry in the probed head
    /// or tail of the file. `None` for pre-title sessions; callers fall
    /// back to [`Self::last_user_prompt`] for a label.
    pub title: Option<String>,
    /// Agent definition name when an `agent` call started this session,
    /// read from the [`AGENT_ENTRY_KIND`] entry wherever it sits, so a
    /// forked child whose marker follows its copied history counts too.
    pub agent: Option<String>,
    /// Configured name of the external ACP agent that owns this session,
    /// absent for native sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_agent: Option<String>,
}

/// The JSON index of summaries older builds kept in the sessions
/// directory. The first listing imports it into the catalog and removes
/// it.
const LEGACY_INDEX_FILE: &str = ".index.json";

/// The legacy index format this build imports.
const LEGACY_INDEX_VERSION: u32 = 2;

/// The most characters of the latest prompt a summary keeps: enough for
/// any label, without carrying a pasted document per session.
const PROMPT_CHARS: usize = 300;

/// Bytes scanned after the header for an early title: titles are
/// appended when generated, usually right after the first exchange.
const HEAD_PROBE: u64 = 64 * 1024;
/// Bytes read from the end of a file for the tail summary. Bounds the
/// decode work for sessions whose tail is a wall of tool output.
const TAIL_LIMIT: u64 = 512 * 1024;

/// Scan `dir` for `*.jsonl` session files and summarize each.
///
/// Files that fail to open or whose first entry is not a header are skipped
/// silently; this lets `kage list` tolerate stray files in the sessions
/// directory without aborting on the first malformed one. Files with a
/// torn trailing line are summarized using everything that did parse.
pub fn list(dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    SessionCache::default().list(dir)
}

/// Memo of per-file summaries keyed by the file's size and mtime at
/// read time. [`SessionCache::list`] re-reads only the files that
/// changed since the previous call, so repeated listings of a mostly
/// static directory cost one stat per file. The first listing starts
/// from the directory's catalog ([`crate::catalog`]), and each listing
/// writes back the rows of the files it summarized anew or saw go.
#[derive(Default)]
pub struct SessionCache {
    by_path: HashMap<PathBuf, crate::catalog::Row>,
    /// Whether the catalog was read for this cache yet.
    loaded: bool,
    catalog: Option<crate::catalog::Catalog>,
}

/// The legacy JSON index.
#[derive(serde::Deserialize, serde::Serialize)]
struct Index {
    version: u32,
    entries: Vec<IndexEntry>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct IndexEntry {
    size: u64,
    modified: Option<SystemTime>,
    summary: SessionSummary,
}

impl SessionCache {
    /// Scan `dir` for `*.jsonl` session files and summarize each, like
    /// [`list`], reusing cached summaries for files whose length and
    /// mtime are unchanged.
    pub fn list(&mut self, dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        let mut changed = Vec::new();
        let mut imported = false;
        if !self.loaded {
            self.loaded = true;
            self.catalog = crate::catalog::Catalog::open(dir);
            match self.catalog.as_ref().map(crate::catalog::Catalog::rows) {
                Some(Ok(rows)) => self.by_path = rows,
                Some(Err(err)) if crate::catalog::is_corrupt(&err) => {
                    drop(self.catalog.take());
                    self.catalog = crate::catalog::Catalog::rebuild(dir);
                }
                Some(Err(_)) | None => {}
            }
            if self.by_path.is_empty() && self.import_legacy_index(dir) {
                imported = true;
                changed.extend(self.by_path.keys().cloned());
            }
        }
        let mut summaries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in crate::resume::jsonl_candidates(dir)? {
            if let Some(summary) = self.summarize(&path, &mut changed) {
                summaries.push(summary);
            }
            seen.insert(path);
        }
        let removed: Vec<PathBuf> = self
            .by_path
            .keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect();
        for path in &removed {
            self.by_path.remove(path);
        }
        if !changed.is_empty() || !removed.is_empty() {
            self.store(dir, &changed, &removed, imported);
        }
        summaries.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(summaries)
    }

    /// Writes the rows of `changed` and drops those of `removed` in the
    /// catalog. A catalog found corrupt is rebuilt with every row this
    /// cache holds. Any other failure only costs the next process a
    /// rescan. The legacy index goes once the rows `imported` from it are
    /// stored.
    fn store(&mut self, dir: &Path, changed: &[PathBuf], removed: &[PathBuf], imported: bool) {
        let Some(catalog) = self.catalog.as_mut() else {
            return;
        };
        let rows = changed
            .iter()
            .filter_map(|path| self.by_path.get(path).map(|row| (path.as_path(), row)));
        let mut stored = catalog.write(rows, removed.iter().map(PathBuf::as_path));
        if let Err(err) = &stored
            && crate::catalog::is_corrupt(err)
        {
            drop(self.catalog.take());
            self.catalog = crate::catalog::Catalog::rebuild(dir);
            if let Some(catalog) = self.catalog.as_mut() {
                let all = self.by_path.iter().map(|(path, row)| (path.as_path(), row));
                stored = catalog.write(all, std::iter::empty());
            }
        }
        if imported && stored.is_ok() {
            let _ = std::fs::remove_file(dir.join(LEGACY_INDEX_FILE));
        }
    }

    fn summarize(&mut self, path: &Path, changed: &mut Vec<PathBuf>) -> Option<SessionSummary> {
        let meta = metadata(path).ok()?;
        let size = meta.len();
        let modified = meta.modified().ok();
        if let Some((cached_size, cached_modified, summary)) = self.by_path.get(path)
            && *cached_size == size
            && *cached_modified == modified
        {
            return Some(summary.clone());
        }
        // Session files only grow, so a file seen before at a smaller
        // size needs its titles looked for in the appended bytes alone.
        let before = self
            .by_path
            .get(path)
            .filter(|(old, _, _)| *old <= size)
            .map(|(old, _, summary)| (*old, summary.title.clone()));
        let summary = summarize_from(path, before)?;
        changed.push(path.to_owned());
        self.by_path
            .insert(path.to_owned(), (size, modified, summary.clone()));
        Some(summary)
    }

    /// Seeds the memo from the legacy JSON index older builds wrote,
    /// reporting whether it was read. A missing, unreadable or
    /// other-version index seeds nothing.
    fn import_legacy_index(&mut self, dir: &Path) -> bool {
        let Ok(bytes) = std::fs::read(dir.join(LEGACY_INDEX_FILE)) else {
            return false;
        };
        let Ok(index) = serde_json::from_slice::<Index>(&bytes) else {
            return false;
        };
        if index.version != LEGACY_INDEX_VERSION {
            return false;
        }
        // An index row read an agent from any marker, which a fork of an
        // agent copies, so its agent rows are read again.
        for entry in index
            .entries
            .into_iter()
            .filter(|e| e.summary.agent.is_none())
        {
            self.by_path.insert(
                entry.summary.path.clone(),
                (entry.size, entry.modified, entry.summary),
            );
        }
        true
    }
}

/// Rewrites the paths of the files `moved` (from, to) in the catalog of
/// `dir`, or in its legacy index when it has no catalog yet, so a listing after a
/// move keeps their summaries instead of reading every moved file again.
/// A missing or unreadable store is left alone.
pub(crate) fn relocate_index(dir: &Path, moved: &[(PathBuf, PathBuf)]) {
    if moved.is_empty() {
        return;
    }
    // A catalog means the legacy index was imported, or never will be.
    if dir.join(crate::catalog::CATALOG_FILE).exists() {
        if let Some(mut catalog) = crate::catalog::Catalog::open(dir) {
            let _ = catalog.relocate(moved);
        }
        return;
    }
    let Ok(bytes) = std::fs::read(dir.join(LEGACY_INDEX_FILE)) else {
        return;
    };
    let Ok(mut index) = serde_json::from_slice::<Index>(&bytes) else {
        return;
    };
    if index.version != LEGACY_INDEX_VERSION {
        return;
    }
    let to: HashMap<&Path, &Path> = moved
        .iter()
        .map(|(from, to)| (from.as_path(), to.as_path()))
        .collect();
    for entry in &mut index.entries {
        if let Some(new) = to.get(entry.summary.path.as_path()) {
            entry.summary.path = new.to_path_buf();
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&index) {
        let _ = kage_core::fsutil::atomic_write_private(&dir.join(LEGACY_INDEX_FILE), &bytes);
    }
}

#[cfg(test)]
fn summarize_one(path: &Path) -> Option<SessionSummary> {
    summarize_from(path, None)
}

/// Summarizes the session at `path`. `before` is the size it had and
/// the title it showed when last summarized: the title scan then starts
/// where that copy ended instead of at the head.
fn summarize_from(path: &Path, before: Option<(u64, Option<String>)>) -> Option<SessionSummary> {
    let mut file = BufReader::with_capacity(128 * 1024, File::open(path).ok()?);
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = Vec::new();
    let mut consumed = 0u64;

    // Head pass: the header and an early title.
    // Titles are appended when generated, so for a long session the
    // only one usually sits right after the first exchange.
    let mut header = None;
    let mut head_title = None;
    while consumed < HEAD_PROBE {
        buf.clear();
        let Ok(n) = file.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        consumed += n as u64;
        let line = buf.trim_ascii_end();
        if line.is_empty() {
            continue;
        }
        if header.is_none() {
            match serde_json::from_slice::<SessionEntry>(line) {
                Ok(SessionEntry::Header(h)) => header = Some(h),
                _ => return None,
            }
            continue;
        }
        if leading_tag(line) == Some(b"title")
            && let Ok(SessionEntry::Title(t)) = serde_json::from_slice(line)
        {
            head_title = Some(t.title);
        }
    }
    let header = header?;
    let agent = agent_session_name(path);

    // The latest title can sit anywhere: generated after a first
    // exchange whose tool output pushed it past the head probe, or set
    // by a rename mid-session. Agent sessions are never listed by
    // title, so only user sessions pay for the scan.
    let scanned = if agent.is_none() {
        let (start, known) = match before {
            Some((size, title)) => (size, title),
            None => (consumed, head_title.clone()),
        };
        latest_title(&mut file, &mut buf, start).or(known)
    } else {
        None
    };

    let (updated_at, last_user_prompt, tail_title) = scan_tail(&mut file, &mut buf);
    let updated_at = updated_at.unwrap_or(header.ts);

    Some(summary_from_header(
        header,
        path.to_path_buf(),
        updated_at,
        last_user_prompt,
        tail_title.or(scanned).or(head_title),
        agent,
    ))
}

/// The last title entry from byte `start` to the end of the file,
/// decoding only the lines tagged `title`.
fn latest_title(file: &mut BufReader<File>, buf: &mut Vec<u8>, start: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut title = None;
    loop {
        buf.clear();
        match file.read_until(b'\n', buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if leading_tag(buf) == Some(b"title")
            && let Ok(SessionEntry::Title(t)) = serde_json::from_slice(buf.trim_ascii_end())
        {
            title = Some(t.title);
        }
    }
    title
}

/// The `"type"` tag when it is the first key of `line`, the way the
/// writer emits entries.
fn leading_tag(line: &[u8]) -> Option<&[u8]> {
    let rest = line
        .trim_ascii_start()
        .strip_prefix(b"{")?
        .trim_ascii_start();
    let rest = rest.strip_prefix(b"\"type\"")?.trim_ascii_start();
    let rest = rest.strip_prefix(b":")?.trim_ascii_start();
    let rest = rest.strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    Some(&rest[..end])
}

/// Walk the file's tail back recording the latest timestamp, the
/// latest user prompt and the latest title on the active branch, the
/// ancestry of the file's head (see [`crate::tree::SessionTree`]):
/// entries a `Branch` marker abandoned stay in the file and must not
/// surface as the summary's prompt. Bounds the decode work for
/// sessions whose tail is a wall of tool output.
fn scan_tail(
    file: &mut BufReader<File>,
    buf: &mut Vec<u8>,
) -> (Option<DateTime<Utc>>, Option<String>, Option<String>) {
    let Ok(len) = file.seek(SeekFrom::End(0)) else {
        return (None, None, None);
    };
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL_LIMIT)))
        .is_err()
    {
        return (None, None, None);
    }
    let mut tail: Vec<SessionEntry> = Vec::new();
    loop {
        buf.clear();
        let Ok(n) = file.read_until(b'\n', buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let line = buf.trim_ascii_end();
        if !line.is_empty()
            && let Ok(entry) = serde_json::from_slice::<SessionEntry>(line)
        {
            tail.push(entry);
        }
    }
    // The head is the last non-marker entry, or the entry a trailing
    // marker continues from. Walking backwards, each ancestry node's
    // parent is the next-older entry unless a `Branch` marker sits
    // between them; entries in between belong to abandoned branches.
    let Some(head) = tail.last() else {
        return (None, None, None);
    };
    // A file ending on a marker has already moved on: its head is the
    // entry the marker continues from, however far back that sits.
    let mut expecting: Option<EntryId> = Some(match head {
        SessionEntry::Branch(marker) => marker.at,
        other => other.id(),
    });
    // Set just after consuming an ancestry node: the next-older line
    // decides its parent.
    let mut after_node = false;
    let mut updated_at = None;
    let mut last_user_prompt = None;
    let mut title = None;
    for entry in tail.iter().rev() {
        if updated_at.is_some() && last_user_prompt.is_some() && title.is_some() {
            break;
        }
        match entry {
            SessionEntry::Header(_) => break,
            SessionEntry::Branch(marker) => {
                if after_node {
                    expecting = Some(marker.at);
                    after_node = false;
                }
            }
            node => {
                let id = node.id();
                if after_node {
                    // No marker between this entry and its child, so
                    // it is the child's parent.
                    expecting = Some(id);
                    after_node = false;
                }
                if expecting != Some(id) {
                    continue;
                }
                updated_at.get_or_insert_with(|| node.ts());
                match node {
                    SessionEntry::Message(m)
                        if m.message.role == kage_core::Role::User
                            && last_user_prompt.is_none() =>
                    {
                        last_user_prompt = Some(
                            first_text(&m.message)
                                .map(|text| text.chars().take(PROMPT_CHARS).collect()),
                        );
                    }
                    SessionEntry::Title(t) if title.is_none() => title = Some(t.title.clone()),
                    _ => {}
                }
                after_node = true;
                expecting = None;
            }
        }
    }
    (updated_at, last_user_prompt.flatten(), title)
}

fn first_text(message: &kage_core::Message) -> Option<String> {
    for block in &message.content {
        if let kage_core::Content::Text { text } = block {
            return Some(text.clone());
        }
    }
    None
}

fn summary_from_header(
    header: Header,
    path: PathBuf,
    updated_at: DateTime<Utc>,
    last_user_prompt: Option<String>,
    title: Option<String>,
    agent: Option<String>,
) -> SessionSummary {
    let external_agent = header.backend.map(|backend| match backend {
        crate::entry::SessionBackend::ExternalAcp(external) => external.agent_name,
    });
    SessionSummary {
        id: header.session,
        path,
        created_at: header.ts,
        updated_at,
        cwd: header.cwd,
        model: header.model,
        parent_session: header.parent_session,
        last_user_prompt,
        title,
        agent,
        external_agent,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use chrono::Utc;
    use kage_core::{Content, Message, Role};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{
        Custom, EntryId, FORMAT_VERSION, Header, Label, MessageEntry, SessionEntry, SessionId,
    };
    use crate::writer::SessionWriter;

    fn write_session(dir: &Path, name: &str, prompt: &str) -> PathBuf {
        let path = dir.join(name);
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            backend: None,
            parent_entry: None,
        };
        let mut writer = SessionWriter::create(&path, header).unwrap();
        writer
            .append(&SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::User,
                    vec![Content::Text {
                        text: prompt.to_owned(),
                    }],
                    None,
                )),
                usage: None,
            }))
            .unwrap();
        writer
            .append(&SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text: "tail".into(),
                anchor: EntryId::new(),
            }))
            .unwrap();
        path
    }

    #[test]
    fn returns_empty_for_missing_dir() {
        let summaries = list(Path::new("/nonexistent/path/here")).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn returns_empty_for_empty_dir() {
        let dir = tempdir().unwrap();
        let summaries = list(dir.path()).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn a_title_buried_mid_file_still_labels_the_session_and_a_rename_wins() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "a.jsonl", "first ask");
        let line = |entry: &SessionEntry| format!("{}\n", serde_json::to_string(entry).unwrap());
        let title = |text: &str| {
            SessionEntry::Title(crate::SessionTitle {
                id: EntryId::new(),
                ts: Utc::now(),
                title: text.into(),
            })
        };
        let bulk = message(Role::Assistant, text(&"x".repeat(200 * 1024)));
        append_raw(&path, &line(&bulk));
        append_raw(&path, &line(&title("the real title")));
        for _ in 0..4 {
            append_raw(&path, &line(&bulk));
        }
        append_raw(&path, &line(&message(Role::User, text("later ask"))));

        let mut cache = SessionCache::default();
        let listed = cache.list(dir.path()).unwrap();
        assert_eq!(listed[0].title.as_deref(), Some("the real title"));

        for _ in 0..4 {
            append_raw(&path, &line(&bulk));
        }
        append_raw(&path, &line(&title("renamed")));
        for _ in 0..4 {
            append_raw(&path, &line(&bulk));
        }
        let listed = cache.list(dir.path()).unwrap();
        assert_eq!(
            listed[0].title.as_deref(),
            Some("renamed"),
            "the appended bytes are scanned for a newer title"
        );
    }

    fn catalog_rows(dir: &Path) -> HashMap<PathBuf, crate::catalog::Row> {
        crate::catalog::Catalog::open(dir).unwrap().rows().unwrap()
    }

    #[test]
    fn the_catalog_carries_summaries_across_caches_and_drops_deleted_files() {
        let dir = tempdir().unwrap();
        let a = write_session(dir.path(), "a.jsonl", "ask one");
        write_session(dir.path(), "b.jsonl", "ask two");
        let first = list(dir.path()).unwrap();
        let catalog = dir.path().join(crate::CATALOG_FILE);
        assert!(catalog.exists(), "a listing writes the catalog");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&catalog).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the catalog quotes prompts");
        }
        assert_eq!(catalog_rows(dir.path()).len(), 2);
        assert_eq!(
            SessionCache::default().list(dir.path()).unwrap(),
            first,
            "a new cache starts from the catalog"
        );

        std::fs::remove_file(&a).unwrap();
        assert_eq!(list(dir.path()).unwrap().len(), 1);
        assert_eq!(
            catalog_rows(dir.path()).len(),
            1,
            "the deleted file left the catalog"
        );
    }

    #[test]
    fn a_legacy_index_follows_a_move_until_it_is_imported() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "a.jsonl", "ask one");
        let meta = std::fs::metadata(&path).unwrap();
        let mut summary = summarize_one(&path).unwrap();
        summary.title = Some("from the old index".to_owned());
        let index = serde_json::json!({
            "version": LEGACY_INDEX_VERSION,
            "entries": [{"size": meta.len(), "modified": meta.modified().unwrap(), "summary": summary}],
        });
        std::fs::write(
            dir.path().join(LEGACY_INDEX_FILE),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let moved = dir.path().join("kage-personal-0123456789/a.jsonl");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&path, &moved).unwrap();

        relocate_index(dir.path(), &[(path, moved.clone())]);
        let listed = list(dir.path()).unwrap();
        assert_eq!(listed[0].path, moved);
        assert_eq!(
            listed[0].title.as_deref(),
            Some("from the old index"),
            "the moved file keeps its summary"
        );
    }

    #[test]
    fn a_legacy_index_is_imported_then_removed() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "a.jsonl", "ask one");
        let meta = std::fs::metadata(&path).unwrap();
        let mut summary = summarize_one(&path).unwrap();
        summary.title = Some("from the old index".to_owned());
        let index = serde_json::json!({
            "version": LEGACY_INDEX_VERSION,
            "entries": [{"size": meta.len(), "modified": meta.modified().unwrap(), "summary": summary}],
        });
        let legacy = dir.path().join(LEGACY_INDEX_FILE);
        std::fs::write(&legacy, serde_json::to_vec(&index).unwrap()).unwrap();

        let listed = list(dir.path()).unwrap();
        assert_eq!(
            listed[0].title.as_deref(),
            Some("from the old index"),
            "an unchanged file keeps its imported summary"
        );
        assert!(!legacy.exists(), "the import removes the old index");
        assert_eq!(catalog_rows(dir.path()).len(), 1);
    }

    #[test]
    fn a_legacy_index_row_of_an_agent_is_read_again() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "fork.jsonl", "ask one");
        let meta = std::fs::metadata(&path).unwrap();
        let mut summary = summarize_one(&path).unwrap();
        // What an older build recorded for a fork of an agent.
        summary.agent = Some("explore".to_owned());
        let index = serde_json::json!({
            "version": LEGACY_INDEX_VERSION,
            "entries": [{"size": meta.len(), "modified": meta.modified().unwrap(), "summary": summary}],
        });
        let legacy = dir.path().join(LEGACY_INDEX_FILE);
        std::fs::write(&legacy, serde_json::to_vec(&index).unwrap()).unwrap();

        assert_eq!(list(dir.path()).unwrap()[0].agent, None);
        assert!(!legacy.exists(), "the import removes the old index");
    }

    #[test]
    fn a_corrupt_catalog_is_rebuilt() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", "ask one");
        std::fs::write(dir.path().join(crate::CATALOG_FILE), b"not a database").unwrap();
        assert_eq!(list(dir.path()).unwrap().len(), 1);
        assert_eq!(catalog_rows(dir.path()).len(), 1);
    }

    #[test]
    fn a_catalog_damaged_past_its_header_is_rebuilt() {
        let dir = tempdir().unwrap();
        for n in 0..200 {
            write_session(dir.path(), &format!("s{n}.jsonl"), &"ask ".repeat(50));
        }
        list(dir.path()).unwrap();
        let catalog = dir.path().join(crate::CATALOG_FILE);
        let mut bytes = std::fs::read(&catalog).unwrap();
        assert!(bytes.len() > 16 * 4096, "the rows span many pages");
        bytes[8 * 4096..12 * 4096].fill(0xA5);
        std::fs::write(&catalog, bytes).unwrap();

        let mut cache = SessionCache::default();
        assert_eq!(cache.list(dir.path()).unwrap().len(), 200);
        let mut wal = catalog.clone().into_os_string();
        wal.push("-wal");
        assert!(
            std::path::Path::new(&wal).exists(),
            "closing the corrupt connection must not delete the new catalog's WAL"
        );
        assert_eq!(
            catalog_rows(dir.path()).len(),
            200,
            "another connection reads the rebuilt catalog while the cache holds it"
        );
        drop(cache);
    }

    #[test]
    fn only_damage_counts_as_corruption() {
        let failure = |code| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        assert!(crate::catalog::is_corrupt(&failure(
            rusqlite::ffi::SQLITE_CORRUPT
        )));
        assert!(crate::catalog::is_corrupt(&failure(
            rusqlite::ffi::SQLITE_NOTADB
        )));
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_CANTOPEN,
            rusqlite::ffi::SQLITE_READONLY,
        ] {
            assert!(
                !crate::catalog::is_corrupt(&failure(code)),
                "code {code} must not delete a catalog in use"
            );
        }
    }

    #[test]
    fn an_older_kages_index_survives_once_the_catalog_exists() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "a.jsonl", "ask one");
        list(dir.path()).unwrap();
        let legacy = dir.path().join(LEGACY_INDEX_FILE);
        std::fs::write(&legacy, b"{}").unwrap();
        append_raw(&path, "\n");
        list(dir.path()).unwrap();
        assert!(legacy.exists(), "only an import removes the old index");
    }

    #[test]
    fn the_agent_check_trusts_only_a_current_row() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "a.jsonl", "ask one");
        assert!(!is_agent_session(&path), "no catalog yet: the file is read");
        list(dir.path()).unwrap();
        let mut rows = catalog_rows(dir.path());
        let (size, mtime, mut summary) = rows.remove(&path).unwrap();
        summary.agent = Some("planted".to_owned());
        crate::catalog::Catalog::open(dir.path())
            .unwrap()
            .write([(path.as_path(), &(size, mtime, summary))], [])
            .unwrap();
        assert!(is_agent_session(&path), "a current row answers");
        append_raw(&path, "\n");
        assert!(
            !is_agent_session(&path),
            "a stale row is ignored and the file is read"
        );
    }

    #[test]
    fn summarizes_valid_sessions() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", "ask one");
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_session(dir.path(), "b.jsonl", "ask two");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        // Newest first.
        assert!(summaries[0].created_at >= summaries[1].created_at);
        assert!(
            summaries
                .iter()
                .any(|s| s.last_user_prompt.as_deref() == Some("ask one"))
        );
        assert!(
            summaries
                .iter()
                .any(|s| s.last_user_prompt.as_deref() == Some("ask two"))
        );
        for s in &summaries {
            assert!(s.updated_at >= s.created_at);
        }
    }

    #[test]
    fn summary_carries_parent_session() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "root.jsonl", "root prompt");
        let parent = SessionId::new();
        let child_path = dir.path().join("child.jsonl");
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: Some(parent),
            backend: None,
            parent_entry: Some(EntryId::new()),
        };
        SessionWriter::create(&child_path, header).unwrap();

        let summaries = list(dir.path()).unwrap();
        let root = summaries
            .iter()
            .find(|s| s.path.ends_with("root.jsonl"))
            .unwrap();
        let child = summaries
            .iter()
            .find(|s| s.path.ends_with("child.jsonl"))
            .unwrap();
        assert_eq!(root.parent_session, None);
        assert_eq!(child.parent_session, Some(parent));
    }

    #[test]
    fn ignores_non_jsonl_files() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "real.jsonl", "hi");
        std::fs::write(dir.path().join("notes.txt"), b"random").unwrap();
        std::fs::write(dir.path().join("README"), b"random").unwrap();

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
    }

    #[test]
    fn lists_uppercase_extensions() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "REAL.JSONL", "hi");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].last_user_prompt.as_deref(), Some("hi"));
    }

    // APFS and NTFS reject non-UTF-8 names, so the fixture only
    // exists on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn lists_a_session_with_a_non_utf8_stem() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempdir().unwrap();
        let path = dir.path().join(OsStr::from_bytes(b"weird\xff.jsonl"));
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            backend: None,
            parent_entry: None,
        };
        SessionWriter::create(&path, header).unwrap();

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].path, path);
    }

    #[test]
    fn summary_title_is_none_without_a_title_entry() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", "hello");
        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries[0].title, None, "pre-title sessions have None");
        assert_eq!(summaries[0].last_user_prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn latest_title_entry_wins_in_summary() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "t.jsonl", "do a thing");
        let mut writer = crate::writer::SessionWriter::open(&path).unwrap();
        for t in ["first title", "better title"] {
            writer
                .append(&SessionEntry::Title(crate::SessionTitle {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    title: t.to_owned(),
                }))
                .unwrap();
        }
        drop(writer);
        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries[0].title.as_deref(), Some("better title"));
    }

    #[test]
    fn skips_files_without_header_first() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("garbage.jsonl"), b"{\"oops\":true}\n").unwrap();
        write_session(dir.path(), "good.jsonl", "ok");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].last_user_prompt.as_deref(), Some("ok"));
    }

    fn write_agent_session(dir: &Path, name: &str, first_is_agent: bool) -> PathBuf {
        let path = dir.join(name);
        let parent = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "explore".into(),
            parent_session: Some(parent),
            backend: None,
            parent_entry: None,
        };
        let marker = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({
                "parent": parent.to_string(),
                "agent": "explore",
                "description": "map exports",
            }),
        });
        let title = SessionEntry::Title(crate::SessionTitle {
            id: EntryId::new(),
            ts: Utc::now(),
            title: "map exports".into(),
        });
        let mut writer = SessionWriter::create(&path, header).unwrap();
        let entries = if first_is_agent {
            [marker, title]
        } else {
            [title, marker]
        };
        for entry in &entries {
            writer.append(entry).unwrap();
        }
        path
    }

    #[test]
    fn an_agent_marker_anywhere_marks_an_agent_session() {
        let dir = tempdir().unwrap();
        let first = write_agent_session(dir.path(), "agent.jsonl", true);
        let late = write_agent_session(dir.path(), "forked.jsonl", false);
        write_session(dir.path(), "plain.jsonl", "hi");
        assert!(is_agent_session(&first));
        assert!(
            is_agent_session(&late),
            "a forked child's marker follows its copy"
        );
        assert!(!is_agent_session(&dir.path().join("plain.jsonl")));
        assert!(!is_agent_session(&dir.path().join("missing.jsonl")));
    }

    #[test]
    fn summary_reports_the_agent_of_a_spawned_session() {
        let dir = tempdir().unwrap();
        write_agent_session(dir.path(), "agent.jsonl", true);
        write_agent_session(dir.path(), "late.jsonl", false);
        write_session(dir.path(), "plain.jsonl", "hi");

        let summaries = list(dir.path()).unwrap();
        let agent_of = |file: &str| {
            summaries
                .iter()
                .find(|s| s.path.ends_with(file))
                .unwrap()
                .agent
                .clone()
        };
        assert_eq!(agent_of("agent.jsonl").as_deref(), Some("explore"));
        assert_eq!(
            agent_of("late.jsonl").as_deref(),
            Some("explore"),
            "the marker after other entries still names the agent"
        );
        assert_eq!(agent_of("plain.jsonl"), None);
    }

    #[test]
    fn a_fork_child_s_late_marker_names_its_agent_and_marks_the_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("forked.jsonl");
        let parent = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "explore".into(),
            parent_session: Some(parent),
            backend: None,
            parent_entry: None,
        };
        let marker = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({
                "parent": parent.to_string(),
                "agent": "explore",
                "description": "map exports",
            }),
        });
        let mut writer = SessionWriter::create(&path, header).unwrap();
        // The shape a forked child is written in: the copied history
        // first, then the fork notice, then the marker appended after.
        writer
            .append(&message(Role::User, text("copied ask")))
            .unwrap();
        writer
            .append(&message(Role::Assistant, text("copied reply")))
            .unwrap();
        writer
            .append(&SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text: "forked from the parent".into(),
                anchor: EntryId::new(),
            }))
            .unwrap();
        writer.append(&marker).unwrap();
        drop(writer);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary.agent.as_deref(), Some("explore"));
        assert!(is_agent_session(&path));
    }

    /// The summary a full decode of every entry gives, to check that
    /// decoding only the needed lines agrees with it.
    fn full_decode(path: &Path) -> SessionSummary {
        let mut entries = crate::reader::SessionReader::iter(path)
            .unwrap()
            .filter_map(Result::ok);
        let Some(SessionEntry::Header(header)) = entries.next() else {
            panic!("no header");
        };
        let mut updated_at = header.ts;
        let (mut last_user_prompt, mut title, mut agent) = (None, None, None);
        for entry in entries {
            updated_at = entry.ts();
            match entry {
                SessionEntry::Custom(c) if is_agent_marker(&c) => {
                    agent = Some(c.data["agent"].as_str().unwrap_or_default().to_owned());
                }
                SessionEntry::Message(m) if m.message.role == Role::User => {
                    last_user_prompt = first_text(&m.message);
                }
                SessionEntry::Title(t) => title = Some(t.title),
                _ => {}
            }
        }
        summary_from_header(
            header,
            path.to_path_buf(),
            updated_at,
            last_user_prompt,
            title,
            agent,
        )
    }

    fn message(role: Role, content: Vec<Content>) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(role, content, None)),
            usage: None,
        })
    }

    fn text(text: &str) -> Vec<Content> {
        vec![Content::Text { text: text.into() }]
    }

    fn append_raw(path: &Path, line: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(line.as_bytes()).unwrap();
    }

    fn title_entry(title: &str) -> SessionEntry {
        SessionEntry::Title(crate::SessionTitle {
            id: EntryId::new(),
            ts: Utc::now(),
            title: title.into(),
        })
    }

    #[test]
    fn early_title_and_last_user_prompt_survive_a_long_tail() {
        let dir = tempdir().unwrap();
        let path = write_agent_session(dir.path(), "long.jsonl", true);
        let mut writer = SessionWriter::open(&path).unwrap();
        writer
            .append(&message(Role::User, text("first ask")))
            .unwrap();
        writer.append(&title_entry("early title")).unwrap();
        writer
            .append(&message(Role::User, text("second ask")))
            .unwrap();
        for i in 0..50 {
            writer
                .append(&message(Role::Assistant, text(&format!("step {i}"))))
                .unwrap();
            writer
                .append(&message(Role::ToolResult, text(&format!("output {i}"))))
                .unwrap();
        }
        drop(writer);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.title.as_deref(), Some("early title"));
        assert_eq!(summary.last_user_prompt.as_deref(), Some("second ask"));
        assert_eq!(summary.agent.as_deref(), Some("explore"));
    }

    #[test]
    fn a_textless_last_user_message_clears_the_prompt() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "img.jsonl", "with text");
        let mut writer = SessionWriter::open(&path).unwrap();
        let image = Content::Image {
            source: kage_core::MediaSource::Url {
                url: "https://example.com/a.png".into(),
            },
            mime: "image/png".into(),
        };
        writer.append(&message(Role::User, vec![image])).unwrap();
        drop(writer);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.last_user_prompt, None);
    }

    #[test]
    fn an_abandoned_branch_does_not_become_the_summary() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "branch.jsonl", "kept");
        let mut writer = SessionWriter::open(&path).unwrap();
        let reply = message(Role::Assistant, text("kept reply"));
        let reply_at = reply.id();
        writer.append(&reply).unwrap();
        writer
            .append(&message(Role::User, text("dropped")))
            .unwrap();
        writer
            .append(&message(Role::Assistant, text("dropped reply")))
            .unwrap();
        // The in-place undo of the dropped exchange: the conversation
        // continues from the kept reply.
        writer
            .append(&SessionEntry::Branch(crate::BranchEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                at: reply_at,
            }))
            .unwrap();
        drop(writer);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(
            summary.last_user_prompt.as_deref(),
            Some("kept"),
            "the abandoned prompt stays out of the label"
        );
        assert_eq!(summary.title, None);
    }

    #[test]
    fn an_agent_forked_from_an_agent_is_named_by_its_own_marker() {
        let dir = tempdir().unwrap();
        let root = SessionId::new();
        let outer = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "general".into(),
            parent_session: Some(outer),
            backend: None,
            parent_entry: None,
        };
        let marker = |parent: SessionId, agent: &str| {
            SessionEntry::Custom(Custom {
                id: EntryId::new(),
                ts: Utc::now(),
                kind: AGENT_ENTRY_KIND.into(),
                data: serde_json::json!({ "parent": parent.to_string(), "agent": agent }),
            })
        };
        let path = dir.path().join("nested.jsonl");
        let mut writer = SessionWriter::create(&path, header).unwrap();
        // The outer agent's history, its marker included, then this
        // agent's own marker.
        writer.append(&marker(root, "explore")).unwrap();
        writer
            .append(&message(Role::User, text("copied ask")))
            .unwrap();
        writer.append(&marker(outer, "general")).unwrap();
        drop(writer);

        assert_eq!(agent_session_name(&path).as_deref(), Some("general"));
        assert_eq!(
            agent_marker(&path).unwrap()["parent"],
            outer.to_string().as_str()
        );
    }

    #[test]
    fn a_marker_with_reordered_keys_or_spaces_still_names_the_agent() {
        let dir = tempdir().unwrap();
        let parent = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "explore".into(),
            parent_session: Some(parent),
            backend: None,
            parent_entry: None,
        };
        let ts = serde_json::to_string(&Utc::now()).unwrap();
        let id = serde_json::to_string(&EntryId::new()).unwrap();
        let reordered = format!(
            "{{\"data\":{{\"agent\":\"reordered\",\"parent\":\"{parent}\"}},\"kind\":\"kage:agent\",\"ts\":{ts},\"id\":{id},\"type\":\"custom\"}}\n"
        );
        let spaced = format!(
            "{{\"type\": \"custom\", \"id\": {id}, \"ts\": {ts}, \"kind\": \"kage:agent\", \"data\": {{ \"parent\" : \"{parent}\", \"agent\": \"spaced\"}}}}\n"
        );

        for (name, raw, agent) in [
            ("reordered.jsonl", reordered, "reordered"),
            ("spaced.jsonl", spaced, "spaced"),
        ] {
            let path = dir.path().join(name);
            SessionWriter::create(&path, header.clone()).unwrap();
            append_raw(&path, &raw);
            assert!(is_agent_session(&path), "{name} is an agent session");
            assert_eq!(agent_session_name(&path).as_deref(), Some(agent));
        }

        // The marker text inside a message never promotes a user
        // session.
        let plain = write_session(dir.path(), "plain.jsonl", "quotes \"kage:agent\" inline");
        assert!(!is_agent_session(&plain));
    }

    #[test]
    fn a_torn_trailing_line_is_not_counted_or_dated() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "torn.jsonl", "hi");
        append_raw(&path, "{\"type\":\"title\",\"id\":\"01");

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.title, None);
    }

    #[test]
    fn an_entry_whose_tag_is_not_first_is_still_read() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "late.jsonl", "hi");
        let mut writer = SessionWriter::open(&path).unwrap();
        writer.append(&title_entry("tagged")).unwrap();
        drop(writer);
        let line = format!(
            "{{\"title\":\"untagged\",\"id\":{},\"ts\":{},\"type\":\"title\"}}\n\n",
            serde_json::to_string(&EntryId::new()).unwrap(),
            serde_json::to_string(&Utc::now()).unwrap(),
        );
        append_raw(&path, &line);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.title.as_deref(), Some("untagged"));
    }
}
