//! Listing recorded sessions in a directory.
//!
//! [`list`] scans a directory for `*.jsonl` files, reads each one's header
//! plus the most recent user prompt, and returns the resulting summaries
//! sorted by creation time (newest first).
//!
//! A summary needs only the header, the entry after it, and the latest
//! entry, user message and title. So each file is scanned for line
//! boundaries and entry tags without decoding, and only those few lines
//! are decoded, found by walking back from the end.
//!
//! Even so, reading a head and a tail of every file costs seconds once a
//! directory holds thousands of sessions. The summaries are therefore
//! kept in an index file beside the sessions ([`INDEX_FILE`]), keyed by
//! each file's length and mtime, so a listing re-reads only the files
//! that changed since any process last listed.

use std::collections::HashMap;
use std::fs::{File, metadata};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::entry::{Header, SessionEntry, SessionId};
use crate::error::SessionError;

/// Kind of the [`SessionEntry::Custom`] entry that marks a session an
/// `agent` call started. Written as the first entry after the header.
pub const AGENT_ENTRY_KIND: &str = "kage:agent";

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
    /// Timestamp of the most recently appended entry.
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
    /// read from the [`AGENT_ENTRY_KIND`] entry right after the header.
    pub agent: Option<String>,
}

/// The index of summaries kept in the sessions directory. Its extension
/// is not `jsonl`, so listings never mistake it for a session.
pub const INDEX_FILE: &str = ".index.json";

/// The index format; an index written with another is ignored.
const INDEX_VERSION: u32 = 1;

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
/// from the directory's [`INDEX_FILE`], and a listing that summarized
/// anything anew writes the index back.
#[derive(Default)]
pub struct SessionCache {
    by_path: HashMap<PathBuf, (u64, Option<SystemTime>, SessionSummary)>,
    /// Whether the index file was read for this cache yet.
    loaded: bool,
}

/// The on-disk form of a [`SessionCache`].
#[derive(serde::Serialize, serde::Deserialize)]
struct Index {
    version: u32,
    entries: Vec<IndexEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
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
        if !self.loaded {
            self.loaded = true;
            self.load_index(dir);
        }
        let read_dir = match std::fs::read_dir(dir) {
            Ok(d) => d,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(SessionError::Io {
                    path: dir.to_path_buf(),
                    source: err,
                });
            }
        };

        let mut summaries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut fresh = false;
        for entry in read_dir {
            let entry = entry.map_err(|err| SessionError::Io {
                path: dir.to_path_buf(),
                source: err,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(summary) = self.summarize(&path, &mut fresh) {
                summaries.push(summary);
            }
            seen.insert(path);
        }
        let before = self.by_path.len();
        self.by_path.retain(|path, _| seen.contains(path));
        if fresh || self.by_path.len() != before {
            self.save_index(dir);
        }
        summaries.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(summaries)
    }

    fn summarize(&mut self, path: &Path, fresh: &mut bool) -> Option<SessionSummary> {
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
        *fresh = true;
        self.by_path
            .insert(path.to_owned(), (size, modified, summary.clone()));
        Some(summary)
    }

    /// Seeds the memo from the directory's index. A missing, unreadable
    /// or other-version index seeds nothing.
    fn load_index(&mut self, dir: &Path) {
        let Ok(bytes) = std::fs::read(dir.join(INDEX_FILE)) else {
            return;
        };
        let Ok(index) = serde_json::from_slice::<Index>(&bytes) else {
            return;
        };
        if index.version != INDEX_VERSION {
            return;
        }
        for entry in index.entries {
            self.by_path.insert(
                entry.summary.path.clone(),
                (entry.size, entry.modified, entry.summary),
            );
        }
    }

    /// Writes the memo back as the directory's index, atomically, so a
    /// concurrent listing reads either the old index or the new one, and
    /// readable by the owner only, as it quotes prompts. A failed write
    /// only costs the next process a rescan.
    fn save_index(&self, dir: &Path) {
        let index = Index {
            version: INDEX_VERSION,
            entries: self
                .by_path
                .values()
                .map(|(size, modified, summary)| IndexEntry {
                    size: *size,
                    modified: *modified,
                    summary: summary.clone(),
                })
                .collect(),
        };
        let Ok(bytes) = serde_json::to_vec(&index) else {
            return;
        };
        let _ = kage_core::fsutil::atomic_write_private(&dir.join(INDEX_FILE), &bytes);
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

    // Head pass: the header, the agent marker and an early title.
    // Titles are appended when generated, so for a long session the
    // only one usually sits right after the first exchange.
    let mut header = None;
    let mut agent = None;
    let mut head_title = None;
    let mut agent_pending = true;
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
        if agent_pending && let Ok(entry) = serde_json::from_slice::<SessionEntry>(line) {
            agent_pending = false;
            if let SessionEntry::Custom(c) = entry
                && c.kind == AGENT_ENTRY_KIND
            {
                agent = Some(
                    c.data
                        .get("agent")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
        if leading_tag(line) == Some(b"title")
            && let Ok(SessionEntry::Title(t)) = serde_json::from_slice(line)
        {
            head_title = Some(t.title);
        }
    }
    let header = header?;

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
/// latest user prompt and the latest title, decoding only the lines
/// those need. Bounds the decode work for sessions whose tail is a
/// wall of tool output.
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
    let mut updated_at = None;
    let mut last_user_prompt = None;
    let mut title = None;
    let mut tail: Vec<Vec<u8>> = Vec::new();
    loop {
        buf.clear();
        let Ok(n) = file.read_until(b'\n', buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let line = buf.trim_ascii_end().to_owned();
        if !line.is_empty() {
            tail.push(line);
        }
    }
    for line in tail.iter().rev() {
        let tag = leading_tag(line);
        let wanted = updated_at.is_none()
            || tag.is_none()
            || (tag == Some(b"message") && last_user_prompt.is_none())
            || tag == Some(b"title");
        if !wanted {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<SessionEntry>(line) else {
            continue;
        };
        updated_at.get_or_insert_with(|| entry.ts());
        match entry {
            SessionEntry::Message(m)
                if m.message.role == kage_core::Role::User && last_user_prompt.is_none() =>
            {
                last_user_prompt = Some(
                    first_text(&m.message).map(|text| text.chars().take(PROMPT_CHARS).collect()),
                );
            }
            SessionEntry::Title(t) if title.is_none() => title = Some(t.title),
            _ => {}
        }
        if updated_at.is_some() && last_user_prompt.is_some() && title.is_some() {
            break;
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

    #[test]
    fn the_index_carries_summaries_across_caches_and_drops_deleted_files() {
        let dir = tempdir().unwrap();
        let a = write_session(dir.path(), "a.jsonl", "ask one");
        write_session(dir.path(), "b.jsonl", "ask two");
        let first = list(dir.path()).unwrap();
        let index = dir.path().join(INDEX_FILE);
        assert!(index.exists(), "a listing writes the index");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&index).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the index quotes prompts");
        }

        let mut fresh = SessionCache::default();
        fresh.load_index(dir.path());
        assert_eq!(fresh.by_path.len(), 2, "a new cache starts from the index");
        assert_eq!(fresh.list(dir.path()).unwrap(), first);

        std::fs::remove_file(&a).unwrap();
        let after = list(dir.path()).unwrap();
        assert_eq!(after.len(), 1);
        let mut reread = SessionCache::default();
        reread.load_index(dir.path());
        assert_eq!(reread.by_path.len(), 1, "the deleted file left the index");
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
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "explore".into(),
            parent_session: Some(SessionId::new()),
            parent_entry: None,
        };
        let marker = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({ "agent": "explore", "description": "map exports" }),
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
        assert_eq!(agent_of("late.jsonl"), None);
        assert_eq!(agent_of("plain.jsonl"), None);
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
        let mut index = 1;
        for entry in entries {
            index += 1;
            updated_at = entry.ts();
            match entry {
                SessionEntry::Custom(c) if index == 2 && c.kind == AGENT_ENTRY_KIND => {
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
            source: kage_core::ImageSource::Url {
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
